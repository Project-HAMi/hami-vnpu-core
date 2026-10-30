#!/usr/bin/env bash
# Linux integration test for the actual helper/interceptor with a fake driver.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
artifacts="${HAMI_ARTIFACT_DIR:-$root/target/debug}"
export NPU_COMPUTE_LIMIT=30 NPU_MEMORY_LIMIT_MB=4096 NPU_SCHED_POLICY=dynamic
fixture="$(mktemp -d "${TMPDIR:-/tmp}/vnpu-dcmi-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/lib64/driver"

cat > "$fixture/runtime.c" <<'C'
#include <stdint.h>
#include <stdlib.h>
int rtGetDevice(int *device) { *device = 0; return 0; }
int rtGetDevicePhyIdByIndex(unsigned device, unsigned *physical) { *physical = device; return 0; }
int rtStreamCreate(uint64_t *stream, int priority) { (void)priority; *stream = 1; return 0; }
size_t rtEventCreate(uint64_t *event) { *event = 1; return 0; }
__attribute__((constructor)) static void init(void) {
    if (getenv("VNPU_EXPECT_NO_RUNTIME")) _Exit(99);
}
uint64_t rtMemGetInfoEx(uint64_t kind, size_t *free, size_t *total) {
    (void)kind; *free = 4096; *total = 8192; return 0;
}
C
cat > "$fixture/driver.c" <<'C'
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
struct memory { int pid; uint64_t bytes; };
int dcmi_init(void) {
    /* Exercise the vendor fork/exit shape in the fresh process. */
    pid_t child = fork();
    if (child < 0) return -1;
    if (child == 0) {
        if (getenv("VNPU_HANG_QUERY")) {
            if (getenv("VNPU_DETACH_CHILD")) setsid();
            for (;;) pause();
        }
        exit(0);
    }
    const char *pidfile = getenv("VNPU_QUERY_PIDS");
    if (pidfile) {
        FILE *out = fopen(pidfile, "w");
        if (!out) return -1;
        fprintf(out, "%d %d\n", getpid(), child); fclose(out);
    }
    int status;
    if (waitpid(child, &status, 0) != child || status) return -1;
    printf("driver diagnostic on stdout\n"); fflush(stdout);
    return 0;
}
int dcmi_get_card_list(int *count, int *cards, int capacity) {
    if (capacity < 1) return -1;
    cards[0] = 0; *count = getenv("VNPU_BAD_COUNT") ? 9 : 1; return 0;
}
int dcmi_get_card_id_device_id_from_phyid(int *card, int *dev, unsigned physical) {
    if (physical == 0) { *card = 0; *dev = 0; return 0; }
    if (physical == 7) { *card = 42; *dev = 2; return 0; }
    return -1;
}
int dcmi_get_device_resource_info(int card, int dev, struct memory *out, int *count) {
    if (card == 42 && dev == 2 && *count >= 1) {
        out[0].pid = 123; out[0].bytes = 7007; *count = 1; return 0;
    }
    if (card || dev || *count < 1) return -1;
    out[0].pid = 123; out[0].bytes = 4096; *count = 1; return 0;
}
C
cat > "$fixture/launch.c" <<'C'
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    char parent[32]; snprintf(parent, sizeof(parent), "%d", getpid());
    pid_t child = fork();
    if (child < 0) return 3;
    if (!child) {
        if (setpgid(0, 0)) _exit(4);
        execl(argv[1], argv[1], parent, (char *)0); _exit(5);
    }
    int status;
    while (waitpid(child, &status, 0) < 0) if (errno != EINTR) return 6;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 7;
}
C
cat > "$fixture/probe.c" <<'C'
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>
int main(void) {
    uint64_t (*info)(uint64_t, size_t *, size_t *) = dlsym(RTLD_DEFAULT, "rtMemGetInfoEx");
    if (!info) return 2;
    size_t free = 0, total = 0;
    if (info(0, &free, &total)) return 3;
    if (getenv("NPU_DCMI_HELPER_PROCESS")) return free != 4096 || total != 8192;
    return total != (size_t)4096 * 1024 * 1024;
}
C
cc -shared -fPIC "$fixture/runtime.c" -o "$fixture/libruntime.so"
cc -shared -fPIC "$fixture/driver.c" -o "$fixture/lib64/driver/libdcmi.so"
cc "$fixture/launch.c" -o "$fixture/launch"
cc "$fixture/probe.c" -ldl -o "$fixture/probe"

