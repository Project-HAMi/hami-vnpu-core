use crate::registry_lock::{RegistryGuard, RegistryLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, info, warn};

const DCMI_UPDATE_INTERVAL_SECS: u64 = 5;
const DCMI_QUERY_TIMEOUT_SECS: u64 = 2;
const REAP_INTERVAL_SECS: u64 = 2;

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
        let _ = local
            .memory_used
            .fetch_update(Ordering::SeqCst, Ordering::Acquire, |cur| {
                Some(cur.saturating_sub(leaked))
            });
        debug!(
            "[Reap] cleared dead slot pid={} released {} MB",
            pid,
            leaked / 1024 / 1024
        );
    }
}

// A targeted helper emits device zero for the selected physical card. Hardware
// samples are diagnostic: never overwrite a live or in-flight hook reservation.
fn observe_snapshot(
    local: &LocalContainerShmem,
    physical: Option<u32>,
    records: &[crate::dcmi_protocol::Record],
) -> usize {
    let mut matched = 0;
    for record in records {
        for slot in &local.procs {
            if slot.pid.load(Ordering::Acquire) == record.pid {
                debug!(
                    "[DCMI physical:{physical:?} device:{} PID:{}] hardware={} hook={} bytes",
                    record.device,
                    record.pid,
                    record.bytes,
                    slot.hbm_used[record.device].load(Ordering::Acquire)
                );
                matched += 1;
            }
        }
    }
    matched
}

use crate::config::{ManagerConfig, SchedPolicy};
use crate::shmem::{
    GlobalRegistry, LocalContainerShmem, MAX_MANAGERS, MAX_WORKERS, NPU_DEVICE_MAX, STATE_IDLE,
    STATE_MEASURING, STATE_RUNNING, futex,
};
use crate::worker::proc_alive;

const GLOBAL_WATCHDOG_TIMEOUT_US: u64 = 1_000_000;
const GLOBAL_WAIT_POLL_US: u64 = 10_000;
const STALE_MANAGER_US: u64 = 3_000_000;
// Cap a complete run/rest period without changing ordinary 500us seed budgets.
const MAX_RUN_US: u64 = 5_000_000;
const NO_PROGRESS_US: u64 = 5_000;
pub(crate) const REGISTRY_PROTOCOL: u32 = 0x564e_5033;
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
    expected_run_us: u64,
    rest_wait_us: u64,
    #[allow(dead_code)]
    anchor_speed_us: u64,
}

pub struct ContainerManager {
    global: &'static GlobalRegistry,
    local: &'static LocalContainerShmem,
    my_pid: i32,
    my_global_idx: Option<usize>,
    physical_device_id: Option<u32>,
    current_avg_us: u64,
    compute_limit: f64,
    token_scale: f64,
    ema_alpha: f64,
    sched_policy: SchedPolicy,
    next_run_not_before: Option<Instant>,
    generation: u32,
    registry: Arc<RegistryLock>,
    stopped: Arc<AtomicBool>,
}

