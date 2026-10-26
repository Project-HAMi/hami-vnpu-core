use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Barrier, mpsc};
use crate::{Call, calls, clear_calls};

fn client(tokens: u64, active: bool) -> SchedulerClient {
    let shmem = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
    shmem.state.store(STATE_RUNNING, Ordering::Release);
    shmem.batch_id.store(42, Ordering::Release);
    shmem.tokens_remaining.store(tokens, Ordering::Release);
    shmem.active_workers.store(if active {1} else {0}, Ordering::Release);
    shmem.compute_limit.store(50, Ordering::Release);
    shmem.reports[0].cpu_start_us.store(if active {100} else {0}, Ordering::Release);
    shmem.reports[0].occupied.store(1, Ordering::Release);
    shmem.reports[0].pid.store(std::process::id() as i32, Ordering::Release);
    SchedulerClient { inner: Arc::new(SchedulerClientInner {
        shmem, my_proc_idx: 0, physical_id: 0,
        contexts: Mutex::new(HashMap::from([(0, Arc::new(ContextTiming {
            my_slot_idx: 0,
            lock: RwLock::new(InnerLock { internal_stream:10, start_event:11, end_event:12,
                tracking_event:13, batch_active:active, current_batch_id:if active {42} else {0},
                last_user_stream:6, start_time_us:if active {100} else {0} }),
            retired: AtomicBool::new(false),
        }))])),
        hbm_handle_map:Mutex::new(HashMap::new()), is_stub:false, sched_policy:SchedPolicy::Dynamic,
    }) }
}
fn timing(c: &SchedulerClient) -> Arc<ContextTiming> { c.context_timing(0) }
pub fn remaining(c: &SchedulerClient) -> u64 { c.inner.shmem.tokens_remaining.load(Ordering::Acquire) }
fn wait_until(mut predicate: impl FnMut()->bool) {
    let limit=Instant::now()+Duration::from_secs(2);
    while !predicate() { assert!(Instant::now()<limit,"bounded condition wait failed"); thread::yield_now(); }
}
fn waiting_thread(c: SchedulerClient, stream: u64) -> (mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let (ready_tx, ready_rx)=mpsc::channel(); let (tx,rx)=mpsc::channel();
    let join=thread::spawn(move || { ready_tx.send(()).unwrap(); c.wait_for_token(stream); tx.send(()).unwrap(); });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    (rx,join)
}

#[test]
fn spatial_bypasses_all_admission_states_without_borrowing_locking_or_measurement() {
    for state in [STATE_IDLE, STATE_RUNNING, STATE_MEASURING] {
        let mut c = client(17, true);
        Arc::get_mut(&mut c.inner).unwrap().sched_policy = SchedPolicy::Spatial;
        c.inner.shmem.state.store(state, Ordering::Release);
        c.inner.shmem.overdraft_limit.store(10, Ordering::Release);
        let round = c.inner.shmem.overdraft_pool.begin_round().unwrap();
        assert!(c.inner.shmem.overdraft_pool.open_idle(round));
        let pool = c.inner.shmem.overdraft_pool.snapshot();
        let context = timing(&c);let guard = context.lock.write().unwrap();
        let (rx, thread) = waiting_thread(c.clone(), 99);
        rx.recv_timeout(Duration::from_secs(2)).expect("spatial must bypass a held timing writer lock");
        thread.join().unwrap();
        assert_eq!(remaining(&c), 17);
        assert_eq!(c.inner.shmem.overdraft_pool.snapshot(), pool);
        assert_eq!(c.inner.shmem.active_workers.load(Ordering::Acquire), 1);
        assert_eq!(c.inner.shmem.reported_count.load(Ordering::Acquire), 0);
        assert_eq!(guard.last_user_stream, 6);
        assert!(calls().is_empty(), "spatial must not invoke measurement events or synchronization");
        drop(guard);
    }
}

