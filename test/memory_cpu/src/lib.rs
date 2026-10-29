#![allow(dead_code, non_snake_case)]
#[path = "../../../crates/limiter/src/dynamic.rs"]
pub mod dynamic;
include!(concat!(env!("OUT_DIR"), "/source.rs"));
pub mod shmem { include!(concat!(env!("OUT_DIR"), "/shared.rs")); }
#[macro_export]
macro_rules! check_rts { ($call:expr) => {{ ($call) as u32 }}; }
pub mod externed_api {
    pub const RT_ERROR_NONE: u64 = 0;
    pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;
    pub unsafe fn rtStreamCreate(value: *mut u64, _: i32) -> i32 { unsafe { *value = 1 }; 0 }
    pub unsafe fn rtEventCreate(value: *mut u64) -> usize { unsafe { *value = 1 }; 0 }
    pub unsafe fn rtEventRecord(_: u64, _: u64) -> usize { 0 }
    pub unsafe fn rtStreamWaitEvent(_: u64, _: u64) -> usize { 0 }
    pub unsafe fn rtStreamSynchronize(_: u64) -> i32 { 0 }
    pub unsafe fn rtEventElapsedTime(value: *mut f32, _: u64, _: u64) -> usize { unsafe { *value = 1.0 }; 0 }
    pub unsafe fn rtCtxGetCurrent(out: *mut u64) -> i32 { unsafe { *out = 0 }; 0 }
    pub unsafe fn rtGetDevicePhyIdByIndex(logical: u32, out: *mut u32) -> i32 { unsafe { *out = logical }; 0 }
    pub unsafe fn rtSetDevice(_: i32) -> i32 { 0 }
    pub unsafe fn rtGetDevice(value: *mut i32) -> i32 { unsafe { *value = 0 }; 0 }
    pub unsafe fn rtDeviceSynchronize() -> i32 { 0 }
    pub unsafe fn rtStreamGetCaptureInfo(_: u64, status: *mut u32, _: *mut u64) -> i32 { unsafe { *status = 0 }; 0 }
}
#[cfg(test)] pub mod procfs_test_double;

#[cfg(target_os = "linux")]
#[path = "../../../crates/limiter/src/dcmi.rs"]
pub mod dcmi;
#[path = "../../../crates/limiter/src/dcmi_protocol.rs"]
pub mod dcmi_protocol;
#[path = "../../../crates/limiter/src/registry_lock.rs"]
pub mod registry_lock;
// These portable tests exercise quota conversion only, never the Linux helper.
#[cfg(not(target_os = "linux"))]
pub mod dcmi {
    pub fn helper_path() -> Option<std::path::PathBuf> {
        None
    }
    #[derive(Default)]
    pub struct Collector;
    impl Collector {
        pub fn snapshot_for_device(
            &mut self,
            _: &std::path::Path,
            _: std::time::Duration,
            _: &std::sync::atomic::AtomicBool,
            _: Option<u32>,
        ) -> std::io::Result<Vec<crate::dcmi_protocol::Record>> {
            panic!("unexpected DCMI call in quota test")
        }
    }
}