if readelf -d "$artifacts/libvnpu.so" | grep -E 'NEEDED.*lib(runtime|dcmi)'; then
    echo 'interceptor must not initialize CANN in the query process' >&2
    exit 1
fi
env LD_LIBRARY_PATH="$fixture${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    LD_PRELOAD="$artifacts/libvnpu.so" NPU_DCMI_HELPER_PROCESS=1 \
    ASCEND_DRIVER_HOME="$fixture" VNPU_EXPECT_NO_RUNTIME=1 \
    timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > "$fixture/snapshot"
printf 'VNPU_DCMI_READY\nVNPU_DCMI_1\n0 123 4096\n' > "$fixture/expected"
cmp "$fixture/expected" "$fixture/snapshot"

if env LD_LIBRARY_PATH="$fixture${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    LD_PRELOAD="$artifacts/libvnpu.so" NPU_DCMI_HELPER_PROCESS=1 \
    ASCEND_DRIVER_HOME="$fixture" VNPU_BAD_COUNT=1 \
    timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > /dev/null; then
    echo 'helper accepted an invalid driver count' >&2
    exit 1
fi

# A physical ID is not the card-list position or the card/chip pair.
env LD_LIBRARY_PATH="$fixture" LD_PRELOAD="$artifacts/libvnpu.so" \
    NPU_DCMI_HELPER_PROCESS=1 NPU_DCMI_PHYSICAL_DEVICE=7 ASCEND_DRIVER_HOME="$fixture" \
    VNPU_BAD_COUNT=1 timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > "$fixture/targeted"
printf 'VNPU_DCMI_READY\nVNPU_DCMI_1\n0 123 7007\n' > "$fixture/targeted-expected"
cmp "$fixture/targeted-expected" "$fixture/targeted"
if env NPU_DCMI_HELPER_PROCESS=1 NPU_DCMI_PHYSICAL_DEVICE=8 ASCEND_DRIVER_HOME="$fixture" \
    timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > /dev/null; then
    echo 'missing physical device incorrectly fell back to card zero' >&2
    exit 1
fi

# Even an incidental intercepted API in a helper must not start a manager or
# register a worker. Its real runtime symbol is still forwarded correctly.
env LD_LIBRARY_PATH="$fixture${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    LD_PRELOAD="$artifacts/libvnpu.so" NPU_DCMI_HELPER_PROCESS=1 \
    NPU_GLOBAL_SHM_PATH="$fixture/global" NPU_LOCAL_SHM_PATH="$fixture/local" \
    timeout 5 "$fixture/probe"
test ! -e "$fixture/global"
test ! -e "$fixture/local"
# The real supervisor must reap query descendants even when container PID 1
# does not do it. Include a driver child that escapes the query process group.
for detached in 0 1; do
    extra=()
    if test "$detached" = 1; then extra=(VNPU_DETACH_CHILD=1); fi
    if env LD_LIBRARY_PATH="$fixture${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        LD_PRELOAD="$artifacts/libvnpu.so" NPU_DCMI_HELPER_PROCESS=1 \
        ASCEND_DRIVER_HOME="$fixture" VNPU_HANG_QUERY=1 VNPU_QUERY_PIDS="$fixture/pids" \
        "${extra[@]}" timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > /dev/null; then
        echo 'helper accepted a hung driver query' >&2
        exit 1
    fi
    read -r query descendant < "$fixture/pids"
    test ! -e "/proc/$query"
    test ! -e "/proc/$descendant"
