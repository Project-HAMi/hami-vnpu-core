use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH, Instant, Duration};
use std::thread;
use std::fmt;
use std::collections::HashMap;
use std::fs;

use log::{info, debug, warn};

use crate::shmem::{self, LocalContainerShmem, futex, MAX_WORKERS, STATE_IDLE, STATE_RUNNING, STATE_MEASURING};
use crate::config::{local_shmem_path, VIRTUAL_OVERHEAD_MB};
use crate::externed_api::*;
use crate::check_rts;

/// Max time a worker waits for the manager to create + initialize shmem before
/// giving up. On timeout SchedulerClient::new panics and npu_limiter() falls back
/// to a no-op stub, so a worker never hangs forever when no manager is elected.
const SHMEM_WAIT_TIMEOUT_SECS: u64 = 10;

/// Atomically subtract `v` from `a`, saturating at 0 (never wraps to ~2^64 on an
/// accounting slip). Returns true if it hit the floor, so the caller can heal.
fn atomic_saturating_sub(a: &std::sync::atomic::AtomicU64, v: u64) -> bool {
    a.fetch_update(Ordering::SeqCst, Ordering::Acquire, |cur| Some(cur.saturating_sub(v)))
        .map(|prev| prev < v)
        .unwrap_or(false)
}

/// Authoritative ledger: each slot includes pending reservations and allocations.
/// SeqCst publication before admission ensures the last concurrent reservation
/// observes all still-live earlier reservations. Contention may reject both
/// callers conservatively, but cannot admit more than the limit.
fn ledger_total(shmem: &LocalContainerShmem) -> Option<u64> {
    shmem.procs.iter().flat_map(|slot| &slot.hbm_used)
        .try_fold(0u64, |sum, used| sum.checked_add(used.load(Ordering::SeqCst)))
}

/// Publish a sampled compatibility metric; admission uses the per-slot ledger.
fn publish_usage(shmem: &LocalContainerShmem) {
    // Compatibility telemetry only. Never use this sampled aggregate for admission.
    shmem.memory_used.store(ledger_total(shmem).unwrap_or(u64::MAX), Ordering::Release);
}

/// Negative PID holds a slot while it is being retired or initialized. If the
/// owner dies mid-cleanup, a subsequent reaper can claim and finish the cleanup.
fn retire_slot(slot: &shmem::ProcessSlot, observed_pid: i32) -> bool {
    let owner = -(std::process::id() as i32);
    if slot.pid.compare_exchange(observed_pid, owner, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return false;
    }
    for used in &slot.hbm_used {
        used.store(0, Ordering::SeqCst);
    }
    slot.is_active.store(0, Ordering::Release);
    slot.pid.store(0, Ordering::SeqCst);
    true
}

/// Retire exited owners, including abandoned negative-PID cleanup claims.
pub(crate) fn reclaim_dead_process_slots(shmem: &LocalContainerShmem) {
    for slot in &shmem.procs {
        let pid = slot.pid.load(Ordering::SeqCst);
        if pid != 0 && !proc_alive(pid.saturating_abs()) {
            retire_slot(slot, pid);
        }
    }
    publish_usage(shmem);
}

#[derive(Debug)]
struct InnerLock {
    internal_stream: u64,
    start_event: u64,
    end_event: u64,
    tracking_event: u64,
    
    batch_active: bool,
    current_batch_id: u64,
    last_user_stream: u64, // To track where to record the tracking_event
    start_time_us: u64,    // Wall-clock fallback start timestamp
}

#[derive(Clone, Debug)]
pub struct SchedulerClient {
    inner: Arc<SchedulerClientInner>,
}

struct SchedulerClientInner {
    shmem: &'static LocalContainerShmem,
    my_slot_idx: usize,
    my_proc_idx: usize,
    device_id: usize,
    lock: Mutex<InnerLock>,
    hbm_handle_map: Mutex<HashMap<u64, (usize, u64)>>,
    /// True for the no-op stub (no shmem / no manager). All limit checks are
    /// short-circuited so the worker never blocks on the state futex.
    is_stub: bool,
}

