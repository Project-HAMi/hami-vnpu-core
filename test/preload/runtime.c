#include <stdint.h>
#include <stdlib.h>

#ifdef FORBID_LOAD
__attribute__((constructor)) static void forbid_load(void) {
    // Loading an SDK/driver during an ordinary command is itself a failure.
    abort();
}
#endif

uint64_t rtFree(uint64_t address) {
    (void)address;
    return 73;
}

uint64_t rtMalloc(uint64_t out, uint64_t size, uint32_t type, uint16_t module) {
    (void)size;
    (void)type;
    (void)module;
    *(uint64_t *)(uintptr_t)out = 0x1234;
    return 0;
}
