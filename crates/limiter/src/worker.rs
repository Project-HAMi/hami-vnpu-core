use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, info, warn};

use crate::check_rts;
use crate::config::{ManagerConfig, SchedPolicy, VIRTUAL_OVERHEAD_MB};
use crate::externed_api::*;
use crate::shmem::{
    self, LocalContainerShmem, MAX_WORKERS, STATE_IDLE, STATE_MEASURING, STATE_RUNNING, futex,
};

/// Max time a worker waits for the manager to create + initialize shmem before
/// giving up. On timeout SchedulerClient::new panics and npu_limiter() falls back
/// to a no-op stub, so a worker never hangs forever when no manager is elected.
const SHMEM_WAIT_TIMEOUT_SECS: u64 = 10;

/// Atomically subtract `v` from `a`, saturating at 0 (never wraps to ~2^64 on an
/// accounting slip). Returns true if it hit the floor, so the caller can heal.
fn atomic_saturating_sub(a: &std::sync::atomic::AtomicU64, v: u64) -> bool {
    a.fetch_update(Ordering::SeqCst, Ordering::Acquire, |cur| {
        Some(cur.saturating_sub(v))
    })
    .map(|prev| prev < v)
    .unwrap_or(false)
}

/// Authoritative per-card ledger: counter zero in each process slot includes
/// pending reservations and allocations. Other entries remain for ABI compatibility.
/// SeqCst publication before admission ensures the last concurrent reservation
/// observes all still-live earlier reservations. Contention may reject both
/// callers conservatively, but cannot admit more than the limit.
fn ledger_total(shmem: &LocalContainerShmem) -> Option<u64> {
    shmem
        .procs
        .iter()
        .map(|slot| &slot.hbm_used[0])
        .try_fold(0u64, |sum, used| {
            sum.checked_add(used.load(Ordering::SeqCst))
        })
}

/// Publish a sampled compatibility metric; admission uses the per-slot ledger.
fn publish_usage(shmem: &LocalContainerShmem) {
    // Compatibility telemetry only. Never use this sampled aggregate for admission.
    shmem
        .memory_used
        .store(ledger_total(shmem).unwrap_or(u64::MAX), Ordering::Release);
}

/// Negative PID holds a slot while it is being retired or initialized. If the
/// owner dies mid-cleanup, a subsequent reaper can claim and finish the cleanup.
fn retire_slot(slot: &shmem::ProcessSlot, observed_pid: i32) -> bool {
    let owner = -(std::process::id() as i32);
    if slot
        .pid
        .compare_exchange(observed_pid, owner, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
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
/// The sampled aggregate is telemetry; admission reads the owned reservations.
pub(crate) fn reclaim_dead_process_slots(shmem: &LocalContainerShmem) {
    for slot in &shmem.procs {
        let pid = slot.pid.load(Ordering::SeqCst);
        if pid != 0 && !proc_alive(pid.saturating_abs()) {
            retire_slot(slot, pid);
        }
    }
    publish_usage(shmem);
}

#[derive(Debug, Default)]
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

/// Runtime virtual addresses and opaque physical-memory handles are separate namespaces.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AllocationKind {
    Virtual,
    Physical,
}

/// Private backing identity. Retained physical handles share one byte charge.
#[derive(Debug)]
struct HbmAllocationRecord {
    size: u64,
    state: Mutex<HbmAllocationState>,
}

#[derive(Debug)]
struct HbmAllocationState {
    live_refs: u64,
    pending_free: u64,
    pending_retain: u64,
    refunded: bool,
}

/// A claim on one native release, tied to the pre-call backing identity.
#[derive(Debug)]
#[must_use = "complete the ticket after the native free returns"]
pub struct HbmFreeTicket {
    client: SchedulerClient,
    handle: u64,
    kind: AllocationKind,
    record: Arc<HbmAllocationRecord>,
}

impl HbmFreeTicket {
    pub fn finish(self, result: u64) {
        let mut map = self.client.inner.hbm_handle_map.lock().unwrap();
        let mut state = self.record.state.lock().unwrap();
        state.pending_free -= 1;
        if result == RT_ERROR_NONE {
            state.live_refs -= 1;
        } else {
            match map.entry((self.kind, self.handle)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(self.record.clone());
                }
                std::collections::hash_map::Entry::Occupied(entry)
                    if !Arc::ptr_eq(entry.get(), &self.record) =>
                {
                    // A valid native failure cannot have freed this backing
                    // for reuse. Never overwrite a newer allocation's record.
                    warn!(
                        "[Limiter] failed free of {:?} handle 0x{:x} conflicts with a newer allocation; original {} bytes remain charged",
                        self.kind, self.handle, self.record.size
                    );
                }
                _ => {}
            }
            warn!(
                "[Limiter] native free FAILED (code: {}), handle: 0x{:x}. Quota not released.",
                result, self.handle
            );
        }
        let refund =
            retire_hbm_record(&mut map, (self.kind, self.handle), &self.record, &mut state);
        drop(state);
        drop(map);
        if refund {
            self.client.refund_hbm(self.record.size);
        }
    }
}

