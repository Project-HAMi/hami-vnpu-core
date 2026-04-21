//! Memory metrics for gRPC reporting: quota view from `LocalContainerShmem` when
//! `NPU_MEM_QUOTA` is set; otherwise `rtMemGetInfoEx` on the device (same source
//! the hook uses when quota is not enforced).

use crate::externed_api::{rtMemGetInfoEx, rtSetDevice};
use crate::shmem::LocalContainerShmem;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

const MB: u64 = 1024 * 1024;
/// Matches the hook's `rtMemGetInfoEx` first argument when not using quota.
const MEM_INFO_TYPE: u64 = 0;

/// Serialize runtime memory queries so `rtSetDevice` / `rtMemGetInfoEx` do not
/// race with other threads using the runtime.
fn rt_mem_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("memory report lock poisoned")
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryMetrics {
    pub limit_enforced: bool,
    pub total_mb: u64,
    pub used_mb: u64,
    pub free_mb: u64,
}

/// `logical_device` is the index among visible devices (0, 1, …), passed to
/// `rtSetDevice` when falling back to the runtime.
pub fn memory_metrics(local: &LocalContainerShmem, logical_device: i32) -> MemoryMetrics {
    let limit = local.memory_limit.load(Ordering::Relaxed);
    if limit > 0 {
        let total_mb = limit / MB;
        let used = local.memory_used.load(Ordering::Acquire);
        let used_mb = used / MB;
        let free_mb = total_mb.saturating_sub(used_mb);
        return MemoryMetrics {
            limit_enforced: true,
            total_mb,
            used_mb,
            free_mb,
        };
    }

    let _g = rt_mem_lock();
    unsafe {
        if rtSetDevice(logical_device) != 0 {
            return MemoryMetrics {
                limit_enforced: false,
                total_mb: 0,
                used_mb: 0,
                free_mb: 0,
            };
        }
        let mut free = 0usize;
        let mut total = 0usize;
        let rc = rtMemGetInfoEx(MEM_INFO_TYPE, &mut free, &mut total);
        if rc != 0 {
            return MemoryMetrics {
                limit_enforced: false,
                total_mb: 0,
                used_mb: 0,
                free_mb: 0,
            };
        }
        let total_mb = (total as u64) / MB;
        let free_mb = (free as u64) / MB;
        let used_mb = total_mb.saturating_sub(free_mb);
        MemoryMetrics {
            limit_enforced: false,
            total_mb,
            used_mb,
            free_mb,
        }
    }
}
