use super::*;

fn manager(compute_limit: f64, fixed: bool, average: u64) -> ContainerManager {
    let global = Box::leak(Box::new(unsafe { std::mem::zeroed::<GlobalRegistry>() }));
    let local = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    global.slots[0].is_active.store(1, Ordering::Relaxed);
    global.slots[0].avg_kernel_time.store(average, Ordering::Relaxed);
    ContainerManager { global, local, my_pid: std::process::id() as i32,
        my_global_idx: Some(0), current_avg_us: average, compute_limit,
        token_scale: 100.0, ema_alpha: 0.7,
        sched_policy: if fixed { SchedPolicy::Fixed } else { SchedPolicy::Dynamic },
        physical_device_id: None, next_run_not_before: None }
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

fn global_snapshot(global: &GlobalRegistry) -> Vec<u64> {
    let mut values = vec![global.lock_owner.load(Ordering::Acquire) as u64,
        global.lock_timestamp.load(Ordering::Acquire),
        global.queue_head.load(Ordering::Acquire) as u64,
        global.queue_tail.load(Ordering::Acquire) as u64,
        global.signal_counter.load(Ordering::Acquire) as u64];
    values.extend(global.queue.iter().map(|value| value.load(Ordering::Acquire) as u64));
    for slot in &global.slots {
        values.extend([slot.pid.load(Ordering::Acquire) as u64,
            slot.avg_kernel_time.load(Ordering::Acquire), slot.last_heartbeat.load(Ordering::Acquire),
            slot.is_active.load(Ordering::Acquire) as u64, slot.compute_limit.load(Ordering::Acquire)]);
    }
    values
}

#[test]
fn spatial_memory_manager_does_not_register_or_change_another_pods_compute_turn() {
    let global = Box::leak(Box::new(unsafe { std::mem::zeroed::<GlobalRegistry>() }));
    let local = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    // Even a full compute registry must not prevent spatial memory management.
    for (index, slot) in global.slots.iter().enumerate() {
        slot.is_active.store(1, Ordering::Relaxed);
        slot.pid.store(index as i32 + 100, Ordering::Relaxed);
        slot.avg_kernel_time.store(1000 + index as u64, Ordering::Relaxed);
    }
    global.lock_owner.store(5, Ordering::Relaxed);
    global.lock_timestamp.store(123, Ordering::Relaxed);
    global.queue_head.store(7, Ordering::Relaxed);
    global.queue_tail.store(12, Ordering::Relaxed);
    global.signal_counter.store(14, Ordering::Relaxed);
    let before = global_snapshot(global);
    local.initialized.store(1, Ordering::Relaxed);
    local.memory_used.store(4096, Ordering::Relaxed);
    local.manager_pid.store(std::process::id() as i32, Ordering::Relaxed);
    let round = local.overdraft_pool.begin_round().unwrap();
    assert!(local.overdraft_pool.open_idle(round));
    let config = ManagerConfig { global_shm_path: String::new(), local_shm_path: String::new(),
        compute_limit: 33, memory_limit_mb: 20480, sched_policy: SchedPolicy::Spatial,
        physical_device_id: None,
        overdraft: crate::config::OverdraftConfig { limit: 10, timeout_us: 5000, poll_interval_us: 1000 } };
    let mut m = ContainerManager::new(global, local, std::process::id() as i32, config).unwrap();
    assert_eq!(m.my_global_idx, None);
    assert_eq!(local.manager_global_idx.load(Ordering::Acquire), -1);
    assert_eq!(local.memory_limit.load(Ordering::Acquire), 20480 * MB_TO_BYTES);
    assert_eq!(local.memory_used.load(Ordering::Acquire), 4096);
    assert_eq!(local.initialized.load(Ordering::Acquire), 1);
    assert_eq!(local.overdraft_limit.load(Ordering::Acquire), 0);
    assert!(!local.overdraft_pool.try_acquire(local.overdraft_pool.snapshot(), 10, true));
    m.join_global_queue();
    m.wait_for_global_turn();
    m.pass_baton();
    m.update_local_stats(1000, 1);
    m.update_heartbeat();
    assert!(local.manager_heartbeat.load(Ordering::Acquire) > 0);
    assert_eq!(global_snapshot(global), before);
    drop(m);
    assert_eq!(global_snapshot(global), before, "memory-only Drop must not signal or clear compute slots");
}

#[test]
fn spatial_and_dynamic_can_share_a_registry_without_spatial_joining_rotation() {
    let global = Box::leak(Box::new(unsafe { std::mem::zeroed::<GlobalRegistry>() }));
    let dynamic_local = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    let spatial_local = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    let config = ManagerConfig { global_shm_path: String::new(), local_shm_path: String::new(),
        compute_limit: 33, memory_limit_mb: 20480, sched_policy: SchedPolicy::Dynamic,
        physical_device_id: None,
        overdraft: crate::config::OverdraftConfig { limit: 10, timeout_us: 5000, poll_interval_us: 1000 } };
    let dynamic = ContainerManager::new(global, dynamic_local, 1001, config.clone()).unwrap();
    let spatial = ContainerManager::new(global, spatial_local, 1002,
        ManagerConfig { sched_policy: SchedPolicy::Spatial, ..config }).unwrap();
    assert_eq!(dynamic.my_global_idx, Some(0));
    assert_eq!(spatial.my_global_idx, None);
    assert_eq!(global.slots.iter().filter(|slot| slot.is_active.load(Ordering::Acquire) == 1).count(), 1);
    spatial.join_global_queue();
    spatial.wait_for_global_turn();
    spatial.pass_baton();
    assert_eq!(global.queue_tail.load(Ordering::Acquire), 0);
    dynamic.join_global_queue();
    dynamic.wait_for_global_turn();
    dynamic.pass_baton();
    assert_eq!(global.lock_owner.load(Ordering::Acquire), 0);
    assert_eq!(global.queue_tail.load(Ordering::Acquire), 2);
    let before = global_snapshot(global);
    spatial.update_heartbeat();
    drop(spatial);
    assert_eq!(global_snapshot(global), before);
    assert_eq!(global.slots[0].pid.load(Ordering::Acquire), 1001);
    assert_eq!(global.slots[0].is_active.load(Ordering::Acquire), 1);
}

#[test]
fn physical_card_domains_keep_registry_quota_and_allowance_independent() {
    let directory = std::env::temp_dir().join(format!(
        "vnpu-card-domains-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    let base = ManagerConfig {
        global_shm_path: directory.join("global").to_str().unwrap().to_owned(),
        local_shm_path: directory.join("local").to_str().unwrap().to_owned(),
        physical_device_id: None,
        compute_limit: 30,
        memory_limit_mb: 1024,
        sched_policy: SchedPolicy::Overdraft,
        overdraft: crate::config::OverdraftConfig {
            limit: 10, timeout_us: 5000, poll_interval_us: 1000,
        },
    };
    let a = base.for_device(7);
    let b = base.for_device(3);
    let global_a = crate::shmem::setup::open_global_registry(&a.global_shm_path);
    let global_b = crate::shmem::setup::open_global_registry(&b.global_shm_path);
    let local_a = crate::shmem::setup::create_shmem::<LocalContainerShmem>(&a.local_shm_path);
    let local_b = crate::shmem::setup::create_shmem::<LocalContainerShmem>(&b.local_shm_path);
    let pid = std::process::id() as i32;
    let manager_a = ContainerManager::new(global_a, local_a, pid, a.clone()).unwrap();
    let manager_b = ContainerManager::new(global_b, local_b, pid, b).unwrap();
    assert_eq!(manager_a.my_global_idx, Some(0));
    assert_eq!(manager_b.my_global_idx, Some(0));
    assert_eq!(manager_a.physical_device_id, Some(7));
    assert_eq!(manager_b.physical_device_id, Some(3));
    assert_eq!(local_a.memory_limit.load(Ordering::Acquire), 1024 * MB_TO_BYTES);
    assert_eq!(local_b.memory_limit.load(Ordering::Acquire), 1024 * MB_TO_BYTES);

    let other_registry_before = global_snapshot(global_b);
    local_a.memory_used.store(1024 * MB_TO_BYTES, Ordering::Release);
    local_a.tokens_remaining.store(17, Ordering::Release);
    local_a.state.store(STATE_RUNNING, Ordering::Release);
    local_a.procs[0].pid.store(pid, Ordering::Release);
    local_a.procs[0].hbm_used[0].store(1024 * MB_TO_BYTES, Ordering::Release);
    let round = local_a.overdraft_pool.begin_round().unwrap();
    assert!(local_a.overdraft_pool.open_idle(round));
    manager_a.join_global_queue();
    manager_a.wait_for_global_turn();
    manager_a.pass_baton();
    assert_eq!(global_snapshot(global_b), other_registry_before);
    assert_eq!(local_b.memory_used.load(Ordering::Acquire), 0);
    assert_eq!(local_b.tokens_remaining.load(Ordering::Acquire), 0);
    assert_eq!(local_b.state.load(Ordering::Acquire), STATE_IDLE);
    assert!(!local_b.overdraft_pool.try_acquire(local_b.overdraft_pool.snapshot(), 10, true));

    // A later client mapping the same card observes its live ledger; another card
    // cannot reset it merely because both manager threads have the same PID.
    let reopened = crate::shmem::setup::create_shmem::<LocalContainerShmem>(&a.local_shm_path);
    assert_eq!(reopened.memory_used.load(Ordering::Acquire), 1024 * MB_TO_BYTES);
    assert_eq!(reopened.procs[0].hbm_used[0].load(Ordering::Acquire), 1024 * MB_TO_BYTES);
    assert_eq!(reopened.tokens_remaining.load(Ordering::Acquire), 17);
    drop(manager_a);
    assert_eq!(global_snapshot(global_b), other_registry_before);
    assert_eq!(global_b.slots[0].is_active.load(Ordering::Acquire), 1);
    drop(manager_b);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dcmi_observation_uses_physical_mapping_without_rewriting_hook_reservations() {
    let local = unsafe { std::mem::zeroed::<LocalContainerShmem>() };
    local.procs[0].pid.store(1234, Ordering::Release);
    local.procs[0].hbm_used[0].store(80, Ordering::Release);
    local.procs[0].hbm_used[1].store(12, Ordering::Release);
    local.memory_used.store(80, Ordering::Release);
    let observed = observe_physical_device(&local, 7,
        |physical| {
            assert_eq!(physical, 7);
            Some((42, 2))
        },
        |card, chip| {
            assert_eq!((card, chip), (42, 2));
            Some(HashMap::from([(1234, 999), (4321, 500)]))
        });
    assert_eq!(observed, Some(1));
    assert_eq!(local.memory_used.load(Ordering::Acquire), 80);
    assert_eq!(local.procs[0].hbm_used[0].load(Ordering::Acquire), 80);
    assert_eq!(local.procs[0].hbm_used[1].load(Ordering::Acquire), 12);
}

#[test]
fn missing_dcmi_mapping_or_query_does_not_fall_back_to_another_card() {
    let local = unsafe { std::mem::zeroed::<LocalContainerShmem>() };
    local.memory_used.store(137, Ordering::Release);
    assert_eq!(observe_physical_device(&local, 7,
        |_| None,
        |_, _| panic!("a missing physical mapping must not query logical card zero")), None);
    assert_eq!(observe_physical_device(&local, 7,
        |_| Some((42, 2)),
        |card, chip| { assert_eq!((card, chip), (42, 2)); None }), None);
    assert_eq!(local.memory_used.load(Ordering::Acquire), 137);
}
