#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xpc/xpc.h>

#define MAX_FRAME (1024 * 1024)

typedef void (*senate_handle_message)(void *, const void *, size_t, void *);

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
    if (xpc_connection_set_peer_code_signing_requirement(listener, requirement) != 0) {
        xpc_connection_cancel(listener);
        xpc_release(listener);
        return NULL;
    }
    uid_t owner = geteuid();
    xpc_connection_set_event_handler(listener, ^(xpc_object_t event) {
        if (xpc_get_type(event) != XPC_TYPE_CONNECTION) return;
        xpc_connection_t peer = (xpc_connection_t)event;
        if (xpc_connection_get_euid(peer) != owner) {
            xpc_connection_cancel(peer);
            return;
        }
        xpc_connection_set_context(peer, xpc_retain(listener));
        xpc_connection_set_finalizer_f(peer, senate_release_connection);
        xpc_connection_set_event_handler(peer, ^(xpc_object_t message) {
            if (xpc_get_type(message) != XPC_TYPE_DICTIONARY) return;
            size_t len = 0;
            const void *bytes = xpc_dictionary_get_data(message, "senate", &len);
            if (!bytes || len > MAX_FRAME) return;
            xpc_object_t reply_object = xpc_dictionary_create_reply(message);
            if (!reply_object) return;
            senate_reply reply = {peer, reply_object};
            handler(context, bytes, len, &reply);
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
    xpc_object_t reply = xpc_connection_send_message_with_reply_sync(peer, message);
    xpc_release(message);
    void *result = NULL;
    if (reply && xpc_get_type(reply) == XPC_TYPE_DICTIONARY) {
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
    if (reply) xpc_release(reply);
    xpc_connection_cancel(peer);
    xpc_release(peer);
    return result;
}
