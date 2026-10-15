use super::*;
use crate::procfs_test_double::{self as proc_mock, Answer};
const MIB: u64 = 1024 * 1024;

fn client(limit: u64, used: u64) -> SchedulerClient {
    let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    shmem.memory_limit.store(limit, Ordering::Relaxed);
    shmem.memory_used.store(used, Ordering::Relaxed);
    shmem.state.store(STATE_RUNNING, Ordering::Relaxed);
    shmem.batch_id.store(19, Ordering::Relaxed);
    shmem.tokens_remaining.store(137, Ordering::Relaxed);
    shmem.compute_priority.store(50, Ordering::Relaxed);
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
    slot.hbm_used[0].store(used, Ordering::Relaxed);
}
fn assert_compute_unchanged(shmem: &LocalContainerShmem) {
    assert_eq!(STATE_RUNNING, shmem.state.load(Ordering::Acquire));
    assert_eq!(19, shmem.batch_id.load(Ordering::Acquire));
    assert_eq!(137, shmem.tokens_remaining.load(Ordering::Acquire));
    assert_eq!(50, shmem.compute_priority.load(Ordering::Acquire));
}
fn me() -> i32 { std::process::id() as i32 }
fn other(offset: i32) -> i32 { me().checked_add(100_000 + offset).unwrap() }

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
fn memory_query_preserves_reconciliation_and_128mib_overhead() {
    let c = client(512 * MIB, 200 * MIB);
    set_slot(c.inner.shmem, 1, other(1), 28 * MIB);
    c.inner.shmem.memory_used.store(1, Ordering::Release); // Existing reconciliation must still run.
    proc_mock::set(&[(me(), Answer::Unreadable), (other(1), Answer::Live)]);
    let (mut free, mut total) = (usize::MAX, usize::MAX);
    c.get_hbm_info(&mut free, &mut total);
    assert_eq!(512 * MIB as usize, total);
    assert_eq!(156 * MIB as usize, free);
    assert_eq!(228 * MIB, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(vec![other(1)], proc_mock::calls());
    c.inner.shmem.procs[0].hbm_used[0].store(470 * MIB, Ordering::Release);
    c.get_hbm_info(&mut free, &mut total);
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
    assert_eq!(10, c.recalculate_usage());
    let slot = &c.inner.shmem.procs[1];
    assert_eq!(replacement, slot.pid.load(Ordering::Acquire));
    assert_eq!(1, slot.is_active.load(Ordering::Acquire));
    assert_eq!(20, slot.hbm_used[0].load(Ordering::Acquire));
    proc_mock::set(&[(replacement, Answer::Live)]);
    assert_eq!(30, c.recalculate_usage());
    assert_compute_unchanged(c.inner.shmem);
}

#[test]
fn selected_device_scan_reconciles_all_devices_and_reaps_all_counters() {
    let c = client(1000, 40);
    let shmem = c.inner.shmem;
    shmem.procs[0].hbm_used[1].store(70, Ordering::Release);
    set_slot(shmem, 1, other(1), 20);
    shmem.procs[1].hbm_used[1].store(30, Ordering::Release);
    proc_mock::set(&[(other(1), Answer::Live)]);
    assert_eq!(100, c.recalculate_usage_for_device(1));
    assert_eq!(160, shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(40, shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(20, shmem.procs[1].hbm_used[0].load(Ordering::Acquire));
    proc_mock::set(&[(other(1), Answer::Zombie)]);
    assert_eq!(70, c.recalculate_usage_for_device(1));
    assert_eq!(0, shmem.procs[1].hbm_used[1].load(Ordering::Acquire));
    // The merged multi-device fix must clear every device counter on a successful reap.
    assert_eq!(0, shmem.procs[1].hbm_used[0].load(Ordering::Acquire));
    assert_eq!(110, shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(0, shmem.procs[1].pid.load(Ordering::Acquire));
    assert_compute_unchanged(shmem);
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
fn memory_abi_size_and_process_offset_remain_original_main() {
    assert_eq!(6224, std::mem::size_of::<LocalContainerShmem>());
    assert_eq!(1080, std::mem::offset_of!(LocalContainerShmem, procs));
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
fn quota_reservation_counts_allocations_on_every_device() {
    let c = client(100, 40);
    c.inner.shmem.procs[0].hbm_used[1].store(50, Ordering::Release);
    proc_mock::set(&[(me(), Answer::Unreadable)]);
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, c.check_memory_quota(11));
    assert_eq!(90, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(RT_ERROR_NONE, c.check_memory_quota(10));
    assert_eq!(100, c.inner.shmem.memory_used.load(Ordering::Acquire));
    c.post_alloc_hbm(0, 10, RT_ERROR_MEMORY_ALLOCATION);
    assert_eq!(90, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert!(proc_mock::calls().is_empty());
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
