use super::*;
use crate::procfs_test_double::{self as proc_mock, Answer};
const MIB: u64 = 1024 * 1024;

fn client(limit: u64, used: u64) -> SchedulerClient {
    let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    shmem.memory_limit.store(limit, Ordering::Relaxed);
    shmem.state.store(STATE_RUNNING, Ordering::Relaxed);
    shmem.batch_id.store(19, Ordering::Relaxed);
    shmem.tokens_remaining.store(137, Ordering::Relaxed);
    shmem.compute_limit.store(50, Ordering::Relaxed);
    set_slot(shmem, 0, std::process::id() as i32, used);
    // Start with the production constructor's lock type so this independent
    // suite also composes with the worker read-lock PR.
    let mut client = SchedulerClient::stub();
    let inner = Arc::get_mut(&mut client.inner).unwrap();
    inner.shmem = shmem;
    inner.is_stub = false;
    client
}
fn set_slot(shmem: &LocalContainerShmem, index: usize, pid: i32, used: u64) {
    let slot = &shmem.procs[index];
    slot.pid.store(pid, Ordering::Relaxed);
    slot.is_active.store(1, Ordering::Relaxed);
    let previous = slot.hbm_used[0].swap(used, Ordering::Relaxed);
    shmem.memory_used.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |total| {
        Some(if used >= previous { total.saturating_add(used - previous) }
             else { total.saturating_sub(previous - used) })
    }).unwrap();
}
fn assert_compute_unchanged(shmem: &LocalContainerShmem) {
    assert_eq!(STATE_RUNNING, shmem.state.load(Ordering::Acquire));
    assert_eq!(19, shmem.batch_id.load(Ordering::Acquire));
    assert_eq!(137, shmem.tokens_remaining.load(Ordering::Acquire));
    assert_eq!(50, shmem.compute_limit.load(Ordering::Acquire));
}
fn me() -> i32 { std::process::id() as i32 }
fn other(offset: i32) -> i32 { me().checked_add(100_000 + offset).unwrap() }

