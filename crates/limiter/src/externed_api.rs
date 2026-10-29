use std::ffi::CStr;
use std::sync::atomic::AtomicPtr;

macro_rules! runtime_apis {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)+) => {$(
        /// # Safety
        /// Arguments and handles must satisfy the corresponding CANN runtime API.
        #[allow(non_snake_case)]
        pub unsafe fn $name($($arg: $ty),*) -> $ret {
            type Real = unsafe extern "C" fn($($ty),*) -> $ret;
            static REAL: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
            let name = CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes())
                .expect("static runtime symbol");
            let address = crate::dynamic::resolve_sdk_symbol(&REAL, name)
                .unwrap_or_else(|| panic!("cannot find Ascend runtime symbol: {}", name.to_string_lossy()));
            let real: Real = unsafe { std::mem::transmute(address) };
            unsafe { real($($arg),*) }
        }
    )+};
}

runtime_apis! {
    rtStreamCreate(stream: *mut u64, priority: i32) -> i32;
    rtEventCreate(evt: *mut u64) -> usize;
    rtEventRecord(evt: u64, stm: u64) -> usize;
    rtStreamWaitEvent(stream: u64, event: u64) -> usize;
    rtStreamSynchronize(stream: u64) -> i32;
    rtEventElapsedTime(time_interval: *mut f32, start_event: u64, end_event: u64) -> usize;
    rtSetDevice(device: i32) -> i32;
    rtGetDevice(device: *mut i32) -> i32;
    rtGetDevicePhyIdByIndex(device: u32, physical: *mut u32) -> i32;
    rtCtxGetCurrent(context: *mut u64) -> i32;
    rtDeviceSynchronize() -> i32;
    rtStreamGetCaptureInfo(stream: u64, status: *mut u32, model: *mut u64) -> i32;
}

// RT ERROR CODE
pub const RT_ERROR_NONE: u64 = 0;
pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;
