// The unified log's API is macros (os_log_with_type), which Rust cannot
// call; these two functions are the whole bridge. Messages are logged as
// %{public}s: the core logs no secrets, and a private string would show as
// <private> in Console.
#include <os/log.h>
#include <stdint.h>

void *senate_os_log_create(const char *subsystem, const char *category) {
    return (void *)os_log_create(subsystem, category);
}

void senate_os_log(void *log, uint8_t type, const char *message) {
    os_log_with_type((os_log_t)log, (os_log_type_t)type, "%{public}s", message);
}