/// A retain returns its backing handle only after native code completes. A
/// process-private scope pins both existing and newly published physical
/// records until the returned handle can be matched to its current identity.
#[derive(Debug)]
#[must_use = "complete the ticket after the native retain returns"]
pub struct HbmRetainTicket {
    pid: u32,
    id: u64,
}

#[derive(Debug)]
struct ScopedHbmRecord {
    client: SchedulerClient,
    handle: u64,
    record: Arc<HbmAllocationRecord>,
}

struct HbmRetainScopes {
    pid: u32,
    next_id: u64,
    active: HashMap<u64, Vec<ScopedHbmRecord>>,
}

impl HbmRetainScopes {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            next_id: 0,
            active: HashMap::new(),
        }
    }
}

// Check PID before touching a mutex inherited across fork. Published states
// are retained: dropping inherited clients could clear the parent's SHM slot.
struct ProcessRetainScopes {
    pid: u32,
    scopes: Mutex<HbmRetainScopes>,
}

static HBM_RETAIN_SCOPES: std::sync::atomic::AtomicPtr<ProcessRetainScopes> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

fn retain_scopes() -> std::sync::MutexGuard<'static, HbmRetainScopes> {
    let pid = std::process::id();
    loop {
        let old = HBM_RETAIN_SCOPES.load(Ordering::Acquire);
        if !old.is_null() && unsafe { (*old).pid } == pid {
            return unsafe { &*old }.scopes.lock().unwrap();
        }
        let fresh = Box::into_raw(Box::new(ProcessRetainScopes {
            pid,
            scopes: Mutex::new(HbmRetainScopes::new(pid)),
        }));
        if HBM_RETAIN_SCOPES
            .compare_exchange(old, fresh, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return unsafe { &*fresh }.scopes.lock().unwrap();
        }
        // A losing state was never published and contains no client records.
        unsafe {
            drop(Box::from_raw(fresh));
        }
    }
}

/// Do not create clients here. Allocations published by any client while this
/// scope is live also join it in post_alloc_hbm_kind below.
pub fn prepare_hbm_retain(clients: impl FnOnce() -> Vec<SchedulerClient>) -> HbmRetainTicket {
    let mut scopes = retain_scopes();
    let mut records = Vec::new();
    // Collect registered clients only after acquiring the scope lock. A new
    // client's allocation either appears here or joins the published scope.
    for client in clients() {
        let map = client.inner.hbm_handle_map.lock().unwrap();
        for (&(kind, handle), record) in map.iter() {
            if kind != AllocationKind::Physical {
                continue;
            }
            let mut state = record.state.lock().unwrap();
            if !state.refunded {
                state.pending_retain += 1;
                records.push(ScopedHbmRecord {
                    client: client.clone(),
                    handle,
                    record: record.clone(),
                });
            }
        }
    }
    let id = scopes.next_id;
    scopes.next_id = id.checked_add(1).expect("retain scope overflow");
    scopes.active.insert(id, records);
    HbmRetainTicket {
        pid: scopes.pid,
        id,
    }
}

impl HbmRetainTicket {
    pub fn finish(self, retained_handle: Option<u64>) {
        let mut scopes = retain_scopes();
        if scopes.pid != self.pid {
            return;
        }
        let Some(records) = scopes.active.remove(&self.id) else {
            return;
        };
        let mut refunds = Vec::new();
        for ScopedHbmRecord {
            client,
            handle,
            record,
        } in records
        {
            let mut map = client.inner.hbm_handle_map.lock().unwrap();
            let mut state = record.state.lock().unwrap();
            if retained_handle == Some(handle)
                && map
                    .get(&(AllocationKind::Physical, handle))
                    .is_some_and(|current| Arc::ptr_eq(current, &record))
            {
                // Only this process's charged current generation is eligible.
                state.live_refs = state
                    .live_refs
                    .checked_add(1)
                    .expect("physical reference overflow");
            }
            state.pending_retain -= 1;
            if retire_hbm_record(
                &mut map,
                (AllocationKind::Physical, handle),
                &record,
                &mut state,
            ) {
                refunds.push((client.clone(), record.size));
            }
        }
        drop(scopes);
        for (client, size) in refunds {
            client.refund_hbm(size);
        }
    }
}

