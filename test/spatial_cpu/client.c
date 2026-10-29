#define _DEFAULT_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sys/wait.h>
#include <unistd.h>

int aclrtSetDevice(int32_t);
int aclrtCreateStream(void **);
int aclrtCreateStreamV2(void **, const void *);
int rtStreamCreateWithConfig(void **, void *);
int rtDeviceReset(int);
int rtDeviceResetEx(int);
int rtDeviceResetWithoutTsd(int);
int rtDeviceResetForce(int);
int aclrtCreateStreamWithConfig(void **, unsigned, unsigned);
int aclrtDestroyStream(void *);
int aclrtDestroyStreamForce(void *);
int aclrtCreateContext(void **, int);
int aclrtDestroyContext(void *);
int rtStreamCreate(void **, int);
int rtStreamCreateWithFlags(void **, int, unsigned);
int rtStreamDestroy(void *);
int rtStreamDestroyForce(void *);
int rtCtxCreate(void **, unsigned, int);
int rtCtxDestroy(void *);
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
unsigned fake_reference_count(int);
int fake_initializing(void);
int fake_destroy_pending(void);
void fake_continue_destroy(void);
int fake_fork_status(void);
int rtSetDevice(int);
uint64_t rtMalloc(uint64_t, uint64_t, uint32_t, uint16_t);

