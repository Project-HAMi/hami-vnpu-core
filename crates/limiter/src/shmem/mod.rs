use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64};

pub mod futex;
pub mod setup;

// =========================================================================================
// GLOBAL SHARED MEMORY LAYOUT
// =========================================================================================

pub const MAX_MANAGERS: usize = 64;

#[repr(C)]
pub struct GlobalManagerSlot {
    pub pid: AtomicI32,
    // Uses the former alignment padding; published with is_active under the
    // registry transaction lock. Distinguishes a reused global slot.
    pub generation: AtomicU32,
    pub avg_kernel_time: AtomicU64,
    pub last_heartbeat: AtomicU64,
    pub is_active: AtomicU32,
    pub compute_limit: AtomicU64,
}

#[repr(C)]
pub struct GlobalRegistry {
    pub lock_owner: AtomicU32,
    pub lock_timestamp: AtomicU64,

    pub queue_head: AtomicU32,
    pub queue_tail: AtomicU32,
    pub queue: [AtomicU32; MAX_MANAGERS],

    pub slots: [GlobalManagerSlot; MAX_MANAGERS],

    pub signal_counter: AtomicU32,
}

// =========================================================================================
// LOCAL SHARED MEMORY LAYOUT
// =========================================================================================

pub const MAX_WORKERS: usize = 32;
pub const MAX_PROCESSES: usize = 64;
pub const NPU_DEVICE_MAX: usize = 8;

pub type LocalState = u32;

pub const STATE_IDLE: LocalState = 0;
pub const STATE_RUNNING: LocalState = 1;
pub const STATE_MEASURING: LocalState = 2;

#[repr(C)]
#[derive(Debug)]
pub struct LocalWorkerReport {
    pub batch_id: AtomicU64,
    pub cpu_start_us: AtomicU64,
    pub duration_us: AtomicU64,
    pub occupied: AtomicU32,
    pub pid: AtomicI32,
}

#[repr(C)]
#[derive(Debug)]
pub struct ProcessSlot {
    pub pid: AtomicI32,                        // container PID; 0 = free
    pub hbm_used: [AtomicU64; NPU_DEVICE_MAX], // per-device HBM bytes; 4B pad precedes (host_pid removed)
    pub is_active: AtomicU32,                  // 1 = registered
}

// The device-plugin monitor reads ProcessSlot by hardcoded byte offsets; lock the layout
// so a field change fails the build here instead of corrupting its readings.
const _: () = {
    assert!(std::mem::size_of::<ProcessSlot>() == 80);
    assert!(std::mem::offset_of!(ProcessSlot, pid) == 0);
    assert!(std::mem::offset_of!(ProcessSlot, hbm_used) == 8);
    assert!(std::mem::offset_of!(ProcessSlot, is_active) == 72);
};

#[repr(C)]
#[derive(Debug)]
pub struct LocalContainerShmem {
    pub memory_limit: AtomicU64,
    pub memory_used: AtomicU64,
    pub compute_limit: AtomicU64,

    pub state: AtomicU32,
    pub batch_id: AtomicU64,

    pub tokens_remaining: AtomicU64,

    pub active_workers: AtomicU32,
    pub reported_count: AtomicU32,

    pub reports: [LocalWorkerReport; MAX_WORKERS],

    pub procs: [ProcessSlot; MAX_PROCESSES],

    pub manager_pid: AtomicI32, // elected manager's container PID; 0 = none
    pub manager_generation: AtomicU32, // former padding; fences a replaced manager
    pub manager_heartbeat: AtomicU64, // manager heartbeat (us); stale => eligible for takeover
    pub manager_global_idx: AtomicI32, // global slot the manager holds (for takeover reclaim)
    pub initialized: AtomicU32, // manager published resource configuration

    // Append only: the monitor's existing reports/procs offsets remain stable.
    pub overdraft_pool: crate::overdraft::OverdraftPool,
    pub overdraft_limit: AtomicU32,
    pub overdraft_timeout_us: AtomicU64,
    pub overdraft_poll_interval_us: AtomicU64,

    // Append only: keep report/process/overdraft offsets used by monitors stable.
    // A worker writes its monotonic entry timestamp before device synchronization;
    // the report's release-store of batch_id publishes it with the completed report.
    pub measurement_started_us: [AtomicU64; MAX_WORKERS],
}

const _: () = {
    assert!(std::mem::size_of::<GlobalManagerSlot>() == 40);
    assert!(std::mem::size_of::<GlobalRegistry>() == 2848);
    assert!(std::mem::size_of::<LocalContainerShmem>() == 6512);
    assert!(std::mem::offset_of!(GlobalManagerSlot, compute_limit) == 32);
    assert!(std::mem::offset_of!(LocalContainerShmem, compute_limit) == 16);
    assert!(std::mem::offset_of!(LocalContainerShmem, reports) == 56);
    assert!(std::mem::offset_of!(LocalContainerShmem, overdraft_pool) == 6224);
    assert!(std::mem::offset_of!(LocalContainerShmem, procs) == 1080);
    assert!(std::mem::offset_of!(LocalContainerShmem, manager_generation) == 6204);
    assert!(std::mem::offset_of!(LocalContainerShmem, measurement_started_us) == 6256);
};

/// Comparable across processes on the same host, unaffected by wall-clock changes.
/// Zero means no usable timestamp; callers must reject that timing sample.
pub fn monotonic_time_us() -> u64 {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000))
        .and_then(|micros| micros.checked_add(time.tv_nsec as u64 / 1000))
        .unwrap_or(0)
}
