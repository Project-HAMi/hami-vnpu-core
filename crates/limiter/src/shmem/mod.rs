use std::sync::atomic::Ordering;
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

/// Normal admission reserves one ticket from one batch in a single CAS.
/// The low 21 bits hold the balance (manager budgets are at most 2,000,000);
/// the upper 43 bits hold the batch. Closed batches retain their identity.
/// Batch IDs never wrap or reopen, so an old snapshot cannot match a refill.
#[repr(transparent)]
#[derive(Debug)]
pub struct NormalTokens(AtomicU64);

impl NormalTokens {
    pub const MAX_TOKENS: u64 = (1 << 21) - 1;
    pub const MAX_BATCH: u64 = u64::MAX >> 21;

    pub fn remaining(&self) -> u64 {
        self.snapshot() & Self::MAX_TOKENS
    }

    /// Publish a strictly newer batch. Only the elected manager may call this,
    /// while holding the registry transaction and before publishing RUNNING.
    pub fn open(&self, batch: u64, tokens: u64) -> bool {
        if batch == 0 || batch > Self::MAX_BATCH || tokens > Self::MAX_TOKENS {
            return false;
        }
        let mut current = self.snapshot();
        loop {
            if batch <= current >> 21 {
                return false;
            }
            match self.0.compare_exchange_weak(
                current,
                (batch << 21) | tokens,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Invalidate positive-balance snapshots before MEASURING or lease cleanup.
    /// Keep the batch tag so closing cannot allow the same batch to reopen.
    pub fn close(&self) -> u64 {
        self.0.fetch_and(!Self::MAX_TOKENS, Ordering::AcqRel) & Self::MAX_TOKENS
    }

    pub fn snapshot(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    pub fn try_acquire(&self, snapshot: u64, batch: u64) -> bool {
        batch != 0
            && snapshot >> 21 == batch
            && snapshot & Self::MAX_TOKENS != 0
            && self
                .0
                .compare_exchange(snapshot, snapshot - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
}

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

    pub tokens_remaining: NormalTokens,

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

    // The report batch release-store publishes this count with its duration.
    // Only successful post-checked admissions contribute, never retired tickets.
    pub admitted_tokens: [AtomicU64; MAX_WORKERS],
}

const _: () = {
    assert!(std::mem::size_of::<GlobalManagerSlot>() == 40);
    assert!(std::mem::size_of::<GlobalRegistry>() == 2848);
    assert!(std::mem::size_of::<LocalContainerShmem>() == 6768);
    assert!(std::mem::size_of::<NormalTokens>() == 8);
    assert!(std::mem::offset_of!(LocalContainerShmem, tokens_remaining) == 40);
    assert!(std::mem::offset_of!(GlobalManagerSlot, compute_limit) == 32);
    assert!(std::mem::offset_of!(LocalContainerShmem, compute_limit) == 16);
    assert!(std::mem::offset_of!(LocalContainerShmem, reports) == 56);
    assert!(std::mem::offset_of!(LocalContainerShmem, overdraft_pool) == 6224);
    assert!(std::mem::offset_of!(LocalContainerShmem, procs) == 1080);
    assert!(std::mem::offset_of!(LocalContainerShmem, manager_generation) == 6204);
    assert!(std::mem::offset_of!(LocalContainerShmem, measurement_started_us) == 6256);
    assert!(std::mem::offset_of!(LocalContainerShmem, admitted_tokens) == 6512);
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

#[cfg(test)]
mod normal_token_tests {
    use super::*;

    #[test]
    fn stale_snapshot_cannot_spend_equal_balance_in_a_new_batch() {
        let pool = NormalTokens(AtomicU64::new(0));
        assert!(pool.open(42, 1));
        let old = pool.snapshot();
        assert_eq!(pool.close(), 1);
        assert!(pool.open(43, 1));
        assert!(!pool.try_acquire(old, 42));
        assert_eq!(
            pool.remaining(),
            1,
            "no conservative loss of the new batch's ticket"
        );
        let fresh = pool.snapshot();
        assert!(
            !pool.try_acquire(fresh, 42),
            "a new snapshot cannot use an old context"
        );
        assert!(pool.try_acquire(fresh, 43));
        assert_eq!(pool.remaining(), 0);
        assert!(!pool.try_acquire(fresh, 43));
    }

    #[test]
    fn closed_and_exhausted_batches_cannot_reopen_or_wrap() {
        let pool = NormalTokens(AtomicU64::new(0));
        assert!(!pool.try_acquire(0, 0));
        assert!(!pool.open(0, 1));
        assert!(!pool.open(1, NormalTokens::MAX_TOKENS + 1));
        assert!(pool.open(1, 1));
        let old = pool.snapshot();
        pool.close();
        assert!(!pool.try_acquire(old, 1));
        assert!(!pool.open(1, 1), "closing must retain the batch identity");
        assert!(pool.open(NormalTokens::MAX_BATCH, NormalTokens::MAX_TOKENS));
        assert!(pool.try_acquire(pool.snapshot(), NormalTokens::MAX_BATCH));
        assert_eq!(pool.close(), NormalTokens::MAX_TOKENS - 1);
        assert!(!pool.open(NormalTokens::MAX_BATCH + 1, 1));
        assert!(!pool.open(1, 1));
        assert_eq!(pool.remaining(), 0);
    }
}