#[test]
fn take_one_sequential_zero_never_wraps() {
    let c=client(37,true);
    for expected in (0..37).rev() { assert!(c.try_consume_one_token()); assert_eq!(expected,remaining(&c)); }
    for _ in 0..100 { assert!(!c.try_consume_one_token()); assert_eq!(0,remaining(&c)); }
    assert_eq!(1,c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert!(calls().is_empty());
}

#[test]
fn exactly_one_contender_can_consume_the_last_ticket() {
    let c = client(1, true);
    let barrier = Arc::new(Barrier::new(33));
    let threads: Vec<_> = (0..32).map(|_| {
        let c = c.clone();
        let barrier = barrier.clone();
        thread::spawn(move || { barrier.wait(); c.try_consume_one_token() })
    }).collect();
    barrier.wait();
    let winners = threads.into_iter().filter_map(|t| t.join().unwrap().then_some(())).count();
    assert_eq!(winners, 1);
    assert_eq!(remaining(&c), 0);
    assert!(!c.try_consume_one_token());
}

#[test]
fn concurrent_take_one_conserves_budget_and_never_exposes_wrapped_balance() {
    const BUDGET:u64=10003;
    let c=client(BUDGET,true); let successes=Arc::new(AtomicU64::new(0));
    let barrier=Arc::new(Barrier::new(17)); let done=Arc::new(AtomicBool::new(false));
    let monitor={let c=c.clone();let done=done.clone();thread::spawn(move || {
        while !done.load(Ordering::Acquire) { assert!(remaining(&c)<=BUDGET,"wrapped balance exposed"); thread::yield_now(); }
    })};
    let threads:Vec<_>=(0..16).map(|_| {let c=c.clone();let n=successes.clone();let b=barrier.clone();
        thread::spawn(move || { b.wait(); loop {
            if c.try_consume_one_token() {n.fetch_add(1,Ordering::Relaxed);}
            else if remaining(&c)==0 {break;} // A CAS collision alone is not exhaustion.
        } }) }).collect();
    barrier.wait();for h in threads {h.join().unwrap();}
    done.store(true,Ordering::Release);monitor.join().unwrap();
    assert_eq!(BUDGET,successes.load(Ordering::Acquire)); assert_eq!(0,remaining(&c));
}

#[test]
fn real_wait_many_same_stream_readers_spend_exactly_one_each() {
    let c=client(1600,true); let barrier=Arc::new(Barrier::new(9));
    let threads:Vec<_>=(0..8).map(|_|{let c=c.clone();let b=barrier.clone();thread::spawn(move||{
        b.wait(); for _ in 0..200 {c.wait_for_token(6);}
    })}).collect();
    barrier.wait();for h in threads {h.join().unwrap();}
    assert_eq!(0,remaining(&c));assert!(calls().is_empty());
    assert_eq!(1,c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert_eq!(0,c.inner.shmem.reported_count.load(Ordering::Acquire));
    assert_eq!(100,c.inner.shmem.reports[0].cpu_start_us.load(Ordering::Acquire));
}

#[test]
fn same_stream_fast_path_succeeds_while_another_read_guard_is_held() {
    let c=client(2,true);let context=timing(&c);let guard=context.lock.read().unwrap();
    let (rx,h)=waiting_thread(c.clone(),6);
    rx.recv_timeout(Duration::from_secs(2)).expect("read fast path must not require write lock");
    assert_eq!(1,remaining(&c));assert!(calls().is_empty());
    assert_eq!(42,guard.current_batch_id);assert_eq!(6,guard.last_user_stream);
    assert_eq!(100,guard.start_time_us);drop(guard);h.join().unwrap();
}

#[test]
fn different_stream_requires_writer_and_preserves_current_batch_events() {
    let c=client(2,true);let context=timing(&c);let guard=context.lock.read().unwrap();
    let (rx,h)=waiting_thread(c.clone(),7);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());assert_eq!(2,remaining(&c));
    drop(guard);rx.recv_timeout(Duration::from_secs(2)).unwrap();h.join().unwrap();
    assert_eq!(1,remaining(&c));assert_eq!(7,timing(&c).lock.read().unwrap().last_user_stream);
    assert!(calls().is_empty());assert_eq!(1,c.inner.shmem.active_workers.load(Ordering::Acquire));
}

#[test]
fn new_batch_requires_writer_and_records_one_new_start_event() {
    let c=client(2,true);c.inner.shmem.batch_id.store(43,Ordering::Release);
    c.inner.shmem.active_workers.store(0,Ordering::Release);
    let context=timing(&c);let guard=context.lock.read().unwrap();let (rx,h)=waiting_thread(c.clone(),6);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());assert_eq!(2,remaining(&c));
    drop(guard);rx.recv_timeout(Duration::from_secs(2)).unwrap();h.join().unwrap();
    assert_eq!(1,remaining(&c));assert_eq!(vec![Call::Record(11,10)],calls());
    let context=timing(&c);let lock=context.lock.read().unwrap();assert_eq!(43,lock.current_batch_id);assert!(lock.batch_active);
    assert_eq!(1,c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert_eq!(0,c.inner.shmem.reported_count.load(Ordering::Acquire));
}

#[test]
fn try_read_failure_drops_guard_and_enters_writer_without_upgrade_deadlock() {
    let c=client(2,true);let context=timing(&c);let guard=context.lock.write().unwrap();
    let (rx,h)=waiting_thread(c.clone(),6);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());assert_eq!(2,remaining(&c));
    drop(guard);rx.recv_timeout(Duration::from_secs(2)).unwrap();h.join().unwrap();
    assert_eq!(1,remaining(&c));assert!(calls().is_empty());
}

