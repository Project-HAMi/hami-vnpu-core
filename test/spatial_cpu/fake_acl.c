/* CANN-free backend for the actual Rust libvnpu.so ABI tests. */
#define _DEFAULT_SOURCE
#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
#include <pthread.h>

static uint32_t physical[2][2], caps[2][2], streams[2][2];
static unsigned references[2], initialized, gets, sets;
static _Thread_local int current;
static _Thread_local void *current_context;
static int stream_devices[128], context_devices[128];
static void *stream_contexts[128];
static pthread_mutex_t lifecycle_lock = PTHREAD_MUTEX_INITIALIZER;
static _Atomic int initializing;
static _Atomic int destroy_pending, destroy_continue;
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
    for (int i = 0; i < 128; ++i) stream_devices[i] = context_devices[i] = -1;
    initialized = 1;
}
uint32_t fake_snapshot(int device, int type) { init(); return caps[device][type]; }
unsigned fake_get_calls(void) { return gets; }
unsigned fake_reference_count(int device) { init(); return references[device]; }
int fake_initializing(void) { return atomic_load(&initializing); }
int fake_destroy_pending(void) { return atomic_load(&destroy_pending); }
void fake_continue_destroy(void) { atomic_store(&destroy_continue, 1); }
static void delay_destroy(const char *setting) {
    if (!getenv(setting)) return;
    atomic_store(&destroy_pending, 1);
    while (!atomic_load(&destroy_continue)) usleep(1000);
}
static int stream_device(void *stream) {
    return stream ? stream_devices[(uintptr_t)stream] : current;
}
static void default_context(int device) { current_context = (void *)(uintptr_t)(256 + device); }
static int context_device(void *context) {
    uintptr_t value = (uintptr_t)context;
    if (value < 2) return (int)value; /* Legacy context-selection probes. */
    if (value >= 256 && value < 258) return (int)value - 256;
    return value >= 512 && value < 640 ? context_devices[value - 512] : -1;
}
uint32_t fake_stream_snapshot(void *stream, int type) { init(); return streams[stream_device(stream)][type]; }

#ifndef OMIT_PRIMARY_HANDLE
int rtGetPriCtxByDeviceId(int device, void **context) {
    init();
    if (device < 0 || device > 1 || getenv("FAKE_FAIL_PRIMARY_HANDLE")) return 51;
    *context = references[device] ? (void *)(uintptr_t)(256 + device) : 0;
    return 0;
}
#endif
#ifndef OMIT_RUNTIME_PRIMARY_STATE
int rtsGetPrimaryCtxState(int device, uint32_t *flags, int *active) {
    init();
    if (!flags || device < 0 || device > 1 || getenv("FAKE_FAIL_PRIMARY_STATE")) return 52;
    *flags = 0;
    *active = getenv("FAKE_BAD_PRIMARY_STATE") ? 2 : references[device] != 0;
    return 0;
}
#endif
#ifndef OMIT_ACL_PRIMARY_STATE
int aclrtGetPrimaryCtxState(int device, uint32_t *flags, int *active) {
    init();
    if (flags || device < 0 || device > 1 || getenv("FAKE_FAIL_PRIMARY_STATE")) return 53;
    *active = getenv("FAKE_BAD_PRIMARY_STATE") ? 2 : references[device] != 0;
    return 0;
}
#endif

int aclrtSetDevice(int device) {
    if (device < 0 || device > 1) return 123;
    if (getenv("FAKE_FORK_IN_SET") && !forked_in_set) {
        forked_in_set = 1;
        fork_callback();
    }
    init(); current = device; default_context(device); ++references[device]; return 0;
}
int aclrtGetDevice(int *device) {
    if (getenv("FAKE_FAIL_CURRENT")) return 39;
    *device = current; return 0;
}
int aclrtSetCurrentContext(void *context) {
    int device = context_device(context);
    if (device < 0 || device > 1) return 123;
    init(); current = device; current_context = context; return 0;
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
    int device = stream_device(stream);
    if (device < 0 || device > 1) return 123;
    if (!getenv("FAKE_IGNORE_STREAM_SET")) streams[device][type] = value;
    return 0;
}
int aclrtResetStreamResLimit(void *stream) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    int device = stream_device(stream);
    for (int r = 0; r < 2; ++r) streams[device][r] = physical[device][r];
    return 0;
}
static void release_primary(int device) {
    void *primary = (void *)(uintptr_t)(256 + device);
    for (int r = 0; r < 2; ++r) caps[device][r] = streams[device][r] = physical[device][r];
    pthread_mutex_lock(&lifecycle_lock);
    for (unsigned i = 1; i < 128; ++i)
        if (stream_contexts[i] == primary) stream_devices[i] = -1;
    pthread_mutex_unlock(&lifecycle_lock);
    if (current_context == primary) current_context = 0;
}
int aclrtResetDevice(int device) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    if (references[device]) --references[device];
    if (!references[device]) release_primary(device);
    return 0;
}
int aclrtResetDeviceForce(int device) {
    init(); if (getenv("FAKE_FAIL_RESET")) return 43;
    references[device] = 0;
    release_primary(device);
    return 0;
}