impl fmt::Debug for SchedulerClientInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerClientInner")
            .field("my_slot_idx", &self.my_slot_idx)
            .field("my_proc_idx", &self.my_proc_idx)
            .field("shmem", &self.shmem)
            .finish()
    }
}

impl SchedulerClient {
    /// Initialized ONCE per NPUDeviceList (per process/device)
    pub fn new() -> Self {
        let my_pid = std::process::id() as i32;
        info!("[Worker PID:{}] Initialize SchedulerClient...", my_pid);

        let shmem_path = local_shmem_path();
        let deadline = Instant::now() + Duration::from_secs(SHMEM_WAIT_TIMEOUT_SECS);
        let shmem = loop {
            if let Some(s) = shmem::setup::try_open_shmem::<LocalContainerShmem>(shmem_path.as_str()) {
                if s.initialized.load(Ordering::Acquire) == 1 {
                    break s;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "[Worker PID:{}] timed out after {}s waiting for manager to initialize shmem",
                    my_pid, SHMEM_WAIT_TIMEOUT_SECS
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };

        // Register THIS Client in a free worker report slot
        let my_slot_idx = Self::register_worker_slot(shmem, my_pid);
        debug!("[Scheduler] Client Registered at worker slot {}", my_slot_idx);

        // Register THIS Process in a free process slot
        let my_proc_idx = Self::register_proc_slot(shmem, my_pid);
        info!("[Worker PID:{}] Registered at proc slot {}", my_pid, my_proc_idx);

        // Initialize internal NPU resources used for timing.
        let mut dev: i32 = 0;
        unsafe { rtGetDevice(&mut dev); }
        let device_id = dev.max(0) as usize;

        let inner_lock = Self::create_inner_lock();

        Self {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                my_slot_idx,
                my_proc_idx,
                device_id,
                lock: Mutex::new(inner_lock),
                hbm_handle_map: Mutex::new(HashMap::new()),
                is_stub: false,
            }),
        }
    }

    fn register_worker_slot(shmem: &'static LocalContainerShmem, pid: i32) -> usize {
        // Pass 1: reuse our own existing slot
        for (i, slot) in shmem.reports.iter().enumerate() {
            if slot.pid.load(Ordering::Relaxed) == pid {
                slot.batch_id.store(0, Ordering::Relaxed);
                slot.cpu_start_us.store(0, Ordering::Relaxed);
                slot.duration_us.store(0, Ordering::Relaxed);
                return i;
            }
        }

        // Pass 2: claim a free slot or CAS-reclaim a dead process's slot
        for (i, slot) in shmem.reports.iter().enumerate() {
            let slot_pid = slot.pid.load(Ordering::Relaxed);
            if slot_pid == 0 {
                if slot.occupied.compare_exchange(0, 1, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                    slot.pid.store(pid, Ordering::Relaxed);
                    return i;
                }
            } else if !proc_alive(slot_pid) {
                // Try to atomically swap our PID in — only one thread wins
                if slot.pid.compare_exchange(slot_pid, pid, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                    slot.batch_id.store(0, Ordering::Relaxed);
                    slot.cpu_start_us.store(0, Ordering::Relaxed);
                    slot.duration_us.store(0, Ordering::Relaxed);
                    return i;
                }
                // Lost the race — slot_pid already changed, continue searching
            }
        }

        panic!("[Scheduler] Registry full ({} workers). Increase MAX_WORKERS.", MAX_WORKERS);
    }

    /// Claim a process slot without clearing an existing same-process ledger.
    fn register_proc_slot(shmem: &'static LocalContainerShmem, pid: i32) -> usize {
        reclaim_dead_process_slots(shmem);
        for (i, slot) in shmem.procs.iter().enumerate() {
            if slot.pid.load(Ordering::SeqCst) == pid {
                return i;
            }
        }
        for (i, slot) in shmem.procs.iter().enumerate() {
            if slot.pid.compare_exchange(0, -pid, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                for used in &slot.hbm_used {
                    used.store(0, Ordering::SeqCst);
                }
                slot.is_active.store(1, Ordering::Release);
                slot.pid.store(pid, Ordering::SeqCst);
                publish_usage(shmem);
                return i;
            }
        }
        panic!("[Scheduler] Process registry full. Increase MAX_PROCESSES.");
    }

