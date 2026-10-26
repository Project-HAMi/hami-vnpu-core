/* Link-only test dependencies. These are never a CANN implementation.
 * Spatial tests resolve their exercised ACL/RT calls from fake_acl.c; any call
 * reaching one of these fallback symbols must fail rather than fake success.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static _Noreturn void unexpected(const char *name) {
    fprintf(stderr, "CI link stub called unexpectedly: %s\n", name);
    abort();
}

#if defined(STUB_RUNTIME)
int rtStreamCreate(uint64_t *stream, int priority) {
    (void)stream; (void)priority; unexpected(__func__);
}
uintptr_t rtEventCreate(uint64_t *event) {
    (void)event; unexpected(__func__);
}
uintptr_t rtEventRecord(uint64_t event, uint64_t stream) {
    (void)event; (void)stream; unexpected(__func__);
}
uintptr_t rtStreamWaitEvent(uint64_t stream, uint64_t event) {
    (void)stream; (void)event; unexpected(__func__);
}
int rtStreamSynchronize(uint64_t stream) {
    (void)stream; unexpected(__func__);
}
uintptr_t rtEventElapsedTime(float *elapsed, uint64_t start, uint64_t end) {
    (void)elapsed; (void)start; (void)end; unexpected(__func__);
}
int rtSetDevice(int device) {
    (void)device; unexpected(__func__);
}
int rtGetDevice(int *device) {
    (void)device; unexpected(__func__);
}
int rtGetDevicePhyIdByIndex(uint32_t device, uint32_t *physical) {
    (void)device; (void)physical; unexpected(__func__);
}
int rtCtxGetCurrent(uint64_t *context) {
    (void)context; unexpected(__func__);
}
int rtDeviceSynchronize(void) { unexpected(__func__); }
int rtStreamGetCaptureInfo(uint64_t stream, uint32_t *status, uint64_t *model) {
    (void)stream; (void)status; (void)model; unexpected(__func__);
}
#elif defined(STUB_DCMI)
int dcmi_init(void) { unexpected(__func__); }
int dcmi_get_card_list(int *count, int *cards, int capacity) {
    (void)count; (void)cards; (void)capacity; unexpected(__func__);
}
int dcmi_get_card_id_device_id_from_phyid(int *card, int *device, unsigned physical) {
    (void)card; (void)device; (void)physical; unexpected(__func__);
}
int dcmi_get_device_resource_info(int card, int device, void *processes, int *count) {
    (void)card; (void)device; (void)processes; (void)count; unexpected(__func__);
}
#else
#error "Select STUB_RUNTIME or STUB_DCMI for this CPU-only link dependency"
#endif
