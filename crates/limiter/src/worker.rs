use crate::shmem::{self, LocalContainerShmem, futex, STATE_IDLE, STATE_RUNNING, STATE_MEASURING, local_shmem_name_for};
use crate::externed_api::*; 
use log::{info, debug, warn};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use std::thread;
use std::fmt;
use std::collections::HashMap;

const VIRTUAL_OVERHEAD_MB: usize = 128; // Reserve 128MB as HBM overhead

#[macro_export]
macro_rules! check_rts {
    ($call:expr) => {
        {
            let ret = unsafe { 
                $call 
            } as u32; // Cast the result to `u32`.
            if ret != 0 {
                println!(
                    "RTS error: {}, from file '{}', line {} - function: `{}`",
	                ret,
                    file!(),
	                line!(),
	                stringify!($call),
                );
            } else {
                // add debug log if necessary
            }
            ret // Return the result (0 in this case) for further use if needed.
        }
    };
}

// =========================================================================================
// INNER LOCK (The Protected State)
// =========================================================================================
// Exactly like your old code: holds the specific resources for measurement.

#[derive(Debug)]
pub struct InnerLock {
    internal_stream: u64,
    start_event: u64,
    end_event: u64,
    tracking_event: u64,
    
    batch_active: bool,
    current_batch_id: u64,
    last_user_stream: u64, // To track where to record the tracking_event
    start_time_us: u64,    // Wall-clock fallback start timestamp
}

// =========================================================================================
// SCHEDULER CLIENT (The Public Interface)
// =========================================================================================
pub struct SchedulerClient {
    pub state: RwLock<Option<Arc<SchedulerClientInner>>>,
}

impl SchedulerClient {
    pub const fn empty() -> Self {
        Self {
            state: RwLock::new(None),
        }
    }

    pub fn get_or_init(&self) -> Arc<SchedulerClientInner> {
        {
            let read_guard = self.state.read().unwrap();
            if let Some(inner) = &*read_guard {
                return inner.clone();
            }
        }
        let mut write_guard = self.state.write().unwrap();
        if let Some(inner) = &*write_guard {
            return inner.clone();
        }
        let inner = Arc::new(SchedulerClientInner::new());
        *write_guard = Some(inner.clone());
        inner
    }