static void show(int device) {
    printf("%u %u\n", fake_snapshot(device, 0), fake_snapshot(device, 1));
}
static void show_stream(void *stream) {
    printf("stream=%u/%u\n", fake_stream_snapshot(stream, 0), fake_stream_snapshot(stream, 1));
}
static int reset_selected(void) {
    const char *api = getenv("FAKE_RESET_API");
    if (!api || !strcmp(api, "acl")) return aclrtResetDevice(0);
    if (!strcmp(api, "acl-force")) return aclrtResetDeviceForce(0);
    if (!strcmp(api, "runtime")) return rtDeviceReset(0);
    if (!strcmp(api, "ex")) return rtDeviceResetEx(0);
    if (!strcmp(api, "without-tsd")) return rtDeviceResetWithoutTsd(0);
    if (!strcmp(api, "force")) return rtDeviceResetForce(0);
    abort();
}
static void *select_many(void *unused) {
    (void)unused;
    for (int i = 0; i < 100; ++i) if (aclrtSetDevice(0)) abort();
    return 0;
}
static void *select_distinct(void *argument) {
    int device = (int)(uintptr_t)argument;
    for (unsigned i = 0; i < 100; ++i) {
        void *stream = 0;
        if (aclrtSetDevice(device) || aclrtCreateStream(&stream)
            || aclrtSetStreamResLimit(stream, 0, 100)
            || aclrtSetStreamResLimit(stream, 1, 100)) return (void *)(uintptr_t)1;
        if (fake_stream_snapshot(stream, 0) != (device ? 14u : 7u)
            || fake_stream_snapshot(stream, 1) != (device ? 27u : 14u)) return (void *)(uintptr_t)2;
        if (aclrtDestroyStream(stream)) return (void *)(uintptr_t)3;
    }
    return 0;
}
static void *destroy_old_stream(void *stream) {
    return (void *)(uintptr_t)aclrtDestroyStream(stream);
}
static void *destroy_old_context(void *context) {
    return (void *)(uintptr_t)aclrtDestroyContext(context);
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
    if (!strcmp(mode, "partial-stream-reset") || !strcmp(mode, "final-stream-reset")
        || !strcmp(mode, "explicit-context-reset")) {
        void *stream = 0, *context = 0;
        if (!strcmp(mode, "partial-stream-reset") && aclrtSetDevice(0)) return 2;
        if (!strcmp(mode, "explicit-context-reset") && aclrtCreateContext(&context, 0)) return 2;
        if (aclrtCreateStream(&stream)) return 2;
        int rc = reset_selected();
        if (getenv("FAKE_FAIL_RESET")) { if (rc != 43) return 3; }
        else if (rc) return 3;
        printf("references=%u\n", fake_reference_count(0)); fflush(stdout);
        if (aclrtSetStreamResLimit(stream, 0, 100) || aclrtSetStreamResLimit(stream, 1, 100)) return 4;
        show_stream(stream); return 0;
    }
    if (!strcmp(mode, "raw-switch-stream-set") || !strcmp(mode, "raw-switch-stream-reset")) {
        if (rtSetDevice(1)) return 2;
        if (aclrtSetDevice(0)) return 3;
        void *device_zero_stream = 0;
        if (aclrtCreateStream(&device_zero_stream) || rtSetDevice(1)) return 4;
        int rc = !strcmp(mode, "raw-switch-stream-reset")
            ? aclrtResetStreamResLimit(device_zero_stream)
            : aclrtSetStreamResLimit(device_zero_stream, 1, 80);
        if (rc) return 5;
        show_stream(device_zero_stream); return 0;
    }
    if (!strcmp(mode, "fork-callback")) {
        show(0); printf("child=%d\n", fake_fork_status()); return 0;
    }
    if (!strcmp(mode, "initial")) { show(0); return 0; }
    if (!strcmp(mode, "concurrent-multi-device")) {
        pthread_t threads[2];
        for (unsigned i = 0; i < 2; ++i)
            if (pthread_create(&threads[i], 0, select_distinct, (void *)(uintptr_t)i)) return 2;
        for (unsigned i = 0; i < 2; ++i) {
            void *result;
            if (pthread_join(threads[i], &result) || result) return 3;
        }
        show(0); show(1); return 0;
    }
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
        void *stream = 0;
        if (aclrtCreateStream(&stream)) return 1;
        if (aclrtSetStreamResLimit(stream, 0, 100) || aclrtSetStreamResLimit(stream, 1, 100)) return 2;
        if (!strcmp(mode, "stream-reset") && aclrtResetStreamResLimit(stream)) return 3;
        show_stream(stream); return 0;
    }
    if (!strcmp(mode, "stream-destroy-reuse-race") || !strcmp(mode, "context-destroy-reuse-race")) {
        int is_context = !strcmp(mode, "context-destroy-reuse-race");
        void *context = 0, *stream = 0, *next_context = 0, *next = 0;
        pthread_t destroyer;
        if (aclrtCreateContext(&context, 0) || aclrtCreateStream(&stream)) return 2;
        if (pthread_create(&destroyer, 0, is_context ? destroy_old_context : destroy_old_stream,
                           is_context ? context : stream)) return 3;
        while (!fake_destroy_pending()) usleep(1000);
        if (aclrtCreateContext(&next_context, 1) || aclrtCreateStream(&next)) return 4;
        if (next != stream || (is_context && next_context != context)) return 5;
        fake_continue_destroy();
        void *result;
        if (pthread_join(destroyer, &result) || result) return 6;
        if (aclrtSetStreamResLimit(next, 0, 100) || aclrtSetStreamResLimit(next, 1, 100)) return 7;
        show_stream(next); return 0;
    }
    if (!strcmp(mode, "acl-stream-v2") || !strcmp(mode, "runtime-stream-config")) {
        void *stream = 0;
        if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 2;
        int rc = !strcmp(mode, "acl-stream-v2")
            ? aclrtCreateStreamV2(&stream, 0) : rtStreamCreateWithConfig(&stream, 0);
        if (rc || aclrtSetStreamResLimit(stream, 0, 100)
            || aclrtSetStreamResLimit(stream, 1, 100)) return 3;
        show_stream(stream); return 0;
    }
    if (!strncmp(mode, "runtime-reset-", 14)) {
        void *stream = 0;
        if (aclrtCreateStream(&stream)) return 2;
        int rc = !strcmp(mode, "runtime-reset-ex") ? rtDeviceResetEx(0)
            : !strcmp(mode, "runtime-reset-without-tsd") ? rtDeviceResetWithoutTsd(0)
            : !strcmp(mode, "runtime-reset-force") ? rtDeviceResetForce(0) : rtDeviceReset(0);
        if (getenv("FAKE_FAIL_RESET")) {
            if (rc != 43) return 3;
            if (aclrtSetStreamResLimit(stream, 0, 100) || aclrtSetStreamResLimit(stream, 1, 100)) return 4;
            show_stream(stream); return 0;
        }
        if (rc) return 3;
        if (strstr(mode, "reapply")) {
            if (aclrtSetDevice(0)) return 4;
            show(0); return 0;
        }
        return aclrtSetStreamResLimit(stream, 0, 100);
    }
    if (!strcmp(mode, "unknown-stream")) {
        if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 2;
        return aclrtSetStreamResLimit((void *)(uintptr_t)127, 1, 100);
    }
    if (!strcmp(mode, "multi-default-stream")) {
        if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 2;
        if (aclrtSetStreamResLimit(0, 0, 100) || aclrtSetStreamResLimit(0, 1, 100)) return 3;
        show_stream(0); return 0;
    }
    if (!strcmp(mode, "multi-stream")) {
        void *zero = 0, *one = 0;
        if (aclrtCreateStream(&zero) || aclrtSetCurrentContext((void *)(uintptr_t)1)
            || aclrtCreateStreamWithConfig(&one, 0, 0)) return 2;
        if (aclrtSetStreamResLimit(zero, 0, 100) || aclrtSetStreamResLimit(zero, 1, 100)
            || aclrtSetStreamResLimit(one, 0, 100) || aclrtSetStreamResLimit(one, 1, 100)) return 3;
        show_stream(zero); show_stream(one); return 0;
    }
    if (!strncmp(mode, "lifecycle-", 10)) {
        int runtime = strstr(mode, "runtime") != 0;
        void *stream = 0, *context = 0;
        int rc = runtime ? rtCtxCreate(&context, 0, 0) : aclrtCreateContext(&context, 0);
        if (rc) return rc;
        rc = runtime ? rtStreamCreateWithFlags(&stream, 0, 0) : aclrtCreateStream(&stream);
        if (rc) return rc;
        if (strstr(mode, "failed-destroy")) {
            int failure = strstr(mode, "context-failed-destroy")
                ? (runtime ? rtCtxDestroy(context) : aclrtDestroyContext(context))
                : (runtime ? rtStreamDestroy(stream) : aclrtDestroyStream(stream));
            if (failure != 42) return 3;
        } else if (strstr(mode, "context-destroy")) {
            if (runtime ? rtCtxDestroy(context) : aclrtDestroyContext(context)) return 3;
        } else if (strstr(mode, "reset")) {
            if (aclrtResetDeviceForce(0)) return 3;
        } else {
            if (runtime ? rtStreamDestroyForce(stream) : aclrtDestroyStreamForce(stream)) return 3;
        }
        if (strstr(mode, "reuse")) {
            void *next = 0;
            if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 4;
            rc = runtime ? rtStreamCreate(&next, 0) : aclrtCreateStream(&next);
            if (rc || next != stream) return 5;
            stream = next;
        }
        if (aclrtSetStreamResLimit(stream, 0, 100) || aclrtSetStreamResLimit(stream, 1, 100)) return 6;
        show_stream(stream); return 0;
    }
    if (!strcmp(mode, "create-context-only")) {
        void *context = 0;
        if (aclrtCreateContext(&context, 1)) return 2;
        show(1); return 0;
    }
    if (!strcmp(mode, "create-error")) {
        void *stream = (void *)(uintptr_t)123;
        if (aclrtCreateStream(&stream) != 41 || stream != (void *)(uintptr_t)123) return 2;
        show(0); return 0;
    }
    if (!strcmp(mode, "multi-device")) {
        show(0); if (aclrtSetCurrentContext((void *)(uintptr_t)1)) return 2; show(1);
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