#[test]
fn no_ticket_wait_is_not_fail_open_and_new_batch_spends_its_own_ticket() {
    let c=client(0,true);let (rx,h)=waiting_thread(c.clone(),6);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());assert_eq!(0,remaining(&c));
    c.inner.shmem.state.store(STATE_IDLE,Ordering::Release);
    c.inner.shmem.batch_id.store(43,Ordering::Relaxed);
    c.inner.shmem.active_workers.store(0,Ordering::Relaxed);
    c.inner.shmem.tokens_remaining.store(1,Ordering::Relaxed);
    c.inner.shmem.state.store(STATE_RUNNING,Ordering::Release);futex::wake_all(&c.inner.shmem.state);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();h.join().unwrap();
    assert_eq!(0,remaining(&c));assert_eq!(43,timing(&c).lock.read().unwrap().current_batch_id);
    assert_eq!(vec![Call::Record(11,10)],calls());
}

#[test]
fn measuring_uses_exclusive_original_event_chain_and_exactly_one_process_report() {
    let c=client(0,true);c.inner.shmem.state.store(STATE_MEASURING,Ordering::Release);
    let context=timing(&c);let guard=context.lock.read().unwrap();let (rx,h)=waiting_thread(c.clone(),6);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());assert!(calls().is_empty());
    assert_eq!(0,c.inner.shmem.reported_count.load(Ordering::Acquire));drop(guard);
    wait_until(||c.inner.shmem.reported_count.load(Ordering::Acquire)==1);
    assert_eq!(42,c.inner.shmem.reports[0].batch_id.load(Ordering::Acquire));
    assert_eq!(2500,c.inner.shmem.reports[0].duration_us.load(Ordering::Acquire));
    assert_eq!(vec![Call::Capture(6),Call::Record(13,6),Call::Wait(10,13),Call::Record(12,10),
        Call::StreamSync(10),Call::Elapsed(11,12)],calls());
    assert!(rx.try_recv().is_err());clear_calls();
    c.inner.shmem.batch_id.store(43,Ordering::Relaxed);
    c.inner.shmem.active_workers.store(0,Ordering::Relaxed);c.inner.shmem.reported_count.store(0,Ordering::Relaxed);
    c.inner.shmem.tokens_remaining.store(1,Ordering::Relaxed);
    c.inner.shmem.state.store(STATE_RUNNING,Ordering::Release);futex::wake_all(&c.inner.shmem.state);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();h.join().unwrap();
    assert_eq!(0,remaining(&c));assert_eq!(vec![Call::Record(11,10)],calls());
    assert_eq!(1,c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert_eq!(0,c.inner.shmem.reported_count.load(Ordering::Acquire));
}