    pub fn on_device_changed(&self, new_device: i32) {
        // Take inner out and release lock BEFORE dropping. Otherwise Drop calls rtSetDevice,
        // which re-enters our hook and deadlocks trying to acquire the same write_guard.
        let _to_drop = {
            let mut write_guard = self.state.write().unwrap();
            if let Some(ref inner) = *write_guard {
                if inner.logical_device != new_device {
                    info!("[Scheduler] Device changed from {} to {}, dropping old state", inner.logical_device, new_device);
                    write_guard.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        // write_guard released here; Drop runs when to_drop goes out of scope, without holding the lock
    }

    pub fn wait_for_token(&self, user_stream: u64) {
        let inner = self.get_or_init();
        inner.wait_for_token(user_stream);
    }

    pub fn is_hbm_limited(&self) -> bool {
        let inner = self.get_or_init();
        inner.is_hbm_limited()
    }

    pub fn check_memory_quota(&self, size: u64) -> u64 {
        let inner = self.get_or_init();
        inner.check_memory_quota(size)
    }

    pub fn post_alloc_hbm(&self, p: u64, size: u64, rts_return: u64) {
        let inner = self.get_or_init();
        inner.post_alloc_hbm(p, size, rts_return);
    }

    pub fn post_free_hbm(&self, handle: u64, ret: u64) {
        let inner = self.get_or_init();
        inner.post_free_hbm(handle, ret);
    }

    pub fn get_hbm_info(&self, free: *mut usize, total: *mut usize) {
        let inner = self.get_or_init();
        inner.get_hbm_info(free, total);
    }
}

// 1. Keep the struct exactly as you have it
pub struct SchedulerClientInner {
    pub shmem: &'static LocalContainerShmem,
    pub my_slot_idx: usize,
    pub lock: Mutex<InnerLock>,
    pub hbm_handle_map: Mutex<HashMap<u64, u64>>,
    pub logical_device: i32,
    pub physical_device: u32,
}

impl Drop for SchedulerClientInner {
    fn drop(&mut self) {
        // Unregister from shmem
        self.shmem.reports[self.my_slot_idx].occupied.store(0, Ordering::SeqCst);
        // Destroy streams and events. on_device_changed is called BEFORE rtSetDevice passthrough,
        // so we're still on this device and don't need to switch.
        let lock = self.lock.lock().unwrap();
        unsafe {
            rtStreamDestroy(lock.internal_stream);
            rtEventDestroy(lock.start_event);
            rtEventDestroy(lock.end_event);
            rtEventDestroy(lock.tracking_event);
        }
    }
}

// 2. MANUALLY IMPLEMENT DEBUG (Matches what I gave you before)
impl fmt::Debug for SchedulerClientInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerClientInner")
            .field("my_slot_idx", &self.my_slot_idx)
            .field("shmem", &self.shmem) 
            .finish()
    }
}

// 3. ADD THESE TRAIT BOUNDS (This fixes the lazy_static error)
// We are guaranteeing to the compiler that our Mutex and Atomics 
// make this safe to share across threads.
unsafe impl Send for SchedulerClientInner {}
unsafe impl Sync for SchedulerClientInner {}

unsafe impl Send for InnerLock {}
unsafe impl Sync for InnerLock {}
impl SchedulerClientInner {
    /// Initialized ONCE per NPUDeviceList (per process/device)
    pub fn new() -> Self {
        let pid = std::process::id();
        info!("[Worker PID:{}] Initialize SchedulerClient...", pid);
        // Determine the physical device id so we can join the correct shared memory
        // segment created by the matching manager thread.
        // logical_id init to -1: "no device set"; rtGetDevice returns 0 on success.
        let mut logical_id: i32 = -1;
        let ret = unsafe { rtGetDevice(&mut logical_id) };
        if ret != 0 {
            panic!("[Scheduler] rtGetDevice failed (ret={}), device not set or error. logical_id={}", ret, logical_id);
        }
        if logical_id < 0 {
            panic!("[Scheduler] rtGetDevice returned logical_id={} (invalid). Device not set?", logical_id);
        }
        info!("[Scheduler] [pid:{}] logical_id: {}", pid, logical_id);
        let mut phy_id: u32 = 0;
        check_rts!(rtGetDevicePhyIdByIndex(logical_id as u32, &mut phy_id));

        let shmem_name = local_shmem_name_for(phy_id);
        let shmem = shmem::shm_setup::open_shmem::<LocalContainerShmem>(shmem_name.as_str());

        // 2. Register THIS Client in a free slot
        let mut idx = 0;
        let mut found = false;
        for (i, slot) in shmem.reports.iter().enumerate() {
            // CAS: 0 -> 1 (Claim Slot)
            if slot.occupied.compare_exchange(0, 1, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                idx = i;
                found = true;
                // Clear stale data
                slot.batch_id.store(0, Ordering::Relaxed);
                slot.cpu_start_us.store(0, Ordering::Relaxed);
                slot.duration_us.store(0, Ordering::Relaxed);
                break;
            }
        }

        if !found {
            panic!("[Scheduler] Registry Full! Increase MAX_WORKERS.");
        }

        debug!("[Scheduler] Client Registered at Slot {}", idx);

        // 3. Initialize Internal NPU Resources (Protected by Mutex later)
        let mut i_stream: u64 = 0;
        let mut start_evt: u64 = 0;
        let mut end_evt: u64 = 0;
        let mut track_evt: u64 = 0;

        check_rts!(rtStreamCreate(&mut i_stream, 0));
        check_rts!(rtEventCreate(&mut start_evt));
        check_rts!(rtEventCreate(&mut end_evt));
        check_rts!(rtEventCreate(&mut track_evt));

        let inner_lock = InnerLock {
            internal_stream: i_stream,
            start_event: start_evt,
            end_event: end_evt,
            tracking_event: track_evt,
            batch_active: false,
            current_batch_id: 0,
            last_user_stream: 0,
            start_time_us: 0,
        };

        Self {
            shmem,
            my_slot_idx: idx,
            lock: Mutex::new(inner_lock),
            hbm_handle_map: Mutex::new(HashMap::new()),
            logical_device: logical_id,
            physical_device: phy_id,
        }
    }

