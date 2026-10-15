#!/usr/bin/env bash
# Linux integration test for the actual helper/interceptor with a fake driver.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
artifacts="${HAMI_ARTIFACT_DIR:-$root/target/debug}"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/vnpu-dcmi-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/lib64/driver"

cat > "$fixture/runtime.c" <<'C'
#include <stdint.h>
#include <stdlib.h>
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
int dcmi_get_device_resource_info(int card, int dev, struct memory *out, int *count) {
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
    return info(0, &free, &total) || free != 4096 || total != 8192;
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
dd if=/dev/zero of="$fixture/legacy" bs=2848 count=1 status=none
printf '\001' | dd of="$fixture/legacy" bs=1 seek=16 conv=notrunc status=none
set +e
env -u NPU_DCMI_HELPER_PROCESS LD_LIBRARY_PATH="$fixture${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    LD_PRELOAD="$artifacts/libvnpu.so" NPU_GLOBAL_SHM_PATH="$fixture/legacy" \
    NPU_LOCAL_SHM_PATH="$fixture/local-legacy" timeout 8 "$fixture/probe"
status=$?
set -e
test "$status" = 78
echo 'PASS: isolated query, no eager CANN, protocol, helper bypass, descendant reaping, legacy registry rejection'