done

# An incompatible legacy registry must terminate the application, not allow
# the old worker-startup timeout to fall back to an unlimited stub.
# Reject the original queue and fenced protocols without per-worker entry stamps.
for marker in '\001\000\000\000' '\062\120\116\126' '\063\120\116\126'; do
dd if=/dev/zero of="$fixture/legacy.device-0" bs=2848 count=1 status=none
printf '%b' "$marker" | dd of="$fixture/legacy.device-0" bs=1 seek=16 conv=notrunc status=none
set +e
env -u NPU_DCMI_HELPER_PROCESS LD_LIBRARY_PATH="$fixture${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    LD_PRELOAD="$artifacts/libvnpu.so" NPU_GLOBAL_SHM_PATH="$fixture/legacy" \
    NPU_LOCAL_SHM_PATH="$fixture/local-legacy" timeout 8 "$fixture/probe"
status=$?
set -e
test "$status" = 78
done
echo 'PASS: isolated query, no eager CANN, protocol, helper bypass, descendant reaping, legacy registry rejection'

# Simulate kernels without CONFIG_PROC_CHILDREN, and an unreadable fallback.
# Interpose only procfs discovery, leaving waitpid and the query processes real.
cat > "$fixture/proc-fault.c" <<'C'
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>
static int deny(const char *path) {
    const char *mode = getenv("VNPU_PROC_FAULT");
    if (!mode) return 0;
    int children = !strncmp(path, "/proc/self/task/", 16) && strstr(path, "/children");
    int directory = !strcmp(mode, "all") && !strcmp(path, "/proc");
    if (!children && !directory) return 0;
    const char *marker = getenv("VNPU_PROC_FAULT_HIT");
    if (marker) {
        int fd = syscall(SYS_openat, AT_FDCWD, marker, O_WRONLY | O_CREAT | O_APPEND, 0600);
        if (fd >= 0) { write(fd, directory ? "d" : "c", 1); close(fd); }
    }
    errno = children ? ENOENT : EACCES;
    return 1;
}
#define WRAP_OPEN(name) \
int name(const char *path, int flags, ...) { \
    mode_t mode = 0; \
    if ((flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE) { \
        va_list args; va_start(args, flags); mode = va_arg(args, mode_t); va_end(args); \
    } \
    if (deny(path)) return -1; \
    int (*real)(const char *, int, ...) = dlsym(RTLD_NEXT, #name); \
    return real(path, flags, mode); \
}
WRAP_OPEN(open)
WRAP_OPEN(open64)
DIR *opendir(const char *path) {
    if (deny(path)) return NULL;
    DIR *(*real)(const char *) = dlsym(RTLD_NEXT, "opendir");
    return real(path);
}
C
cc -shared -fPIC "$fixture/proc-fault.c" -ldl -o "$fixture/proc-fault.so"
for fault in children all; do
    rm -f "$fixture/fault-hit"
    env LD_PRELOAD="$fixture/proc-fault.so:$artifacts/libvnpu.so" \
        NPU_DCMI_HELPER_PROCESS=1 ASCEND_DRIVER_HOME="$fixture" \
        VNPU_PROC_FAULT="$fault" VNPU_PROC_FAULT_HIT="$fixture/fault-hit" \
        VNPU_QUERY_PIDS="$fixture/pids" \
        timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > "$fixture/snapshot"
    cmp "$fixture/expected" "$fixture/snapshot"
    grep -q c "$fixture/fault-hit"
    if test "$fault" = all; then grep -q d "$fixture/fault-hit"; fi
    read -r query descendant < "$fixture/pids"
    test ! -e "/proc/$query"
    test ! -e "/proc/$descendant"
done
if env LD_PRELOAD="$fixture/proc-fault.so:$artifacts/libvnpu.so" \
    NPU_DCMI_HELPER_PROCESS=1 ASCEND_DRIVER_HOME="$fixture" \
    VNPU_PROC_FAULT=children VNPU_HANG_QUERY=1 VNPU_DETACH_CHILD=1 \
    VNPU_QUERY_PIDS="$fixture/pids" \
    timeout 5 "$fixture/launch" "$artifacts/vnpu-dcmi" > /dev/null; then
    echo 'helper accepted a hung query without proc children' >&2
    exit 1
fi
read -r query descendant < "$fixture/pids"
test ! -e "/proc/$query"
test ! -e "/proc/$descendant"

# Worker validation must run before opening even an existing local mapping.
for alias in same symlink hardlink missing; do
    global="$fixture/path-global-$alias"
    local="$global"
    if test "$alias" != missing; then
        printf 'unchanged registry' > "$global"
        cp "$global" "$fixture/unchanged"
    fi
    if test "$alias" = symlink; then local="$fixture/path-symlink"; ln -s "$global" "$local"; fi
    if test "$alias" = hardlink; then local="$fixture/path-hardlink"; ln "$global" "$local"; fi
    set +e
    env -u NPU_DCMI_HELPER_PROCESS LD_LIBRARY_PATH="$fixture" LD_PRELOAD="$artifacts/libvnpu.so" \
        NPU_GLOBAL_SHM_PATH="$global" NPU_LOCAL_SHM_PATH="$local" \
        timeout 2 "$fixture/probe"
    status=$?
    set -e
    test "$status" = 78
    if test "$alias" = missing; then test ! -e "$global"; else cmp "$global" "$fixture/unchanged"; fi
done

# Use the actual interceptor and helper after chdir both before manager startup
# and between successful periodic queries. No test-only helper-path export.
cat > "$fixture/chdir-probe.c" <<'C'
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static int wait_snapshots(const char *path, int expected) {
    for (int i = 0; i < 100; ++i) {
        FILE *log = fopen(path, "r");
        int count = 0;
        if (log) {
            char line[1024];
            while (fgets(line, sizeof(line), log)) {
                if (strstr(line, "DCMI snapshot received: 1 records")) ++count;
            }
            fclose(log);
        }
        if (count >= expected) return 0;
        usleep(100000);
    }
    return 1;
}
int main(int argc, char **argv) {
    if (argc != 2 || chdir("/")) return 2;
    uint64_t (*info)(uint64_t, size_t *, size_t *) = dlsym(RTLD_DEFAULT, "rtMemGetInfoEx");
    if (!info) return 3;
    size_t free, total;
    if (info(0, &free, &total) || wait_snapshots(argv[1], 1)) return 4;
    if (chdir("/tmp") || wait_snapshots(argv[1], 2)) return 5;
    return 0;
}
C
cc "$fixture/chdir-probe.c" -ldl -o "$fixture/chdir-probe"
mkdir "$fixture/relative"
cp "$artifacts/libvnpu.so" "$artifacts/vnpu-dcmi" "$fixture/relative/"
for mode in adjacent override; do
    extra=()
    if test "$mode" = override; then extra=(NPU_DCMI_HELPER_PATH=./relative/vnpu-dcmi); fi
    (
        cd "$fixture"
        env -u NPU_DCMI_HELPER_PROCESS -u NPU_DCMI_HELPER_PATH \
            LD_LIBRARY_PATH="$fixture" LD_PRELOAD=./relative/libvnpu.so \
            NPU_GLOBAL_SHM_PATH="$fixture/chdir-global-$mode" \
            NPU_LOCAL_SHM_PATH="$fixture/chdir-local-$mode" NPU_MEMORY_LIMIT_MB=4096 \
            ASCEND_DRIVER_HOME="$fixture" RUST_LOG=limiter::manager=debug \
            "${extra[@]}" timeout 25 "$fixture/chdir-probe" "$fixture/chdir-log-$mode" \
            2> "$fixture/chdir-log-$mode"
    )
done
echo 'PASS: procfs fallback/reaping, shared-memory aliases, relative preload/helper after chdir'
