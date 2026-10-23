use super::*;

fn manager(priority: f64, fixed: bool, average: u64) -> ContainerManager {
    let global = Box::leak(Box::new(unsafe { std::mem::zeroed::<GlobalRegistry>() }));
    let local = Box::leak(Box::new(unsafe {
        std::mem::zeroed::<LocalContainerShmem>()
    }));
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "vnpu-manager-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::File::create(&path).unwrap();
    let registry = Arc::new(RegistryLock::open(path.to_str().unwrap()).unwrap());
    std::fs::remove_file(path).unwrap();
    let pid = std::process::id() as i32;
    local.manager_pid.store(pid, Ordering::Relaxed);
    local.manager_generation.store(1, Ordering::Relaxed);
    local
        .manager_heartbeat
        .store(get_time_us(), Ordering::Relaxed);
    global.slots[0].pid.store(pid, Ordering::Relaxed);
    global.slots[0].generation.store(1, Ordering::Relaxed);
    global.slots[0]
        .compute_limit
        .store(priority as u64, Ordering::Relaxed);
    global.slots[0]
        .last_heartbeat
        .store(get_time_us(), Ordering::Relaxed);
    global.slots[0].is_active.store(1, Ordering::Relaxed);
    global.slots[0]
        .avg_kernel_time
        .store(average, Ordering::Relaxed);
    ContainerManager {
        global,
        local,
        my_pid: std::process::id() as i32,
        my_global_idx: 0,
        current_avg_us: average,
        compute_limit: priority,
        token_scale: 100.0,
        ema_alpha: 0.7,
        sched_policy: if fixed {
            SchedPolicy::Fixed
        } else {
            SchedPolicy::Dynamic
        },
        next_run_not_before: None,
        generation: 1,
        registry,
        stopped: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn bounded_overdraft_round_opens_idle_and_next_round_closes_it() {
    let mut m = manager(10.0, false, 500);
    m.sched_policy = SchedPolicy::Overdraft;
    m.local.overdraft_limit.store(10, Ordering::Release);
    let plan = SharePlan {
        tokens: 1,
        time_limit_us: 10_000,
        expected_run_us: 500,
        rest_wait_us: 0,
        anchor_speed_us: 500,
    };
    // Even a round with no submitted work must release normal ownership and
    // open its IDLE allowance when the no-progress deadline expires.
    assert_eq!(m.run_local_round(&plan), (0, 0));
    let local = m.local;
    let acquire = || {
        local.overdraft_pool.try_acquire_idle(
            &local.state,
            &local.batch_id,
            STATE_IDLE,
            &local.overdraft_limit,
            true,
        )
    };
    assert!(acquire());
    assert!(m.start_local_batch(2, 1).is_ok());
    assert!(!acquire());
    // Publishing IDLE alone must not reopen the new round's gate.
    local.state.store(STATE_IDLE, Ordering::Release);
    assert!(!acquire());
    drop(m);
    assert!(!acquire());
}

#[test]
fn replaced_manager_cannot_close_successors_overdraft_allowance() {
    let m = manager(10.0, false, 500);
    let local = m.local;
    local.overdraft_limit.store(10, Ordering::Release);
    local.manager_generation.store(2, Ordering::Release);
    m.global.slots[0].generation.store(2, Ordering::Release);
    let round = local.overdraft_pool.begin_round().unwrap();
    assert!(local.overdraft_pool.open_idle(round));
    assert!(m.start_local_batch(8, 1).is_err());
    drop(m);
    assert!(local.overdraft_pool.try_acquire_idle(
        &local.state,
        &local.batch_id,
        STATE_IDLE,
        &local.overdraft_limit,
        true,
    ));
}

#[test]
fn ordinary_budgets_priorities_and_fixed_rest_are_unchanged() {
    for fixed in [false, true] {
        let a = manager(10.0, fixed, 500).calculate_fair_share();
        let b = manager(50.0, fixed, 500).calculate_fair_share();
        assert_eq!((a.tokens, b.tokens), (1000, 5000));
        assert_eq!((a.expected_run_us, b.expected_run_us), (500_000, 2_500_000));
        assert_eq!((a.time_limit_us, b.time_limit_us), (800_000, 3_800_000));
        assert_eq!(a.rest_wait_us, if fixed { 4_500_000 } else { 0 });
        assert_eq!(b.rest_wait_us, if fixed { 2_500_000 } else { 0 });
    }
}

#[test]
fn saturated_run_budget_is_bounded_instead_of_waiting_for_hours() {
    let m = manager(50.0, false, u64::MAX);
    let plan = m.calculate_fair_share();
    assert_eq!(plan.expected_run_us, MAX_RUN_US);
    assert_eq!(plan.time_limit_us, MAX_RUN_US);
}

fn report(m: &ContainerManager, index: usize, batch: u64, start: u64, duration: u64) {
    let slot = &m.local.reports[index];
    slot.cpu_start_us.store(start, Ordering::Relaxed);
    slot.duration_us.store(duration, Ordering::Relaxed);
    slot.batch_id.store(batch, Ordering::Release);
}

#[test]
fn report_aggregation_keeps_ordinary_interval_and_token_count() {
    let m = manager(50.0, false, 500);
    report(&m, 0, 7, 100, 200);
    report(&m, 1, 7, 200, 300);
    report(&m, 2, 6, 1, 1000);
    assert_eq!(m.aggregate_local_times(7, 13), (400, 13));
    assert_eq!(m.aggregate_local_times(8, 13), (0, 0));
}

#[test]
fn overflowing_report_end_is_clamped_instead_of_disappearing() {
    let m = manager(50.0, false, 500);
    report(&m, 0, 7, u64::MAX - 10, 20);
    assert_eq!(m.aggregate_local_times(7, 13), (10, 13));
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

#[test]
fn polluted_estimates_bound_runtime_report_and_fixed_rest_budgets() {
    for fixed in [false, true] {
        let m = manager(20.0, fixed, 11_069_182);
        let plan = m.calculate_fair_share();
        assert!(plan.expected_run_us <= MAX_RUN_US);
        assert!(plan.time_limit_us <= MAX_RUN_US);
        assert!(plan.rest_wait_us <= MAX_RUN_US);
        assert_eq!(LOCAL_REPORT_GRACE_MS, 500);
    }
}

#[test]
fn capped_common_unit_keeps_relative_priorities() {
    let a = manager(20.0, false, 10_000);
    let b = manager(80.0, false, 10_000);
    a.global.slots[1].is_active.store(1, Ordering::Release);
    a.global.slots[1]
        .avg_kernel_time
        .store(10_000, Ordering::Release);
    a.global.slots[1].compute_limit.store(80, Ordering::Release);
    let a = a.calculate_fair_share();
    let b = b.calculate_fair_share();
    assert_eq!(a.tokens * 4, b.tokens);
    assert_eq!(a.expected_run_us * 4, b.expected_run_us);
}

#[test]
fn partially_consumed_batch_yields_after_progress_stops_without_using_idle_time_as_stats() {
    let m = manager(20.0, false, 11_069_182);
    let plan = SharePlan {
        tokens: 2000,
        time_limit_us: MAX_RUN_US,
        expected_run_us: MAX_RUN_US,
        rest_wait_us: 0,
        anchor_speed_us: 11_069_182,
    };
    let start = Instant::now();
    thread::scope(|scope| {
        let run = scope.spawn(|| m.run_local_round(&plan));
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_RUNNING);
        m.local.active_workers.store(1, Ordering::Release);
        m.local.tokens_remaining.store(1884, Ordering::Release);
        assert_eq!(run.join().unwrap(), (0, 0));
    });
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "waited for the full runtime budget"
    );
    assert_eq!(m.local.state.load(Ordering::Acquire), STATE_IDLE);
    assert_eq!(m.local.tokens_remaining.load(Ordering::Acquire), 0);
}

#[test]
fn completed_reported_batch_still_updates_execution_statistics() {
    let mut m = manager(20.0, false, 500);
    let plan = m.calculate_fair_share();
    let result = thread::scope(|scope| {
        let run = scope.spawn(|| m.run_local_round(&plan));
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_RUNNING);
        let batch = m.local.batch_id.load(Ordering::Acquire);
        m.local.active_workers.store(1, Ordering::Release);
        m.local.reports[0].occupied.store(1, Ordering::Release);
        report(&m, 0, batch, get_time_us(), 2500);
        m.local.reported_count.store(1, Ordering::Release);
        m.local.tokens_remaining.store(0, Ordering::Release);
        run.join().unwrap()
    });
    assert_eq!(result, (2500, plan.tokens));
    m.update_local_stats(result.0, result.1);
    assert!(m.current_avg_us < 500);
}

