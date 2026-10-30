#![allow(dead_code, non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/source.rs"));
pub mod shmem {
    include!(concat!(env!("OUT_DIR"), "/shared.rs"));
}
#[macro_export]
macro_rules! check_rts {
    ($call:expr) => {{ ($call) as u32 }};
}
pub mod externed_api {
    pub const RT_ERROR_NONE: u64 = 0;
    pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;
    pub unsafe fn rtStreamCreate(value: *mut u64, _: i32) -> i32 {
        unsafe { *value = 1 };
        0
    }
    pub unsafe fn rtEventCreate(value: *mut u64) -> usize {
        unsafe { *value = 1 };
        0
    }
    pub unsafe fn rtEventRecord(_: u64, _: u64) -> usize {
        0
    }
    pub unsafe fn rtStreamWaitEvent(_: u64, _: u64) -> usize {
        0
    }
    pub unsafe fn rtStreamSynchronize(_: u64) -> i32 {
        0
    }
    pub unsafe fn rtEventElapsedTime(value: *mut f32, _: u64, _: u64) -> usize {
        unsafe { *value = 1.0 };
        0
    }
    pub unsafe fn rtSetDevice(_: i32) -> i32 {
        0
    }
    pub unsafe fn rtGetDevice(value: *mut i32) -> i32 {
        unsafe { *value = 0 };
        0
    }
    pub unsafe fn rtGetDevicePhyIdByIndex(value: u32, out: *mut u32) -> i32 {
        unsafe { *out = value };
        0
    }
    pub unsafe fn rtCtxGetCurrent(out: *mut u64) -> i32 {
        unsafe { *out = 1 };
        0
    }
    pub unsafe fn rtDeviceSynchronize() -> i32 {
        0
    }
    pub unsafe fn rtStreamGetCaptureInfo(_: u64, status: *mut u32, _: *mut u64) -> i32 {
        unsafe { *status = 0 };
        0
    }
}
