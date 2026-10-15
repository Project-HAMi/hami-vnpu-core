use super::*;

fn manager(priority: f64, fixed: bool, average: u64) -> ContainerManager {
    let global = Box::leak(Box::new(unsafe { std::mem::zeroed::<GlobalRegistry>() }));
    let local = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    global.slots[0].is_active.store(1, Ordering::Relaxed);
    global.slots[0].avg_kernel_time.store(average, Ordering::Relaxed);
    ContainerManager { global, local, my_pid: std::process::id() as i32,
        my_global_idx: 0, current_avg_us: average, my_priority: priority,
        token_scale: 100.0, ema_alpha: 0.7, fixed_share_ratio: fixed,
        next_run_not_before: None }
}

#[test]
fn ordinary_budgets_priorities_and_fixed_rest_are_unchanged() {
    for fixed in [false, true] {
        let a = manager(10.0, fixed, 500).calculate_fair_share();
        let b = manager(50.0, fixed, 500).calculate_fair_share();
        assert_eq!((a.tokens, b.tokens), (1000, 5000));
        assert_eq!((a.expected_run_us, b.expected_run_us), (500_000, 2_500_000));
        assert_eq!((a.time_limit_us, b.time_limit_us), (800_000, 3_800_000));
        assert_eq!(a.rest_wait_us, if fixed {4_500_000} else {0});
        assert_eq!(b.rest_wait_us, if fixed {2_500_000} else {0});
    }
}

#[test]
fn saturated_run_budget_cannot_wrap_its_timeout() {
    let m = manager(50.0, false, u64::MAX);
    let plan = m.calculate_fair_share();
    assert_eq!(plan.expected_run_us, u64::MAX);
    assert_eq!(plan.time_limit_us, u64::MAX);
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