    fn create_inner_lock() -> InnerLock {
        let mut internal_stream: u64 = 0;
        let mut start_event: u64 = 0;
        let mut end_event: u64 = 0;
        let mut tracking_event: u64 = 0;

        check_rts!(rtStreamCreate(&mut internal_stream, 0));
        check_rts!(rtEventCreate(&mut start_event));
        check_rts!(rtEventCreate(&mut end_event));
        check_rts!(rtEventCreate(&mut tracking_event));

        InnerLock {
            internal_stream,
            start_event,
            end_event,
            tracking_event,
            batch_active: false,
            current_batch_id: 0,
            last_user_stream: 0,
            start_time_us: 0,
        }
    }
}

// Limit computing power
impl SchedulerClient {
    /// The Main Entry Point
    pub fn wait_for_token(&self, user_stream: u64) {
        if self.inner.is_stub {
            return;
        }

        // We lock the mutex to safely access/modify internal state.
        // NOTE: In high contention, this serializes access to this check.
        let mut lock = self.inner.lock.lock().unwrap();

        lock.last_user_stream = user_stream;

        loop {
            // Read Shared Memory State
            let state = self.inner.shmem.state.load(Ordering::Acquire);
            let global_batch = self.inner.shmem.batch_id.load(Ordering::Relaxed);

            // Reset Logic (If Manager moved to new batch)
            if lock.batch_active && global_batch != lock.current_batch_id {
                lock.batch_active = false;
                lock.start_time_us = 0;
            }

            match state {
                // ---------------------------------------------------------
                // RUNNING: Try to grab token
                // ---------------------------------------------------------
                STATE_RUNNING => {
                    let tokens = self.inner.shmem.tokens_remaining.load(Ordering::Relaxed);
                    
                    if tokens == 0 {
                        // Release lock while yielding to allow other threads to enter
                        drop(lock); // UNLOCK
                        thread::yield_now();
                        lock = self.inner.lock.lock().unwrap(); // RELOCK
                        continue;
                    }

                    // Try to fetch token
                    let prev = self.inner.shmem.tokens_remaining.fetch_sub(1, Ordering::Acquire);
                    if prev > 0 {
                        // SUCCESS!
                        
                        // First Token of Batch
                        if !lock.batch_active {
                            debug!("[Worker PID:{} Slot:{}] get Batch {} first Token!, start record time...", std::process::id(), self.inner.my_slot_idx, global_batch);

                            lock.current_batch_id = global_batch;
                            lock.batch_active = true;
                            
                            // Notify Manager
                            self.inner.shmem.active_workers.fetch_add(1, Ordering::Release);

                            // CPU Start Time
                            let now_us = get_time_us();
                            self.inner.shmem.reports[self.inner.my_slot_idx].cpu_start_us.store(now_us, Ordering::Relaxed);
                            lock.start_time_us = now_us;

                            // GPU Start Event (Internal Stream)
                            check_rts!(rtEventRecord(lock.start_event, lock.internal_stream));
                        }
                        
                        return; // -> Kernel Launch
                    } else {
                        // Race failed
                        self.inner.shmem.tokens_remaining.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // ---------------------------------------------------------
                // MEASURING: Report Time
                // ---------------------------------------------------------
                STATE_MEASURING => {
                    if lock.batch_active && global_batch == lock.current_batch_id {
                        debug!("[Worker PID:{} Slot:{}] start measuring Batch {} ...", std::process::id(), self.inner.my_slot_idx, global_batch);
                        self.measure_and_report_batch(&mut lock);
                    }
                    
                    // Wait for state change
                    drop(lock); // UNLOCK
                    futex::wait(&self.inner.shmem.state, STATE_MEASURING);
                    lock = self.inner.lock.lock().unwrap(); // RELOCK
                }

                // ---------------------------------------------------------
                // IDLE
                // ---------------------------------------------------------
                _ => {
                    drop(lock); // UNLOCK
                    futex::wait(&self.inner.shmem.state, STATE_IDLE);
                    lock = self.inner.lock.lock().unwrap(); // RELOCK
                }
            }
        }
    }

    fn measure_and_report_batch(&self, lock: &mut InnerLock) {
        let mut duration_us: u64 = 0;
        let mut used_wall_clock = false;

        if lock.last_user_stream != 0 {
            let mut status: u32 = 0;
            let mut model: u64 = 0;
            let _ = check_rts!(rtStreamGetCaptureInfo(lock.last_user_stream, &mut status, &mut model));
            if status != 0 {
                debug!(
                    "[Limiter] stream 0x{:x} is capturing; using device sync timing.",
                    lock.last_user_stream
                );
                check_rts!(rtDeviceSynchronize());
                if lock.start_time_us != 0 {
                    duration_us = get_time_us().saturating_sub(lock.start_time_us);
                }
                used_wall_clock = true;
            }
        }

        // If there is no user stream, fall back to device sync + wall-clock.
        if !used_wall_clock && lock.last_user_stream == 0 {
            debug!("[Limiter] no last user stream; using device sync timing.");
            check_rts!(rtDeviceSynchronize());
            if lock.start_time_us != 0 {
                duration_us = get_time_us().saturating_sub(lock.start_time_us);
            }
            used_wall_clock = true;
        }

        if !used_wall_clock {
            check_rts!(rtEventRecord(lock.tracking_event, lock.last_user_stream));
            check_rts!(rtStreamWaitEvent(lock.internal_stream, lock.tracking_event));
            check_rts!(rtEventRecord(lock.end_event, lock.internal_stream));
            check_rts!(rtStreamSynchronize(lock.internal_stream));
            let mut ms: f32 = 0.0;
            check_rts!(rtEventElapsedTime(&mut ms, lock.start_event, lock.end_event));
            duration_us = (ms * 1000.0) as u64;
        }

        // Report to Shared Memory
        let slot = &self.inner.shmem.reports[self.inner.my_slot_idx];
        slot.duration_us.store(duration_us, Ordering::Relaxed);
        slot.batch_id.store(lock.current_batch_id, Ordering::Release);

        self.inner.shmem.reported_count.fetch_add(1, Ordering::Release);

        lock.batch_active = false;
        lock.start_time_us = 0;
    }
}

// Limit HBM
impl SchedulerClient {
    /// Publish an owned reservation, then conservatively check all process slots.
    pub fn check_memory_quota(&self, size: u64) -> u64 {
        if self.inner.is_stub { return RT_ERROR_NONE; }
        let shmem = self.inner.shmem;
        let limit = shmem.memory_limit.load(Ordering::Acquire);
        if limit == 0 { return RT_ERROR_NONE; }
        let used = &shmem.procs[self.inner.my_proc_idx].hbm_used[self.current_device()];
        // Publish ownership BEFORE admission and BEFORE the native allocator.
        // An exited process's pending allocation is now reclaimable too.
        if used.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
            |current| current.checked_add(size)).is_err() {
            return RT_ERROR_MEMORY_ALLOCATION;
        }
        let fits = |total: Option<u64>| total.is_some_and(|total| total <= limit);
        if !fits(ledger_total(shmem)) {
            reclaim_dead_process_slots(shmem);
            if !fits(ledger_total(shmem)) {
                atomic_saturating_sub(used, size);
                publish_usage(shmem);
                return RT_ERROR_MEMORY_ALLOCATION;
            }
        }
        publish_usage(shmem);
        RT_ERROR_NONE
    }

    /// Record a successful handle, or roll back its previously owned reservation.
    pub fn post_alloc_hbm(&self, p: u64, size: u64, rts_return: u64) {
        if self.inner.is_stub { return; }
        if rts_return == RT_ERROR_NONE {
            self.inner
                .hbm_handle_map
                .lock()
                .unwrap()
                .insert(p, (self.current_device(), size));
        } else {
            // Roll back the reservation already owned by this process slot.
            let used = &self.inner.shmem.procs[self.inner.my_proc_idx].hbm_used[self.current_device()];
            atomic_saturating_sub(used, size);
            publish_usage(self.inner.shmem);
        }
    }

    /// Return quota only after native free succeeds; duplicate frees return no credit.
    pub fn post_free_hbm(&self, handle: u64, ret: u64) {
        if self.inner.is_stub { return; }
        if ret != RT_ERROR_NONE {
            warn!(
                "[Limiter] Device free failed (code: {}), handle: 0x{:x}; quota retained.",
                ret,
                handle
            );
            return;
        }
        let allocation = self.inner.hbm_handle_map.lock().unwrap().remove(&handle);
        if let Some((device, size)) = allocation {
            let used = &self.inner.shmem.procs[self.inner.my_proc_idx].hbm_used[device];
            atomic_saturating_sub(used, size);
            publish_usage(self.inner.shmem);
        } else {
            warn!("[Limiter] Free Success but Handle 0x{:x} was untracked!", handle);
        }
    }

    fn current_device(&self) -> usize {
        self.inner.device_id
    }

    pub fn recalculate_usage(&self) -> u64 {
        self.recalculate_usage_for_device(self.current_device())
    }

    /// Read per-device usage, including pending reservations; never overwrite it.
    pub fn recalculate_usage_for_device(&self, device: usize) -> u64 {
        if device >= shmem::NPU_DEVICE_MAX { return 0; }
        reclaim_dead_process_slots(self.inner.shmem);
        self.inner.shmem.procs.iter().fold(0u64, |sum, slot| {
            sum.saturating_add(slot.hbm_used[device].load(Ordering::SeqCst))
        })
    }

    /// Report logical container quota, pending reservations and the 128 MiB reserve.
    pub fn get_hbm_info(&self, free: *mut usize, total: *mut usize) {
        let shmem = self.inner.shmem;

        let quota = shmem.memory_limit.load(Ordering::Relaxed) as usize;
        let used = ledger_total(shmem).unwrap_or(u64::MAX) as usize;
        let overhead_bytes = (VIRTUAL_OVERHEAD_MB * 1024 * 1024) as usize;

        let logical_free = if quota > used {
            quota - used
        } else {
            0
        };

        let reported_free = if logical_free > overhead_bytes {
            logical_free - overhead_bytes
        } else {
            0
        };

        unsafe {
            *total = quota;
            *free = reported_free;
        }
    }

    pub fn is_hbm_limited(&self) -> bool {
        self.inner.shmem.memory_limit.load(Ordering::Relaxed) > 0
    }

    pub fn get_hbm_quota(&self) -> u64 {
        self.inner.shmem.memory_limit.load(Ordering::Relaxed)
    }

    /// Create a no-op stub for environments without shared memory (e.g. TBE subprocesses).
    /// All HBM limit checks and compute tracking will be disabled.
    pub fn stub() -> Self {
        // Leak a zeroed shmem on the heap so we have a static reference.
        let shmem: &'static LocalContainerShmem = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        Self {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                my_slot_idx: 0,
                device_id: 0,
                my_proc_idx: 0,
                lock: Mutex::new(InnerLock {
                    internal_stream: 0,
                    start_event: 0,
                    end_event: 0,
                    tracking_event: 0,
                    batch_active: false,
                    current_batch_id: 0,
                    last_user_stream: 0,
                    start_time_us: 0,
                }),
                hbm_handle_map: Mutex::new(HashMap::new()),
                is_stub: true,
            }),
        }
    }

    /// Returns the raw device utilization (no per-pod split).
    pub fn get_compute_share(&self, device_util: u32) -> u32 {
        device_util
    }
}

