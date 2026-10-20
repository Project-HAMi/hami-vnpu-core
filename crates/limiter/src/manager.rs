use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH, Duration, Instant};
use std::thread;

use log::{info, debug, warn};

const MAX_DCMI_PROCS: usize = 64;
const DCMI_UPDATE_INTERVAL_SECS: u64 = 5;
/// How often the lightweight reaper runs. It makes NO DCMI call, so dead slots are cleared
/// promptly even when the DCMI refresh thread stalls on a blocking in-container query.
const REAP_INTERVAL_SECS: u64 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct DcmiProcMemInfo {
    proc_id: i32,
    proc_mem_usage: u64,
}

unsafe extern "C" {
    fn dcmi_init() -> i32;
    fn dcmi_get_card_list(card_num: *mut i32, card_list: *mut i32, list_len: i32) -> i32;
    fn dcmi_get_device_resource_info(
        card_id: i32,
        device_id: i32,
        proc_info: *mut DcmiProcMemInfo,
        proc_num: *mut i32,
    ) -> i32;
}

/// Discover (card_id, device_id) pairs for all present NPU logic devices.
fn discover_npu_devices() -> Vec<(i32, i32)> {
    unsafe { dcmi_init(); }
    let mut card_num: i32 = 0;
    let mut card_list: [i32; 8] = [0; 8];
    unsafe { dcmi_get_card_list(&mut card_num, card_list.as_mut_ptr(), 8); }
    if card_num <= 0 || card_num > 8 {
        return Vec::new();
    }
    // Each card has one device (device_id=0) on 310P
    card_list[..card_num as usize]
        .iter()
        .map(|&card_id| (card_id, 0i32))
        .collect()
}

/// Reap dead process slots and release their memory. Runs on its own fast cadence (see
/// REAP_INTERVAL_SECS) so an abnormal exit (crash/kill-9/OOM/zombie) is reflected within
/// one interval, regardless of the app's allocations.
///
/// Only SUBTRACTS the dead slot's own bytes — it never re-derives the whole counter from
/// the slots, which would clobber the worker-maintained `memory_used` of live processes.
fn reap_dead_slots(local: &LocalContainerShmem) {
    for slot in &local.procs {
        let pid = slot.pid.load(Ordering::Relaxed);
        if pid == 0 || proc_alive(pid) {
            continue;
        }
        if slot
            .pid
            .compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        let mut leaked = 0u64;
        for d in 0..NPU_DEVICE_MAX {
            leaked += slot.hbm_used[d].swap(0, Ordering::AcqRel);
        }
        slot.is_active.store(0, Ordering::Release);
        let _ = local.memory_used.fetch_update(Ordering::SeqCst, Ordering::Acquire, |cur| {
            Some(cur.saturating_sub(leaked))
        });
        debug!("[Reap] cleared dead slot pid={} released {} MB", pid, leaked / 1024 / 1024);
    }
}

/// DCMI hardware refresh of LIVE slots (per device). In its own thread because the
/// in-container `dcmi_get_device_resource_info` can block indefinitely; isolating it means
/// such a stall only delays hardware correction, never the reaper above.
fn discover_and_update(local: &LocalContainerShmem) {
    let devices = discover_npu_devices();
    for (idx, &(card_id, device_id)) in devices.iter().enumerate() {
        let mut procs: [DcmiProcMemInfo; MAX_DCMI_PROCS] =
            unsafe { std::mem::zeroed() };
        let mut proc_num: i32 = MAX_DCMI_PROCS as i32;
        unsafe {
            if dcmi_get_device_resource_info(card_id, device_id, procs.as_mut_ptr(), &mut proc_num) != 0 {
                continue;
            }
        }
        if proc_num <= 0 || proc_num as usize > MAX_DCMI_PROCS {
            continue;
        }
        let dcmimap: HashMap<i32, u64> = procs[..proc_num as usize]
            .iter()
            .filter(|p| p.proc_id > 0)
            .map(|p| (p.proc_id, p.proc_mem_usage))
            .collect();
        let dev = idx % NPU_DEVICE_MAX;
        for slot in &local.procs {
            let pid = slot.pid.load(Ordering::Relaxed);
            if pid == 0 {
                continue;
            }
            // ascend runtime reports CONTAINER PIDs here (== slot.pid = std::process::id()).
            // Bare docker w/o ascend runtime reports HOST PIDs and won't match — memory then
            // stays at the rtMalloc-hook value, which is the correct per-pod allocation anyway.
            if let Some(&mem) = dcmimap.get(&pid) {
                slot.hbm_used[dev].store(mem, Ordering::Release);
            }
        }
    }
}

