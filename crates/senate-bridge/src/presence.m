#import <Foundation/Foundation.h>
#import <LocalAuthentication/LocalAuthentication.h>

bool senate_device_owner_authenticated(const char *reason) {
    @autoreleasepool {
        NSString *text = [NSString stringWithUTF8String:reason];
        if (!text.length) return false;
        LAContext *context = [[LAContext alloc] init];
        context.touchIDAuthenticationAllowableReuseDuration = 0;
        if (![context canEvaluatePolicy:LAPolicyDeviceOwnerAuthentication error:nil]) return false;
        dispatch_semaphore_t finished = dispatch_semaphore_create(0);
        __block BOOL accepted = NO;
        [context evaluatePolicy:LAPolicyDeviceOwnerAuthentication localizedReason:text reply:^(BOOL success, NSError *error) {
            (void)error;
            accepted = success;
            dispatch_semaphore_signal(finished);
        }];
        dispatch_semaphore_wait(finished, DISPATCH_TIME_FOREVER);
        return accepted;
    }
}
