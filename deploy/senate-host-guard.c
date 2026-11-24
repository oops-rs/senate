/*
 * The setuid launcher performs one privileged operation: put itself into the
 * root-owned cgroup for a Senate host. It executes the user's program only
 * after relinquishing every root UID. It never invokes a shell or uses PATH
 * while privileged.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <sys/prctl.h>
#include <sys/vfs.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static const char *scope_mount = "/sys/fs/cgroup";

static void fail(const char *operation) {
    perror(operation);
    _exit(1);
}

static bool host_id(const char *name) {
    const char *alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    return strlen(name) == 26 && strspn(name, alphabet) == 26;
}

static void check_directory(int fd) {
    struct stat stat;
    if (fstat(fd, &stat) != 0) fail("stat guard scope");
    if (!S_ISDIR(stat.st_mode) || stat.st_uid != 0 || (stat.st_mode & 0022)) {
        errno = EPERM;
        fail("untrusted guard scope");
    }
}

static int child_directory(int parent, const char *name) {
    if (mkdirat(parent, name, 0755) && errno != EEXIST) fail("create guard scope");
    int child = openat(parent, name, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (child < 0) fail("open guard scope");
    check_directory(child);
    return child;
}

static void check_tasks_file(int directory) {
    int tasks = openat(directory, "cgroup.procs", O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (tasks < 0) fail("inspect cgroup.procs");
    struct stat st;
    if (fstat(tasks, &st) != 0) fail("stat cgroup.procs");
    if (st.st_uid != 0 || (st.st_mode & 0022)) {
        errno = EPERM;
        fail("untrusted cgroup migration permissions");
    }
    close(tasks);
}

int main(int argc, char **argv) {
    uid_t user = getuid();
    gid_t group = getgid();
    if (argc < 3 || !host_id(argv[1]) || argv[2][0] != '/' || user == 0 || geteuid() != 0) {
        fputs("invalid Senate guarded launch\n", stderr);
        return 64;
    }

    int mount = open(scope_mount, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (mount < 0) fail("open cgroup mount");
    check_directory(mount);
    struct statfs fs;
    if (fstatfs(mount, &fs) != 0) fail("identify guard filesystem");
    if (fs.f_type != CGROUP2_SUPER_MAGIC) {
        errno = EPERM;
        fail("guard root is not cgroup v2");
    }
    check_tasks_file(mount);
    int root = child_directory(mount, "senate-hosted");
    check_tasks_file(root);
    close(mount);
    char uid[32];
    snprintf(uid, sizeof uid, "%lu", (unsigned long)user);
    int account = child_directory(root, uid);
    check_tasks_file(account);
    int scope = child_directory(account, argv[1]);
    check_tasks_file(scope);
    int tasks = openat(scope, "cgroup.procs", O_WRONLY | O_CLOEXEC | O_NOFOLLOW);
    if (tasks < 0) fail("open cgroup.procs");
    char self[32];
    int count = snprintf(self, sizeof self, "%ld\n", (long)getpid());
    if (write(tasks, self, (size_t)count) != count) fail("enter guard scope");
    close(tasks);
    close(scope);
    close(account);
    close(root);

    if (prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0) != 0) fail("clear preserved capabilities");
    if (setresgid(group, group, group) != 0) fail("drop group credentials");
    if (setresuid(user, user, user) != 0) fail("drop root credentials");
    uid_t real, effective, saved;
    if (getresuid(&real, &effective, &saved) != 0 ||
        real != user || effective != user || saved != user) {
        errno = EPERM;
        fail("verify dropped root credentials");
    }
    gid_t real_group, effective_group, saved_group;
    if (getresgid(&real_group, &effective_group, &saved_group) != 0 ||
        real_group != group || effective_group != group || saved_group != group) {
        errno = EPERM;
        fail("verify dropped group credentials");
    }
    execv(argv[2], &argv[2]);
    fail("exec guarded program");
}