fn retire_hbm_record(
    map: &mut HashMap<(AllocationKind, u64), Arc<HbmAllocationRecord>>,
    key: (AllocationKind, u64),
    record: &Arc<HbmAllocationRecord>,
    state: &mut HbmAllocationState,
) -> bool {
    if state.live_refs != 0
        || state.pending_free != 0
        || state.pending_retain != 0
        || state.refunded
    {
        return false;
    }
    state.refunded = true;
    if map
        .get(&key)
        .is_some_and(|current| Arc::ptr_eq(current, record))
    {
        map.remove(&key);
    }
    true
}

#[derive(Clone, Debug)]
pub struct SchedulerClient {
    inner: Arc<SchedulerClientInner>,
}

#[derive(Debug)]
struct ContextTiming {
    my_slot_idx: usize,
    lock: RwLock<InnerLock>,
    retired: AtomicBool,
}

struct SchedulerClientInner {
    shmem: &'static LocalContainerShmem,
    my_proc_idx: usize,
    physical_id: u32,
    contexts: Mutex<HashMap<u64, Arc<ContextTiming>>>,
    hbm_handle_map: Mutex<HashMap<(AllocationKind, u64), Arc<HbmAllocationRecord>>>,
    /// True for the no-op stub (no shmem / no manager). All limit checks are
    /// short-circuited so the worker never blocks on the state futex.
    is_stub: bool,
    sched_policy: SchedPolicy,
}

impl fmt::Debug for SchedulerClientInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerClientInner")
            .field("physical_id", &self.physical_id)
            .field("my_proc_idx", &self.my_proc_idx)
            .field("shmem", &self.shmem)
            .finish()
    }
}

impl Default for SchedulerClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerClient {
    /// Resolve the current logical device without treating its index as a physical ID.
    pub fn new() -> Self {
        let mut logical = -1;
        assert_eq!(
            unsafe { rtGetDevice(&mut logical) },
            0,
            "rtGetDevice failed"
        );
        assert!(logical >= 0, "negative current device ID");
        let mut physical = 0;
        assert_eq!(
            unsafe { rtGetDevicePhyIdByIndex(logical as u32, &mut physical) },
            0,
            "rtGetDevicePhyIdByIndex failed"
        );
        Self::for_device(physical)
    }

