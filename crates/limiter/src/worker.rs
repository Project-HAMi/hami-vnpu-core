use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use std::thread;
use std::fmt;
use std::collections::HashMap;

use log::{info, debug, warn};

use crate::shmem::{self, LocalContainerShmem, futex, STATE_IDLE, STATE_RUNNING, STATE_MEASURING};
use crate::config::{local_shmem_name, VIRTUAL_OVERHEAD_MB};
use crate::externed_api::*; 
use crate::check_rts;

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
    lock: Mutex<InnerLock>,
    hbm_handle_map: Mutex<HashMap<u64, u64>>,
}

impl fmt::Debug for SchedulerClientInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerClientInner")
            .field("my_slot_idx", &self.my_slot_idx)
            .field("shmem", &self.shmem) 
            .finish()
    }
}

impl SchedulerClient {
    /// Initialized ONCE per NPUDeviceList (per process/device)
    pub fn new() -> Self {
        info!("[Worker PID:{}] Initialize SchedulerClient...", std::process::id());

        let shmem_name = local_shmem_name();
        let shmem = shmem::setup::open_shmem::<LocalContainerShmem>(shmem_name.as_str());

        // Register THIS Client in a free slot
        let my_slot_idx = Self::register_worker_slot(shmem);
        debug!("[Scheduler] Client Registered at Slot {}", my_slot_idx);

        // Initialize internal NPU resources used for timing.
        let inner_lock = Self::create_inner_lock();

        Self {
            inner: Arc::new(SchedulerClientInner {
                shmem,
                my_slot_idx,
                lock: Mutex::new(inner_lock),
                hbm_handle_map: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn register_worker_slot(shmem: &'static LocalContainerShmem) -> usize {
        for (i, slot) in shmem.reports.iter().enumerate() {
            if slot
                .occupied
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                slot.batch_id.store(0, Ordering::Relaxed);
                slot.cpu_start_us.store(0, Ordering::Relaxed);
                slot.duration_us.store(0, Ordering::Relaxed);
                return i;
            }
        }

        panic!("[Scheduler] Registry full. Increase MAX_WORKERS.");
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
    pub fn check_memory_quota(&self, size: u64) -> u64 {
        let shmem = self.inner.shmem;
        let limit = shmem.memory_limit.load(Ordering::Relaxed);
        
        // if no limit
        if limit == 0 { return 0; }

        let mut current_used = shmem.memory_used.load(Ordering::Acquire);

        loop {
            let new_used = current_used + size;

            if new_used > limit {
                warn!(
                    "[Worker PID:{}] Memory Quota Exceeded! Request: {} MB, Used: {} MB, Limit: {} MB",
                    std::process::id(),
                    size / 1024 / 1024,
                    current_used / 1024 / 1024,
                    limit / 1024 / 1024
                );
                return RT_ERROR_MEMORY_ALLOCATION;
            }

            match shmem.memory_used.compare_exchange(
                current_used,
                new_used,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    debug!(
                        "[Worker] Memory Quota Reserved: {} bytes. Total used: {} bytes",
                        size, new_used
                    );
                    return 0; // success
                }
                Err(actual) => {
                    // If updated by another thread, update the local value and retry.
                    current_used = actual;
                }
            }
        }
    }

    pub fn post_alloc_hbm(&self, p: u64, size: u64, rts_return: u64) {
        if rts_return == RT_ERROR_NONE { // success
             let mut map = self.inner.hbm_handle_map.lock().unwrap();
            map.insert(p, size);
        } else { // fail
            self.inner.shmem.memory_used.fetch_sub(size, Ordering::SeqCst);
        }
    }

    pub fn post_free_hbm(&self, handle: u64, ret: u64) {
        if ret == RT_ERROR_NONE {
            let size = {
                let mut map = self.inner.hbm_handle_map.lock().unwrap();
                map.remove(&handle).unwrap_or(0)
            };

            if size > 0 {
                self.inner.shmem.memory_used.fetch_sub(size, Ordering::SeqCst);
                debug!(
                    "[Limiter] Free Success: Handle 0x{:x}, Size {} bytes returned to quota.",
                    handle, size
                );
            } else {
                // ptr not exist in hamp
                warn!("[Limiter] Free Success but Handle 0x{:x} was untracked!", handle);
            }
        } else {
            // rtFree Failed
            warn!(
                "[Limiter] rtFreePhysical FAILED (code: {}), handle: 0x{:x}. Quota not released.",
                ret, handle
            );
        }
    }

    pub fn get_hbm_info(&self, free: *mut usize, total: *mut usize) {
        let shmem = self.inner.shmem;
        
        let quota = shmem.memory_limit.load(Ordering::Relaxed) as usize;
        let used = shmem.memory_used.load(Ordering::Relaxed) as usize;
        let overhead_bytes = (VIRTUAL_OVERHEAD_MB * 1024 * 1024) as usize;

        let logical_free = if quota > used {
            quota - used
        } else {
            0
        };

        // 返回给用户的 Free = max(0, 逻辑剩余 - 预留缓冲)
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
}

// Helper
fn get_time_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}