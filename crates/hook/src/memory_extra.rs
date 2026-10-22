//! Direct runtime DVPP device-memory entry points.
//!
//! CANN 8.5.2's public ACL DVPP allocators already enter rtMemAlloc. These
//! exports cover callers of the separate runtime DVPP allocation/free table.

#![allow(non_snake_case)]

use limiter::worker::AllocationKind;

#[unsafe(no_mangle)]
pub extern "C" fn rtDvppMalloc(devPtr: u64, size: u64, moduleId: u16) -> u64 {
    crate::memory::allocate_virtual(devPtr, size, false, || {
        passthrough!("rtDvppMalloc", (u64, u64, u16), devPtr, size, moduleId)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rtDvppMallocWithFlag(devPtr: u64, size: u64, flags: u32, moduleId: u16) -> u64 {
    crate::memory::allocate_virtual(devPtr, size, false, || {
        passthrough!(
            "rtDvppMallocWithFlag",
            (u64, u64, u32, u16),
            devPtr,
            size,
            flags,
            moduleId
        )
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rtDvppFree(ptr: u64) -> u64 {
    crate::memory::free_with_accounting(ptr, AllocationKind::Virtual, || {
        passthrough!("rtDvppFree", (u64), ptr)
    })
}
