/* CANN-free backend for the actual Rust libvnpu.so ABI tests. */
#define _DEFAULT_SOURCE
#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static uint32_t physical[2][2], caps[2][2], streams[2][2];
static unsigned references[2], initialized, gets, sets;
static _Thread_local int current;
static _Atomic int initializing;
static int fork_status = -1;
static int forked_in_set;

int aclrtSetDevice(int device);
static void fork_callback(void) {
    pid_t pid = fork();
    if (pid < 0) abort();
    if (!pid) _exit(aclrtSetDevice(0) ? 79 : 0);
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) abort();
    fork_status = WEXITSTATUS(status);
}
int fake_fork_status(void) { return fork_status; }

static uint32_t number(const char *name, uint32_t fallback) {
    const char *value = getenv(name);
    return value ? (uint32_t)strtoull(value, 0, 10) : fallback;
}
static void init(void) {
    if (initialized) return;
    physical[0][0] = number("FAKE_CUBE", 20);
    physical[0][1] = number("FAKE_VECTOR", 40);
    physical[1][0] = number("FAKE_CUBE1", physical[0][0]);
    physical[1][1] = number("FAKE_VECTOR1", physical[0][1]);
    for (int d = 0; d < 2; ++d)
        for (int r = 0; r < 2; ++r) caps[d][r] = streams[d][r] = physical[d][r];
    initialized = 1;
}
uint32_t fake_snapshot(int device, int type) { init(); return caps[device][type]; }
unsigned fake_get_calls(void) { return gets; }
int fake_initializing(void) { return atomic_load(&initializing); }
static int stream_device(void *stream) { return stream ? (int)(uintptr_t)stream - 1 : current; }
uint32_t fake_stream_snapshot(void *stream, int type) { init(); return streams[stream_device(stream)][type]; }

int aclrtSetDevice(int device) {
    if (device < 0 || device > 1) return 123;
    if (getenv("FAKE_FORK_IN_SET") && !forked_in_set) {
        forked_in_set = 1;
        fork_callback();
    }
    init(); current = device; ++references[device]; return 0;
}
int aclrtGetDevice(int *device) {
    if (getenv("FAKE_FAIL_CURRENT")) return 39;
    *device = current; return 0;
}
int aclrtSetCurrentContext(void *context) {
    int device = (int)(uintptr_t)context;
    if (device < 0 || device > 1) return 123;
    init(); current = device; return 0;
}
#ifndef OMIT_GET
int aclrtGetDeviceResLimit(int device, int type, uint32_t *value) {
    init(); ++gets;
    if (getenv("FAKE_FAIL_GET") || device < 0 || device > 1 || type < 0 || type > 1) return 39;
    if (getenv("FAKE_FORK_IN_GET") && gets == 1) fork_callback();
    if (getenv("FAKE_SLOW_GET") && gets == 1) {
        atomic_store(&initializing, 1); usleep(500000);
    }
    *value = caps[device][type];
    if (getenv("FAKE_BAD_READBACK") && sets) ++*value;
    return 0;
}
#endif
#ifndef OMIT_SET
int aclrtSetDeviceResLimit(int device, int type, uint32_t value) {
    init(); ++sets;
    if (getenv("FAKE_FAIL_SET")) return 37;
    if (!getenv("FAKE_IGNORE_SET")) caps[device][type] = value;
    return 0;
}
#endif
int aclrtResetDeviceResLimit(int device) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    for (int r = 0; r < 2; ++r) caps[device][r] = physical[device][r];
    return 0;
}
#ifndef OMIT_STREAM_GET
int aclrtGetStreamResLimit(void *stream, int type, uint32_t *value) {
    init(); ++gets;
    if (getenv("FAKE_FAIL_STREAM_GET")) return 39;
    *value = streams[stream_device(stream)][type]; return 0;
}
#endif
int aclrtSetStreamResLimit(void *stream, int type, uint32_t value) {
    init(); if (getenv("FAKE_FAIL_STREAM_SET")) return 37;
    if (!getenv("FAKE_IGNORE_STREAM_SET")) streams[stream_device(stream)][type] = value;
    return 0;
}
int aclrtResetStreamResLimit(void *stream) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    int device = stream_device(stream);
    for (int r = 0; r < 2; ++r) streams[device][r] = physical[device][r];
    return 0;
}
int aclrtResetDevice(int device) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    if (references[device]) --references[device];
    if (!references[device])
        for (int r = 0; r < 2; ++r) caps[device][r] = streams[device][r] = physical[device][r];
    return 0;
}
int aclrtResetDeviceForce(int device) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    references[device] = 0;
    for (int r = 0; r < 2; ++r) caps[device][r] = streams[device][r] = physical[device][r];
    return 0;
}

/* Direct-runtime entry probes stop before creating any runtime resource. */
int rtSetDevice(int device) {
    if (device < 0 || device > 1) return 123;
    init(); current = device; return 0;
}
int rtGetDevice(int *device) {
    if (getenv("FAKE_STOP_RUNTIME")) {
        uint32_t cube = number("FAKE_EXPECT_CUBE", 7), vector = number("FAKE_EXPECT_VECTOR", 14);
        _exit(caps[current][0] == cube && caps[current][1] == vector ? 97 : 98);
    }
    *device = current; return 0;
}
uint64_t rtMalloc(uint64_t ptr, uint64_t size, uint32_t type, uint16_t module) {
    (void)ptr; (void)size; (void)type; (void)module;
    _exit(99); /* The SchedulerClient initialization probe should stop first. */
}
