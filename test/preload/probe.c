#define _GNU_SOURCE
#include <assert.h>
#include <dirent.h>
#include <dlfcn.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static volatile sig_atomic_t delivered;

typedef uint64_t (*Free)(uint64_t);
struct ConcurrentCall {
    Free release;
    pthread_barrier_t barrier;
};

static void *concurrent_free(void *argument) {
    struct ConcurrentCall *call = argument;
    pthread_barrier_wait(&call->barrier);
    assert(call->release(0) == 73);
    return NULL;
}

static void handler(int signal) {
    delivered = signal;
}

static void check_passive(void) {
    DIR *tasks = opendir("/proc/self/task");
    assert(tasks != NULL);
    unsigned count = 0;
    struct dirent *entry;
    while ((entry = readdir(tasks)) != NULL) {
        if (entry->d_name[0] != '.') {
            count++;
        }
    }
    closedir(tasks);
    assert(count == 1);

    struct sigaction action = {0}, observed = {0};
    action.sa_handler = handler;
    sigemptyset(&action.sa_mask);
    assert(sigaction(SIGUSR1, &action, NULL) == 0);
    assert(sigaction(SIGUSR1, NULL, &observed) == 0);
    assert(observed.sa_handler == handler);
    assert(raise(SIGUSR1) == 0);
    assert(delivered == SIGUSR1);
}

int main(int argc, char **argv) {
    assert(argc >= 2);
    check_passive();
    if (strcmp(argv[1], "idle") == 0) {
        return 0;
    }
    Free release = (Free)dlsym(RTLD_DEFAULT, "rtFree");
    assert(release != NULL);
    // A configured process must exit here if its required backend is missing.
    assert(release(0) == UINT64_MAX);
    if (strcmp(argv[1], "missing") == 0) {
        return 0;
    }
    assert(argc == 4);
    int visibility = strcmp(argv[2], "global") == 0 ? RTLD_GLOBAL : RTLD_LOCAL;
    void *backend = dlopen(argv[3], RTLD_NOW | visibility);
    if (backend == NULL) {
        fprintf(stderr, "%s\n", dlerror());
        return 1;
    }
    // A previous failed lookup must not poison subsequent hook calls.
    // Race first resolution so losing cache publishers also release their references.
    struct ConcurrentCall call = {.release = release};
    pthread_t threads[8];
    assert(pthread_barrier_init(&call.barrier, NULL, 8) == 0);
    for (unsigned index = 0; index < 8; index++) {
        assert(pthread_create(&threads[index], NULL, concurrent_free, &call) == 0);
    }
    for (unsigned index = 0; index < 8; index++) {
        assert(pthread_join(threads[index], NULL) == 0);
    }
    assert(pthread_barrier_destroy(&call.barrier) == 0);
    assert(release(0) == 73);
    typedef uint64_t (*Malloc)(uint64_t, uint64_t, uint32_t, uint16_t);
    Malloc allocate = (Malloc)dlsym(RTLD_DEFAULT, "rtMalloc");
    assert(allocate != NULL);
    uint64_t address = 0;
    assert(allocate((uint64_t)(uintptr_t)&address, 4096, 0, 0) == 0);
    assert(address == 0x1234);
    check_passive();
    // Cached addresses must survive the application's release of its SDK handle.
    assert(dlclose(backend) == 0);
    assert(release(0) == 73);
    return 0;
}