    /// One client owns the memory and scheduling domain for one physical card.
    pub fn for_device(physical_id: u32) -> Self {
        let my_pid = std::process::id() as i32;
        let config = ManagerConfig::from_env()
            .unwrap_or_else(|error| panic!("{error}"))
            .for_device(physical_id);
        let sched_policy = config.sched_policy;
        let shmem_path = config.local_shm_path;
        info!("[Worker PID:{my_pid}] initialize physical device {physical_id}");
        let deadline = Instant::now() + Duration::from_secs(SHMEM_WAIT_TIMEOUT_SECS);
        let shmem = loop {
            if let Some(s) = shmem::setup::try_open_shmem::<LocalContainerShmem>(&shmem_path)
                && s.initialized.load(Ordering::Acquire) == 1
            {
                break s;
            }
            if Instant::now() >= deadline {
                panic!(
                    "[Worker PID:{my_pid}] timed out after {SHMEM_WAIT_TIMEOUT_SECS}s waiting for device {physical_id} manager"
                );
            }
            thread::sleep(Duration::from_millis(200));
        };
        let my_proc_idx = Self::register_proc_slot(shmem, my_pid);
        Self {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                my_proc_idx,
                physical_id,
                contexts: Mutex::new(HashMap::new()),
                hbm_handle_map: Mutex::new(HashMap::new()),
                is_stub: false,
                sched_policy,
            }),
        }
    }

    fn current_context() -> u64 {
        let mut context = 0;
        assert_eq!(
            unsafe { rtCtxGetCurrent(&mut context) },
            0,
            "rtCtxGetCurrent failed"
        );
        context
    }

    fn context_timing(&self, context: u64) -> Arc<ContextTiming> {
        let mut contexts = self.inner.contexts.lock().unwrap();
        contexts
            .entry(context)
            .or_insert_with(|| {
                let my_slot_idx =
                    Self::register_worker_slot(self.inner.shmem, std::process::id() as i32);
                Arc::new(ContextTiming {
                    my_slot_idx,
                    lock: RwLock::new(Self::create_inner_lock()),
                    retired: AtomicBool::new(false),
                })
            })
            .clone()
    }

    /// Forget timing resources after CANN has successfully destroyed their context.
    /// Allocations belong to this card's client and survive context retirement.
    pub fn retire_context(&self, context: u64) {
        let timing = self.inner.contexts.lock().unwrap().remove(&context);
        if let Some(timing) = timing {
            let mut lock = timing.lock.write().unwrap();
            timing.retired.store(true, Ordering::Release);
            let shmem = self.inner.shmem;
            if lock.batch_active && lock.current_batch_id == shmem.batch_id.load(Ordering::Acquire)
            {
                // The destroyed context can no longer emit its pending GPU timing report.
                let _ = shmem.active_workers.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |active| Some(active.saturating_sub(1)),
                );
            }
            lock.batch_active = false;
            let report = &shmem.reports[timing.my_slot_idx];
            let preserve_report = report.batch_id.load(Ordering::Acquire) != 0
                && report.batch_id.load(Ordering::Relaxed)
                    == shmem.batch_id.load(Ordering::Acquire)
                && shmem.state.load(Ordering::Acquire) != STATE_IDLE;
            if !preserve_report {
                report.batch_id.store(0, Ordering::Relaxed);
                report.cpu_start_us.store(0, Ordering::Relaxed);
                report.duration_us.store(0, Ordering::Relaxed);
            }
            report.pid.store(0, Ordering::Release);
            // Preserve a completed report until its manager has consumed this round.
            report
                .occupied
                .store(if preserve_report { 2 } else { 0 }, Ordering::Release);
        }
    }

    /// Used to return a successful cross-card free to the allocation's owning domain.
    pub fn owns_hbm_handle(&self, handle: u64) -> bool {
        self.owns_hbm_handle_kind(handle, AllocationKind::Virtual)
    }

    pub fn owns_hbm_handle_kind(&self, handle: u64, kind: AllocationKind) -> bool {
        self.inner
            .hbm_handle_map
            .lock()
            .unwrap()
            .get(&(kind, handle))
            .is_some_and(|record| {
                let state = record.state.lock().unwrap();
                state.live_refs > state.pending_free
            })
    }

    fn register_worker_slot(shmem: &'static LocalContainerShmem, pid: i32) -> usize {
        // Each context needs its own report, even when the PID is the same.
        // Claim a free slot or CAS-reclaim a dead process's slot.
        for (i, slot) in shmem.reports.iter().enumerate() {
            let slot_pid = slot.pid.load(Ordering::Relaxed);
            if slot.occupied.load(Ordering::Acquire) == 2
                && (slot.batch_id.load(Ordering::Acquire) != shmem.batch_id.load(Ordering::Acquire)
                    || shmem.state.load(Ordering::Acquire) == STATE_IDLE)
                && slot
                    .occupied
                    .compare_exchange(2, 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                slot.batch_id.store(0, Ordering::Relaxed);
                slot.duration_us.store(0, Ordering::Relaxed);
                slot.cpu_start_us.store(0, Ordering::Relaxed);
                slot.pid.store(pid, Ordering::Release);
                return i;
            }
            if slot_pid == 0 {
                if slot
                    .occupied
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    slot.pid.store(pid, Ordering::Relaxed);
                    return i;
                }
            } else if slot_pid != pid && !proc_alive(slot_pid) {
                // Try to atomically swap our PID in — only one thread wins
                if slot
                    .pid
                    .compare_exchange(slot_pid, pid, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    slot.batch_id.store(0, Ordering::Relaxed);
                    slot.cpu_start_us.store(0, Ordering::Relaxed);
                    slot.duration_us.store(0, Ordering::Relaxed);
                    return i;
                }
                // Lost the race — slot_pid already changed, continue searching
            }
        }

        panic!(
            "[Scheduler] Registry full ({} workers). Increase MAX_WORKERS.",
            MAX_WORKERS
        );
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
            if slot
                .pid
                .compare_exchange(0, -pid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
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

        assert_eq!(
            check_rts!(unsafe { rtStreamCreate(&mut internal_stream, 0) }),
            0,
            "cannot create context timing stream"
        );
        for event in [&mut start_event, &mut end_event, &mut tracking_event] {
            assert_eq!(
                check_rts!(unsafe { rtEventCreate(event) }),
                0,
                "cannot create context timing event"
            );
        }

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
    /// Reserve exactly one ticket, without a transient zero-to-MAX underflow.
    /// The shared balance can change between the initial load and decrement.
    /// Never expose a wrapped balance that another process could consume.
    #[inline]
    fn try_consume_one_token(&self) -> bool {
        let remaining = &self.inner.shmem.tokens_remaining;
        let current = remaining.load(Ordering::Acquire);
        // On contention, return to the caller to recheck state/batch rather
        // than retrying against a potentially replenished round's balance.
        current != 0
            && remaining
                .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    /// Same stream, already active batch: reserve one ticket using this context's
    /// timing state. A writer owns its batch transitions, events, and report.
    #[inline]
    #[cfg(test)]
    #[allow(dead_code)]
    fn try_same_stream_token(&self, user_stream: u64) -> bool {
        let timing = self.context_timing(Self::current_context());
        self.try_context_stream_token(&timing, user_stream)
    }

    fn try_context_stream_token(&self, timing: &ContextTiming, user_stream: u64) -> bool {
        if timing.retired.load(Ordering::Acquire) {
            return false;
        }
        if let Ok(lock) = timing.lock.try_read() {
            let state = self.inner.shmem.state.load(Ordering::Acquire);
            let batch = self.inner.shmem.batch_id.load(Ordering::Relaxed);
            if state == STATE_RUNNING
                && lock.batch_active
                && lock.current_batch_id == batch
                && lock.last_user_stream == user_stream
            {
                return self.try_consume_one_token();
            }
        }
        false
    }

    /// The Main Entry Point
    pub fn wait_for_token(&self, user_stream: u64) {
        if self.inner.is_stub || !self.inner.sched_policy.is_time_sliced() {
            return;
        }

        let timing = self.context_timing(Self::current_context());
        if self.try_context_stream_token(&timing, user_stream) {
            return;
        }

        let wait_start = Instant::now();
        // No event or active-worker accounting for successful IDLE overdraft admissions.
        if self.try_idle_overdraft(false) {
            return;
        }

        // The shared guard is dropped before acquiring the writer lock.
        // Batch initialization, stream changes and measurement remain exclusive.
        let mut lock = timing.lock.write().unwrap();

        lock.last_user_stream = user_stream;

        loop {
            assert!(
                !timing.retired.load(Ordering::Acquire),
                "submission uses a destroyed context"
            );
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
                        lock = timing.lock.write().unwrap(); // RELOCK
                        continue;
                    }

                    if self.try_consume_one_token() {
                        // SUCCESS!

                        // First Token of Batch
                        if !lock.batch_active {
                            debug!(
                                "[Worker PID:{} Slot:{}] get Batch {} first Token!, start record time...",
                                std::process::id(),
                                timing.my_slot_idx,
                                global_batch
                            );

                            lock.current_batch_id = global_batch;
                            lock.batch_active = true;

                            // Notify Manager
                            self.inner
                                .shmem
                                .active_workers
                                .fetch_add(1, Ordering::Release);

                            // CPU Start Time
                            let now_us = get_time_us();
                            self.inner.shmem.reports[timing.my_slot_idx]
                                .cpu_start_us
                                .store(now_us, Ordering::Relaxed);
                            lock.start_time_us = now_us;

                            // GPU Start Event (Internal Stream)
                            check_rts!(unsafe {
                                rtEventRecord(lock.start_event, lock.internal_stream)
                            });
                        }

                        return; // -> Kernel Launch
                    }
                }

                // ---------------------------------------------------------
                // MEASURING: Report Time
                // ---------------------------------------------------------
                STATE_MEASURING => {
                    if lock.batch_active && global_batch == lock.current_batch_id {
                        debug!(
                            "[Worker PID:{} Slot:{}] start measuring Batch {} ...",
                            std::process::id(),
                            timing.my_slot_idx,
                            global_batch
                        );
                        self.measure_and_report_batch(&timing, &mut lock);
                    }

                    // Wait for state change
                    drop(lock); // UNLOCK
                    if self.inner.shmem.state.load(Ordering::Acquire) == STATE_MEASURING {
                        futex::wait(&self.inner.shmem.state, STATE_MEASURING);
                    }
                    lock = timing.lock.write().unwrap(); // RELOCK
                }

                // ---------------------------------------------------------
                // IDLE
                // ---------------------------------------------------------
                _ => {
                    let shared = self.inner.shmem;
                    let limit = shared.overdraft_limit.load(Ordering::Acquire);
                    let timed_out = wait_start.elapsed().as_micros()
                        >= shared.overdraft_timeout_us.load(Ordering::Relaxed) as u128;
                    if limit > 0 && self.try_idle_overdraft(timed_out) {
                        return;
                    }
                    drop(lock); // UNLOCK
                    if shared.state.load(Ordering::Acquire) == STATE_IDLE {
                        if limit > 0 {
                            futex::wait_timeout(
                                &shared.state,
                                STATE_IDLE,
                                shared
                                    .overdraft_poll_interval_us
                                    .load(Ordering::Relaxed)
                                    .max(1),
                            );
                        } else {
                            futex::wait(&shared.state, STATE_IDLE);
                        }
                    }
                    lock = timing.lock.write().unwrap(); // RELOCK
                    // Always re-enter the state machine after a wake/timeout.
                    // A successful poll alone is never permission to launch.
                }
            }
        }
    }

    fn try_idle_overdraft(&self, timed_out: bool) -> bool {
        let shared = self.inner.shmem;
        shared.overdraft_pool.try_acquire_idle(
            &shared.state,
            &shared.batch_id,
            STATE_IDLE,
            &shared.overdraft_limit,
            timed_out,
        )
    }

    fn measure_and_report_batch(&self, timing: &ContextTiming, lock: &mut InnerLock) {
        let mut duration_us: u64 = 0;
        let mut used_wall_clock = false;

        if lock.last_user_stream != 0 {
            let mut status: u32 = 0;
            let mut model: u64 = 0;
            let _ = check_rts!(unsafe {
                rtStreamGetCaptureInfo(lock.last_user_stream, &mut status, &mut model)
            });
            if status != 0 {
                debug!(
                    "[Limiter] stream 0x{:x} is capturing; using device sync timing.",
                    lock.last_user_stream
                );
                check_rts!(unsafe { rtDeviceSynchronize() });
                if lock.start_time_us != 0 {
                    duration_us = get_time_us().saturating_sub(lock.start_time_us);
                }
                used_wall_clock = true;
            }
        }

        // If there is no user stream, fall back to device sync + wall-clock.
        if !used_wall_clock && lock.last_user_stream == 0 {
            debug!("[Limiter] no last user stream; using device sync timing.");
            check_rts!(unsafe { rtDeviceSynchronize() });
            if lock.start_time_us != 0 {
                duration_us = get_time_us().saturating_sub(lock.start_time_us);
            }
            used_wall_clock = true;
        }

        if !used_wall_clock {
            check_rts!(unsafe { rtEventRecord(lock.tracking_event, lock.last_user_stream) });
            check_rts!(unsafe { rtStreamWaitEvent(lock.internal_stream, lock.tracking_event) });
            check_rts!(unsafe { rtEventRecord(lock.end_event, lock.internal_stream) });
            check_rts!(unsafe { rtStreamSynchronize(lock.internal_stream) });
            let mut ms: f32 = 0.0;
            check_rts!(unsafe { rtEventElapsedTime(&mut ms, lock.start_event, lock.end_event) });
            duration_us = (ms * 1000.0) as u64;
        }

        // Report to Shared Memory
        let slot = &self.inner.shmem.reports[timing.my_slot_idx];
        slot.duration_us.store(duration_us, Ordering::Relaxed);
        slot.batch_id
            .store(lock.current_batch_id, Ordering::Release);

        self.inner
            .shmem
            .reported_count
            .fetch_add(1, Ordering::Release);

        lock.batch_active = false;
        lock.start_time_us = 0;
    }
}

