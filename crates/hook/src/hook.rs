#![allow(non_snake_case)]
use lazy_static::lazy_static;
use limiter::worker::SchedulerClient;
use once_cell;
use limiter::externed_api::{RT_ERROR_NONE, RT_ERROR_MEMORY_ALLOCATION};
use once_cell::sync::Lazy;

lazy_static!{
    pub static ref NPU_LIMITER: SchedulerClient = SchedulerClient::new();
}

macro_rules! passthrough {
    ($name:expr, ($($sig:tt)*), $($arg:expr),*) => {
        {
            static REAL: Lazy<extern "C" fn($($sig)*) -> u64> = 
                Lazy::new(|| unsafe {
                    let ptr = libc::dlsym(libc::RTLD_NEXT, concat!($name, "\0").as_ptr() as *const libc::c_char);
                    if ptr.is_null() {
                        panic!("cannot find original function: {}", $name);
                    }
                    std::mem::transmute(ptr)
                });
            // println!("in func {:?}", $name);
            (*REAL)($($arg),*)
        }
    };
}

#[unsafe(no_mangle)]
pub extern "C" fn rtAicpuKernelLaunchExWithArgs(kernelType: u32, opName: u64, blockDim: u32, argsInfo: u64, smDesc: u64, stm: u64, flags: u32) -> u64 {
    NPU_LIMITER.wait_for_token(stm);
    return passthrough!("rtAicpuKernelLaunchExWithArgs", (u32, u64, u32, u64, u64, u64, u32), kernelType, opName, blockDim, argsInfo, smDesc, stm, flags);
}

#[unsafe(no_mangle)]
pub extern "C" fn rtAicpuKernelLaunchWithFlag(launchNames: u64, blockDim: u32, argsInfo: u64, smDesc: u64, stm: u64, flags: u32) -> u64 {
    NPU_LIMITER.wait_for_token(stm);
    return passthrough!("rtAicpuKernelLaunchWithFlag", (u64, u32, u64, u64, u64, u32), launchNames, blockDim, argsInfo, smDesc, stm, flags);
}

#[unsafe(no_mangle)]
pub extern "C" fn rtKernelLaunchWithFlagV2(stubFunc: u64, blockDim: u32, argsInfo: u64, smDesc: u64, stm: u64, flags: u32, cfgInfo: u64) -> u64 {
    NPU_LIMITER.wait_for_token(stm);
    return passthrough!("rtKernelLaunchWithFlagV2", (u64, u32, u64, u64, u64, u32, u64), stubFunc, blockDim, argsInfo, smDesc, stm, flags, cfgInfo);
}

#[unsafe(no_mangle)]
pub extern "C" fn rtKernelLaunchWithHandleV2(handle: u64, tilingKey: u64, blockDim: u32, argsInfo: u64, smDesc: u64, stm: u64, cfgInfo: u64) -> u64 {
    NPU_LIMITER.wait_for_token(stm);
    return passthrough!("rtKernelLaunchWithHandleV2", (u64, u64, u32, u64, u64, u64, u64), handle, tilingKey, blockDim, argsInfo, smDesc, stm, cfgInfo);
}

#[unsafe(no_mangle)]
pub extern "C" fn rtModelExecute(mdl: u64, stm: u64, flag: u32) -> u64 {
    NPU_LIMITER.wait_for_token(stm);
    return passthrough!("rtModelExecute", (u64, u64, u32), mdl, stm, flag);
}

#[unsafe(no_mangle)]
pub extern "C" fn rtMalloc(devPtr: u64, size: u64, t: u32, moduleId: u16) -> u64 {
    if NPU_LIMITER.is_hbm_limited() {
        if NPU_LIMITER.check_memory_quota(size) == 0 {
            let ret = passthrough!("rtMalloc", (u64, u64, u32, u16), devPtr, size, t, moduleId);
            if ret == RT_ERROR_NONE { // malloc successfully
                let actual_ptr = unsafe { *(devPtr as *const u64) };
                NPU_LIMITER.post_alloc_hbm(actual_ptr, size, ret);
                return RT_ERROR_NONE;
            } else { // malloc failed
                NPU_LIMITER.post_alloc_hbm(0, size, ret);
                return RT_ERROR_MEMORY_ALLOCATION;
            }
        } else {
            return  RT_ERROR_MEMORY_ALLOCATION;
        }
    }
    
    return passthrough!("rtMalloc", (u64, u64, u32, u16), devPtr, size, t, moduleId);
}

#[unsafe(no_mangle)]
pub extern "C" fn rtMallocPhysical(handle: u64, size: u64, prop: u64, flags: u64) -> u64 {
    if NPU_LIMITER.is_hbm_limited() {
        if NPU_LIMITER.check_memory_quota(size) == 0 {
                let ret = passthrough!("rtMallocPhysical", (u64, u64, u64, u64), handle, size, prop, flags);
                if ret == RT_ERROR_NONE { // malloc successfully
                     let actual_handle = unsafe { *(handle as *const u64) };
                    NPU_LIMITER.post_alloc_hbm(actual_handle, size, ret);
                    return RT_ERROR_NONE;
                } else { // malloc failed
                    NPU_LIMITER.post_alloc_hbm(0, size, ret);
                    return RT_ERROR_MEMORY_ALLOCATION;
                }
            } else {
                return  RT_ERROR_MEMORY_ALLOCATION;
            }
    }

    return passthrough!("rtMallocPhysical", (u64, u64, u64, u64), handle, size, prop, flags);
}

#[unsafe(no_mangle)]
pub fn rtFreePhysical(handle: u64) -> u64 {
    let ret = passthrough!("rtFreePhysical", (u64), handle);
    if NPU_LIMITER.is_hbm_limited() {
        NPU_LIMITER.post_free_hbm(handle, ret);
    }

    return ret;
}

#[unsafe(no_mangle)]
pub fn rtFree(ptr: u64) -> u64 {
    let ret = passthrough!("rtFree", (u64), ptr);
    if NPU_LIMITER.is_hbm_limited() {
        NPU_LIMITER.post_free_hbm(ptr, ret); 
    }

    return ret;
}

#[unsafe(no_mangle)]
pub fn rtMemGetInfoEx(memInfoType: u64, free: *mut usize, total: *mut usize) -> u64 {
    if NPU_LIMITER.is_hbm_limited() {
        NPU_LIMITER.get_hbm_info(free, total);
        return 0;
    }
    return passthrough!("rtMemGetInfoEx", (u64, *mut usize,  *mut usize), memInfoType, free, total);
}

static REAL_SIGACTION: Lazy<extern "C" fn(libc::c_int, *const libc::sigaction, *mut libc::sigaction) -> libc::c_int> = Lazy::new(|| unsafe {
    let ptr = libc::dlsym(libc::RTLD_NEXT, b"sigaction\0".as_ptr() as *const libc::c_char);
    if ptr.is_null() {
        panic!("Cannot find original sigaction function!");
    }
    std::mem::transmute(ptr)
});

// workaround to solve python error
#[unsafe(no_mangle)]
pub extern "C" fn sigaction(
    signum: libc::c_int,
    act: *const libc::sigaction,
    oldact: *mut libc::sigaction,
) -> libc::c_int {
    let ret = REAL_SIGACTION(signum, act, oldact);

    if ret == 0 && !oldact.is_null() {
        unsafe {
            let handler = (*oldact).sa_sigaction;
            if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
                
                (*oldact).sa_sigaction = libc::SIG_DFL;
            }
        }
    }
    
    ret
}