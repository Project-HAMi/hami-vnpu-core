#[link(name = "runtime")]
unsafe extern "C" {
    pub fn rtStreamCreate(stream: *mut u64, priority: i32) -> i32;
    pub fn rtEventCreate(evt: *mut u64) -> usize;
    pub fn rtEventRecord(evt: u64, stm: u64) -> usize;
    pub fn rtStreamWaitEvent(stream: u64, event: u64) -> usize;
    pub fn rtStreamSynchronize(stream: u64) -> i32;
    pub fn rtEventElapsedTime(time_interval: *mut f32, start_event: u64, end_event: u64) -> usize;
    pub fn rtSetDevice(device: i32) -> i32;
    pub fn rtGetDevice(device: *mut i32) -> i32;
    pub fn rtDeviceSynchronize() -> i32;
    pub fn rtStreamGetCaptureInfo(stream: u64, status: *mut u32, model: *mut u64) -> i32;
}

// RT ERROR CODE
pub const RT_ERROR_NONE: u64 = 0;
pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;