pub(crate) fn proc_alive(pid: i32) -> bool {
    if pid == std::process::id() as i32 { return true; }
    match fs::read_to_string(format!("/proc/{}/stat", pid)) {
        Ok(stat) => {
            // Extract the process state character (3rd field, after pid and comm)
            // Format: pid (comm) state ...
            if let Some(pos) = stat.rfind(')') {
                let rest = &stat[pos + 2..]; // skip ") "
                if let Some(ch) = rest.chars().next() {
                    return ch != 'Z' && ch != 'X' && ch != 'x';
                }
            }
            true // If we can't parse, assume alive
        }
        Err(error) => error.kind() != std::io::ErrorKind::NotFound, // fail closed on unreadable procfs
    }
}

// Helper
fn get_time_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::{InnerLock, SchedulerClient, SchedulerClientInner, ledger_total, retire_slot};
    use crate::shmem::LocalContainerShmem;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    fn test_client(shmem: &'static LocalContainerShmem) -> SchedulerClient {
        SchedulerClient {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                my_slot_idx: 0,
                my_proc_idx: 0,
                device_id: 0,
                lock: std::sync::Mutex::new(InnerLock {
                    internal_stream: 0,
                    start_event: 0,
                    end_event: 0,
                    tracking_event: 0,
                    batch_active: false,
                    current_batch_id: 0,
                    last_user_stream: 0,
                    start_time_us: 0,
                }),
                hbm_handle_map: Mutex::new(HashMap::new()),
                is_stub: false,
            }),
        }
    }

    #[test]
    fn quota_reservation_rollback_and_free_preserve_ledger() {
        let shmem: &'static LocalContainerShmem = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        shmem.memory_limit.store(10, Ordering::Release);
        shmem.procs[0].pid.store(std::process::id() as i32, Ordering::Release);
        shmem.procs[0].is_active.store(1, Ordering::Release);
        let client = test_client(shmem);

        assert_eq!(client.check_memory_quota(8), 0);
        assert_eq!(ledger_total(shmem), Some(8));
        assert_ne!(client.check_memory_quota(3), 0);
        assert_eq!(ledger_total(shmem), Some(8));

        client.post_alloc_hbm(0x11, 8, 1);
        assert_eq!(ledger_total(shmem), Some(0));
        assert_eq!(client.check_memory_quota(6), 0);
        client.post_alloc_hbm(0x22, 6, 0);
        client.post_free_hbm(0x22, 0);
        assert_eq!(ledger_total(shmem), Some(0));
    }

    #[test]
    fn reclaim_dead_processes_preserves_in_flight_reservations() {
        let shmem: &'static LocalContainerShmem = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        let dead_slot = &shmem.procs[0];
        dead_slot.pid.store(4242, Ordering::Release);
        dead_slot.hbm_used[0].store(4, Ordering::Release);
        dead_slot.hbm_used[1].store(2, Ordering::Release);
        dead_slot.is_active.store(1, Ordering::Release);
        shmem.memory_used.store(12, Ordering::Release);

        let reserved = Arc::new(Barrier::new(2));
        let finished = Arc::new(Barrier::new(2));
        let reserve_barrier = Arc::clone(&reserved);
        let finish_barrier = Arc::clone(&finished);
        let reserve_thread = thread::spawn(move || {
            shmem.procs[1].hbm_used[0].store(3, Ordering::SeqCst);
            reserve_barrier.wait();
            finish_barrier.wait();
        });

        // Claim and clear the dead slot while another process publishes a
        // reservation. The live reservation must survive the cleanup.
        reserved.wait();
        assert!(retire_slot(dead_slot, 4242));
        finished.wait();
        reserve_thread.join().unwrap();

        assert_eq!(ledger_total(shmem), Some(3));
        assert_eq!(dead_slot.pid.load(Ordering::Acquire), 0);
        assert_eq!(dead_slot.hbm_used[0].load(Ordering::Acquire), 0);
        assert_eq!(dead_slot.hbm_used[1].load(Ordering::Acquire), 0);
        assert_eq!(dead_slot.is_active.load(Ordering::Acquire), 0);
    }
    /// Dropping one independently constructed client must retain its sibling's quota.
    #[test]
    fn dropping_client_preserves_shared_process_slot() {
        let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
        let pid = std::process::id() as i32;
        shmem.procs[0].pid.store(pid, Ordering::SeqCst);
        shmem.procs[0].is_active.store(1, Ordering::Release);
        shmem.memory_limit.store(10, Ordering::Release);
        let first = test_client(shmem);
        let second = test_client(shmem);
        assert_eq!(first.check_memory_quota(8), 0);
        drop(first);
        assert_eq!(shmem.procs[0].pid.load(Ordering::Acquire), pid);
        assert_eq!(ledger_total(shmem), Some(8));
        assert_ne!(second.check_memory_quota(3), 0);
        drop(second);
        assert_eq!(ledger_total(shmem), Some(8));
    }

    /// Queries include a pending allocation and cannot erase it or another device.
    #[test]
    fn query_preserves_pending_and_multidevice_usage() {
        let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
        let mib = 1024 * 1024;
        shmem.memory_limit.store(1024 * mib, Ordering::Release);
        shmem.procs[0].pid.store(std::process::id() as i32, Ordering::SeqCst);
        shmem.procs[0].hbm_used[1].store(256 * mib, Ordering::SeqCst);
        let client = test_client(shmem);
        assert_eq!(client.check_memory_quota(256 * mib), 0);
        let (mut free, mut total) = (0usize, 0usize);
        client.get_hbm_info(&mut free, &mut total);
        assert_eq!(total as u64, 1024 * mib);
        assert_eq!(free as u64, (1024 - 512 - 128) * mib);
        assert_eq!(ledger_total(shmem), Some(512 * mib));
        client.post_alloc_hbm(0, 256 * mib, 1);
        assert_eq!(ledger_total(shmem), Some(256 * mib));
    }

    /// Both per-slot overflow and cross-slot sum overflow reject without credit.
    #[test]
    fn overflowing_reservations_do_not_wrap() {
        let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
        shmem.memory_limit.store(u64::MAX, Ordering::Release);
        shmem.procs[0].pid.store(std::process::id() as i32, Ordering::SeqCst);
        shmem.procs[0].hbm_used[0].store(u64::MAX, Ordering::SeqCst);
        let client = test_client(shmem);
        assert_ne!(client.check_memory_quota(1), 0);
        assert_eq!(ledger_total(shmem), Some(u64::MAX));
        shmem.procs[0].hbm_used[0].store(0, Ordering::SeqCst);
        shmem.procs[0].hbm_used[1].store(u64::MAX, Ordering::SeqCst);
        assert_ne!(client.check_memory_quota(1), 0);
        assert_eq!(ledger_total(shmem), Some(u64::MAX));
    }

    /// Native free failure keeps its handle and reservation until a successful retry.
    #[test]
    fn failed_free_keeps_reservation_and_duplicate_free_is_idempotent() {
        let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
        shmem.memory_limit.store(10, Ordering::Release);
        let client = test_client(shmem);
        assert_eq!(client.check_memory_quota(6), 0);
        client.post_alloc_hbm(1, 6, 0);
        client.post_free_hbm(1, 1);
        assert_eq!(ledger_total(shmem), Some(6));
        client.post_free_hbm(1, 0);
        client.post_free_hbm(1, 0);
        assert_eq!(ledger_total(shmem), Some(0));
    }

    /// A dead process's slot owns reservations even before post_alloc_hbm runs.
    #[test]
    fn process_death_before_native_completion_is_reclaimable() {
        let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
        shmem.memory_limit.store(10, Ordering::Release);
        shmem.procs[0].pid.store(4242, Ordering::SeqCst);
        let client = test_client(shmem);
        assert_eq!(client.check_memory_quota(7), 0);
        assert_eq!(ledger_total(shmem), Some(7));
        assert!(retire_slot(&shmem.procs[0], 4242));
        assert_eq!(ledger_total(shmem), Some(0));
        assert_eq!(SchedulerClient::register_proc_slot(shmem, std::process::id() as i32), 0);
    }

}
