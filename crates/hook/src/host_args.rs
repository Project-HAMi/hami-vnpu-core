//! CANN's HostArgs eager launch path does not necessarily call a legacy rt* hook.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::LazyLock;

thread_local! {
    // PID rather than a boolean: a fork child must not inherit an admission bypass.
    static HOST_ARGS_PID: Cell<u32> = const { Cell::new(0) };
}

struct HostArgsScope {
    previous: u32,
    outermost: bool,
}

impl HostArgsScope {
    fn enter() -> Self {
        let pid = std::process::id();
        let previous = HOST_ARGS_PID.with(|active| active.replace(pid));
        Self {
            previous,
            outermost: previous != pid,
        }
    }
}

impl Drop for HostArgsScope {
    fn drop(&mut self) {
        HOST_ARGS_PID.with(|active| active.set(self.previous));
    }
}

/// Keep standalone legacy launches unchanged. A HostArgs implementation may
/// delegate to one of them, but the outer call has already handled admission.
pub(crate) fn admit_legacy_launch(stream: u64) {
    let active = HOST_ARGS_PID.with(Cell::get);
    if active == 0 || active != std::process::id() {
        crate::npu_limiter().wait_for_token(stream);
    }
}

type HostArgsFn = unsafe extern "C" fn(
    *mut c_void,
    u32,
    *mut c_void,
    *mut c_void,
    *mut c_void,
    usize,
    *mut c_void,
    usize,
) -> i32;

static REAL: LazyLock<HostArgsFn> = LazyLock::new(|| unsafe {
    let name = c"aclrtLaunchKernelWithHostArgs";
    let mut symbol = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
    if symbol.is_null() {
        // Frameworks can load ACL with RTLD_LOCAL. Resolve its public entry point
        // explicitly in that case and retain the handle for the process lifetime.
        let handle = libc::dlopen(
            c"libascendcl.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        assert!(!handle.is_null(), "cannot load CANN libascendcl.so");
        symbol = libc::dlsym(handle, name.as_ptr());
    }
    assert!(
        !symbol.is_null() && symbol != aclrtLaunchKernelWithHostArgs as *const () as *mut c_void,
        "cannot resolve CANN aclrtLaunchKernelWithHostArgs"
    );
    std::mem::transmute(symbol)
});

// CANN acl_rt.h ABI: aclError is a signed C int; argument sizes/counts are size_t.
// Opaque pointers are forwarded without inspecting their pointed-to layouts.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aclrtLaunchKernelWithHostArgs(
    func_handle: *mut c_void,
    num_blocks: u32,
    stream: *mut c_void,
    config: *mut c_void,
    host_args: *mut c_void,
    args_size: usize,
    placeholders: *mut c_void,
    placeholder_count: usize,
) -> i32 {
    let real = *REAL;
    let scope = HostArgsScope::enter();
    if scope.outermost {
        let mut capture_status = 0;
        let mut model = 0;
        let result = unsafe {
            limiter::externed_api::rtStreamGetCaptureInfo(
                stream as u64,
                &mut capture_status,
                &mut model,
            )
        };
        // Capturing records work for a later graph replay. Waiting here can
        // synchronize an unfinished capture and would also charge it twice.
        // A failed capture query must not disable admission for eager launches.
        if result != 0 || capture_status == 0 {
            crate::npu_limiter().wait_for_token(stream as u64);
        }
    }
    unsafe {
        real(
            func_handle,
            num_blocks,
            stream,
            config,
            host_args,
            args_size,
            placeholders,
            placeholder_count,
        )
    }
}