use crate::shmem::{GlobalRegistry, LocalContainerShmem, futex, MAX_MANAGERS, NPU_DEVICE_MAX, STATE_IDLE, STATE_RUNNING, STATE_MEASURING, MAX_WORKERS};
use crate::config::{ManagerConfig, SchedPolicy};
use crate::worker::proc_alive;

const GLOBAL_WATCHDOG_TIMEOUT_US: u64 = 1_000_000;
const GLOBAL_WAIT_POLL_US: u64 = 1_000;
// How long the manager waits in MEASURING for workers to report.
// Was 50ms, which was too short when kernels take hundreds of ms; make it longer
// and also extend dynamically in the run loop.
const LOCAL_REPORT_GRACE_MS: u64 = 500;
const MIN_TOKENS: u64 = 1;
const MAX_TOKENS: u64 = 2_000_000;
// Seed per-manager average; used both locally and when registering in the global scoreboard.
const DEFAULT_AVG_US: u64 = 500;

const MB_TO_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct SharePlan {
    tokens: u64,
    time_limit_us: u64,
    base_tokens: u64,
    expected_run_us: u64,
    rest_wait_us: u64,
    #[allow(dead_code)]
    anchor_speed_us: u64,
}

pub struct ContainerManager {
    global: &'static GlobalRegistry,
    local: &'static LocalContainerShmem,
    my_pid: i32,
    // Spatial managers only maintain Pod memory state and never own a compute slot.
    my_global_idx: Option<usize>,
    current_avg_us: u64,
    compute_limit: f64,
    token_scale: f64,
    ema_alpha: f64,
    sched_policy: SchedPolicy,
    next_run_not_before: Option<Instant>,
}

impl ContainerManager {
    pub fn new(global: &'static GlobalRegistry, local: &'static LocalContainerShmem, pid: i32, config: ManagerConfig) -> Option<Self> {
        let sched_policy = config.sched_policy;
        let idx = if sched_policy.is_time_sliced() {
            Some(Self::register_global_slot(global, pid)?)
        } else {
            None
        };

        // TODO: Refactor
        let token_scale = std::env::var("NPU_TOKEN_SCALE").unwrap_or_else(|_| "100.0".to_string()).parse::<f64>().unwrap_or(1.0).max(0.1);
        // Faster EMA by default.
        let ema_alpha = std::env::var("NPU_AVG_ALPHA").unwrap_or_else(|_| "0.7".to_string()).parse::<f64>().unwrap_or(0.7).clamp(0.05, 0.95);

        // default value
        let compute_limit = f64::from(config.compute_limit);
        let memory_limit = config.memory_limit_mb;
        // A takeover must terminate the predecessor's allowance before publication.
        local.overdraft_pool.close();
        local.overdraft_limit.store(
            if sched_policy == SchedPolicy::Overdraft { config.overdraft.limit } else { 0 },
            Ordering::Relaxed,
        );
        local.overdraft_timeout_us.store(config.overdraft.timeout_us, Ordering::Relaxed);
        local.overdraft_poll_interval_us.store(config.overdraft.poll_interval_us, Ordering::Relaxed);

        info!(
            "[Manager] Global compute slot: {:?} (PID: {}). Compute limit: {}%, Memory limit: {} MB, Policy: {}",
            idx, pid, compute_limit, memory_limit, sched_policy.as_str()
        );

        if let Some(idx) = idx {
            // Publish the configured scheduling share for diagnostics.
            global.slots[idx].compute_limit.store(compute_limit as u64, Ordering::Release);
        } else {
            local.tokens_remaining.store(0, Ordering::Relaxed);
            local.active_workers.store(0, Ordering::Relaxed);
            local.reported_count.store(0, Ordering::Relaxed);
            local.state.store(STATE_IDLE, Ordering::Release);
        }

        let memory_limit_bytes = memory_limit.saturating_mul(MB_TO_BYTES);

        local.memory_limit.store(memory_limit_bytes, Ordering::Relaxed);
        if local.initialized.load(Ordering::Acquire) == 0 {
            local.memory_used.store(0, Ordering::Relaxed);
        }
        local.compute_limit.store(compute_limit as u64, Ordering::Relaxed);
        local.manager_global_idx.store(idx.map_or(-1, |idx| idx as i32), Ordering::Release);
        local.initialized.store(1, Ordering::Release);

        Some(Self {
            global,
            local,
            my_pid: pid,
            my_global_idx: idx,
            current_avg_us: DEFAULT_AVG_US,
            compute_limit,
            token_scale,
            ema_alpha,
            sched_policy,
            next_run_not_before: None,
        })
    }

