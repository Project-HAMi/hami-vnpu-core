#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

typedef int (*launch_fn)(void *, uint32_t, void *, void *, void *, size_t, void *, size_t);
static launch_fn launch;
static void (*capture)(unsigned, int);
static void (*mode)(int);
static int (*select_device)(int);
static _Atomic uint64_t *tokens;
static atomic_int started, completed;
static void call_stream(void *stream) {
    assert(launch((void *)0x1234, UINT32_C(0xfedcba98), stream,
                  (void *)0x9876, (void *)0x4567, (size_t)UINT64_C(0x100000002),
                  (void *)0x8765, (size_t)UINT64_C(0x200000003)) == -37);
}
static void call(void) { call_stream((void *)0x5678); }
static void *parallel(void *arg) {
    assert(!select_device((int)(intptr_t)arg)); mode(1);
    for (int i = 0; i < 100; ++i) call();
    return NULL;
}
static void *blocked(void *arg) {
    (void)arg; atomic_store(&started, 1); call(); atomic_store(&completed, 1); return NULL;
}
static unsigned char *domain(const char *base, int physical) {
    char *device_path = NULL;
    assert(asprintf(&device_path, "%s.device-%d", base, physical) > 0);
    int fd = open(device_path, O_CREAT | O_RDWR | O_TRUNC, 0600);
    free(device_path);
    assert(fd >= 0 && !ftruncate(fd, 6256));
    unsigned char *shm = mmap(NULL, 6256, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(shm != MAP_FAILED); close(fd);
    atomic_store((_Atomic int32_t *)(shm + 6200), getppid()); // live manager owner
    atomic_store((_Atomic uint64_t *)(shm + 6208), UINT64_MAX); // fresh heartbeat
    atomic_store((_Atomic uint32_t *)(shm + 6220), 1); // initialized
    atomic_store((_Atomic uint64_t *)(shm + 32), 1); // batch
    atomic_store((_Atomic uint64_t *)(shm + 40), 10000);
    return shm;
}
int main(int argc, char **argv) {
    assert(argc == 2);
    void *acl = dlopen("libascendcl.so", RTLD_NOW | (!strcmp(argv[1], "local") ? RTLD_LOCAL : RTLD_GLOBAL));
    assert(acl);
    mode = dlsym(acl, "fake_mode"); capture = dlsym(acl, "fake_capture");
    select_device = dlsym(RTLD_DEFAULT, "rtSetDevice");
    launch = dlsym(RTLD_DEFAULT, "aclrtLaunchKernelWithHostArgs");
    assert(mode && capture && select_device && launch && (void *)launch != dlsym(acl, "aclrtLaunchKernelWithHostArgs"));
    // Current production layout: legacy prefix (6224 bytes) plus overdraft fields.
    const char *base = getenv("NPU_LOCAL_SHM_PATH");
    assert(base);
    unsigned char *shm = domain(base, 0), *shm_one = domain(base, 1);
    tokens = (_Atomic uint64_t *)(shm + 40);
    _Atomic uint64_t *tokens_one = (_Atomic uint64_t *)(shm_one + 40);
    // Capture must return even with STATE_IDLE, without initializing a worker.
    capture(1, 0); mode(2); call();
    assert(atomic_load(tokens) == 10000 && atomic_load((_Atomic uint32_t *)(shm + 48)) == 0);
    atomic_store((_Atomic uint32_t *)(shm + 24), 1); // STATE_RUNNING
    atomic_store((_Atomic uint32_t *)(shm_one + 24), 1);
    capture(0, 0);
    for (int delegation = 0; delegation <= 2; ++delegation) {
        mode(delegation); uint64_t before = atomic_load(tokens); call(); call();
        assert(atomic_load(tokens) == before - 2);
    }
    capture(1, -19); // A failed status query must not bypass admission.
    uint64_t before = atomic_load(tokens); call(); assert(atomic_load(tokens) == before - 1);
    capture(0, 0); mode(0);
    // A graph replay outside HostArgs still consumes its own token.
    uint64_t (*replay)(uint64_t, uint64_t, unsigned) = dlsym(RTLD_DEFAULT, "rtModelExecute");
    before = atomic_load(tokens); assert(!replay(0, 0x5678, 0)); assert(atomic_load(tokens) == before - 1);
    // Default and unrecorded streams use the native current card/context.
    // Delegated legacy launches must not charge either card a second time.
    for (int device = 1; device >= 0; --device) {
        assert(!select_device(device)); mode(1);
        uint64_t before_zero = atomic_load(tokens), before_one = atomic_load(tokens_one);
        call(); call_stream(NULL);
        assert(atomic_load(tokens) == before_zero - (device == 0 ? 2 : 0));
        assert(atomic_load(tokens_one) == before_one - (device == 1 ? 2 : 0));
    }
    pthread_t threads[4]; before = atomic_load(tokens);
    for (int i = 0; i < 4; ++i) assert(!pthread_create(&threads[i], NULL, parallel, NULL));
    for (int i = 0; i < 4; ++i) assert(!pthread_join(threads[i], NULL));
    assert(atomic_load(tokens) == before - 400);
    before = atomic_load(tokens); uint64_t before_one = atomic_load(tokens_one);
    assert(!pthread_create(&threads[0], NULL, parallel, NULL));
    assert(!pthread_create(&threads[1], NULL, parallel, (void *)(intptr_t)1));
    for (int i = 0; i < 2; ++i) assert(!pthread_join(threads[i], NULL));
    assert(atomic_load(tokens) == before - 100 && atomic_load(tokens_one) == before_one - 100);
    mode(3); before = atomic_load(tokens); call(); assert(atomic_load(tokens) == before - 2);
    mode(0); atomic_store(tokens, 0);
    assert(!pthread_create(&threads[0], NULL, blocked, NULL));
    while (!atomic_load(&started)) usleep(1000);
    usleep(50000); assert(!atomic_load(&completed));
    atomic_store(tokens, 1);
    assert(!pthread_join(threads[0], NULL)); assert(atomic_load(&completed) && atomic_load(tokens) == 0);
    // Releasing the application's ACL handle must not invalidate the cached launch.
    mode(0); capture(1, 0);
    assert(dlclose(acl) == 0);
    call();
    printf("PASS %s: ABI, capture, errors, nested delegation, replay, per-card streams, threads, fork, quota wait, SDK lifetime\n", argv[1]);
    return 0;
}
