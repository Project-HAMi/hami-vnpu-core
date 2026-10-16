#define _DEFAULT_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sys/wait.h>
#include <unistd.h>

int aclrtSetDevice(int32_t);
int aclrtSetCurrentContext(void *);
int aclrtResetDevice(int32_t);
int aclrtResetDeviceForce(int32_t);
int aclrtSetDeviceResLimit(int32_t, int, uint32_t);
int aclrtResetDeviceResLimit(int32_t);
int aclrtSetStreamResLimit(void *, int, uint32_t);
int aclrtResetStreamResLimit(void *);
uint32_t fake_snapshot(int, int);
uint32_t fake_stream_snapshot(void *, int);
unsigned fake_get_calls(void);
int fake_initializing(void);
int fake_fork_status(void);
int rtSetDevice(int);
uint64_t rtMalloc(uint64_t, uint64_t, uint32_t, uint16_t);

static void show(int device) {
    printf("%u %u\n", fake_snapshot(device, 0), fake_snapshot(device, 1));
}
static void show_stream(void *stream) {
    printf("stream=%u/%u\n", fake_stream_snapshot(stream, 0), fake_stream_snapshot(stream, 1));
}
static void *select_many(void *unused) {
    (void)unused;
    for (int i = 0; i < 100; ++i) if (aclrtSetDevice(0)) abort();
    return 0;
}
static int fork_case(int initialized, int locked) {
    pthread_t thread;
    if (initialized && aclrtSetDevice(0)) return 1;
    if (locked) {
        if (pthread_create(&thread, 0, select_many, 0)) return 2;
        while (!fake_initializing()) usleep(1000);
    }
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) return 3;
    if (!pid) {
        if (aclrtSetDevice(0)) _exit(4);
        show(0); fflush(stdout); _exit(0);
    }
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) return 5;
    printf("child=%d\n", WEXITSTATUS(status));
    if (locked) pthread_join(thread, 0);
    if (!initialized && !locked) { if (aclrtSetDevice(0)) return 6; show(0); }
    return 0;
}
int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "basic";
    if (!strcmp(mode, "fork-before")) return fork_case(0, 0);
    if (!strcmp(mode, "fork-after")) return fork_case(1, 0);
    if (!strcmp(mode, "fork-locked")) return fork_case(0, 1);
    if (!strcmp(mode, "context")) {
        if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 1;
        show(1); return 0;
    }
    if (!strcmp(mode, "direct-runtime") || !strcmp(mode, "direct-runtime-switch")) {
        int device = !strcmp(mode, "direct-runtime-switch");
        if (device && aclrtSetDevice(0)) return 1;
        if (rtSetDevice(device)) return 2;
        uint64_t ptr = 0;
        return (int)rtMalloc((uint64_t)(uintptr_t)&ptr, 1, 0, 0);
    }
    if (aclrtSetDevice(-1) != 123 || aclrtSetDevice(0)) return 1;
    if (!strcmp(mode, "raw-switch-stream-set") || !strcmp(mode, "raw-switch-stream-reset")) {
        if (rtSetDevice(1)) return 2;
        void *device_zero_stream = (void *)(uintptr_t)1;
        if (!strcmp(mode, "raw-switch-stream-reset"))
            return aclrtResetStreamResLimit(device_zero_stream);
        return aclrtSetStreamResLimit(device_zero_stream, 1, 80);
    }
    if (!strcmp(mode, "fork-callback")) {
        show(0); printf("child=%d\n", fake_fork_status()); return 0;
    }
    if (!strcmp(mode, "initial")) { show(0); return 0; }
    if (!strcmp(mode, "concurrent")) {
        pthread_t threads[8];
        for (unsigned i = 0; i < 8; ++i) if (pthread_create(&threads[i], 0, select_many, 0)) return 2;
        for (unsigned i = 0; i < 8; ++i) pthread_join(threads[i], 0);
        show(0); return 0;
    }
    if (!strcmp(mode, "device-resource-reset")) {
        if (aclrtResetDeviceResLimit(0)) return 2;
        show(0); if (aclrtSetDevice(0)) return 3; show(0); return 0;
    }
    if (!strcmp(mode, "refcount-reset")) {
        if (aclrtSetDevice(0) || aclrtResetDevice(0)) return 2;
        show(0); if (aclrtSetDevice(0)) return 3; show(0); return 0;
    }
    if (!strcmp(mode, "force-reset")) {
        if (aclrtResetDeviceForce(0) || aclrtSetDevice(0)) return 2;
        show(0); return 0;
    }
    if (!strcmp(mode, "failed-reset")) {
        if (aclrtResetDeviceResLimit(0) != 43 || aclrtResetDevice(0) != 43) return 2;
        show(0); return 0;
    }
    if (!strcmp(mode, "stream-reset") || !strcmp(mode, "stream-set")) {
        void *stream = (void *)(uintptr_t)1;
        if (aclrtSetStreamResLimit(stream, 0, 100) || aclrtSetStreamResLimit(stream, 1, 100)) return 2;
        if (!strcmp(mode, "stream-reset") && aclrtResetStreamResLimit(stream)) return 3;
        show_stream(stream); return 0;
    }
    if (!strcmp(mode, "multi-device") || !strcmp(mode, "multi-stream")) {
        show(0); if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 2; show(1);
        if (!strcmp(mode, "multi-stream")) return aclrtSetStreamResLimit((void *)(uintptr_t)1, 1, 100);
        if (aclrtResetDeviceResLimit(0)) return 3;
        show(0); show(1); return 0;
    }
    show(0);
    if (aclrtSetDevice(0)) return 2;
    show(0);
    if (aclrtSetDeviceResLimit(0, 0, 100)) return 3;
    show(0);
    if (aclrtSetStreamResLimit(0, 1, 100)) return 4;
    printf("stream=%u\n", fake_stream_snapshot(0, 1));
    if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 5;
    show(1);
    if (aclrtResetDeviceForce(0) || aclrtSetDevice(0)) return 6;
    show(0);
    if (!strcmp(mode, "passthrough")) printf("gets=%u\n", fake_get_calls());
    return 0;
}