#[test]
fn missing_and_stale_reports_do_not_count_as_completed_work() {
    let m = manager(20.0, false, 500);
    let plan = m.calculate_fair_share();
    thread::scope(|scope| {
        let run = scope.spawn(|| m.run_local_round(&plan));
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_RUNNING);
        m.local.active_workers.store(1, Ordering::Release);
        m.local.reported_count.store(1, Ordering::Release); // late previous-batch writer
        m.local.reports[0].occupied.store(1, Ordering::Release);
        report(&m, 0, 0, get_time_us(), 10_000_000_000);
        m.local.tokens_remaining.store(0, Ordering::Release);
        assert_eq!(run.join().unwrap(), (0, 0));
    });
    assert_eq!(m.current_avg_us, 500);
}

#[test]
fn waiting_over_takeover_timeout_keeps_local_lease_without_refreshing_other_owner() {
    let m = manager(20.0, false, 500);
    m.global.slots[1].is_active.store(1, Ordering::Release);
    m.global.lock_owner.store(1, Ordering::Release);
    m.global.lock_timestamp.store(u64::MAX, Ordering::Release);
    thread::scope(|scope| {
        let run = scope.spawn(|| m.wait_for_global_turn());
        thread::sleep(Duration::from_millis(3150));
        assert!(
            get_time_us().saturating_sub(m.local.manager_heartbeat.load(Ordering::Acquire))
                < 500_000
        );
        assert_eq!(m.global.lock_timestamp.load(Ordering::Acquire), u64::MAX);
        // Stop the other owner's heartbeat; the global watchdog still works.
        m.global.lock_timestamp.store(0, Ordering::Release);
        assert!(run.join().unwrap());
    });
    assert_eq!(m.global.lock_owner.load(Ordering::Acquire), 0);
    assert_eq!(m.global.slots[1].is_active.load(Ordering::Acquire), 0);
}

