//! Resolve CANN only when an application actually calls the runtime. Keeping a
//! DT_NEEDED dependency on libruntime.so would initialize CANN even inside the
//! exec-isolated DCMI helper via /etc/ld.so.preload.

use std::ffi::CStr;
use std::sync::LazyLock;

/// Resolve the real symbol without retaining any vendor library destructor.
/// RTLD_NEXT covers normal interposition; a handle also supports a runtime that
/// the framework loaded with RTLD_LOCAL.
pub fn runtime_symbol(name: &CStr) -> *mut libc::c_void {
    unsafe {
        let symbol = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if !symbol.is_null() {
            return symbol;
        }
        let handle = libc::dlopen(c"libruntime.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            panic!("cannot load CANN libruntime.so to resolve {name:?}");
        }
        let symbol = libc::dlsym(handle, name.as_ptr());
        // Intentionally retain the handle: hooks can run during process teardown.
        assert!(
            !symbol.is_null(),
            "cannot resolve CANN runtime symbol {name:?}"
        );
        symbol
    }
}

macro_rules! runtime_api {
    ($name:ident($($arg:ident: $kind:ty),*) -> $result:ty) => {
        #[allow(non_snake_case)]
        pub unsafe fn $name($($arg: $kind),*) -> $result {
            static FUNCTION: LazyLock<unsafe extern "C" fn($($kind),*) -> $result> = LazyLock::new(|| {
                let name = CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()).unwrap();
                unsafe { std::mem::transmute(runtime_symbol(name)) }
            });
            unsafe { FUNCTION($($arg),*) }
        }
    };
}

runtime_api!(rtStreamCreate(stream: *mut u64, priority: i32) -> i32);
runtime_api!(rtEventCreate(evt: *mut u64) -> usize);
runtime_api!(rtEventRecord(evt: u64, stm: u64) -> usize);
runtime_api!(rtStreamWaitEvent(stream: u64, event: u64) -> usize);
runtime_api!(rtStreamSynchronize(stream: u64) -> i32);
runtime_api!(rtEventElapsedTime(time_interval: *mut f32, start_event: u64, end_event: u64) -> usize);
runtime_api!(rtSetDevice(device: i32) -> i32);
runtime_api!(rtGetDevice(device: *mut i32) -> i32);
runtime_api!(rtDeviceSynchronize() -> i32);
runtime_api!(rtStreamGetCaptureInfo(stream: u64, status: *mut u32, model: *mut u64) -> i32);

pub const RT_ERROR_NONE: u64 = 0;
pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;