/* Direct-runtime entry probes stop before creating any runtime resource. */
int rtSetDevice(int device) {
    if (device < 0 || device > 1) return 123;
    init(); current = device; default_context(device); return 0;
}
int rtGetDevice(int *device) {
    if (getenv("FAKE_FAIL_CURRENT")) return 39;
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

int rtCtxGetCurrent(void **context) {
    if (getenv("FAKE_FAIL_CONTEXT")) return 39;
    if (!current_context) default_context(current);
    *context = current_context;
    return 0;
}
int rtGetDevicePhyIdByIndex(uint32_t logical, uint32_t *physical_id) {
    if (logical > 1) return 39;
    *physical_id = logical == 0 ? 7 : 3;
    return 0;
}
int rtCtxSetCurrent(void *context) { return aclrtSetCurrentContext(context); }
int rtStreamCreate(void **stream, int priority) {
    (void)priority;
    init();
    if (getenv("FAKE_FAIL_CREATE")) return 41;
    pthread_mutex_lock(&lifecycle_lock);
    for (unsigned i = 1; i < 128; ++i) {
        if (stream_devices[i] < 0) {
            stream_devices[i] = current;
            if (!current_context) default_context(current);
            stream_contexts[i] = current_context;
            *stream = (void *)(uintptr_t)i;
            pthread_mutex_unlock(&lifecycle_lock);
            return 0;
        }
    }
    pthread_mutex_unlock(&lifecycle_lock);
    return 41;
}
int rtStreamCreateWithFlags(void **stream, int priority, unsigned flags) {
    (void)flags; return rtStreamCreate(stream, priority);
}
int aclrtCreateStream(void **stream) { return rtStreamCreate(stream, 0); }
int aclrtCreateStreamWithConfig(void **stream, unsigned priority, unsigned flags) {
    return rtStreamCreateWithFlags(stream, (int)priority, flags);
}
int rtStreamDestroy(void *stream) {
    if (getenv("FAKE_FAIL_DESTROY")) return 42;
    uintptr_t index = (uintptr_t)stream;
    if (!index || index >= 128 || stream_devices[index] < 0) return 43;
    pthread_mutex_lock(&lifecycle_lock);
    stream_devices[index] = -1;
    pthread_mutex_unlock(&lifecycle_lock);
    delay_destroy("FAKE_STREAM_DESTROY_RACE");
    return 0;
}
int rtStreamDestroyForce(void *stream) { return rtStreamDestroy(stream); }
int aclrtDestroyStream(void *stream) { return rtStreamDestroy(stream); }
int aclrtDestroyStreamForce(void *stream) { return rtStreamDestroyForce(stream); }
int rtCtxCreate(void **context, unsigned flags, int device) {
    (void)flags;
    init();
    if (device < 0 || device > 1 || getenv("FAKE_FAIL_CREATE")) return 41;
    pthread_mutex_lock(&lifecycle_lock);
    for (unsigned i = 0; i < 128; ++i) {
        if (context_devices[i] < 0) {
            context_devices[i] = device;
            current = device;
            *context = current_context = (void *)(uintptr_t)(512 + i);
            pthread_mutex_unlock(&lifecycle_lock);
            return 0;
        }
    }
    pthread_mutex_unlock(&lifecycle_lock);
    return 41;
}
int aclrtCreateContext(void **context, int device) { return rtCtxCreate(context, 0, device); }
int rtCtxDestroy(void *context) {
    if (getenv("FAKE_FAIL_DESTROY")) return 42;
    uintptr_t index = (uintptr_t)context - 512;
    if (index >= 128 || context_devices[index] < 0) return 43;
    pthread_mutex_lock(&lifecycle_lock);
    context_devices[index] = -1;
    for (unsigned i = 1; i < 128; ++i)
        if (stream_contexts[i] == context) stream_devices[i] = -1;
    pthread_mutex_unlock(&lifecycle_lock);
    delay_destroy("FAKE_CONTEXT_DESTROY_RACE");
    return 0;
}
int aclrtDestroyContext(void *context) { return rtCtxDestroy(context); }

int rtStreamCreateWithConfig(void **stream, void *handle) {
    (void)handle; return rtStreamCreate(stream, 0);
}
int aclrtCreateStreamV2(void **stream, const void *handle) {
    return rtStreamCreateWithConfig(stream, (void *)handle);
}
int rtDeviceReset(int device) { return aclrtResetDevice(device); }
int rtDeviceResetEx(int device) { return aclrtResetDevice(device); }
int rtDeviceResetWithoutTsd(int device) { return aclrtResetDevice(device); }
int rtDeviceResetForce(int device) { return aclrtResetDeviceForce(device); }