#[test]
fn replaced_manager_cannot_reset_successor_state_or_reused_slot_even_on_drop() {
    let mut m = manager(20.0, false, 500);
    let local = m.local;
    let global = m.global;
    {
        let _guard = m.registry.lock().unwrap();
        local.manager_generation.store(2, Ordering::Release);
        global.slots[0].generation.store(2, Ordering::Release);
        local.state.store(STATE_RUNNING, Ordering::Release);
        local.batch_id.store(9, Ordering::Release);
        local.tokens_remaining.store(123, Ordering::Release);
    }
    assert!(!m.update_heartbeat());
    assert!(m.start_local_batch(10, 999).is_err());
    assert!(!m.pass_baton());
    assert!(!m.wait_for_global_turn());
    m.update_local_stats(10_000, 100);
    drop(m);
    assert_eq!(local.state.load(Ordering::Acquire), STATE_RUNNING);
    assert_eq!(local.batch_id.load(Ordering::Acquire), 9);
    assert_eq!(local.tokens_remaining.load(Ordering::Acquire), 123);
    assert_eq!(local.manager_generation.load(Ordering::Acquire), 2);
    assert_eq!(global.slots[0].is_active.load(Ordering::Acquire), 1);
}

#[test]
fn rest_releases_global_turn_and_preserves_other_owners_heartbeat() {
    let mut m = manager(20.0, true, 500);
    m.global.slots[1].is_active.store(1, Ordering::Release);
    m.global.slots[1]
        .last_heartbeat
        .store(get_time_us(), Ordering::Release);
    m.schedule_rest_wait(50_000);
    assert!(m.pass_baton());
    assert_eq!(m.global.lock_owner.load(Ordering::Acquire), 1);
    assert_eq!(m.global.slots[0].is_active.load(Ordering::Acquire), 2);
    let other_heartbeat = m.global.lock_timestamp.load(Ordering::Acquire);
    assert!(m.honor_fixed_rest_wait());
    assert_eq!(
        m.global.lock_timestamp.load(Ordering::Acquire),
        other_heartbeat
    );
    assert_eq!(m.global.slots[0].is_active.load(Ordering::Acquire), 1);
}

