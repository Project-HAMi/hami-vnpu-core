#[link(name = "runtime")]
unsafe extern "C" {
    pub fn rtStreamCreate(stream: *mut u64, priority: i32) -> i32;
    pub fn rtStreamDestroy(stream: u64) -> i32;
    pub fn rtEventCreate(evt: *mut u64) -> usize;
    pub fn rtEventDestroy(evt: u64) -> i32;
    pub fn rtEventRecord(evt: u64, stm: u64) -> usize;
    pub fn rtStreamWaitEvent(stream: u64, event: u64) -> usize;
    pub fn rtStreamSynchronize(stream: u64) -> i32;
    pub fn rtEventElapsedTime(time_interval: *mut f32, start_event: u64, end_event: u64) -> usize;
    pub fn rtSetDevice(device: i32) -> i32;
    pub fn rtGetDevice(device: *mut i32) -> i32;
    pub fn rtGetDevicePhyIdByIndex(device_index: u32, phy_device: *mut u32) -> i32;
    pub fn rtDeviceSynchronize() -> i32;
    pub fn rtStreamGetCaptureInfo(stream: u64, status: *mut u32, model: *mut u64) -> i32;
    /// Same as hooked in libvnpu; used by the limiter daemon when no quota is set.
    pub fn rtMemGetInfoEx(mem_info_type: u64, free: *mut usize, total: *mut usize) -> u64;
}

// RT ERROR CODE
pub const RT_ERROR_NONE: u64 = 0;
pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;