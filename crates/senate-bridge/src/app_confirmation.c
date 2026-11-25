#include <stdint.h>
#include <stdatomic.h>
#include <dispatch/dispatch.h>
#include <mach/message.h>
#include <ApplicationServices/ApplicationServices.h>
#include <Security/Security.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xpc/xpc.h>

#define MAX_FRAME (1024 * 1024)

extern void xpc_dictionary_get_audit_token(xpc_object_t, audit_token_t *);

bool senate_accessibility_trusted(void) {
    return AXIsProcessTrusted();
}

static SecRequirementRef senate_requirement(const char *expression) {
    CFStringRef source = CFStringCreateWithCString(NULL, expression, kCFStringEncodingUTF8);
    SecRequirementRef requirement = NULL;
    if (!source) return NULL;
    OSStatus result = SecRequirementCreateWithString(source, kSecCSDefaultFlags, &requirement);
    CFRelease(source);
    return result == errSecSuccess ? requirement : NULL;
}

static bool senate_copy_code_hash(SecStaticCodeRef code, uint8_t hash[20]) {
    CFDictionaryRef info = NULL;
    if (SecCodeCopySigningInformation(code, kSecCSSigningInformation, &info) != errSecSuccess) return false;
    CFDataRef identity = CFDictionaryGetValue(info, kSecCodeInfoUnique);
    bool valid = identity && CFGetTypeID(identity) == CFDataGetTypeID() && CFDataGetLength(identity) == 20;
    if (valid) memcpy(hash, CFDataGetBytePtr(identity), 20);
    CFRelease(info);
    return valid;
}

bool senate_code_hash_path(const char *path, const char *requirement, uint8_t hash[20]) {
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)path, strlen(path), false);
    SecStaticCodeRef code = NULL;
    if (!url) return false;
    OSStatus result = SecStaticCodeCreateWithPath(url, kSecCSDefaultFlags, &code);
    CFRelease(url);
    if (result != errSecSuccess) return false;
    SecRequirementRef rule = senate_requirement(requirement);
    bool valid = rule && SecStaticCodeCheckValidity(code, kSecCSStrictValidate, rule) == errSecSuccess
        && senate_copy_code_hash(code, hash);
    if (rule) CFRelease(rule);
    CFRelease(code);
    return valid;
}

bool senate_code_hash_guest(const uint32_t *audit, int32_t pid, const char *expression, uint8_t hash[20]) {
    CFTypeRef value = audit ? (CFTypeRef)CFDataCreate(NULL, (const UInt8 *)audit, sizeof(audit_token_t))
                            : (CFTypeRef)CFNumberCreate(NULL, kCFNumberSInt32Type, &pid);
    if (!value) return false;
    const void *key = audit ? kSecGuestAttributeAudit : kSecGuestAttributePid;
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, &key, &value, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFRelease(value);
    if (!attributes) return false;
    SecCodeRef guest = NULL;
    OSStatus result = SecCodeCopyGuestWithAttributes(NULL, attributes, kSecCSDefaultFlags, &guest);
    CFRelease(attributes);
    if (result != errSecSuccess) return false;
    SecRequirementRef rule = senate_requirement(expression);
    bool valid = rule && SecCodeCheckValidity(guest, kSecCSDefaultFlags, rule) == errSecSuccess
        && senate_copy_code_hash(guest, hash);
    if (rule) CFRelease(rule);
    CFRelease(guest);
    return valid;
}

bool senate_guest_team(int32_t pid, char *text, size_t size) {
    CFTypeRef value = CFNumberCreate(NULL, kCFNumberSInt32Type, &pid);
    if (!value) return false;
    const void *key = kSecGuestAttributePid;
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, &key, &value, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFRelease(value);
    if (!attributes) return false;
    SecCodeRef guest = NULL;
    OSStatus result = SecCodeCopyGuestWithAttributes(NULL, attributes, kSecCSDefaultFlags, &guest);
    CFRelease(attributes);
    if (result != errSecSuccess) return false;
    CFDictionaryRef info = NULL;
    bool valid = false;
    if (SecCodeCheckValidity(guest, kSecCSDefaultFlags, NULL) == errSecSuccess
        && SecCodeCopySigningInformation(guest, kSecCSSigningInformation, &info) == errSecSuccess) {
        CFStringRef team = CFDictionaryGetValue(info, kSecCodeInfoTeamIdentifier);
        valid = team && CFGetTypeID(team) == CFStringGetTypeID()
            && CFStringGetCString(team, text, size, kCFStringEncodingUTF8);
        CFRelease(info);
    }
    CFRelease(guest);
    return valid;
}

typedef void (*senate_handle_message)(void *, const void *, size_t, void *, const uint32_t *);

typedef struct {
    xpc_connection_t peer;
    xpc_object_t reply;
} senate_reply;