#[test]
fn waiting_writer_progresses_under_continuous_same_stream_readers() {
    let c=client(1_000_000_000,true);let stop=Arc::new(AtomicBool::new(false));let attempts=Arc::new(AtomicU64::new(0));
    let readers:Vec<_>=(0..8).map(|_|{let c=c.clone();let stop=stop.clone();let n=attempts.clone();thread::spawn(move||{
        while !stop.load(Ordering::Acquire) {let _=c.try_same_stream_token(6);n.fetch_add(1,Ordering::Relaxed);}
    })}).collect();
    wait_until(||attempts.load(Ordering::Acquire)>1000);
    let (tx,rx)=mpsc::channel();let writer={let c=c.clone();thread::spawn(move||{
        let context=timing(&c);let mut g=context.lock.write().unwrap();g.last_user_stream=7;tx.send(()).unwrap();
    })};
    let outcome=rx.recv_timeout(Duration::from_secs(2));stop.store(true,Ordering::Release);
    for h in readers {h.join().unwrap();}writer.join().unwrap();outcome.expect("writer did not progress on tested platform");
    assert_eq!(7,timing(&c).lock.read().unwrap().last_user_stream);
}

#[test]
fn all_five_actual_hook_wrappers_charge_before_native_and_keep_one_process_report() {
    let c=client(5,false);crate::CLIENT.set(c.clone()).unwrap();
    let entries:[(&str,fn()->u64);5]=[
        ("rtAicpuKernelLaunchExWithArgs",||crate::actual_hook::rtAicpuKernelLaunchExWithArgs(1,2,3,4,5,6,7)),
        ("rtAicpuKernelLaunchWithFlag",||crate::actual_hook::rtAicpuKernelLaunchWithFlag(1,2,3,4,6,7)),
        ("rtKernelLaunchWithFlagV2",||crate::actual_hook::rtKernelLaunchWithFlagV2(1,2,3,4,6,7,8)),
        ("rtKernelLaunchWithHandleV2",||crate::actual_hook::rtKernelLaunchWithHandleV2(1,2,3,4,5,6,7)),
        ("rtModelExecute",||crate::actual_hook::rtModelExecute(1,6,7)),
    ];
    for (i,(name,entry)) in entries.into_iter().enumerate() {
        assert_eq!(89,entry());
        assert!(matches!(calls().last(),Some(Call::Native(actual,_,tokens)) if *actual==name && *tokens==4-i as u64));
    }
    assert_eq!(0,remaining(&c));assert_eq!(1,calls().iter().filter(|x|matches!(x,Call::Record(_, _))).count());
    assert_eq!(5,calls().iter().filter(|x|matches!(x,Call::Native(_,_,_))).count());
    assert_eq!(1,c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert_eq!(0,c.inner.shmem.reported_count.load(Ordering::Acquire));
    assert_eq!(1,c.inner.shmem.reports.iter().filter(|r|r.occupied.load(Ordering::Acquire)!=0).count());
}

#[test]
fn shared_memory_abi_preserves_original_prefix_and_appends_overdraft_fields() {
    assert_eq!(6256,std::mem::size_of::<LocalContainerShmem>());
    assert_eq!(1080,std::mem::offset_of!(LocalContainerShmem,procs));
    assert_eq!(6224,std::mem::offset_of!(LocalContainerShmem,overdraft_pool));
}

#[test]
fn inactive_or_mismatched_state_never_uses_the_read_fast_path() {
    let c = client(5, true);
    for state in [STATE_IDLE, STATE_MEASURING, u32::MAX] {
        c.inner.shmem.state.store(state, Ordering::Release);
        assert!(!c.try_same_stream_token(6));
        assert_eq!(remaining(&c), 5);
    }
    c.inner.shmem.state.store(STATE_RUNNING, Ordering::Release);
    c.inner.shmem.batch_id.store(43, Ordering::Release);
    assert!(!c.try_same_stream_token(6));
    c.inner.shmem.batch_id.store(42, Ordering::Release);
    assert!(!c.try_same_stream_token(7));
    timing(&c).lock.write().unwrap().batch_active = false;
    assert!(!c.try_same_stream_token(6));
    assert_eq!(remaining(&c), 5);
    assert!(calls().is_empty());
}

#[test]
fn poisoned_writer_does_not_admit_or_consume_a_ticket() {
    let c = client(5, true);
    let poisoned = c.clone();
    assert!(thread::spawn(move || {
        let context = timing(&poisoned);let _guard = context.lock.write().unwrap();
        panic!("inject writer failure");
    }).join().is_err());
    assert!(!c.try_same_stream_token(6));
    assert!(std::panic::catch_unwind(|| c.wait_for_token(6)).is_err());
    assert_eq!(remaining(&c), 5);
    assert!(calls().is_empty());
}

fn state_change_race(state: u32, after_unlock: bool) {
    use crate::wait_observer as observed;
    let c = client(0, state == STATE_MEASURING);
    observed::REGION.set(c.inner.shmem).unwrap();
    c.inner.shmem.state.store(state, Ordering::Release);
    if after_unlock { observed::CHANGE_AFTER_UNLOCK.store(true, Ordering::Release); }
    else { observed::CHANGE_BEFORE_SYSCALL.store(true, Ordering::Release); }
    c.wait_for_token(6);
    assert_eq!(remaining(&c), 0, "one ticket consumed in the new batch");
    assert_eq!(timing(&c).lock.read().unwrap().current_batch_id, 43);
    assert_eq!(c.inner.shmem.active_workers.load(Ordering::Acquire), 1);
    assert_eq!(observed::WAIT_CALLS.load(Ordering::Acquire), if after_unlock { 0 } else { 1 });
    if !after_unlock { assert_eq!(observed::WAIT_ERRNO.load(Ordering::Acquire), libc::EAGAIN); }
    assert_eq!(c.inner.shmem.reported_count.load(Ordering::Acquire), if state == STATE_MEASURING { 1 } else { 0 });
    if state == STATE_MEASURING {
        assert_eq!(c.inner.shmem.reports[0].batch_id.load(Ordering::Acquire), 42);
        assert_eq!(calls(), vec![Call::Capture(6), Call::Record(13,6), Call::Wait(10,13),
            Call::Record(12,10), Call::StreamSync(10), Call::Elapsed(11,12), Call::Record(11,10)]);
    } else { assert_eq!(calls(), vec![Call::Record(11,10)]); }
}

#[test] fn idle_changed_after_unlock_skips_futex() { state_change_race(STATE_IDLE, true); }
#[test] fn measuring_changed_after_unlock_skips_futex() { state_change_race(STATE_MEASURING, true); }
#[test] fn idle_check_to_syscall_race_still_returns_eagain() { state_change_race(STATE_IDLE, false); }
#[test] fn measuring_check_to_syscall_race_still_returns_eagain() { state_change_race(STATE_MEASURING, false); }

#[test]
fn unchanged_idle_state_really_blocks_in_linux_futex_until_woken() {
    use crate::wait_observer as observed;
    let c = client(0, false);
    observed::REGION.set(c.inner.shmem).unwrap();
    c.inner.shmem.state.store(STATE_IDLE, Ordering::Release);
    let (rx, thread) = waiting_thread(c.clone(), 6);
    wait_until(|| {
        let tid = observed::WAIT_TID.load(Ordering::Acquire);
        tid > 0 && std::fs::read_to_string(format!("/proc/self/task/{tid}/wchan"))
            .unwrap_or_default().contains("futex")
    });
    assert!(rx.try_recv().is_err());
    assert_eq!(remaining(&c), 0);
    observed::publish_next_batch();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    thread.join().unwrap();
    assert_eq!(remaining(&c), 0);
    assert_eq!(observed::WAIT_CALLS.load(Ordering::Acquire), 1);
    assert_eq!(calls(), vec![Call::Record(11,10)]);
}


#[test]
fn contexts_on_one_card_have_independent_timing_resources_and_report_slots() {
    let c = client(4, false);
    crate::externed_api::set_context(101);
    c.wait_for_token(1001);
    let first = c.context_timing(101);
    crate::externed_api::set_context(202);
    c.wait_for_token(2002);
    let second = c.context_timing(202);
    assert_ne!(first.my_slot_idx, second.my_slot_idx);
    let first_stream = first.lock.read().unwrap().internal_stream;
    let second_stream = second.lock.read().unwrap().internal_stream;
    assert_ne!(first_stream, second_stream);
    assert_eq!(2, c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert_eq!(2, remaining(&c));
    assert_eq!(1, calls().iter().filter(|call| **call == Call::StreamCreate(101)).count());
    assert_eq!(1, calls().iter().filter(|call| **call == Call::StreamCreate(202)).count());
    clear_calls();
    crate::externed_api::set_context(101);
    c.measure_and_report_batch(&first, &mut first.lock.write().unwrap());
    crate::externed_api::set_context(202);
    c.measure_and_report_batch(&second, &mut second.lock.write().unwrap());
    assert_eq!(2, c.inner.shmem.reported_count.load(Ordering::Acquire));
    for timing in [&first, &second] {
        assert_eq!(42, c.inner.shmem.reports[timing.my_slot_idx].batch_id.load(Ordering::Acquire));
        assert_eq!(2500, c.inner.shmem.reports[timing.my_slot_idx].duration_us.load(Ordering::Acquire));
    }
    // The fake runtime rejects any event/stream whose creator context differs.
    assert!(calls().contains(&Call::StreamSync(first_stream)));
    assert!(calls().contains(&Call::StreamSync(second_stream)));
}

#[test]
fn retirement_releases_timing_without_losing_card_allocation_ownership() {
    let c = client(4, false);
    c.inner.shmem.memory_limit.store(100, Ordering::Release);
    c.inner.shmem.procs[0].pid.store(std::process::id() as i32, Ordering::Release);
    c.inner.shmem.procs[0].is_active.store(1, Ordering::Release);
    assert_eq!(0, c.check_memory_quota(80));
    c.post_alloc_hbm(777, 80, RT_ERROR_NONE);
    crate::externed_api::set_context(101);
    c.wait_for_token(1001);
    let original = c.context_timing(101);
    let old_stream = original.lock.read().unwrap().internal_stream;
    clear_calls();
    c.retire_context(101);
    assert!(calls().is_empty(), "destroyed contexts must not receive runtime calls");
    assert!(original.retired.load(Ordering::Acquire));
    assert_eq!(0, c.inner.shmem.active_workers.load(Ordering::Acquire));
    assert!(c.owns_hbm_handle(777));
    assert_eq!(80, c.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(80, c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire));
    c.wait_for_token(1002); // CANN reused the context handle for a new context.
    let fresh = c.context_timing(101);
    assert!(!Arc::ptr_eq(&fresh, &original));
    assert_ne!(old_stream, fresh.lock.read().unwrap().internal_stream);
    c.post_free_hbm(777, RT_ERROR_NONE);
    assert_eq!(0, c.inner.shmem.memory_used.load(Ordering::Acquire));
}

#[test]
fn retired_completed_report_survives_until_its_round_is_consumed() {
    let c = client(4, false);
    crate::externed_api::set_context(101);
    c.wait_for_token(1001);
    let original = c.context_timing(101);
    c.measure_and_report_batch(&original, &mut original.lock.write().unwrap());
    let old_slot = original.my_slot_idx;
    c.retire_context(101);
    assert_eq!(2, c.inner.shmem.reports[old_slot].occupied.load(Ordering::Acquire));
    assert_eq!(42, c.inner.shmem.reports[old_slot].batch_id.load(Ordering::Acquire));
    let replacement = c.context_timing(101);
    assert_ne!(old_slot, replacement.my_slot_idx, "current round report must not be overwritten");
    c.inner.shmem.batch_id.store(43, Ordering::Release);
    crate::externed_api::set_context(303);
    let next = c.context_timing(303);
    assert_eq!(old_slot, next.my_slot_idx, "retired slots are reusable in the next round");
}


#[test]
fn physical_allocation_hook_charges_target_card_while_current_card_is_unchanged() {
    let current = client(4, false);
    let target = client(4, false);
    for (client, limit) in [(&current, 64), (&target, 100)] {
        client.inner.shmem.memory_limit.store(limit, Ordering::Release);
        client.inner.shmem.procs[0].pid.store(std::process::id() as i32, Ordering::Release);
        client.inner.shmem.procs[0].is_active.store(1, Ordering::Release);
    }
    crate::CLIENT.set(current.clone()).unwrap();
    crate::DEVICE_ONE.set(target.clone()).unwrap();
    crate::NATIVE_MEMORY.store(true, Ordering::Release);
    // Runtime rtDrvMemProp_t starts with side/devid, not ACL's location structure.
    #[repr(C)]
    struct RuntimePhysicalProp { side: u32, devid: u32, module: u32, page: u32, memory: u32, reserve: u64 }
    let mut prop = RuntimePhysicalProp { side: 1, devid: 1, module: 33, page: 1, memory: 0, reserve: 0 };
    let mut handle = 0u64;
    let result = crate::actual_hook::rtMallocPhysical(
        (&mut handle as *mut u64) as u64, 80, (&mut prop as *mut RuntimePhysicalProp) as u64, 0);
    assert_eq!(0, result, "target card has room although current card cannot fit 80 bytes");
    assert_ne!(0, handle);
    assert_eq!(0, current.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(80, target.inner.shmem.memory_used.load(Ordering::Acquire));
    assert!(target.owns_hbm_handle_kind(handle, AllocationKind::Physical));
    assert!(!target.owns_hbm_handle(handle));
    let mut denied = 0u64;
    assert_eq!(RT_ERROR_MEMORY_ALLOCATION, crate::actual_hook::rtMallocPhysical(
        (&mut denied as *mut u64) as u64, 21, (&mut prop as *mut RuntimePhysicalProp) as u64, 0));
    assert_eq!(0, denied);
    assert_eq!(0, crate::actual_hook::rtFreePhysical(handle));
    assert_eq!(0, target.inner.shmem.memory_used.load(Ordering::Acquire));
    assert_eq!(0, current.inner.shmem.memory_used.load(Ordering::Acquire));
    assert!(calls().iter().all(|call| !matches!(call, Call::DeviceSync)));
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/memory_hooks.rs"));
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime_memory_extra.rs"));
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime_memory_physical.rs"));

#[test]
fn physical_allocation_invalid_target_preserves_native_error_without_charging() {
    let current = client(4, false);
    current.inner.shmem.memory_limit.store(64, Ordering::Release);
    current.inner.shmem.procs[0].pid.store(std::process::id() as i32, Ordering::Release);
    current.inner.shmem.procs[0].is_active.store(1, Ordering::Release);
    crate::CLIENT.set(current.clone()).unwrap();
    assert!(crate::NATIVE_OVERRIDE.set(|name, _args| {
        (name == "rtMallocPhysical").then_some(107001)
    }).is_ok());
    #[repr(C)]
    struct Prop { side: u32, device: u32 }
    for device in [2, i32::MAX as u32, u32::MAX] {
        let prop = Prop { side: 1, device };
        let mut output = 0xfeed_u64;
        clear_calls();
        assert_eq!(107001, crate::actual_hook::rtMallocPhysical(
            &mut output as *mut u64 as u64, 17, &prop as *const Prop as u64, 0));
        assert_eq!(output, 0xfeed);
        assert_eq!(0, current.inner.shmem.memory_used.load(Ordering::Acquire));
        assert_eq!(calls().iter().filter(|c| matches!(c, Call::Native("rtMallocPhysical", _, _))).count(), 1);
    }
}