    fn register_global_slot(global: &'static GlobalRegistry, pid: i32) -> Option<usize> {
        for (i, slot) in global.slots.iter().enumerate() {
            if slot
                .is_active
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                slot.pid.store(pid, Ordering::Relaxed);
                slot.avg_kernel_time.store(DEFAULT_AVG_US, Ordering::Relaxed);
                slot.last_heartbeat.store(get_time_us(), Ordering::Relaxed);
                return Some(i);
            }
        }

        warn!("Global registry full ({} managers); cannot register, staying worker-only.", MAX_MANAGERS);
        None
    }

    pub fn run(&mut self) {
        let reap_shmem = self.local;
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(REAP_INTERVAL_SECS));
                reap_dead_slots(reap_shmem);
            }
        });

        let local_shmem = self.local;
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(DCMI_UPDATE_INTERVAL_SECS));
                discover_and_update(local_shmem);
            }
        });

        if !self.sched_policy.is_time_sliced() {
            // Keep shared memory and process housekeeping alive without joining
            // the compute queue or changing another Pod's global turn.
            loop {
                self.update_heartbeat();
                thread::sleep(Duration::from_millis(200));
            }
        }

        self.join_global_queue();

        loop {
            self.wait_for_global_turn();
            self.update_heartbeat();
            self.honor_fixed_rest_wait();
            
            let plan = self.calculate_fair_share();
            let (actual_duration, tokens_used) = self.run_local_round(&plan);
            
            let has_workers = tokens_used > 0 && actual_duration > 0;
            if has_workers {
                self.update_local_stats(actual_duration, tokens_used);
            }
            self.schedule_rest_wait(plan.rest_wait_us);
            self.pass_baton();
        }
    }

    fn join_global_queue(&self) {
        let Some(my_idx) = self.my_global_idx else { return; };
        let tail = self.global.queue_tail.fetch_add(1, Ordering::SeqCst) as usize;
        let slot_idx = tail % MAX_MANAGERS;
        self.global.queue[slot_idx].store(my_idx as u32, Ordering::Release);
    }

    fn wait_for_global_turn(&self) {
        let Some(my_idx) = self.my_global_idx else { return; };
        loop {
            let owner_idx = self.global.lock_owner.load(Ordering::Acquire);
            if owner_idx == my_idx as u32 { return; }

            // If the recorded owner is invalid or already marked inactive, claim immediately.
            let owner_active = (owner_idx as usize) < MAX_MANAGERS
                && self.global.slots[owner_idx as usize].is_active.load(Ordering::Relaxed) == 1;
            if !owner_active {
                if self.global.lock_owner.compare_exchange(owner_idx, my_idx as u32, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                    self.update_heartbeat();
                    return;
                }
            }

            let last_heartbeat = self.global.lock_timestamp.load(Ordering::Relaxed);
            let now = get_time_us();

            if (now > last_heartbeat) && (now - last_heartbeat > GLOBAL_WATCHDOG_TIMEOUT_US) {
                info!("[Manager] detected stale owner {}; attempting to claim global lock", owner_idx);
                if (owner_idx as usize) < MAX_MANAGERS {
                    self.global.slots[owner_idx as usize].is_active.store(0, Ordering::Relaxed);
                }
                if self.global.lock_owner.compare_exchange(owner_idx, my_idx as u32, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                    self.update_heartbeat();
                    return; 
                }
            }

            let current_sig = self.global.signal_counter.load(Ordering::Relaxed);
            if self.global.lock_owner.load(Ordering::Relaxed) == my_idx as u32 { return; }

            futex::wait_timeout(&self.global.signal_counter, current_sig, GLOBAL_WAIT_POLL_US);
        }
    }

    fn pass_baton(&self) {
        let Some(my_idx) = self.my_global_idx else { return; };
        // Re-enqueue ourselves before handing off to guarantee the next slot is initialized.
        self.join_global_queue();

        // Advance the head to the next queued slot.
        let mut next_head = self.global.queue_head.load(Ordering::Relaxed) as usize + 1;

        // Default to self; the scan may also select our own re-enqueued slot.
        let mut next_manager_idx = my_idx as u32;
        for _ in 0..MAX_MANAGERS {
            let slot_idx = next_head % MAX_MANAGERS;
            let candidate = self.global.queue[slot_idx].load(Ordering::Acquire);

            if (candidate as usize) < MAX_MANAGERS
                && self.global.slots[candidate as usize].is_active.load(Ordering::Relaxed) == 1
            {
                next_manager_idx = candidate;
                break;
            }

            // Stale entry: move forward and keep looking.
            next_head += 1;
        }

        // Commit the head to the slot we actually consumed.
        self.global.queue_head.store(next_head as u32, Ordering::Release);

        // Hand off ownership and wake any waiters.
        self.global.lock_owner.store(next_manager_idx, Ordering::Release);
        self.global.signal_counter.fetch_add(1, Ordering::Release);
        futex::wake_all(&self.global.signal_counter);
    }

    fn calculate_fair_share(&self) -> SharePlan {
        // Anchor is the slowest active; never let it fall below 1us to preserve relativity.
        let mut anchor_speed_us = 1u64; 
        
        for slot in self.global.slots.iter() {
            if slot.is_active.load(Ordering::Relaxed) == 1 {
                let time = slot.avg_kernel_time.load(Ordering::Relaxed);
                if time > anchor_speed_us {
                    anchor_speed_us = time;
                }
            }
        }

        let my_time = self.current_avg_us.max(1);
        // Config validation guarantees an integer in [1,100].
        let limit_for_tokens = self.compute_limit;

        // Base tokens before scaling (used for time budgeting and averaging).
        // Formula: tokens = compute_limit * (anchor / my_time)
        let raw_tokens = limit_for_tokens * (anchor_speed_us as f64 / my_time as f64);
        let mut base_tokens = raw_tokens.ceil() as u64;
        base_tokens = base_tokens.clamp(MIN_TOKENS, MAX_TOKENS);

        // Apply scaling for how many tokens we allow to launch.
        let mut tokens = (base_tokens as f64 * self.token_scale) as u64;
        tokens = tokens.clamp(MIN_TOKENS, MAX_TOKENS);

        // Time budget should reflect the scaled tokens actually handed out.
        let expected_run_us = tokens.saturating_mul(my_time);
        let timeout_us = expected_run_us
            .saturating_add(expected_run_us / 2)
            .saturating_add(50_000);

        // Compute an off-duty window so the runtime/wait ratio follows the configured percentage.
        let rest_wait_us = if self.sched_policy.is_fixed() {
            // User-requested: rest = (100 - p) * (anchor * scale)
            let p = self.compute_limit.clamp(1.0, 100.0);
            let wait = ((100.0 - p) * (anchor_speed_us as f64) * self.token_scale).round();
            wait.clamp(0.0, u64::MAX as f64) as u64
        } else {
            0
        };

        debug!(
            "[Sched] Anchor: {}us, MyAvg: {}us, ComputeLimit: {}, BaseTokens: {}, Scale: {}, FinalTokens: {}, Timeout: {}ms, Rest: {}ms",
            anchor_speed_us, my_time, self.compute_limit, base_tokens, self.token_scale, tokens, timeout_us / 1000, rest_wait_us / 1000
        );

        SharePlan {
            tokens,
            time_limit_us: timeout_us,
            base_tokens,
            expected_run_us: expected_run_us.max(1),
            rest_wait_us,
            anchor_speed_us,
        }
    }

    fn start_local_batch(&self, batch_id: u64, tokens: u64) -> Option<u32> {
        // One CAS word closes old admissions and binds the new allowance's tag.
        let overdraft_round = self.local.overdraft_pool.begin_round();
        self.local.batch_id.store(batch_id, Ordering::Relaxed);
        self.local.tokens_remaining.store(tokens, Ordering::Relaxed);
        self.local.active_workers.store(0, Ordering::Relaxed);
        self.local.reported_count.store(0, Ordering::Relaxed);

        self.local.state.store(STATE_RUNNING, Ordering::Release);
        futex::wake_all(&self.local.state);
        overdraft_round
    }

    fn enter_measuring_state(&self) {
        self.local.tokens_remaining.store(0, Ordering::Relaxed);
        self.update_heartbeat();
        self.local.state.store(STATE_MEASURING, Ordering::Release);
        futex::wake_all(&self.local.state);
    }

    fn run_local_round(&self, plan: &SharePlan) -> (u64, u64) {
        let tokens = plan.tokens;
        let base_tokens = plan.base_tokens;
        let batch_id = self.local.batch_id.load(Ordering::Relaxed) + 1;
        debug!("\n=======================================================");
        debug!("[Manager] >>> Start Batch ID: {}. initial Tokens: {}, base_tokens: {}, time_limit: {} us, budget: {} us", batch_id, tokens, base_tokens, plan.time_limit_us, plan.expected_run_us);

        let overdraft_round = self.start_local_batch(batch_id, tokens);

        let start_time = Instant::now();
        let timeout_duration = Duration::from_micros(plan.time_limit_us);
        let budget_duration = Duration::from_micros(plan.expected_run_us);
        let mut saw_progress = false;
        let mut last_tokens = tokens;
        let mut tokens_consumed_for_stats = 0u64;
        
        loop {
            let current_tokens = self.local.tokens_remaining.load(Ordering::Relaxed);

            if current_tokens == 0 { 
                debug!("[Manager] --- Token all used, enter STATE_MEASURING ");
                break;
            }

            if self.sched_policy.is_fixed() {
                // Hold the lock for the full budgeted runtime even if no worker progresses,
                // so the run/wait ratio follows the configured percentage.
                if start_time.elapsed() >= budget_duration {
                    tokens_consumed_for_stats = tokens.saturating_sub(current_tokens);
                    break;
                }
            } else {
                // Detect zero-progress batches (e.g., other container paused) and bail early
                // instead of waiting full timeout_us.
                if current_tokens < last_tokens || self.local.active_workers.load(Ordering::Relaxed) > 0 {
                    saw_progress = true;
                }
                if !saw_progress && start_time.elapsed() > Duration::from_millis(5) {
                    debug!("[Manager] --- no progress for 5ms, enter STATE_MEASURING early");
                    break;
                }
            }
            last_tokens = current_tokens;

            if start_time.elapsed() > timeout_duration {
                let remaining = self.local.tokens_remaining.load(Ordering::Relaxed);
                if remaining < tokens {
                    debug!("[Manager] --- time up! Remaining Token: {}/{}.", remaining, tokens);
                }
                tokens_consumed_for_stats = tokens.saturating_sub(remaining);
                break;
            }
            self.update_heartbeat(); 
            thread::sleep(Duration::from_micros(200));
        }

        // Capture how many tokens were actually spent before we zero-out the bucket.
        if tokens_consumed_for_stats == 0 {
            let remaining = self.local.tokens_remaining.load(Ordering::Relaxed);
            tokens_consumed_for_stats = tokens.saturating_sub(remaining);
        }

        self.enter_measuring_state();
        
        let report_start = Instant::now();
        // Wait longer when batches are long: at least LOCAL_REPORT_GRACE_MS, or 1/4 of limit_us.
        let grace_duration = Duration::from_millis(LOCAL_REPORT_GRACE_MS)
            .max(Duration::from_micros(plan.time_limit_us / 4).saturating_add(Duration::from_millis(10)));
        
        // While waiting for worker reports, keep heartbeating so other managers
        // do not declare us dead during a long grace window.
        let mut last_hb = Instant::now();
        loop {
            let active = self.local.active_workers.load(Ordering::Acquire);
            let reported = self.local.reported_count.load(Ordering::Acquire);
            
            if reported >= active {
                break; 
            }
            if report_start.elapsed() > grace_duration {
                break;
            }
            if last_hb.elapsed() > Duration::from_millis(100) {
                self.update_heartbeat();
                last_hb = Instant::now();
            }
            thread::yield_now();
        }

        let (duration, tokens_used) = self.aggregate_local_times(batch_id, tokens_consumed_for_stats);
        self.local.state.store(STATE_IDLE, Ordering::Release);
        if self.sched_policy == SchedPolicy::Overdraft {
            if let Some(round) = overdraft_round {
                self.local.overdraft_pool.open_idle(round);
            }
            // Wake workers that slept in MEASURING so they can start timed IDLE waits.
            futex::wake_all(&self.local.state);
        }

        if tokens_used > 0 {
            debug!("[Manager] <<< Batch {} ends. consume Token (for stats): {}, duration: {} us", batch_id, tokens_used, duration);
        }

        (duration, tokens_used)
    }

    fn honor_fixed_rest_wait(&mut self) {
        if !self.sched_policy.is_fixed() {
            return;
        }
        if let Some(deadline) = self.next_run_not_before {
            let now = Instant::now();
            if now < deadline {
                let mut last_hb = Instant::now();
                loop {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    if last_hb.elapsed() > Duration::from_millis(100) {
                        self.update_heartbeat();
                        last_hb = Instant::now();
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let sleep_for = remaining.min(Duration::from_millis(2));
                    thread::sleep(sleep_for);
                }
            }
            self.next_run_not_before = None;
        }
    }

    fn aggregate_local_times(&self, batch_id: u64, tokens_for_stats: u64) -> (u64, u64) {
        let mut global_start = u64::MAX;
        let mut global_end = 0;
        let mut participants = 0;

        for i in 0..MAX_WORKERS {
            let slot = &self.local.reports[i];
            if slot.batch_id.load(Ordering::Acquire) == batch_id {
                let start = slot.cpu_start_us.load(Ordering::Relaxed);
                let dur = slot.duration_us.load(Ordering::Relaxed);
                
                if dur > 0 {
                    let end = start.saturating_add(dur);

                    if start < global_start { global_start = start; }
                    if end > global_end { global_end = end; }
                    participants += 1;
                }
            }
        }

        if participants == 0 { return (0, 0); }
        let total_duration = if global_end > global_start { global_end - global_start } else { 0 };
        // Use the actual tokens consumed (post-scale) so averages are per real token.
        let tokens_used = tokens_for_stats;

        debug!("[Manager-Aggregate] total participants: {}, earliest time: {}, latest time: {}", 
                 participants, global_start, global_end);
        debug!("[Manager-Aggregate] total duration: {} us, consume Tokens: {}", 
                 total_duration, tokens_used);
                 
        (total_duration, tokens_used)
    }

    fn update_local_stats(&mut self, duration_us: u64, tokens: u64) {
        let Some(my_idx) = self.my_global_idx else { return; };
        if tokens == 0 { return; }
        let new_avg = duration_us / tokens;
        // Clamp swings but allow faster adaptation.
        let lower = (self.current_avg_us / 4).max(1);
        let upper = self.current_avg_us.saturating_mul(4).max(1);
        let clamped_avg = new_avg.clamp(lower, upper);
        let alpha = self.ema_alpha;
        self.current_avg_us = ((self.current_avg_us as f64 * (1.0 - alpha)) + (clamped_avg as f64 * alpha)) as u64;
        
        self.global.slots[my_idx].avg_kernel_time.store(self.current_avg_us, Ordering::Relaxed);
        self.global.slots[my_idx].last_heartbeat.store(get_time_us(), Ordering::Relaxed);
    }

    fn schedule_rest_wait(&mut self, rest_wait_us: u64) {
        if !self.sched_policy.is_fixed() || rest_wait_us == 0 {
            self.next_run_not_before = None;
            return;
        }
        let now = Instant::now();
        let deadline = now
            .checked_add(Duration::from_micros(rest_wait_us))
            .unwrap_or(now);
        self.next_run_not_before = Some(deadline);
    }
    
    fn update_heartbeat(&self) {
        let now = get_time_us();
        if let Some(my_idx) = self.my_global_idx {
            self.global.lock_timestamp.store(now, Ordering::Relaxed);
            self.global.slots[my_idx].last_heartbeat.store(now, Ordering::Relaxed);
        }
        // Local heartbeat so a same-pod supervisor can detect this manager dying.
        self.local.manager_heartbeat.store(now, Ordering::Relaxed);
    }
}

impl Drop for ContainerManager {
    fn drop(&mut self) {
        // Mark this manager inactive so others will skip its slot and can steal the lock.
        if let Some(my_idx) = self.my_global_idx {
            self.global.slots[my_idx].is_active.store(0, Ordering::Relaxed);
        }

        // Release local manager ownership so a same-pod supervisor can take over.
        // Best-effort: run() is an infinite loop and kill -9 skips Drop, so real recovery
        // relies on the supervisor's proc_alive + heartbeat-timeout checks, not this.
        let _ = self.local.manager_pid.compare_exchange(
            self.my_pid, 0, Ordering::AcqRel, Ordering::Relaxed,
        );
        self.local.manager_global_idx.store(-1, Ordering::Release);

        if self.my_global_idx.is_some() {
            // Nudge global waiters only if this manager participated in rotation.
            self.global.signal_counter.fetch_add(1, Ordering::Release);
            futex::wake_all(&self.global.signal_counter);
        }
    }
}

fn get_time_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}
