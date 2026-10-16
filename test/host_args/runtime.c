#include <stdint.h>
#include <stddef.h>

static _Thread_local unsigned capture_status;
static _Thread_local int capture_error;
void fake_capture(unsigned status, int error) { capture_status = status; capture_error = error; }
int rtStreamGetCaptureInfo(uint64_t stream, unsigned *status, uint64_t *model) {
    (void)stream; *status = capture_status; *model = 0; return capture_error;
}
int rtGetDevice(int *device) { *device = 0; return 0; }
int rtSetDevice(int device) { (void)device; return 0; }
int rtStreamCreate(uint64_t *stream, int priority) { (void)priority; *stream = 42; return 0; }
size_t rtEventCreate(uint64_t *event) { *event = 42; return 0; }
size_t rtEventRecord(uint64_t event, uint64_t stream) { (void)event; (void)stream; return 0; }
size_t rtStreamWaitEvent(uint64_t stream, uint64_t event) { (void)stream; (void)event; return 0; }
int rtStreamSynchronize(uint64_t stream) { (void)stream; return 0; }
int rtDeviceSynchronize(void) { return 0; }
size_t rtEventElapsedTime(float *ms, uint64_t start, uint64_t end) { (void)start; (void)end; *ms = 1; return 0; }
uint64_t rtAicpuKernelLaunchExWithArgs(unsigned a, uint64_t b, unsigned c, uint64_t d, uint64_t e, uint64_t f, unsigned g) { return 0; }
uint64_t rtAicpuKernelLaunchWithFlag(uint64_t a, unsigned b, uint64_t c, uint64_t d, uint64_t e, unsigned f) { return 0; }
uint64_t rtKernelLaunchWithFlagV2(uint64_t a, unsigned b, uint64_t c, uint64_t d, uint64_t e, unsigned f, uint64_t g) { return 0; }
uint64_t rtKernelLaunchWithHandleV2(uint64_t a, uint64_t b, unsigned c, uint64_t d, uint64_t e, uint64_t f, uint64_t g) { return 0; }
uint64_t rtModelExecute(uint64_t a, uint64_t b, unsigned c) { return 0; }

// The main-branch interceptor still links DCMI eagerly. These are never queried
// because this fixture supplies initialized local SHM without a global manager.
int dcmi_init(void) { return -1; }
int dcmi_get_card_list(int *count, int *cards, int capacity) { *count = 0; return -1; }
int dcmi_get_device_resource_info(int card, int device, void *out, int *count) { return -1; }
