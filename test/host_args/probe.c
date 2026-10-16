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
static _Atomic uint64_t *tokens;
static atomic_int started, completed;
static void call(void) {
    assert(launch((void *)0x1234, UINT32_C(0xfedcba98), (void *)0x5678,
                  (void *)0x9876, (void *)0x4567, (size_t)UINT64_C(0x100000002),
                  (void *)0x8765, (size_t)UINT64_C(0x200000003)) == -37);
}
static void *parallel(void *arg) {
    (void)arg; mode(1);
    for (int i = 0; i < 100; ++i) call();
    return NULL;
}
static void *blocked(void *arg) {
    (void)arg; atomic_store(&started, 1); call(); atomic_store(&completed, 1); return NULL;
}
int main(int argc, char **argv) {
    assert(argc == 2);
    void *acl = dlopen("libascendcl.so", RTLD_NOW | (!strcmp(argv[1], "local") ? RTLD_LOCAL : RTLD_GLOBAL));
    assert(acl);
    mode = dlsym(acl, "fake_mode"); capture = dlsym(acl, "fake_capture");
    launch = dlsym(RTLD_DEFAULT, "aclrtLaunchKernelWithHostArgs");
    assert(mode && capture && launch && (void *)launch != dlsym(acl, "aclrtLaunchKernelWithHostArgs"));
    int fd = open(getenv("NPU_LOCAL_SHM_PATH"), O_CREAT | O_RDWR | O_TRUNC, 0600);
    assert(fd >= 0 && !ftruncate(fd, 6224));
    unsigned char *shm = mmap(NULL, 6224, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(shm != MAP_FAILED); close(fd);
    tokens = (_Atomic uint64_t *)(shm + 40);
    atomic_store((_Atomic uint32_t *)(shm + 6220), 1); // initialized
    atomic_store((_Atomic uint64_t *)(shm + 32), 1);  // batch
    atomic_store(tokens, 10000);
    // Capture must return even with STATE_IDLE, without initializing a worker.
    capture(1, 0); mode(2); call();
    assert(atomic_load(tokens) == 10000 && atomic_load((_Atomic uint32_t *)(shm + 48)) == 0);
    atomic_store((_Atomic uint32_t *)(shm + 24), 1); // STATE_RUNNING
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
    pthread_t threads[4]; before = atomic_load(tokens);
    for (int i = 0; i < 4; ++i) assert(!pthread_create(&threads[i], NULL, parallel, NULL));
    for (int i = 0; i < 4; ++i) assert(!pthread_join(threads[i], NULL));
    assert(atomic_load(tokens) == before - 400);
    mode(3); before = atomic_load(tokens); call(); assert(atomic_load(tokens) == before - 2);
    mode(0); atomic_store(tokens, 0);
    assert(!pthread_create(&threads[0], NULL, blocked, NULL));
    while (!atomic_load(&started)) usleep(1000);
    usleep(50000); assert(!atomic_load(&completed));
    atomic_store(tokens, 1);
    assert(!pthread_join(threads[0], NULL)); assert(atomic_load(&completed) && atomic_load(tokens) == 0);
    printf("PASS %s: ABI, capture, errors, nested delegation, replay, threads, fork, quota wait\n", argv[1]);
    return 0;
}