void senate_xpc_send_reply(void *opaque, const void *bytes, size_t len) {
    senate_reply *reply = opaque;
    if (len <= MAX_FRAME) {
        xpc_dictionary_set_data(reply->reply, "senate", bytes, len);
        xpc_connection_send_message(reply->peer, reply->reply);
    }
}

static void senate_release_connection(void *connection) {
    xpc_release(connection);
}

void *senate_xpc_listen(const char *name, const char *requirement, void *context,
                        void (*drop_context)(void *), senate_handle_message handler) {
    xpc_connection_t listener = xpc_connection_create_mach_service(name, NULL, XPC_CONNECTION_MACH_SERVICE_LISTENER);
    if (!listener) {
        drop_context(context);
        return NULL;
    }
    xpc_connection_set_context(listener, context);
    xpc_connection_set_finalizer_f(listener, drop_context);
    if (requirement && xpc_connection_set_peer_code_signing_requirement(listener, requirement) != 0) {
        xpc_connection_cancel(listener);
        xpc_release(listener);
        return NULL;
    }
    uid_t owner = geteuid();
    xpc_connection_set_event_handler(listener, ^(xpc_object_t event) {
        if (xpc_get_type(event) != XPC_TYPE_CONNECTION) return;
        xpc_connection_t peer = (xpc_connection_t)event;
        xpc_connection_set_context(peer, xpc_retain(listener));
        xpc_connection_set_finalizer_f(peer, senate_release_connection);
        xpc_connection_set_event_handler(peer, ^(xpc_object_t message) {
            if (xpc_get_type(message) != XPC_TYPE_DICTIONARY) return;
            size_t len = 0;
            const void *bytes = xpc_dictionary_get_data(message, "senate", &len);
            if (!bytes || len > MAX_FRAME) return;
            audit_token_t token = {0};
            xpc_dictionary_get_audit_token(message, &token);
            if (token.val[1] != owner || token.val[5] <= 1) return;
            xpc_object_t reply_object = xpc_dictionary_create_reply(message);
            if (!reply_object) return;
            senate_reply reply = {peer, reply_object};
            handler(context, bytes, len, &reply, token.val);
            xpc_release(reply_object);
        });
        xpc_connection_activate(peer);
    });
    xpc_connection_activate(listener);
    return listener;
}

void senate_xpc_stop(void *listener) {
    xpc_connection_cancel(listener);
    xpc_release(listener);
}

typedef struct {
    atomic_uint references;
    dispatch_semaphore_t done;
    xpc_object_t reply;
} senate_call;

static void release_call(senate_call *call) {
    if (atomic_fetch_sub_explicit(&call->references, 1, memory_order_acq_rel) != 1) return;
    if (call->reply) xpc_release(call->reply);
    dispatch_release(call->done);
    free(call);
}

void *senate_xpc_call(const char *name, const char *requirement, const void *bytes,
                      size_t len, size_t *reply_len) {
    *reply_len = 0;
    if (len > MAX_FRAME) return NULL;
    xpc_connection_t peer = xpc_connection_create_mach_service(name, NULL, 0);
    if (!peer) return NULL;
    if (xpc_connection_set_peer_code_signing_requirement(peer, requirement) != 0) {
        xpc_connection_cancel(peer);
        xpc_release(peer);
        return NULL;
    }
    xpc_connection_set_event_handler(peer, ^(xpc_object_t event) { (void)event; });
    xpc_connection_activate(peer);
    xpc_object_t message = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_data(message, "senate", bytes, len);
    senate_call *call = calloc(1, sizeof(*call));
    if (!call) {
        xpc_release(message);
        xpc_connection_cancel(peer);
        xpc_release(peer);
        return NULL;
    }
    call->done = dispatch_semaphore_create(0);
    atomic_init(&call->references, 2);
    // The completion owns its reference even when the caller times out.
    xpc_connection_send_message_with_reply(peer, message,
        dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0), ^(xpc_object_t reply) {
            call->reply = xpc_retain(reply);
            dispatch_semaphore_signal(call->done);
            release_call(call);
        });
    xpc_release(message);
    void *result = NULL;
    // A successful wait synchronizes with the sole writer of call->reply.
    if (dispatch_semaphore_wait(call->done,
            dispatch_time(DISPATCH_TIME_NOW, 1500 * NSEC_PER_MSEC)) == 0 &&
        xpc_get_type(call->reply) == XPC_TYPE_DICTIONARY) {
        xpc_object_t reply = call->reply;
        size_t size = 0;
        const void *data = xpc_dictionary_get_data(reply, "senate", &size);
        if (data && size <= MAX_FRAME) {
            result = malloc(size);
            if (result) {
                memcpy(result, data, size);
                *reply_len = size;
            }
        }
    }
    release_call(call);
    xpc_connection_cancel(peer);
    xpc_release(peer);
    return result;
}