    /// The Main Entry Point
    pub fn wait_for_token(&self, user_stream: u64) {
        // We lock the mutex to safely access/modify internal state.
        // NOTE: In high contention, this serializes access to this check.
        let mut lock = self.lock.lock().unwrap();
        
        lock.last_user_stream = user_stream;

        loop {
            // Read Shared Memory State
            let state = self.shmem.state.load(Ordering::Acquire);
            let global_batch = self.shmem.batch_id.load(Ordering::Relaxed);

            // 1. Reset Logic (If Manager moved to new batch)
            if lock.batch_active && global_batch != lock.current_batch_id {
                lock.batch_active = false;
                lock.start_time_us = 0;
            }

            match state {
                // ---------------------------------------------------------
                // RUNNING: Try to grab token
                // ---------------------------------------------------------
                STATE_RUNNING => {
                    let tokens = self.shmem.tokens_remaining.load(Ordering::Relaxed);
                    
                    if tokens == 0 {
                        // Release lock while yielding to allow other threads to enter?
                        // Actually, simpler to just yield. 
                        drop(lock); // UNLOCK
                        thread::yield_now();
                        lock = self.lock.lock().unwrap(); // RELOCK
                        continue;
                    }

                    // Try to fetch token
                    let prev = self.shmem.tokens_remaining.fetch_sub(1, Ordering::Acquire);
                    if prev > 0 {
                        // SUCCESS!
                        self.shmem
                            .tokens_consumed_cumulative
                            .fetch_add(1, Ordering::Relaxed);
                        
                        // First Token of Batch?
                        if !lock.batch_active {
                            debug!("[Worker PID:{} Slot:{}] get Batch {} first Token!, start record time...", std::process::id(), self.my_slot_idx, global_batch);

                            lock.current_batch_id = global_batch;
                            lock.batch_active = true;
                            
                            // A. Notify Manager
                            self.shmem.active_workers.fetch_add(1, Ordering::Release);

                            // B. CPU Start Time
                            let now_us = get_time_us();
                            self.shmem.reports[self.my_slot_idx].cpu_start_us.store(now_us, Ordering::Relaxed);
                            lock.start_time_us = now_us;

                            // C. GPU Start Event (Internal Stream)
                            check_rts!(rtEventRecord(lock.start_event, lock.internal_stream));
                        }
                        
                        return; // -> Kernel Launch
                    } else {
                        // Race failed
                        self.shmem.tokens_remaining.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // ---------------------------------------------------------
                // MEASURING: Report Time
                // ---------------------------------------------------------
                STATE_MEASURING => {
                    if lock.batch_active && global_batch == lock.current_batch_id {
                        debug!("[Worker PID:{} Slot:{}] start measuring Batch {} ...", std::process::id(), self.my_slot_idx, global_batch);
                        self.measure_and_report_batch(&mut lock);
                    }
                    
                    // Wait for state change
                    drop(lock); // UNLOCK
                    futex::wait(&self.shmem.state, STATE_MEASURING);
                    lock = self.lock.lock().unwrap(); // RELOCK
                }

                // ---------------------------------------------------------
                // IDLE
                // ---------------------------------------------------------
                _ => {
                    drop(lock); // UNLOCK
                    futex::wait(&self.shmem.state, STATE_IDLE);
                    lock = self.lock.lock().unwrap(); // RELOCK
                }
            }
        }
    }

    fn measure_and_report_batch(&self, lock: &mut InnerLock) {
        let mut duration_us: u64 = 0;
        let mut used_wall_clock = false;

        // Prefer device-sync + wall-clock when the user stream is being captured.
        
        {
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
            // 1. Record User Stream End
            check_rts!(rtEventRecord(lock.tracking_event, lock.last_user_stream));
            
            // 2. Cross-Stream Sync
            check_rts!(rtStreamWaitEvent(lock.internal_stream, lock.tracking_event));
            
            // 3. Record Internal End
            check_rts!(rtEventRecord(lock.end_event, lock.internal_stream));
            
            // 4. Block & Sync
            check_rts!(rtStreamSynchronize(lock.internal_stream));
            
            // 5. Calc Duration
            let mut ms: f32 = 0.0;
            check_rts!(rtEventElapsedTime(&mut ms, lock.start_event, lock.end_event));
            duration_us = (ms * 1000.0) as u64;
        }

        // 6. Report to Shared Memory
        let slot = &self.shmem.reports[self.my_slot_idx];
        slot.duration_us.store(duration_us, Ordering::Relaxed);
        slot.batch_id.store(lock.current_batch_id, Ordering::Release);

        debug!("!!!! duration is {:?}", duration_us);
        self.shmem.reported_count.fetch_add(1, Ordering::Release);
        debug!("!!!! reported count is {:?}", self.shmem.reported_count.load(Ordering::Acquire));

        lock.batch_active = false;
        lock.start_time_us = 0;
    }

    pub fn check_memory_quota(&self, size: u64) -> u64 {
        let shmem = self.shmem;
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
             let mut map = self.hbm_handle_map.lock().unwrap();
            map.insert(p, size);
        } else { // fail
            self.shmem.memory_used.fetch_sub(size, Ordering::SeqCst);
        }
    }

    pub fn post_free_hbm(&self, handle: u64, ret: u64) {
        if ret == RT_ERROR_NONE {
            let size = {
                let mut map = self.hbm_handle_map.lock().unwrap();
                map.remove(&handle).unwrap_or(0)
            };

            if size > 0 {
                self.shmem.memory_used.fetch_sub(size, Ordering::SeqCst);
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
        let shmem = self.shmem;
        
        let quota = shmem.memory_limit.load(Ordering::Relaxed) as usize;
        let used = shmem.memory_used.load(Ordering::Relaxed) as usize;
        let overhead_bytes = (VIRTUAL_OVERHEAD_MB * 1024 * 1024) as usize;

        let logical_free = if quota > used {
            quota - used
        } else {
            0
        };

        // 2. 返回给用户的 Free = max(0, 逻辑剩余 - 预留缓冲)
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
        return self.shmem.memory_limit.load(Ordering::Relaxed) > 0
    }
}

// Helper
fn get_time_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}