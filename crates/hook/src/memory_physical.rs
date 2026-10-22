//! The RTS physical allocator creates its backing directly in CANN 8.5.2.
//! Its DEVICE_ID configuration selects a logical target, including a card
//! other than the current card; it is not the ordinary rtMemAlloc contract.

#![allow(non_snake_case)]

use limiter::externed_api::{RT_ERROR_MEMORY_ALLOCATION, RT_ERROR_NONE};
use limiter::worker::AllocationKind;

#[repr(C)]
struct MallocAttribute {
    kind: u32,
    value: [u32; 2],
}

#[repr(C)]
struct MallocConfig {
    attrs: *const MallocAttribute,
    count: usize,
}

// The actual ARM64 SDK layout is tag@0, value@4, stride12; config is 16 bytes.
const _: () = {
    assert!(std::mem::size_of::<MallocAttribute>() == 12);
    assert!(std::mem::offset_of!(MallocAttribute, value) == 4);
    assert!(std::mem::size_of::<MallocConfig>() == 16);
};

fn target_device(cfg: u64) -> Option<i32> {
    let mut configured = None;
    if cfg != 0 {
        // As in CANN, valid callers own the configuration and its attributes
        // for the duration of this call. Never rewrite or reinterpret it as
        // ACL's physical-property structure.
        let config = unsafe { std::ptr::read_unaligned(cfg as *const MallocConfig) };
        if config.attrs.is_null() && config.count != 0 {
            return None; // Preserve native validation of an invalid array.
        }
        for index in 0..config.count {
            let attr = unsafe { std::ptr::read_unaligned(config.attrs.add(index)) };
            if attr.kind == 2 {
                // CANN's ParseMallocCfg uses the last duplicate DEVICE_ID.
                configured = Some(attr.value[0]);
            }
        }
    }
    let logical = if let Some(device) = configured {
        i32::try_from(device).ok()?
    } else {
        let mut current = -1;
        if unsafe { limiter::externed_api::rtGetDevice(&mut current) } != 0 {
            return None;
        }
        current
    };
    let mut physical = 0;
    if logical < 0
        || unsafe { limiter::externed_api::rtGetDevicePhyIdByIndex(logical as u32, &mut physical) }
            != 0
    {
        // An invalid target belongs to native validation, not the limiter's
        // fatal path for failing to initialize a valid device client.
        return None;
    }
    Some(logical)
}

#[unsafe(no_mangle)]
pub extern "C" fn rtsMemMallocPhysical(handle: u64, size: u64, policy: u32, cfg: u64) -> u64 {
    let native = || {
        passthrough!(
            "rtsMemMallocPhysical",
            (u64, u64, u32, u64),
            handle,
            size,
            policy,
            cfg
        )
    };
    if handle == 0 || size == 0 {
        return native();
    }
    let Some(target) = target_device(cfg) else {
        return native();
    };
    let client = crate::npu_limiter_for_device(target);
    if !client.is_hbm_limited() {
        return native();
    }
    if client.check_memory_quota(size) != RT_ERROR_NONE {
        return RT_ERROR_MEMORY_ALLOCATION;
    }
    let result = native();
    let actual_handle = if result == RT_ERROR_NONE {
        unsafe { std::ptr::read_unaligned(handle as *const u64) }
    } else {
        0
    };
    client.post_alloc_hbm_kind(actual_handle, size, result, AllocationKind::Physical);
    result
}