#[test]
fn spatial_compute_bypass_preserves_memory_quota_reservation_and_rejection() {
    let mut c = client(1000, 600);
    Arc::get_mut(&mut c.inner).unwrap().sched_policy = SchedPolicy::Spatial;
    c.wait_for_token(99);
    assert!(c.is_hbm_limited());
    assert_eq!(c.check_memory_quota(401), RT_ERROR_MEMORY_ALLOCATION);
    assert_eq!(c.inner.shmem.memory_used.load(Ordering::Acquire), 600);
    assert_eq!(c.check_memory_quota(400), 0);
    assert_eq!(c.inner.shmem.memory_used.load(Ordering::Acquire), 1000);
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn self_proc_error_is_not_queried_or_misreaped() {
    let c = client(100, 40);
    proc_mock::set(&[(me(), Answer::Unreadable)]);
    assert_eq!(40, c.recalculate_usage());
    assert!(proc_mock::calls().is_empty(), "self /proc test-double must not be called");
    assert_eq!(me(), c.inner.shmem.procs[0].pid.load(Ordering::Acquire));
    assert_eq!(1, c.inner.shmem.procs[0].is_active.load(Ordering::Acquire));
    assert_eq!(40, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(40, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn other_live_zombie_and_unreadable_pids_keep_existing_checks() {
    let c = client(1000, 60);
    set_slot(c.inner.shmem, 0, me(), 10);
    set_slot(c.inner.shmem, 1, other(1), 20);
    set_slot(c.inner.shmem, 2, other(2), 30);
    set_slot(c.inner.shmem, 3, other(3), 40);
    c.inner.shmem.memory_used.store(100, Ordering::Release);
    proc_mock::set(&[(me(), Answer::Unreadable), (other(1), Answer::Live),
                    (other(2), Answer::Zombie), (other(3), Answer::Unreadable)]);
    assert_eq!(30, c.recalculate_usage());
    assert_eq!(vec![other(1), other(2), other(3)], proc_mock::calls());
    assert_eq!(30, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(other(1), c.inner.shmem.procs[1].pid.load(Ordering::Acquire));
    for i in [2, 3] {
        assert_eq!(0, c.inner.shmem.procs[i].pid.load(Ordering::Acquire));
        assert_eq!(0, c.inner.shmem.procs[i].is_active.load(Ordering::Acquire));
        assert_eq!(0, c.inner.shmem.procs[i].hbm_used[0].load(Ordering::Acquire));
    }
    proc_mock::set(&[(other(1), Answer::Live)]);
    assert_eq!(30, c.recalculate_usage());
    assert_eq!(vec![other(1)], proc_mock::calls()); // Reaped/pid0 slots are not read or refunded twice.
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn exemption_uses_current_pid_not_cached_process_slot_index() {
    let c = client(100, 50);
    set_slot(c.inner.shmem, 0, other(9), 33); // my_proc_idx still points here.
    set_slot(c.inner.shmem, 1, me(), 17);
    proc_mock::set(&[(other(9), Answer::Zombie), (me(), Answer::Unreadable)]);
    assert_eq!(17, c.recalculate_usage());
    assert_eq!(vec![other(9)], proc_mock::calls());
    assert_eq!(0, c.inner.shmem.procs[0].pid.load(Ordering::Acquire));
    assert_eq!(me(), c.inner.shmem.procs[1].pid.load(Ordering::Acquire));
    assert_eq!(17, c.inner.shmem.memory_used.load(Ordering::Acquire));
}

#[test]
fn memory_query_preserves_pending_reservations_and_128mib_overhead() {
    let c = client(512 * MIB, 200 * MIB);
    set_slot(c.inner.shmem, 1, other(1), 28 * MIB);
    c.inner.shmem.memory_used.store(300 * MIB, Ordering::Release); // Includes pending allocations absent from handles.
    proc_mock::set(&[(me(), Answer::Unreadable), (other(1), Answer::Live)]);
    let (mut free, mut total) = (usize::MAX, usize::MAX);
    unsafe { c.get_hbm_info(&mut free, &mut total) };
    assert_eq!(512 * MIB as usize, total);
    assert_eq!(84 * MIB as usize, free);
    assert_eq!(300 * MIB, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(vec![other(1)], proc_mock::calls());
    c.inner.shmem.memory_used.store(498 * MIB, Ordering::Release);
    unsafe { c.get_hbm_info(&mut free, &mut total) };
    assert_eq!(512 * MIB as usize, total);
    assert_eq!(0, free); // 14MiB logical free still reports zero after 128MiB overhead.
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn exact_quota_zero_size_failure_and_free_rules_are_unchanged() {
    let c = client(100, 99);
    proc_mock::set(&[(me(), Answer::Unreadable)]);
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(1));
    c.post_alloc_hbm(42, 1, RT_ERROR_NONE);
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(0));
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, c.check_memory_quota(1));
    assert_eq!(100, c.inner.shmem.memory_used.load(Ordering::Acquire));
    c.post_free_hbm(42, RT_ERROR_MEMORY_ALLOCATION);
    assert_eq!(100, c.inner.shmem.memory_used.load(Ordering::Acquire));
    c.post_free_hbm(42, RT_ERROR_NONE);
    assert_eq!(99, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(1));
    c.post_alloc_hbm(0, 1, RT_ERROR_MEMORY_ALLOCATION);
    assert_eq!(99, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert!(proc_mock::calls().is_empty());
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn unlimited_quota_does_not_gain_scans_or_reservations() {
    let c = client(0, u64::MAX);
    proc_mock::set(&[(me(), Answer::Unreadable)]);
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(u64::MAX));
    assert_eq!(u64::MAX, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert!(proc_mock::calls().is_empty());
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn lost_reap_cas_preserves_replacement_owner_and_slot_memory() {
    let c = client(100, 10);
    let dead = other(1);
    let replacement = other(2);
    set_slot(c.inner.shmem, 1, dead, 20);
    proc_mock::set(&[(dead, Answer::Zombie)]);
    proc_mock::replace_pid_on_read(dead, &c.inner.shmem.procs[1].pid, replacement);
    // Main skips this racing slot during this scan after losing its CAS.
    assert_eq!(30, c.recalculate_usage());
    let slot = &c.inner.shmem.procs[1];
    assert_eq!(replacement, slot.pid.load(Ordering::Acquire));
    assert_eq!(1, slot.is_active.load(Ordering::Acquire));
    assert_eq!(20, slot.hbm_used[0].load(Ordering::Acquire));
    proc_mock::set(&[(replacement, Answer::Live)]);
    assert_eq!(30, c.recalculate_usage());
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn per_card_domains_have_independent_quota_and_accept_large_physical_ids() {
    let mut first = client(100, 0);
    let mut second = client(200, 0);
    Arc::get_mut(&mut first.inner).unwrap().physical_id = 17;
    Arc::get_mut(&mut second.inner).unwrap().physical_id = 99;
    proc_mock::set(&[]);
    assert_eq!(0, first.check_memory_quota(100));
    first.post_alloc_hbm(1, 100, RT_ERROR_NONE);
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, first.check_memory_quota(1));
    assert_eq!(0, second.check_memory_quota(150));
    second.post_alloc_hbm(2, 150, RT_ERROR_NONE);
    assert_eq!(100, first.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(150, second.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(100, first.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(150, second.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert!(first.owns_hbm_handle(1));
    assert!(!second.owns_hbm_handle(1));
    first.post_free_hbm(1, RT_ERROR_NONE);
    assert_eq!(0, first.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(150, second.inner.shmem.memory_used.load(Ordering::Acquire));
}

#[test]
fn reusing_live_process_slot_preserves_its_card_reservations() {
    let c = client(100, 40);
    assert_eq!(0, SchedulerClient::register_proc_slot(c.inner.shmem, me()));
    assert_eq!(40, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(40, c.inner.shmem.memory_used.load(Ordering::Acquire));
}

#[test]
fn pending_allocation_is_charged_before_handle_publication_and_cannot_be_erased() {
    let c = client(100, 0);
    assert_eq!(0, c.check_memory_quota(80));
    assert_eq!(80, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, c.check_memory_quota(21));
    let (mut free, mut total) = (0, 0);
    unsafe { c.get_hbm_info(&mut free, &mut total) };
    assert_eq!(100, total);
    assert_eq!(80, c.inner.shmem.memory_used.load(Ordering::Acquire));
    c.post_alloc_hbm(0, 80, RT_ERROR_MEMORY_ALLOCATION);
    assert_eq!(0, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(0, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
}

#[test]
fn concurrent_pending_allocations_never_overbook_one_card() {
    let c = client(100, 0);
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let threads: Vec<_> = (0..16).map(|_| {
        let c = c.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            c.check_memory_quota(11) == 0
        })
    }).collect();
    barrier.wait();
    let admitted = threads.into_iter().map(|thread| thread.join().unwrap()).filter(|admitted| *admitted).count();
    assert_eq!(9, admitted);
    assert_eq!(99, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(99, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
}

#[test]
fn stub_does_not_gain_quota_checks_or_proc_reads() {
    let c = SchedulerClient::stub();
    proc_mock::set(&[]);
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(u64::MAX));
    assert!(proc_mock::calls().is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn real_fork_refreshes_self_pid_without_exempting_inherited_parent_slot() {
    let mut c = client(100, 40);
    let size = std::mem::size_of::<LocalContainerShmem>();
    let raw = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED | libc::MAP_ANONYMOUS, -1, 0) };
    assert_ne!(raw, libc::MAP_FAILED);
    unsafe { std::ptr::copy_nonoverlapping(c.inner.shmem as *const LocalContainerShmem,
        raw.cast::<LocalContainerShmem>(), 1); }
    let shared = unsafe { &*raw.cast::<LocalContainerShmem>() };
    Arc::get_mut(&mut c.inner).unwrap().shmem = shared;
    let parent = me();
    proc_mock::set(&[(parent, Answer::Live)]);
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        unsafe { libc::alarm(3); }
        let child_pid = me();
        set_slot(shared, 1, child_pid, 7);
        // c.inner.my_proc_idx still denotes the parent's slot. Each scan must
        // query the parent but must not read /proc for the executing child.
        let total = c.recalculate_usage();
        let ok = total == 47 && proc_mock::calls() == vec![parent]
            && shared.procs[0].pid.load(Ordering::Acquire) == parent
            && shared.procs[1].pid.load(Ordering::Acquire) == child_pid;
        unsafe { libc::_exit(if ok { 0 } else { 41 }); }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if result == child { break; }
        assert_eq!(0, result);
        if std::time::Instant::now() >= deadline {
            unsafe { libc::kill(child, libc::SIGKILL); libc::waitpid(child, &mut status, 0); }
            panic!("fork child exceeded bounded timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(libc::WIFEXITED(status));
    assert_eq!(0, libc::WEXITSTATUS(status));
    assert_eq!(parent, shared.procs[0].pid.load(Ordering::Acquire));
    assert_eq!(47, shared.memory_used.load(Ordering::Acquire));
    assert_compute_unchanged(shared);
    // Shared mapping remains live while SchedulerClient is dropped. The child
    // used _exit and never ran the inherited client's destructor.
    drop(c);
    assert_eq!(0, unsafe { libc::munmap(raw, size) });
}

#[test]
fn memory_abi_preserves_original_prefix_and_appends_overdraft_fields() {
    assert_eq!(6256, std::mem::size_of::<LocalContainerShmem>());
    assert_eq!(1080, std::mem::offset_of!(LocalContainerShmem, procs));
    assert_eq!(6224, std::mem::offset_of!(LocalContainerShmem, overdraft_pool));
}

#[test]
fn overflowing_reservation_is_rejected_without_changing_usage() {
    let c = client(u64::MAX, u64::MAX - 4);
    proc_mock::set(&[(me(), Answer::Unreadable)]);
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, c.check_memory_quota(5));
    assert_eq!(u64::MAX - 4, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(4));
    assert_eq!(u64::MAX, c.inner.shmem.memory_used.load(Ordering::Acquire));
}

#[test]
fn overflowing_live_slot_sum_stays_exhausted() {
    let c = client(u64::MAX, u64::MAX - 4);
    set_slot(c.inner.shmem, 1, other(1), 10);
    proc_mock::set(&[(other(1), Answer::Live)]);
    assert_eq!(u64::MAX, c.recalculate_usage());
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, c.check_memory_quota(1));
    assert_eq!(u64::MAX, c.inner.shmem.memory_used.load(Ordering::Acquire));
}

#[test]
fn invalid_device_scan_keeps_slots_and_container_usage_unchanged() {
    let c = client(100, 40);
    proc_mock::set(&[(me(), Answer::Unreadable)]);
    assert_eq!(0, c.recalculate_usage_for_device(shmem::NPU_DEVICE_MAX));
    assert_eq!(40, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(40, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(me(), c.inner.shmem.procs[0].pid.load(Ordering::Acquire));
    assert!(proc_mock::calls().is_empty());
}


#[test]
fn equal_numeric_virtual_and_physical_handles_retain_separate_ownership() {
    let c = client(1000, 0);
    assert_eq!(0, c.check_memory_quota(100));
    c.post_alloc_hbm(42, 100, RT_ERROR_NONE);
    assert_eq!(0, c.check_memory_quota(200));
    c.post_alloc_hbm_kind(42, 200, RT_ERROR_NONE, AllocationKind::Physical);
    assert!(c.owns_hbm_handle(42));
    assert!(c.owns_hbm_handle_kind(42, AllocationKind::Physical));
    assert_eq!(300, c.inner.shmem.memory_used.load(Ordering::Acquire));
    c.post_free_hbm_kind(42, RT_ERROR_NONE, AllocationKind::Physical);
    assert!(!c.owns_hbm_handle_kind(42, AllocationKind::Physical));
    assert!(c.owns_hbm_handle(42));
    assert_eq!(100, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(100, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    c.post_free_hbm(42, RT_ERROR_NONE);
    assert_eq!(0, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(0, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
}
