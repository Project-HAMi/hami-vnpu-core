use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64};

pub mod setup;
pub mod futex;

// =========================================================================================
// GLOBAL SHARED MEMORY LAYOUT
// =========================================================================================

pub const MAX_MANAGERS: usize = 64;

#[repr(C)]
pub struct GlobalManagerSlot {
    pub pid: AtomicI32,
    pub avg_kernel_time: AtomicU64,
    pub last_heartbeat: AtomicU64,
    pub is_active: AtomicU32,
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
}

#[repr(C)]
#[derive(Debug)]
pub struct LocalContainerShmem {
    pub memory_limit: AtomicU64,
    pub memory_used: AtomicU64,

    pub state: AtomicU32,
    pub batch_id: AtomicU64,

    pub tokens_remaining: AtomicU64,

    pub active_workers: AtomicU32,
    pub reported_count: AtomicU32,

    pub reports: [LocalWorkerReport; MAX_WORKERS],
}