impl ContainerManager {
    pub fn new(
        global: &'static GlobalRegistry,
        local: &'static LocalContainerShmem,
        pid: i32,
        config: ManagerConfig,
        registry: Arc<RegistryLock>,
        generation: u32,
    ) -> Option<Self> {
        // Caller holds the registry transaction across election and publication.
        let idx = if config.sched_policy.is_time_sliced() {
            Some(Self::register_global_slot(global, pid, generation)?)
        } else {
            None
        };

        // TODO: Refactor
        let token_scale = std::env::var("NPU_TOKEN_SCALE")
            .unwrap_or_else(|_| "100.0".to_string())
            .parse::<f64>()
            .unwrap_or(1.0)
            .max(0.1);
        // Faster EMA by default.
        let ema_alpha = std::env::var("NPU_AVG_ALPHA")
            .unwrap_or_else(|_| "0.7".to_string())
            .parse::<f64>()
            .unwrap_or(0.7)
            .clamp(0.05, 0.95);
        let sched_policy = config.sched_policy;

        // default value
        let compute_limit = f64::from(config.compute_limit);
        let memory_limit = config.memory_limit_mb;
        local.overdraft_pool.close();
        local.overdraft_limit.store(
            if sched_policy == SchedPolicy::Overdraft {
                config.overdraft.limit
            } else {
                0
            },
            Ordering::Relaxed,
        );
        local
            .overdraft_timeout_us
            .store(config.overdraft.timeout_us, Ordering::Relaxed);
        local
            .overdraft_poll_interval_us
            .store(config.overdraft.poll_interval_us, Ordering::Relaxed);

        info!(
            "[Manager] Global compute slot: {:?} (PID: {}). Compute limit: {}%, Memory limit: {} MB, Policy: {}",
            idx,
            pid,
            compute_limit,
            memory_limit,
            sched_policy.as_str()
        );

        // Write our priority to the global slot so other pods can calculate compute share
        if let Some(idx) = idx {
            global.slots[idx]
                .compute_limit
                .store(compute_limit as u64, Ordering::Release);
        } else {
            local.tokens_remaining.store(0, Ordering::Relaxed);
            local.active_workers.store(0, Ordering::Relaxed);
            local.reported_count.store(0, Ordering::Relaxed);
            local.state.store(STATE_IDLE, Ordering::Release);
        }

        let memory_limit_bytes = memory_limit.saturating_mul(MB_TO_BYTES);

        local
            .memory_limit
            .store(memory_limit_bytes, Ordering::Relaxed);
        if local.initialized.load(Ordering::Acquire) == 0 {
            local.memory_used.store(0, Ordering::Relaxed);
        }
        local
            .compute_limit
            .store(compute_limit as u64, Ordering::Relaxed);
        local
            .manager_global_idx
            .store(idx.map_or(-1, |idx| idx as i32), Ordering::Release);
        local.initialized.store(1, Ordering::Release);

        Some(Self {
            global,
            local,
            my_pid: pid,
            my_global_idx: idx,
            physical_device_id: config.physical_device_id,
            current_avg_us: DEFAULT_AVG_US,
            compute_limit,
            token_scale,
            ema_alpha,
            sched_policy,
            next_run_not_before: None,
            generation,
            registry,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }

    fn register_global_slot(
        global: &'static GlobalRegistry,
        pid: i32,
        generation: u32,
    ) -> Option<usize> {
        let now = get_time_us();
        for (i, slot) in global.slots.iter().enumerate() {
            if slot.is_active.load(Ordering::Acquire) == 0
                || now.saturating_sub(slot.last_heartbeat.load(Ordering::Acquire))
                    > STALE_MANAGER_US
            {
                slot.pid.store(pid, Ordering::Relaxed);
                slot.generation.store(generation, Ordering::Relaxed);
                slot.avg_kernel_time
                    .store(DEFAULT_AVG_US, Ordering::Relaxed);
                slot.last_heartbeat.store(now, Ordering::Relaxed);
                slot.is_active.store(1, Ordering::Release);
                return Some(i);
            }
        }
        warn!("Global registry full ({} managers)", MAX_MANAGERS);
        None
    }

    fn owns_local_lease(&self) -> bool {
        self.local.manager_pid.load(Ordering::Acquire) == self.my_pid
            && self.local.manager_generation.load(Ordering::Acquire) == self.generation
    }

    fn is_current(&self) -> bool {
        let Some(index) = self.my_global_idx else {
            return self.owns_local_lease()
                && self.local.manager_global_idx.load(Ordering::Acquire) == -1;
        };
        let slot = &self.global.slots[index];
        self.owns_local_lease()
            && self.local.manager_global_idx.load(Ordering::Acquire) == index as i32
            && slot.is_active.load(Ordering::Acquire) != 0
            && slot.pid.load(Ordering::Acquire) == self.my_pid
            && slot.generation.load(Ordering::Acquire) == self.generation
    }

    fn transaction(&self) -> Option<RegistryGuard<'_>> {
        let guard = self.registry.lock().ok()?;
        self.is_current().then_some(guard)
    }

    fn has_turn(&self) -> bool {
        self.my_global_idx
            .is_some_and(|index| self.global.lock_owner.load(Ordering::Acquire) == index as u32)
    }

    pub fn run(&mut self) {
        // Background collectors are stopped when this lease is replaced. Their
        // results must be revalidated under the same registry transaction.
        self.start_collectors();
        if !self.sched_policy.is_time_sliced() {
            while self.update_heartbeat() {
                thread::sleep(Duration::from_millis(200));
            }
            return;
        }
        while self.honor_fixed_rest_wait() && self.wait_for_global_turn() {
            let plan = self.calculate_fair_share();
            let (duration, tokens) = self.run_local_round(&plan);
            self.update_local_stats(duration, tokens);
            self.schedule_rest_wait(plan.rest_wait_us);
            if !self.pass_baton() {
                break;
            }
        }
    }

    fn start_collectors(&self) {
        let local = self.local;
        let stopped = self.stopped.clone();
        let registry = self.registry.clone();
        let pid = self.my_pid;
        let generation = self.generation;
        thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                thread::sleep(Duration::from_secs(REAP_INTERVAL_SECS));
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let Ok(_guard) = registry.lock() else {
                    break;
                };
                if local.manager_pid.load(Ordering::Acquire) != pid
                    || local.manager_generation.load(Ordering::Acquire) != generation
                {
                    break;
                }
                reap_dead_slots(local);
            }
        });
        let local = self.local;
        let stopped = self.stopped.clone();
        let registry = self.registry.clone();
        let physical_device_id = self.physical_device_id;
        let Some(helper) = crate::dcmi::helper_path().filter(|path| path.is_file()) else {
            warn!(
                "[Manager] vnpu-dcmi helper unavailable; hardware refresh disabled, hook accounting remains active"
            );
            return;
        };
        thread::spawn(move || {
            let mut collector = crate::dcmi::Collector::default();
            while !stopped.load(Ordering::Acquire) {
                thread::sleep(Duration::from_secs(DCMI_UPDATE_INTERVAL_SECS));
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let snapshot = match collector.snapshot_for_device(
                    &helper,
                    Duration::from_secs(DCMI_QUERY_TIMEOUT_SECS),
                    &stopped,
                    physical_device_id,
                ) {
                    Ok(snapshot) => {
                        debug!(
                            "[Manager] DCMI snapshot received: {} records",
                            snapshot.len()
                        );
                        snapshot
                    }
                    Err(error) => {
                        warn!("[Manager] DCMI snapshot failed: {error}");
                        continue;
                    }
                };
                let Ok(_guard) = registry.lock() else {
                    break;
                };
                if stopped.load(Ordering::Acquire)
                    || local.manager_pid.load(Ordering::Acquire) != pid
                    || local.manager_generation.load(Ordering::Acquire) != generation
                {
                    break;
                }
                observe_snapshot(local, physical_device_id, &snapshot);
            }
        });
    }

    fn wait_for_global_turn(&self) -> bool {
        let Some(my_idx) = self.my_global_idx else {
            return false;
        };
        loop {
            let signal = {
                let Some(_guard) = self.transaction() else {
                    return false;
                };
                self.heartbeat_locked();
                if self.has_turn() {
                    return true;
                }
                let owner = self.global.lock_owner.load(Ordering::Acquire) as usize;
                let active = owner < MAX_MANAGERS
                    && self.global.slots[owner].is_active.load(Ordering::Acquire) == 1;
                let stale = get_time_us()
                    .saturating_sub(self.global.lock_timestamp.load(Ordering::Acquire))
                    > GLOBAL_WATCHDOG_TIMEOUT_US;
                if !active || stale {
                    if active {
                        self.global.slots[owner]
                            .is_active
                            .store(0, Ordering::Release);
                    }
                    self.global
                        .lock_owner
                        .store(my_idx as u32, Ordering::Release);
                    self.heartbeat_locked();
                    return true;
                }
                self.global.signal_counter.load(Ordering::Acquire)
            };
            futex::wait_timeout(&self.global.signal_counter, signal, GLOBAL_WAIT_POLL_US);
        }
    }

    fn pass_baton(&self) -> bool {
        let Some(my_idx) = self.my_global_idx else {
            return false;
        };
        let Some(_guard) = self.transaction() else {
            return false;
        };
        if !self.has_turn() {
            return false;
        }
        // A fixed-share rest never holds the global turn. State 2 means a live,
        // resting manager; waiting heartbeats keep that slot from being reclaimed.
        if self.next_run_not_before.is_some() {
            self.global.slots[my_idx]
                .is_active
                .store(2, Ordering::Release);
        }
        let now = get_time_us();
        let mut next = MAX_MANAGERS;
        // Slots already form a bounded set: round-robin scanning needs no queue
        // entries that can be duplicated or left behind after manager takeover.
        for offset in 1..=MAX_MANAGERS {
            let index = (my_idx + offset) % MAX_MANAGERS;
            let slot = &self.global.slots[index];
            if slot.is_active.load(Ordering::Acquire) == 0 {
                continue;
            }
            if now.saturating_sub(slot.last_heartbeat.load(Ordering::Acquire)) > STALE_MANAGER_US {
                slot.is_active.store(0, Ordering::Release);
            } else if slot.is_active.load(Ordering::Acquire) == 1 {
                next = index;
                break;
            }
        }
        self.global.lock_timestamp.store(now, Ordering::Relaxed);
        self.global.lock_owner.store(next as u32, Ordering::Release);
        self.global.signal_counter.fetch_add(1, Ordering::Release);
        futex::wake_all(&self.global.signal_counter);
        true
    }

    fn calculate_fair_share(&self) -> SharePlan {
        // Anchor is the slowest active; never let it fall below 1us to preserve relativity.
        let mut anchor_speed_us = 1u64;
        let mut highest_priority = self.compute_limit.max(1.0);

        for slot in self.global.slots.iter() {
            if slot.is_active.load(Ordering::Relaxed) == 1 {
                highest_priority =
                    highest_priority.max(slot.compute_limit.load(Ordering::Relaxed) as f64);
                let time = slot.avg_kernel_time.load(Ordering::Relaxed);
                if time > anchor_speed_us {
                    anchor_speed_us = time;
                }
            }
        }

        let my_time = self.current_avg_us.max(1);
        // Resource validation guarantees an integer percentage in [1, 100].
        let prio_for_tokens = self.compute_limit;

        // Base tokens before scaling (used for time budgeting and averaging).
        // Formula: tokens = priority * (anchor / my_time)
        let raw_tokens = prio_for_tokens * (anchor_speed_us as f64 / my_time as f64);
        let mut base_tokens = raw_tokens.ceil() as u64;
        base_tokens = base_tokens.clamp(MIN_TOKENS, MAX_TOKENS);

        // Apply scaling for how many tokens we allow to launch.
        let mut tokens = (base_tokens as f64 * self.token_scale) as u64;
        tokens = tokens.clamp(MIN_TOKENS, MAX_TOKENS);

        let priority_span = if self.sched_policy.is_fixed() {
            100.0
        } else {
            highest_priority
        };
        let unbounded_unit = anchor_speed_us as f64 * self.token_scale;
        let unit_us = unbounded_unit.min(MAX_RUN_US as f64 / priority_span);
        if unit_us < unbounded_unit {
            tokens = ((prio_for_tokens * unit_us / my_time as f64).ceil() as u64)
                .clamp(MIN_TOKENS, MAX_TOKENS);
        }
        let expected_run_us = tokens.saturating_mul(my_time).min(MAX_RUN_US);
        let timeout_us = expected_run_us
            .saturating_add(expected_run_us / 2)
            .saturating_add(50_000)
            .min(MAX_RUN_US);

        // Compute an off-duty window so the runtime/wait ratio depends on priority only.
        let rest_wait_us = if self.sched_policy.is_fixed() {
            // User-requested: rest = (100 - p) * (anchor * scale)
            let p = self.compute_limit.clamp(1.0, 100.0);
            let wait = ((100.0 - p) * unit_us).round();
            wait.clamp(0.0, MAX_RUN_US as f64) as u64
        } else {
            0
        };

        debug!(
            "[Sched] Anchor: {}us, MyAvg: {}us, ComputeLimit: {}, BaseTokens: {}, Scale: {}, FinalTokens: {}, Timeout: {}ms, Rest: {}ms",
            anchor_speed_us,
            my_time,
            self.compute_limit,
            base_tokens,
            self.token_scale,
            tokens,
            timeout_us / 1000,
            rest_wait_us / 1000
        );

        SharePlan {
            tokens,
            time_limit_us: timeout_us,
            expected_run_us: expected_run_us.max(1),
            rest_wait_us,
            anchor_speed_us,
        }
    }

    fn start_local_batch(&self, batch_id: u64, tokens: u64) -> Result<Option<u32>, ()> {
        let Some(_guard) = self.transaction() else {
            return Err(());
        };
        if !self.has_turn() {
            return Err(());
        }
        let round = self.local.overdraft_pool.begin_round();
        self.local.batch_id.store(batch_id, Ordering::Relaxed);
        self.local.tokens_remaining.store(tokens, Ordering::Relaxed);
        self.local.active_workers.store(0, Ordering::Relaxed);
        self.local.reported_count.store(0, Ordering::Relaxed);
        self.local.state.store(STATE_RUNNING, Ordering::Release);
        futex::wake_all(&self.local.state);
        Ok(round)
    }

    fn run_local_round(&self, plan: &SharePlan) -> (u64, u64) {
        let start = Instant::now();
        let batch = self
            .local
            .batch_id
            .load(Ordering::Acquire)
            .wrapping_add(1)
            .max(1);
        let Ok(overdraft_round) = self.start_local_batch(batch, plan.tokens) else {
            return (0, 0);
        };
        let mut last_progress = start;
        let mut last_observation = start;
        let mut timely_observations = true;
        let mut remaining = plan.tokens;
        let complete = loop {
            if !self.update_heartbeat() {
                return (0, 0);
            }
            let observed = Instant::now();
            timely_observations &=
                observed.duration_since(last_observation).as_micros() <= NO_PROGRESS_US as u128;
            last_observation = observed;
            let current = self.local.tokens_remaining.load(Ordering::Acquire);
            if current < remaining {
                last_progress = Instant::now();
            }
            remaining = current;
            if current == 0 {
                break true;
            }
            if self.sched_policy.is_fixed() {
                if start.elapsed().as_micros() >= plan.expected_run_us as u128 {
                    break false;
                }
            } else if last_progress.elapsed().as_micros() >= NO_PROGRESS_US as u128 {
                break false;
            }
            if start.elapsed().as_micros() >= plan.time_limit_us as u128 {
                break false;
            }
            thread::sleep(Duration::from_micros(200));
        };
        let consumed = {
            let Some(_guard) = self.transaction() else {
                return (0, 0);
            };
            if !self.has_turn() {
                return (0, 0);
            }
            let remaining = self.local.tokens_remaining.swap(0, Ordering::AcqRel);
            self.local.state.store(STATE_MEASURING, Ordering::Release);
            futex::wake_all(&self.local.state);
            plan.tokens.saturating_sub(remaining)
        };
        // A worker may be inside CANN or idle between requests, unable to report.
        // Bound this wait; do not turn missing/late reports into a timing sample.
        let report_start = Instant::now();
        timely_observations &=
            report_start.duration_since(last_observation).as_micros() <= NO_PROGRESS_US as u128;
        let mut all_reported = false;
        while report_start.elapsed() < Duration::from_millis(LOCAL_REPORT_GRACE_MS) {
            if !self.update_heartbeat() {
                return (0, 0);
            }
            let active = self.local.active_workers.load(Ordering::Acquire);
            let reports = self
                .local
                .reports
                .iter()
                .filter(|r| {
                    r.occupied.load(Ordering::Acquire) != 0
                        && r.batch_id.load(Ordering::Acquire) == batch
                })
                .count();
            if reports >= active as usize {
                all_reported = true;
                break;
            }
            thread::sleep(Duration::from_micros(200));
        }
        let Some(_guard) = self.transaction() else {
            return (0, 0);
        };
        if !self.has_turn() {
            return (0, 0);
        }
        self.local.state.store(STATE_IDLE, Ordering::Release);
        if self.sched_policy == SchedPolicy::Overdraft
            && let Some(round) = overdraft_round
        {
            self.local.overdraft_pool.open_idle(round);
        }
        futex::wake_all(&self.local.state);
        // Event endpoints are recorded by a later hook. A delayed report can
        // include idle time after the final launch, even if all tokens were used.
        // Conservatively keep the previous estimate when completion reporting
        // was not prompt; device draining/report completion still happened.
        if !timely_observations
            || !complete
            || !all_reported
            || report_start.elapsed().as_micros() > NO_PROGRESS_US as u128
        {
            return (0, 0);
        }
        // The token total includes every participant. An untimed completion
        // would make a partial duration look like the cost of all those tokens.
        if self.local.reports.iter().any(|report| {
            report.occupied.load(Ordering::Acquire) != 0
                && report.batch_id.load(Ordering::Acquire) == batch
                && report.duration_us.load(Ordering::Relaxed) == 0
        }) {
            return (0, 0);
        }
        let measured = self.aggregate_local_times(batch, consumed);
        if measured.0 > MAX_RUN_US + LOCAL_REPORT_GRACE_MS * 1000 {
            return (0, 0);
        }
        measured
    }

    fn honor_fixed_rest_wait(&mut self) -> bool {
        let Some(my_idx) = self.my_global_idx else {
            return false;
        };
        if let Some(deadline) = self.next_run_not_before.take() {
            while Instant::now() < deadline {
                if !self.update_heartbeat() {
                    return false;
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                );
            }
            let Some(_guard) = self.transaction() else {
                return false;
            };
            self.global.slots[my_idx]
                .is_active
                .store(1, Ordering::Release);
            self.global.signal_counter.fetch_add(1, Ordering::Release);
            futex::wake_all(&self.global.signal_counter);
        }
        true
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

                    if start < global_start {
                        global_start = start;
                    }
                    if end > global_end {
                        global_end = end;
                    }
                    participants += 1;
                }
            }
        }

        if participants == 0 {
            return (0, 0);
        }
        let total_duration = global_end.saturating_sub(global_start);
        // Use the actual tokens consumed (post-scale) so averages are per real token.
        let tokens_used = tokens_for_stats;

        debug!(
            "[Manager-Aggregate] total participants: {}, earliest time: {}, latest time: {}",
            participants, global_start, global_end
        );
        debug!(
            "[Manager-Aggregate] total duration: {} us, consume Tokens: {}",
            total_duration, tokens_used
        );

        (total_duration, tokens_used)
    }

    fn update_local_stats(&mut self, duration_us: u64, tokens: u64) {
        let Some(my_idx) = self.my_global_idx else {
            return;
        };
        if tokens == 0
            || duration_us == 0
            || duration_us > MAX_RUN_US + LOCAL_REPORT_GRACE_MS * 1000
        {
            return;
        }
        let registry = self.registry.clone();
        let Ok(_guard) = registry.lock() else {
            return;
        };
        if !self.is_current() || !self.has_turn() {
            return;
        }
        let new_avg = duration_us / tokens;
        // Clamp swings but allow faster adaptation.
        let lower = (self.current_avg_us / 4).max(1);
        let upper = self.current_avg_us.saturating_mul(4).max(1);
        let clamped_avg = new_avg.clamp(lower, upper);
        let alpha = self.ema_alpha;
        self.current_avg_us =
            ((self.current_avg_us as f64 * (1.0 - alpha)) + (clamped_avg as f64 * alpha)) as u64;

        self.global.slots[my_idx]
            .avg_kernel_time
            .store(self.current_avg_us, Ordering::Relaxed);
        self.global.slots[my_idx]
            .last_heartbeat
            .store(get_time_us(), Ordering::Relaxed);
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

    fn update_heartbeat(&self) -> bool {
        let Some(_guard) = self.transaction() else {
            return false;
        };
        self.heartbeat_locked();
        true
    }

    // Only call while holding the registry transaction. A waiting manager must
    // stay locally alive without extending the current global owner's lifetime.
    fn heartbeat_locked(&self) {
        let now = get_time_us();
        if let Some(my_idx) = self.my_global_idx {
            self.global.slots[my_idx]
                .last_heartbeat
                .store(now, Ordering::Release);
        }
        self.local.manager_heartbeat.store(now, Ordering::Release);
        if self.has_turn() {
            self.global.lock_timestamp.store(now, Ordering::Release);
        }
    }
}

impl Drop for ContainerManager {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let registry = self.registry.clone();
        let Ok(_guard) = registry.lock() else {
            return;
        };
        if self.is_current()
            && let Some(my_idx) = self.my_global_idx
        {
            self.global.slots[my_idx]
                .is_active
                .store(0, Ordering::Release);
            if self.has_turn() {
                self.global
                    .lock_owner
                    .store(MAX_MANAGERS as u32, Ordering::Release);
            }
        }
        if self.owns_local_lease() {
            self.local.overdraft_pool.close();
            self.local.manager_pid.store(0, Ordering::Release);
            self.local.manager_global_idx.store(-1, Ordering::Release);
            self.local.tokens_remaining.store(0, Ordering::Release);
            self.local.state.store(STATE_IDLE, Ordering::Release);
            futex::wake_all(&self.local.state);
        }
        if self.my_global_idx.is_some() {
            self.global.signal_counter.fetch_add(1, Ordering::Release);
            futex::wake_all(&self.global.signal_counter);
        }
    }
}

fn get_time_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}
