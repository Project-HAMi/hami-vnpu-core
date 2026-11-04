#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

#define MIB (UINT64_C(1024) * 1024)
extern uint64_t rtMemGetInfoEx(uint64_t, size_t *, size_t *);
extern unsigned fake_queries(void);
extern unsigned fake_device_queries(void);
extern uint64_t fake_last_kind(void);

static void domain(const char *base, int physical, uint64_t quota, uint64_t used) {
    char *path = NULL;
    assert(asprintf(&path, "%s.device-%d", base, physical) > 0);
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    free(path);
    assert(fd >= 0 && !ftruncate(fd, 6512));
    unsigned char *shm = mmap(NULL, 6512, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(shm != MAP_FAILED);
    close(fd);
    if (getenv("FAKE_COLD_QUERY")) {
        /* Leave publication to the real in-process supervisor. */
        assert(!munmap(shm, 6512));
        return;
    }
    atomic_store((_Atomic uint64_t *)(shm + 0), quota);
    atomic_store((_Atomic int32_t *)(shm + 1080), getpid());
    atomic_store((_Atomic uint64_t *)(shm + 1088), used);
    atomic_store((_Atomic uint32_t *)(shm + 1152), 1);
    /* Keep the synthetic manager lease alive even on a slow CI machine. */
    atomic_store((_Atomic int32_t *)(shm + 6200), getppid());
    atomic_store((_Atomic uint64_t *)(shm + 6208), UINT64_MAX);
    atomic_store((_Atomic uint32_t *)(shm + 6220), 1);
    assert(!munmap(shm, 6512));
}

static void assert_supervisor_published(const char *base) {
    char *path = NULL;
    assert(asprintf(&path, "%s.device-7", base) > 0);
    int fd = open(path, O_RDONLY);
    free(path);
    assert(fd >= 0);
    unsigned char *shm = mmap(NULL, 6512, PROT_READ, MAP_SHARED, fd, 0);
    assert(shm != MAP_FAILED);
    close(fd);
    /* Prove that the query waited for publication, rather than falling back. */
    assert(atomic_load((_Atomic uint32_t *)(shm + 6220)) == 1);
    assert(atomic_load((_Atomic int32_t *)(shm + 6200)) == getpid());
    assert(atomic_load((_Atomic uint64_t *)(shm + 0)) == 0);
    assert(!munmap(shm, 6512));
}

int main(int argc, char **argv) {
    assert(argc == 7);
    const char *local = getenv("NPU_LOCAL_SHM_PATH");
    if (local) {
        uint64_t quota = strtoull(getenv("NPU_MEMORY_LIMIT_MB"), NULL, 10) * MIB;
        const char *used_env = getenv("FAKE_USED_MB");
        uint64_t used = used_env ? strtoull(used_env, NULL, 10) * MIB : 1024 * MIB;
        domain(local, 7, quota, used);
        domain(local, 3, quota, 2048 * MIB);
    }
    uint64_t kind = strtoull(argv[1], NULL, 10);
    uint64_t expected_result = strtoull(argv[2], NULL, 10);
    size_t expected_free = strtoull(argv[3], NULL, 10);
    size_t expected_total = strtoull(argv[4], NULL, 10);
    unsigned expected_device_queries = strtoul(argv[5], NULL, 10);
    int null_output = atoi(argv[6]);
    size_t free = 111, total = 222;
    uint64_t result = rtMemGetInfoEx(kind, null_output == 1 ? NULL : &free,
                                   null_output == 2 ? NULL : &total);
    if (result != expected_result || free != expected_free || total != expected_total ||
        fake_queries() != 1 || fake_last_kind() != kind ||
        fake_device_queries() != expected_device_queries) {
        fprintf(stderr, "kind=%" PRIu64 " result=%" PRIu64 " free=%zu total=%zu native_calls=%u device_calls=%u\n",
                kind, result, free, total, fake_queries(), fake_device_queries());
        return 1;
    }
    if (getenv("FAKE_COLD_QUERY")) {
        assert(local);
        assert_supervisor_published(local);
    }
    return 0;
}