// Limit HBM
impl SchedulerClient {
    /// Publish an owned reservation, then conservatively check all process slots.
    pub fn check_memory_quota(&self, size: u64) -> u64 {
        if self.inner.is_stub {
            return RT_ERROR_NONE;
        }
        let shmem = self.inner.shmem;
        let limit = shmem.memory_limit.load(Ordering::Acquire);
        if limit == 0 {
            return RT_ERROR_NONE;
        }
        let used = &shmem.procs[self.inner.my_proc_idx].hbm_used[0];
        // Publish ownership BEFORE admission and BEFORE the native allocator.
        // An exited process's pending allocation is now reclaimable too.
        if used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(size)
            })
            .is_err()
        {
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

    pub fn post_alloc_hbm(&self, p: u64, size: u64, rts_return: u64) {
        self.post_alloc_hbm_kind(p, size, rts_return, AllocationKind::Virtual);
    }

    pub fn post_alloc_hbm_kind(&self, p: u64, size: u64, rts_return: u64, kind: AllocationKind) {
        if self.inner.is_stub {
            return;
        }
        if rts_return == RT_ERROR_NONE {
            // Global scope -> card map -> record is the only retain lock order.
            // No one holds any of these guards while executing native code.
            let mut scopes = (kind == AllocationKind::Physical).then(retain_scopes);
            let mut map = self.inner.hbm_handle_map.lock().unwrap();
            let pending_retain = scopes
                .as_ref()
                .map_or(0, |scopes| scopes.active.len() as u64);
            let record = Arc::new(HbmAllocationRecord {
                size,
                state: Mutex::new(HbmAllocationState {
                    live_refs: 1,
                    pending_free: 0,
                    pending_retain,
                    refunded: false,
                }),
            });
            if let Some(scopes) = scopes.as_mut() {
                for records in scopes.active.values_mut() {
                    records.push(ScopedHbmRecord {
                        client: self.clone(),
                        handle: p,
                        record: record.clone(),
                    });
                }
            }
            map.insert((kind, p), record);
            // Reservation already charged this card's process and quota ledgers.
        } else {
            self.refund_hbm(size);
        }
    }

    pub fn post_free_hbm(&self, handle: u64, ret: u64) {
        self.post_free_hbm_kind(handle, ret, AllocationKind::Virtual);
    }

    /// Claim the old record before calling native code. A concurrent allocation
    /// may reuse its numeric handle after the native free but before its caller
    /// returns; completing this ticket must not remove that newer record.
    pub fn claim_hbm_free(&self, handle: u64, kind: AllocationKind) -> Option<HbmFreeTicket> {
        let mut map = self.inner.hbm_handle_map.lock().unwrap();
        let record = map.get(&(kind, handle))?.clone();
        let mut state = record.state.lock().unwrap();
        if state.refunded || state.live_refs <= state.pending_free {
            return None;
        }
        state.pending_free += 1;
        if kind == AllocationKind::Virtual {
            // Ordinary addresses have a single reference. Detach now so an
            // alias cannot claim it and a reused address gets a new identity.
            map.remove(&(kind, handle));
        }
        drop(state);
        Some(HbmFreeTicket {
            client: self.clone(),
            handle,
            kind,
            record,
        })
    }

    fn refund_hbm(&self, size: u64) {
        let slot = &self.inner.shmem.procs[self.inner.my_proc_idx];
        atomic_saturating_sub(&slot.hbm_used[0], size);
        publish_usage(self.inner.shmem);
    }

    // Compatibility for callers that already completed a synchronous operation.
    // Native interceptors claim a ticket BEFORE entering CANN instead.
    pub fn post_free_hbm_kind(&self, handle: u64, ret: u64, kind: AllocationKind) {
        if ret == RT_ERROR_NONE {
            if let Some(ticket) = self.claim_hbm_free(handle, kind) {
                ticket.finish(ret);
            } else {
                warn!(
                    "[Limiter] Free Success but Handle 0x{:x} was untracked!",
                    handle
                );
            }
        } else {
            warn!(
                "[Limiter] native free FAILED (code: {}), handle: 0x{:x}. Quota not released.",
                ret, handle
            );
        }
    }

    /// Reap dead process reservations without overwriting live or pending allocations.
    pub fn recalculate_usage(&self) -> u64 {
        self.recalculate_usage_for_device(0)
    }

    pub fn recalculate_usage_for_device(&self, device: usize) -> u64 {
        // Each physical card owns a separate domain whose counter is index zero.
        if device != 0 {
            return 0;
        }
        reclaim_dead_process_slots(self.inner.shmem);
        ledger_total(self.inner.shmem).unwrap_or(u64::MAX)
    }

    /// Write the quota-adjusted free and total HBM sizes to the output pointers.
    ///
    /// # Safety
    /// `free` and `total` must be aligned, writable pointers to `usize` values
    /// for the duration of this call, with no concurrent access to those values.
    pub unsafe fn get_hbm_info(&self, free: *mut usize, total: *mut usize) {
        let shmem = self.inner.shmem;

        let quota = shmem.memory_limit.load(Ordering::Relaxed) as usize;
        let used = ledger_total(shmem).unwrap_or(u64::MAX) as usize;
        let overhead_bytes = VIRTUAL_OVERHEAD_MB * 1024 * 1024;

        let logical_free = quota.saturating_sub(used);

        let reported_free = logical_free.saturating_sub(overhead_bytes);

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
        let shmem: &'static LocalContainerShmem =
            Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        Self {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                physical_id: 0,
                my_proc_idx: 0,
                contexts: Mutex::new(HashMap::new()),
                hbm_handle_map: Mutex::new(HashMap::new()),
                is_stub: true,
                sched_policy: SchedPolicy::Dynamic,
            }),
        }
    }

    /// Returns the raw device utilization (no per-pod split).
    pub fn get_compute_share(&self, device_util: u32) -> u32 {
        device_util
    }
}

