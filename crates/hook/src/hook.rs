#![allow(non_snake_case)]
use crate::npu_limiter;
use limiter::externed_api::{RT_ERROR_MEMORY_ALLOCATION, RT_ERROR_NONE};
use limiter::worker::AllocationKind;

#[unsafe(no_mangle)]
pub extern "C" fn rtAicpuKernelLaunchExWithArgs(
    kernelType: u32,
    opName: u64,
    blockDim: u32,
    argsInfo: u64,
    smDesc: u64,
    stm: u64,
    flags: u32,
) -> u64 {
    npu_limiter().wait_for_token(stm);
    passthrough!(
        "rtAicpuKernelLaunchExWithArgs",
        (u32, u64, u32, u64, u64, u64, u32),
        kernelType,
        opName,
        blockDim,
        argsInfo,
        smDesc,
        stm,
        flags
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn rtAicpuKernelLaunchWithFlag(
    launchNames: u64,
    blockDim: u32,
    argsInfo: u64,
    smDesc: u64,
    stm: u64,
    flags: u32,
) -> u64 {
    npu_limiter().wait_for_token(stm);
    passthrough!(
        "rtAicpuKernelLaunchWithFlag",
        (u64, u32, u64, u64, u64, u32),
        launchNames,
        blockDim,
        argsInfo,
        smDesc,
        stm,
        flags
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn rtKernelLaunchWithFlagV2(
    stubFunc: u64,
    blockDim: u32,
    argsInfo: u64,
    smDesc: u64,
    stm: u64,
    flags: u32,
    cfgInfo: u64,
) -> u64 {
    npu_limiter().wait_for_token(stm);
    passthrough!(
        "rtKernelLaunchWithFlagV2",
        (u64, u32, u64, u64, u64, u32, u64),
        stubFunc,
        blockDim,
        argsInfo,
        smDesc,
        stm,
        flags,
        cfgInfo
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn rtKernelLaunchWithHandleV2(
    handle: u64,
    tilingKey: u64,
    blockDim: u32,
    argsInfo: u64,
    smDesc: u64,
    stm: u64,
    cfgInfo: u64,
) -> u64 {
    npu_limiter().wait_for_token(stm);
    passthrough!(
        "rtKernelLaunchWithHandleV2",
        (u64, u64, u32, u64, u64, u64, u64),
        handle,
        tilingKey,
        blockDim,
        argsInfo,
        smDesc,
        stm,
        cfgInfo
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn rtModelExecute(mdl: u64, stm: u64, flag: u32) -> u64 {
    npu_limiter().wait_for_token(stm);
    passthrough!("rtModelExecute", (u64, u64, u32), mdl, stm, flag)
}

#[unsafe(no_mangle)]
pub extern "C" fn rtMalloc(devPtr: u64, size: u64, t: u32, moduleId: u16) -> u64 {
    crate::memory::allocate_virtual(devPtr, size, true, || {
        passthrough!("rtMalloc", (u64, u64, u32, u16), devPtr, size, t, moduleId)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rtMallocCached(devPtr: u64, size: u64, t: u32, moduleId: u16) -> u64 {
    crate::memory::allocate_virtual(devPtr, size, false, || {
        passthrough!(
            "rtMallocCached",
            (u64, u64, u32, u16),
            devPtr,
            size,
            t,
            moduleId
        )
    })
}

// CANN 8.5.2's rtsMalloc tail-calls this five-argument runtime entry point.
// Preserve both 32-bit arguments and the configuration pointer unchanged; this
// policy/configuration ABI differs from rtMalloc's type flags/module ID.
#[unsafe(no_mangle)]
pub extern "C" fn rtMemAlloc(devPtr: u64, size: u64, policy: u32, flags: u32, cfg: u64) -> u64 {
    crate::memory::allocate_virtual(devPtr, size, false, || {
        passthrough!(
            "rtMemAlloc",
            (u64, u64, u32, u32, u64),
            devPtr,
            size,
            policy,
            flags,
            cfg
        )
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rtMallocPhysical(handle: u64, size: u64, prop: u64, flags: u64) -> u64 {
    // rtDrvMemProp_t in CANN 8.5.2 begins with uint32 side, uint32 devid.
    // Device memory is side=1, and devid is the target logical ordinal. ACL's
    // public physical-memory property has a DIFFERENT layout.
    let target = if prop != 0 {
        let prefix = unsafe { std::ptr::read_unaligned(prop as *const [u32; 2]) };
        (prefix[0] == 1)
            .then_some(prefix[1])
            .filter(|id| *id <= i32::MAX as u32)
    } else {
        None
    };
    let Some(target) = target else {
        // Host allocations and invalid/null properties retain native validation.
        return passthrough!(
            "rtMallocPhysical",
            (u64, u64, u64, u64),
            handle,
            size,
            prop,
            flags
        );
    };
    let client = crate::npu_limiter_for_device(target as i32);
    if client.is_hbm_limited() {
        if client.check_memory_quota(size) == 0 {
            let ret = passthrough!(
                "rtMallocPhysical",
                (u64, u64, u64, u64),
                handle,
                size,
                prop,
                flags
            );
            if ret == RT_ERROR_NONE {
                // malloc successfully
                let actual_handle = unsafe { *(handle as *const u64) };
                client.post_alloc_hbm_kind(actual_handle, size, ret, AllocationKind::Physical);
                return RT_ERROR_NONE;
            } else {
                // malloc failed
                client.post_alloc_hbm_kind(0, size, ret, AllocationKind::Physical);
                return RT_ERROR_MEMORY_ALLOCATION;
            }
        } else {
            return RT_ERROR_MEMORY_ALLOCATION;
        }
    }

    passthrough!(
        "rtMallocPhysical",
        (u64, u64, u64, u64),
        handle,
        size,
        prop,
        flags
    )
}

// CANN's runtime ABI takes the mapped VA first, then the output handle.
#[unsafe(no_mangle)]
pub extern "C" fn rtMemRetainAllocationHandle(virPtr: u64, handle: u64) -> u64 {
    crate::memory::retain_with_accounting(handle, || {
        passthrough!("rtMemRetainAllocationHandle", (u64, u64), virPtr, handle)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rtFreePhysical(handle: u64) -> u64 {
    crate::memory::free_with_accounting(handle, AllocationKind::Physical, || {
        passthrough!("rtFreePhysical", (u64), handle)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rtFree(ptr: u64) -> u64 {
    crate::memory::free_with_accounting(ptr, AllocationKind::Virtual, || {
        passthrough!("rtFree", (u64), ptr)
    })
}

// CANN 8.5.2's rtsFree dispatches directly to the same one-pointer native
// implementation as rtFree; it does not pass through the rtFree export.
#[unsafe(no_mangle)]
pub extern "C" fn rtsFree(ptr: u64) -> u64 {
    crate::memory::free_with_accounting(ptr, AllocationKind::Virtual, || {
        passthrough!("rtsFree", (u64), ptr)
    })
}

#[unsafe(no_mangle)]
/// # Safety
/// `free` and `total` must satisfy the CANN API's writable output-pointer contract.
pub unsafe extern "C" fn rtMemGetInfoEx(
    memInfoType: u64,
    free: *mut usize,
    total: *mut usize,
) -> u64 {
    let client = npu_limiter();
    if client.is_hbm_limited() {
        unsafe { client.get_hbm_info(free, total) };
        return 0;
    }
    passthrough!(
        "rtMemGetInfoEx",
        (u64, *mut usize, *mut usize),
        memInfoType,
        free,
        total
    )
}

// ACL ABI from CANN 9.1 acl_rt.h; resource enums and aclError are C ints.
#[unsafe(no_mangle)]
pub extern "C" fn aclrtSetDevice(device: i32) -> i32 {
    crate::spatial::set_device(device)
}

#[unsafe(no_mangle)]
/// # Safety
/// `context` must be a valid live CANN context handle.
pub unsafe extern "C" fn aclrtSetCurrentContext(context: *mut std::ffi::c_void) -> i32 {
    unsafe { crate::spatial::set_context(context) }
}

#[unsafe(no_mangle)]
pub extern "C" fn aclrtResetDevice(device: i32) -> i32 {
    crate::spatial::reset_device(device, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn aclrtResetDeviceForce(device: i32) -> i32 {
    crate::spatial::reset_device(device, true)
}

#[unsafe(no_mangle)]
pub extern "C" fn aclrtSetDeviceResLimit(device: i32, resource: i32, value: u32) -> i32 {
    crate::spatial::set_device_resource(device, resource, value)
}

#[unsafe(no_mangle)]
pub extern "C" fn aclrtResetDeviceResLimit(device: i32) -> i32 {
    crate::spatial::reset_device_resources(device)
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be a valid live stream for the current context, or null for the
/// default stream as permitted by CANN.
pub unsafe extern "C" fn aclrtSetStreamResLimit(
    stream: *mut std::ffi::c_void,
    resource: i32,
    value: u32,
) -> i32 {
    unsafe { crate::spatial::set_stream_resource(stream, resource, value) }
}

#[unsafe(no_mangle)]
/// # Safety
/// `stream` must be a valid live stream for the current context, or null for the
/// default stream as permitted by CANN.
pub unsafe extern "C" fn aclrtResetStreamResLimit(stream: *mut std::ffi::c_void) -> i32 {
    unsafe { crate::spatial::reset_stream_resources(stream) }
}

// Runtime-only applications must receive the same before-tiling budgets as ACL users.
#[unsafe(no_mangle)]
pub extern "C" fn rtSetDevice(device: i32) -> i32 {
    crate::spatial::runtime_selection(|| passthrough!("rtSetDevice", (i32), device) as i32)
}

#[unsafe(no_mangle)]
/// # Safety
/// `context` must be a valid live CANN context handle.
pub unsafe extern "C" fn rtCtxSetCurrent(context: *mut std::ffi::c_void) -> i32 {
    crate::spatial::runtime_selection(|| {
        passthrough!("rtCtxSetCurrent", (*mut std::ffi::c_void), context) as i32
    })
}