#[test]
fn handoff_ignores_duplicate_old_queue_entries_and_skips_dead_slots() {
    let m = manager(20.0, false, 500);
    for entry in &m.global.queue {
        entry.store(0, Ordering::Relaxed);
    }
    m.global.queue_head.store(u32::MAX, Ordering::Relaxed);
    m.global.queue_tail.store(u32::MAX, Ordering::Relaxed);
    m.global.slots[1].is_active.store(1, Ordering::Release);
    m.global.slots[1].last_heartbeat.store(0, Ordering::Release);
    m.global.slots[2].is_active.store(1, Ordering::Release);
    m.global.slots[2]
        .last_heartbeat
        .store(get_time_us(), Ordering::Release);
    for _ in 0..128 {
        m.global.lock_owner.store(0, Ordering::Release);
        assert!(m.pass_baton());
        assert_eq!(m.global.lock_owner.load(Ordering::Acquire), 2);
    }
    assert_eq!(m.global.slots[1].is_active.load(Ordering::Acquire), 0);
    assert_eq!(m.global.queue_tail.load(Ordering::Acquire), u32::MAX);
}

#[test]
fn delayed_report_after_token_exhaustion_cannot_train_average() {
    let mut m = manager(20.0, false, 500);
    let plan = m.calculate_fair_share();
    let result = thread::scope(|scope| {
        let run = scope.spawn(|| m.run_local_round(&plan));
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_RUNNING);
        let batch = m.local.batch_id.load(Ordering::Acquire);
        m.local.active_workers.store(1, Ordering::Release);
        m.local.reports[0].occupied.store(1, Ordering::Release);
        m.local.tokens_remaining.store(0, Ordering::Release);
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_MEASURING);
        // The next request/hook arrives within the 500ms completion grace, but
        // its event interval includes an idle gap after the last admission.
        thread::sleep(Duration::from_millis(40));
        report(&m, 0, batch, get_time_us() - 40_000, 40_000);
        m.local.reported_count.store(1, Ordering::Release);
        run.join().unwrap()
    });
    assert_eq!(result, (0, 0));
    m.update_local_stats(result.0, result.1);
    assert_eq!(m.current_avg_us, 500);
}

#[test]
fn mixed_timed_and_untimed_workers_do_not_train_a_partial_average() {
    let m = manager(20.0, false, 500);
    let plan = m.calculate_fair_share();
    thread::scope(|scope| {
        let run = scope.spawn(|| m.run_local_round(&plan));
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_RUNNING);
        let batch = m.local.batch_id.load(Ordering::Acquire);
        m.local.active_workers.store(2, Ordering::Release);
        for slot in &m.local.reports[..2] {
            slot.occupied.store(1, Ordering::Release);
        }
        report(&m, 0, batch, get_time_us(), 2500);
        report(&m, 1, batch, get_time_us(), 0);
        m.local.tokens_remaining.store(0, Ordering::Release);
        assert_eq!(run.join().unwrap(), (0, 0));
    });
}

#[test]
fn manager_observation_gap_before_measuring_cannot_train_idle_time() {
    let m = manager(20.0, true, 500);
    let plan = SharePlan {
        tokens: 2000,
        expected_run_us: MAX_RUN_US,
        time_limit_us: MAX_RUN_US,
        rest_wait_us: 0,
        anchor_speed_us: 500,
    };
    thread::scope(|scope| {
        let run = scope.spawn(|| m.run_local_round(&plan));
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_RUNNING);
        let batch = m.local.batch_id.load(Ordering::Acquire);
        {
            // Delay the next real manager observation, as if it was descheduled
            // after a worker consumed the final token but before MEASURING.
            let _guard = m.registry.lock().unwrap();
            m.local.active_workers.store(1, Ordering::Release);
            m.local.reports[0].occupied.store(1, Ordering::Release);
            m.local.tokens_remaining.store(0, Ordering::Release);
            thread::sleep(Duration::from_millis(40));
        }
        wait_until(|| m.local.state.load(Ordering::Acquire) == STATE_MEASURING);
        report(&m, 0, batch, get_time_us() - 40_000, 40_000);
        assert_eq!(run.join().unwrap(), (0, 0));
    });
}