pub(crate) fn proc_alive(pid: i32) -> bool {
    if pid == std::process::id() as i32 {
        return true;
    }
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
        Err(error) => error.kind() != std::io::ErrorKind::NotFound, // retain unreadable owners
    }
}

// Helper
fn get_time_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::{SchedulerClient, SchedulerClientInner, ledger_total, retire_slot};
    use crate::externed_api::{RT_ERROR_MEMORY_ALLOCATION, RT_ERROR_NONE};
    use crate::{config::SchedPolicy, shmem::LocalContainerShmem};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    fn test_client(shmem: &'static LocalContainerShmem) -> SchedulerClient {
        test_client_on_device(shmem, 0)
    }

    fn test_client_on_device(
        shmem: &'static LocalContainerShmem,
        physical_id: u32,
    ) -> SchedulerClient {
        SchedulerClient {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                my_proc_idx: 0,
                physical_id,
                contexts: Mutex::new(HashMap::new()),
                hbm_handle_map: Mutex::new(HashMap::new()),
                is_stub: false,
                sched_policy: SchedPolicy::Dynamic,
            }),
        }
    }

    #[test]
    fn quota_reservation_rollback_and_free_preserve_ledger() {
        let shmem: &'static LocalContainerShmem =
            Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        shmem.memory_limit.store(10, Ordering::Release);
        shmem.procs[0]
            .pid
            .store(std::process::id() as i32, Ordering::Release);
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

    /// An orphaned aggregate update cannot strand quota without an owning slot.
    #[test]
    fn stale_aggregate_cannot_hold_or_erase_owned_reservations() {
        let shmem = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
        shmem.memory_limit.store(10, Ordering::Release);
        shmem.procs[0]
            .pid
            .store(std::process::id() as i32, Ordering::Release);
        // A legacy process may exit after updating this aggregate but before
        // publishing ownership. This value alone must not reserve nine bytes.
        shmem.memory_used.store(9, Ordering::Release);
        let client = test_client(shmem);
        assert_eq!(client.check_memory_quota(8), RT_ERROR_NONE);
        assert_eq!(ledger_total(shmem), Some(8));
        // Conversely, stale low telemetry cannot admit beyond the owned quota.
        shmem.memory_used.store(0, Ordering::Release);
        assert_eq!(client.check_memory_quota(3), RT_ERROR_MEMORY_ALLOCATION);
        assert_eq!(ledger_total(shmem), Some(8));
    }

    #[test]
    fn reclaim_dead_processes_preserves_in_flight_reservations() {
        let shmem: &'static LocalContainerShmem =
            Box::leak(Box::new(unsafe { std::mem::zeroed() }));
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
        let shmem = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
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

    /// Per-card queries include pending bytes and cannot affect another card.
    #[test]
    fn query_preserves_pending_usage_in_independent_card_domains() {
        let first = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
        let second = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
        let mib = 1024 * 1024;
        for shmem in [&*first, &*second] {
            shmem.memory_limit.store(1024 * mib, Ordering::Release);
            shmem.procs[0]
                .pid
                .store(std::process::id() as i32, Ordering::SeqCst);
        }
        let client = test_client_on_device(first, 0);
        let other = test_client_on_device(second, 7);
        assert_eq!(client.check_memory_quota(256 * mib), 0);
        assert_eq!(other.check_memory_quota(512 * mib), 0);
        let (mut free, mut total) = (0usize, 0usize);
        unsafe {
            client.get_hbm_info(&mut free, &mut total);
        }
        assert_eq!(total as u64, 1024 * mib);
        assert_eq!(free as u64, (1024 - 256 - 128) * mib);
        assert_eq!(ledger_total(first), Some(256 * mib));
        assert_eq!(ledger_total(second), Some(512 * mib));
        client.post_alloc_hbm(0, 256 * mib, 1);
        assert_eq!(ledger_total(first), Some(0));
        assert_eq!(ledger_total(second), Some(512 * mib));
    }

    /// Both per-slot overflow and cross-slot sum overflow reject without credit.
    #[test]
    fn overflowing_reservations_do_not_wrap() {
        let shmem = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
        shmem.memory_limit.store(u64::MAX, Ordering::Release);
        shmem.procs[0]
            .pid
            .store(std::process::id() as i32, Ordering::SeqCst);
        shmem.procs[0].hbm_used[0].store(u64::MAX, Ordering::SeqCst);
        let client = test_client(shmem);
        assert_ne!(client.check_memory_quota(1), 0);
        assert_eq!(ledger_total(shmem), Some(u64::MAX));
        shmem.procs[0].hbm_used[0].store(0, Ordering::SeqCst);
        shmem.procs[1].hbm_used[0].store(u64::MAX, Ordering::SeqCst);
        assert_ne!(client.check_memory_quota(1), 0);
        assert_eq!(ledger_total(shmem), Some(u64::MAX));
    }

    /// Native free failure keeps its handle and reservation until a successful retry.
    #[test]
    fn failed_free_keeps_reservation_and_duplicate_free_is_idempotent() {
        let shmem = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
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
        let shmem = Box::leak(Box::new(unsafe {
            std::mem::zeroed::<LocalContainerShmem>()
        }));
        shmem.memory_limit.store(10, Ordering::Release);
        shmem.procs[0].pid.store(4242, Ordering::SeqCst);
        let client = test_client(shmem);
        assert_eq!(client.check_memory_quota(7), 0);
        assert_eq!(ledger_total(shmem), Some(7));
        assert!(retire_slot(&shmem.procs[0], 4242));
        assert_eq!(ledger_total(shmem), Some(0));
        assert_eq!(
            SchedulerClient::register_proc_slot(shmem, std::process::id() as i32),
            0
        );
    }
}
