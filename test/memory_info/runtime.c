#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#define MIB ((size_t)1024 * 1024)
static unsigned queries, device_queries;
static uint64_t last_kind;

unsigned fake_queries(void) { return queries; }
unsigned fake_device_queries(void) { return device_queries; }
uint64_t fake_last_kind(void) { return last_kind; }

int rtGetDevice(int *device) {
    ++device_queries;
    if (getenv("FAKE_NO_DEVICE")) return 107001;
    const char *value = getenv("FAKE_DEVICE");
    *device = value ? atoi(value) : 0;
    return 0;
}

int rtGetDevicePhyIdByIndex(unsigned logical, unsigned *physical) {
    if (logical > 1) return 107001;
    *physical = logical ? 3 : 7;
    return 0;
}

uint64_t rtMemGetInfoEx(uint64_t kind, size_t *free, size_t *total) {
    ++queries;
    last_kind = kind;
    if (!free || !total) return 101;
    switch (kind) {
        case 0: *free = 1024 * MIB; *total = 2048 * MIB; return 0;
        case 1: *free = 6144 * MIB; *total = 8192 * MIB; return 0;
        case 4: *free = 256 * MIB; *total = 512 * MIB; return 0;
        case 5: *free = 0; *total = 256 * MIB; return 0;
        case 8: *free = 0; *total = 0; return 0;
        case 12: *free = 32 * MIB; *total = 64 * MIB; return 0;
        default:
            /* Preserve native errors and their output behavior. */
            *free = 333;
            *total = 444;
            return kind == 10 ? 207000 : 107001;
    }
}

/* A memory query must not create compute resources or apply spatial budgets. */
static void unexpected(void) {
    fputs("memory query initialized compute resources\n", stderr);
    _Exit(97);
}
int rtSetDevice(int device) { (void)device; unexpected(); return 97; }
int aclrtSetDevice(int device) { (void)device; unexpected(); return 97; }
int aclrtGetDeviceResLimit(int device, int resource, unsigned *value) {
    (void)device; (void)resource; (void)value; unexpected(); return 97;
}
int aclrtSetDeviceResLimit(int device, int resource, unsigned value) {
    (void)device; (void)resource; (void)value; unexpected(); return 97;
}
