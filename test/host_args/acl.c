#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

typedef int (*launch_fn)(void *, uint32_t, void *, void *, void *, size_t, void *, size_t);
extern uint64_t rtAicpuKernelLaunchExWithArgs(unsigned, uint64_t, unsigned, uint64_t, uint64_t, uint64_t, unsigned);
extern uint64_t rtAicpuKernelLaunchWithFlag(uint64_t, unsigned, uint64_t, uint64_t, uint64_t, unsigned);
extern uint64_t rtKernelLaunchWithFlagV2(uint64_t, unsigned, uint64_t, uint64_t, uint64_t, unsigned, uint64_t);
extern uint64_t rtKernelLaunchWithHandleV2(uint64_t, uint64_t, unsigned, uint64_t, uint64_t, uint64_t, uint64_t);
extern uint64_t rtModelExecute(uint64_t, uint64_t, unsigned);

static _Thread_local int mode, depth;
void fake_mode(int value) { mode = value; }
int aclrtLaunchKernelWithHostArgs(void *func, uint32_t blocks, void *stream, void *cfg,
                                void *args, size_t size, void *placeholders, size_t count) {
    assert(func == (void *)0x1234 && blocks == UINT32_C(0xfedcba98));
    assert(stream == (void *)0x5678 && cfg == (void *)0x9876 && args == (void *)0x4567);
    assert(size == (size_t)UINT64_C(0x100000002) && placeholders == (void *)0x8765);
    assert(count == (size_t)UINT64_C(0x200000003));
    ++depth;
    if (mode) {
        assert(!rtAicpuKernelLaunchExWithArgs(0, 0, 0, 0, 0, (uintptr_t)stream, 0));
        assert(!rtAicpuKernelLaunchWithFlag(0, 0, 0, 0, (uintptr_t)stream, 0));
        assert(!rtKernelLaunchWithFlagV2(0, 0, 0, 0, (uintptr_t)stream, 0, 0));
        assert(!rtKernelLaunchWithHandleV2(0, 0, 0, 0, 0, (uintptr_t)stream, 0));
        assert(!rtModelExecute(0, (uintptr_t)stream, 0));
    }
    if (depth == 1 && mode >= 2) {
        launch_fn entry = (launch_fn)dlsym(RTLD_DEFAULT, "aclrtLaunchKernelWithHostArgs");
        assert(entry);
        if (mode == 2) {
            assert(entry(func, blocks, stream, cfg, args, size, placeholders, count) == -37);
        } else {
            pid_t child = fork(); assert(child >= 0);
            if (child == 0) _exit(entry(func, blocks, stream, cfg, args, size, placeholders, count) == -37 ? 0 : 1);
            int status; assert(waitpid(child, &status, 0) == child);
            assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        }
    }
    --depth;
    return -37;
}
