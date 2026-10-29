mod memory_hooks {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize};

    const PLAIN: u32 = 0;
    const ALIAS: u32 = 1;
    const BLOCK: u32 = 2;
    const FORK: u32 = 3;
    const INDEPENDENT: u32 = 4;
    const OTHER_CARD: u32 = 5;
    const RECURSE_ALIAS: u32 = 6;
    const FREE_REUSE: u32 = 7;
    const FREE_ALIAS_RT: u32 = 8;
    const FREE_ALIAS_RTS: u32 = 9;
    const RETAIN_BLOCK: u32 = 10;
    const FREE_MULTI: u32 = 11;
    const FREE_PHYSICAL_ALIAS: u32 = 12;
    const FREE_CROSS_KIND: u32 = 13;
    const RETAIN_ALIAS: u32 = 14;
    static MODE: AtomicU32 = AtomicU32::new(PLAIN);
    static RESULT: AtomicU64 = AtomicU64::new(0);
    static NATIVE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SEEN_RESERVATION: AtomicU64 = AtomicU64::new(0);
    static BLOCK_ENTERED: AtomicBool = AtomicBool::new(false);
    static RELEASE_BLOCK: AtomicBool = AtomicBool::new(false);
    static FORK_ONCE: AtomicBool = AtomicBool::new(true);
    static CHILD_STATUS: AtomicI32 = AtomicI32::new(-1);
    static NESTED_HANDLE: AtomicU64 = AtomicU64::new(0);
    static NESTED_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);
    static NESTED_SIZE: AtomicU64 = AtomicU64::new(0);
    static OUTER_FAILURE: AtomicBool = AtomicBool::new(false);
    static FREE_RESULT: AtomicU64 = AtomicU64::new(0);
    static FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static FREE_ENTERED: AtomicBool = AtomicBool::new(false);
    static RELEASE_FREE: AtomicBool = AtomicBool::new(false);
    static REUSED_HANDLE: AtomicU64 = AtomicU64::new(0);
    static RETAIN_HANDLE: AtomicU64 = AtomicU64::new(0);
    static RETAIN_RESULT: AtomicU64 = AtomicU64::new(0);
    static RETAIN_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RETAIN_ENTERED: AtomicBool = AtomicBool::new(false);
    static RELEASE_RETAIN: AtomicBool = AtomicBool::new(false);
    static FREE_ENTERED_MASK: AtomicUsize = AtomicUsize::new(0);
    static FREE_RELEASE_MASK: AtomicUsize = AtomicUsize::new(0);
    static FIRST_FREE_RESULT: AtomicU64 = AtomicU64::new(0);
    static SECOND_FREE_RESULT: AtomicU64 = AtomicU64::new(0);

    fn used(client: &SchedulerClient) -> u64 {
        client.inner.shmem.memory_used.load(Ordering::Acquire)
    }

    fn memory_client(limit: u64, physical: u32) -> SchedulerClient {
        let mut c = client(9, false);
        Arc::get_mut(&mut c.inner).unwrap().physical_id = physical;
        c.inner.shmem.memory_limit.store(limit, Ordering::Release);
        c.inner.shmem.procs[0]
            .pid
            .store(std::process::id() as i32, Ordering::Release);
        c.inner.shmem.procs[0].is_active.store(1, Ordering::Release);
        c
    }

    fn install(limit: u64) -> SchedulerClient {
        let c = memory_client(limit, 0);
        crate::CLIENT.set(c.clone()).unwrap();
        assert!(crate::NATIVE_OVERRIDE.set(native_memory).is_ok());
        c
    }

    fn native_memory(name: &'static str, args: &[u64]) -> Option<u64> {
        if name == "rtMemRetainAllocationHandle" {
            let index = RETAIN_CALLS.fetch_add(1, Ordering::AcqRel);
            match MODE.load(Ordering::Acquire) {
                RETAIN_BLOCK => {
                    RETAIN_ENTERED.store(true, Ordering::Release);
                    while !RELEASE_RETAIN.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                }
                RETAIN_ALIAS if index == 0 => {
                    return Some(crate::actual_hook::rtMemRetainAllocationHandle(
                        args[0], args[1],
                    ));
                }
                _ => {}
            }
            let result = RETAIN_RESULT.load(Ordering::Acquire);
            if result == 0 {
                unsafe { *(args[1] as *mut u64) = RETAIN_HANDLE.load(Ordering::Acquire) };
            }
            return Some(result);
        }
        if matches!(name, "rtFree" | "rtFreePhysical" | "rtsFree") {
            let index = FREE_CALLS.fetch_add(1, Ordering::AcqRel);
            match MODE.load(Ordering::Acquire) {
                FREE_REUSE if !FREE_ENTERED.swap(true, Ordering::AcqRel) => {
                    // Model an allocator releasing the address/handle before
                    // the old free wrapper returns and completes accounting.
                    while !RELEASE_FREE.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                }
                FREE_MULTI => {
                    assert!(index < 2, "only the two controlled calls may be in flight");
                    let bit = 1 << index;
                    FREE_ENTERED_MASK.fetch_or(bit, Ordering::AcqRel);
                    while FREE_RELEASE_MASK.load(Ordering::Acquire) & bit == 0 {
                        thread::yield_now();
                    }
                    return Some(if index == 0 {
                        FIRST_FREE_RESULT.load(Ordering::Acquire)
                    } else {
                        SECOND_FREE_RESULT.load(Ordering::Acquire)
                    });
                }
                FREE_PHYSICAL_ALIAS if name == "rtFreePhysical" && index % 2 == 0 => {
                    return Some(crate::actual_hook::rtFreePhysical(args[0]));
                }
                FREE_CROSS_KIND if name == "rtFree" => {
                    return Some(crate::actual_hook::rtFreePhysical(args[0]));
                }
                FREE_ALIAS_RT if name == "rtFree" => {
                    return Some(crate::actual_hook::rtsFree(args[0]));
                }
                FREE_ALIAS_RTS if name == "rtsFree" => {
                    return Some(crate::actual_hook::rtFree(args[0]));
                }
                _ => {}
            }
            return Some(FREE_RESULT.load(Ordering::Acquire));
        }
        if !matches!(
            name,
            "rtMalloc" | "rtMallocCached" | "rtMemAlloc" | "rtMallocPhysical"
        ) {
            return None;
        }
        NATIVE_CALLS.fetch_add(1, Ordering::AcqRel);
        SEEN_RESERVATION.store(used(crate::npu_limiter()), Ordering::Release);
        match MODE.load(Ordering::Acquire) {
            RECURSE_ALIAS => {
                // A safety ceiling in the mock turns an absent production
                // bound into an assertion failure rather than stack overflow.
                if NATIVE_CALLS.load(Ordering::Acquire) > 64 {
                    return Some(0);
                }
                return Some(crate::actual_hook::rtMallocCached(
                    args[0],
                    args[1],
                    args[2] as u32,
                    33,
                ));
            }
            ALIAS if name == "rtMemAlloc" => {
                return Some(crate::actual_hook::rtMallocCached(
                    args[0],
                    args[1],
                    args[2] as u32,
                    33,
                ));
            }
            ALIAS if name == "rtMallocCached" => {
                return Some(crate::actual_hook::rtMalloc(
                    args[0],
                    args[1],
                    args[2] as u32,
                    args[3] as u16,
                ));
            }
            BLOCK if name == "rtMallocCached" => {
                if !BLOCK_ENTERED.swap(true, Ordering::AcqRel) {
                    while !RELEASE_BLOCK.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                }
            }
            FORK if FORK_ONCE.swap(false, Ordering::AcqRel) => {
                // No mock or client mutex is held here. Fork while the actual
                // outer allocation guard is live, then use a fresh child entry.
                let pid = unsafe { libc::fork() };
                if pid == 0 {
                    let before = used(crate::npu_limiter());
                    // Reuse the inherited output address deliberately. It is
                    // not a same-process alias after fork and must be charged.
                    let rc = crate::actual_hook::rtMallocCached(args[0], 8, 2, 33);
                    let handle = unsafe { *(args[0] as *const u64) };
                    let valid = rc == 0 && handle != 0 && used(crate::npu_limiter()) == before + 8;
                    unsafe { libc::_exit(if valid { 0 } else { 21 }) }
                }
                if pid > 0 {
                    let mut status = -1;
                    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
                    CHILD_STATUS.store(if waited == pid { status } else { -2 }, Ordering::Release);
                } else {
                    CHILD_STATUS.store(-3, Ordering::Release);
                }
            }
            INDEPENDENT | OTHER_CARD if name == "rtMallocCached" => {
                let previous = crate::externed_api::current_device();
                if MODE.load(Ordering::Acquire) == OTHER_CARD {
                    unsafe { crate::externed_api::rtSetDevice(1) };
                }
                let mut handle = 0;
                let rc = mem_alloc(&mut handle, NESTED_SIZE.load(Ordering::Acquire), 0);
                NESTED_HANDLE.store(handle, Ordering::Release);
                NESTED_RESULT.store(rc, Ordering::Release);
                unsafe { crate::externed_api::rtSetDevice(previous) };
                if OUTER_FAILURE.load(Ordering::Acquire) {
                    return Some(37);
                }
            }
            _ => {}
        }
        let rc = RESULT.load(Ordering::Acquire);
        if rc == 0 {
            let reused = REUSED_HANDLE.load(Ordering::Acquire);
            let handle = if matches!(MODE.load(Ordering::Acquire), FREE_REUSE | RETAIN_BLOCK)
                && reused != 0
            {
                reused
            } else {
                crate::NEXT_MEMORY.fetch_add(1, Ordering::Relaxed)
            };
            unsafe { *(args[0] as *mut u64) = handle };
        }
        Some(rc)
    }

    fn cached(output: &mut u64, size: u64) -> u64 {
        crate::actual_hook::rtMallocCached(output as *mut u64 as u64, size, 2, 33)
    }

    fn mem_alloc(output: &mut u64, size: u64, config: u64) -> u64 {
        crate::actual_hook::rtMemAlloc(output as *mut u64 as u64, size, 2, 0x87654321, config)
    }

    fn retain(output: &mut u64) -> u64 {
        crate::actual_hook::rtMemRetainAllocationHandle(0x789a0000, output as *mut u64 as u64)
    }

    fn physical_client(size: u64) -> (SchedulerClient, SchedulerClient, u64) {
        let current = install(256);
        let owner = memory_client(256, 1);
        crate::DEVICE_ONE.set(owner.clone()).unwrap();
        let mut handle = 0;
        assert_eq!(
            allocate_kind(&mut handle, size, AllocationKind::Physical),
            0
        );
        RETAIN_HANDLE.store(handle, Ordering::Release);
        (current, owner, handle)
    }

    fn assert_balance(c: &SchedulerClient, expected: u64) {
        assert_eq!(used(c), expected);
        assert_eq!(
            c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire),
            expected
        );
    }

    fn allocate_kind(output: &mut u64, size: u64, kind: AllocationKind) -> u64 {
        if kind == AllocationKind::Virtual {
            return cached(output, size);
        }
        #[repr(C)]
        struct Prop {
            side: u32,
            devid: u32,
            module: u32,
            page: u32,
            memory: u32,
            reserve: u64,
        }
        let prop = Prop {
            side: 1,
            devid: 1,
            module: 33,
            page: 1,
            memory: 0,
            reserve: 0,
        };
        crate::actual_hook::rtMallocPhysical(
            output as *mut u64 as u64,
            size,
            &prop as *const Prop as u64,
            0,
        )
    }

    fn free_kind(handle: u64, kind: AllocationKind) -> u64 {
        if kind == AllocationKind::Physical {
            crate::actual_hook::rtFreePhysical(handle)
        } else {
            crate::actual_hook::rtFree(handle)
        }
    }

    #[test]
    fn cached_enforces_quota_before_native_and_refunds_successful_free() {
        let c = install(64);
        let mut handle = 0;
        assert_eq!(cached(&mut handle, 32), 0);
        assert_ne!(handle, 0);
        assert_balance(&c, 32);
        assert_eq!(SEEN_RESERVATION.load(Ordering::Acquire), 32);
        assert!(c.owns_hbm_handle(handle));
        let mut denied = 0;
        assert_eq!(cached(&mut denied, 96), RT_ERROR_MEMORY_ALLOCATION);
        assert_eq!(denied, 0);
        assert_eq!(NATIVE_CALLS.load(Ordering::Acquire), 1);
        assert_balance(&c, 32);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
        assert!(!c.owns_hbm_handle(handle));
    }

    #[test]
    fn cached_native_failure_rolls_back_reservation_and_next_allocation_works() {
        let c = install(64);
        RESULT.store(17, Ordering::Release);
        let mut handle = 0;
        assert_ne!(cached(&mut handle, 32), 0);
        assert_eq!(SEEN_RESERVATION.load(Ordering::Acquire), 32);
        assert_eq!(handle, 0);
        assert_balance(&c, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
        RESULT.store(0, Ordering::Release);
        assert_eq!(cached(&mut handle, 64), 0);
        assert_balance(&c, 64);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
    }

    #[test]
    fn legacy_malloc_translates_only_its_own_limited_native_failure() {
        let c = install(64);
        RESULT.store(17, Ordering::Release);
        let mut handle = 0;
        assert_eq!(
            crate::actual_hook::rtMalloc(&mut handle as *mut u64 as u64, 32, 2, 33),
            RT_ERROR_MEMORY_ALLOCATION
        );
        assert_eq!(SEEN_RESERVATION.load(Ordering::Acquire), 32);
        assert_balance(&c, 0);
        assert_eq!(cached(&mut handle, 32), 17);
        assert_balance(&c, 0);
        assert_eq!(mem_alloc(&mut handle, 32, 0), 17);
        assert_balance(&c, 0);
        c.inner.shmem.memory_limit.store(0, Ordering::Release);
        assert_eq!(
            crate::actual_hook::rtMalloc(&mut handle as *mut u64 as u64, 32, 2, 33),
            17
        );
        assert_eq!(SEEN_RESERVATION.load(Ordering::Acquire), 0);
        assert_balance(&c, 0);
        // A limited Cached call may delegate through rtMalloc, but that inner
        // alias did not reserve and must not translate the outer native error.
        c.inner.shmem.memory_limit.store(64, Ordering::Release);
        MODE.store(ALIAS, Ordering::Release);
        assert_eq!(cached(&mut handle, 32), 17);
        assert_balance(&c, 0);
        assert_eq!(handle, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn cached_native_alias_to_actual_malloc_charges_once_and_failure_rolls_back_once() {
        let c = install(48);
        MODE.store(ALIAS, Ordering::Release);
        let mut handle = 0;
        assert_eq!(
            cached(&mut handle, 32),
            0,
            "nested malloc must not reserve another 32"
        );
        assert_balance(&c, 32);
        assert_eq!(NATIVE_CALLS.load(Ordering::Acquire), 2);
        assert_eq!(c.inner.hbm_handle_map.lock().unwrap().len(), 1);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
        RESULT.store(19, Ordering::Release);
        handle = 0;
        assert_ne!(cached(&mut handle, 32), 0);
        assert_balance(&c, 0);
        assert_eq!(handle, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
        RESULT.store(0, Ordering::Release);
        assert_eq!(cached(&mut handle, 48), 0);
        assert_balance(&c, 48);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
    }

    #[test]
    fn cached_allocations_stay_per_card_and_cross_card_free_uses_owner() {
        let current = install(64);
        let other = memory_client(100, 1);
        crate::DEVICE_ONE.set(other.clone()).unwrap();
        let mut first = 0;
        assert_eq!(cached(&mut first, 32), 0);
        assert_eq!(unsafe { crate::externed_api::rtSetDevice(1) }, 0);
        let mut second = 0;
        assert_eq!(cached(&mut second, 80), 0);
        assert_balance(&current, 32);
        assert_balance(&other, 80);
        assert_eq!(unsafe { crate::externed_api::rtSetDevice(0) }, 0);
        assert_eq!(crate::actual_hook::rtFree(second), 0);
        assert_balance(&current, 32);
        assert_balance(&other, 0);
        assert_eq!(unsafe { crate::externed_api::rtSetDevice(1) }, 0);
        assert_eq!(crate::actual_hook::rtFree(first), 0);
        assert_balance(&current, 0);
        assert_balance(&other, 0);
    }

    #[test]
    fn allocation_reentry_guard_is_thread_local_not_a_global_bypass() {
        let c = install(64);
        MODE.store(BLOCK, Ordering::Release);
        let first = thread::spawn(|| {
            let mut handle = 0;
            let result = cached(&mut handle, 40);
            (result, handle)
        });
        wait_until(|| BLOCK_ENTERED.load(Ordering::Acquire));
        let (sender, receiver) = mpsc::channel();
        let second = thread::spawn(move || {
            let mut handle = 0;
            let result = mem_alloc(&mut handle, 40, 0);
            sender.send((result, handle)).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        RELEASE_BLOCK.store(true, Ordering::Release);
        let (first_rc, first_handle) = first.join().unwrap();
        second.join().unwrap();
        let (second_rc, second_handle) =
            result.expect("other thread must not wait for the native callback");
        assert_eq!(first_rc, 0);
        assert_eq!(second_rc, RT_ERROR_MEMORY_ALLOCATION);
        assert_eq!(second_handle, 0);
        assert_eq!(NATIVE_CALLS.load(Ordering::Acquire), 1);
        assert_balance(&c, 40);
        assert_eq!(crate::actual_hook::rtFree(first_handle), 0);
        assert_balance(&c, 0);
    }

    #[test]
    fn child_forked_inside_native_callback_does_not_inherit_memory_bypass() {
        let c = install(64);
        MODE.store(FORK, Ordering::Release);
        let mut handle = 0;
        assert_eq!(cached(&mut handle, 16), 0);
        assert_eq!(
            CHILD_STATUS.load(Ordering::Acquire),
            0,
            "child must charge its fresh hook entry"
        );
        assert_balance(&c, 16);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
    }

    #[test]
    fn mem_alloc_and_cached_share_the_same_current_card_quota() {
        let c = install(64);
        let mut first = 0;
        assert_eq!(cached(&mut first, 32), 0);
        let mut denied = 0;
        assert_eq!(mem_alloc(&mut denied, 33, 0), RT_ERROR_MEMORY_ALLOCATION);
        assert_eq!(denied, 0);
        assert_eq!(NATIVE_CALLS.load(Ordering::Acquire), 1);
        let mut second = 0;
        assert_eq!(mem_alloc(&mut second, 32, 0), 0);
        assert_balance(&c, 64);
        assert_eq!(SEEN_RESERVATION.load(Ordering::Acquire), 64);
        assert_eq!(crate::actual_hook::rtFree(first), 0);
        assert_balance(&c, 32);
        assert_eq!(crate::actual_hook::rtFree(second), 0);
        assert_balance(&c, 0);
    }

    // CANN 8.5.2's public attribute layout is packed: enum at byte 0,
    // eight-byte value at byte 4. The production hook must NOT decode it.
    #[repr(C, packed)]
    struct PublicAttr {
        kind: u32,
        value: u64,
    }

    #[repr(C)]
    struct PublicConfig {
        attrs: *const PublicAttr,
        count: usize,
    }

    #[test]
    fn mem_alloc_forwards_all_five_arguments_without_decoding_config() {
        let c = install(64);
        let empty = PublicConfig {
            attrs: std::ptr::null(),
            count: 0,
        };
        let attrs = [PublicAttr { kind: 2, value: 0 }];
        let populated = PublicConfig {
            attrs: attrs.as_ptr(),
            count: attrs.len(),
        };
        for config in [
            0,
            &empty as *const PublicConfig as u64,
            &populated as *const PublicConfig as u64,
        ] {
            clear_calls();
            let mut handle = 0;
            let output = &mut handle as *mut u64 as u64;
            assert_eq!(mem_alloc(&mut handle, 32, config), 0);
            assert_ne!(handle, 0);
            assert_balance(&c, 32);
            let native: Vec<_> = calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Native(name, args, _) => Some((name, args)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                native,
                vec![("rtMemAlloc", vec![output, 32, 2, 0x87654321, config])]
            );
            assert_eq!(crate::actual_hook::rtFree(handle), 0);
            assert_balance(&c, 0);
        }
    }

    #[test]
    fn mem_alloc_config_native_failure_rolls_back_current_card_without_redirecting() {
        let current = install(64);
        // The documented DEVICE_ID attribute must match the current device;
        // SDK rejection is simulated rather than inventing target redirection.
        let attrs = [PublicAttr { kind: 2, value: 1 }];
        let config = PublicConfig {
            attrs: attrs.as_ptr(),
            count: attrs.len(),
        };
        let config_ptr = &config as *const PublicConfig as u64;
        RESULT.store(23, Ordering::Release);
        let mut handle = 0;
        assert_ne!(mem_alloc(&mut handle, 32, config_ptr), 0);
        assert_eq!(handle, 0);
        assert_eq!(SEEN_RESERVATION.load(Ordering::Acquire), 32);
        assert_balance(&current, 0);
        assert!(current.inner.hbm_handle_map.lock().unwrap().is_empty());
        assert!(
            crate::DEVICE_ONE.get().is_none(),
            "opaque config must not create another card's client"
        );
        assert!(
            matches!(calls().last(), Some(Call::Native("rtMemAlloc", args, _)) if args[4] == config_ptr)
        );
        RESULT.store(0, Ordering::Release);
        assert_eq!(mem_alloc(&mut handle, 64, 0), 0);
        assert_balance(&current, 64);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&current, 0);
    }

    #[test]
    fn mem_alloc_native_alias_chain_charges_once_and_restores_guard_after_failure() {
        let c = install(48);
        MODE.store(ALIAS, Ordering::Release);
        let mut handle = 0;
        assert_eq!(
            mem_alloc(&mut handle, 32, 0),
            0,
            "one request crosses three actual wrappers"
        );
        assert_balance(&c, 32);
        assert_eq!(NATIVE_CALLS.load(Ordering::Acquire), 3);
        assert_eq!(c.inner.hbm_handle_map.lock().unwrap().len(), 1);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
        RESULT.store(29, Ordering::Release);
        handle = 0;
        assert_ne!(mem_alloc(&mut handle, 32, 0), 0);
        assert_eq!(handle, 0);
        assert_balance(&c, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
        RESULT.store(0, Ordering::Release);
        assert_eq!(mem_alloc(&mut handle, 48, 0), 0);
        assert_balance(&c, 48);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
    }

    #[test]
    fn nested_allocation_with_distinct_output_must_obey_shared_quota() {
        let c = install(48);
        MODE.store(INDEPENDENT, Ordering::Release);
        NESTED_SIZE.store(24, Ordering::Release);
        let mut outer = 0;
        assert_eq!(cached(&mut outer, 32), 0);
        assert_eq!(
            NESTED_RESULT.load(Ordering::Acquire),
            RT_ERROR_MEMORY_ALLOCATION
        );
        assert_eq!(NESTED_HANDLE.load(Ordering::Acquire), 0);
        assert_eq!(
            NATIVE_CALLS.load(Ordering::Acquire),
            1,
            "independent 24-byte request cannot fit beside 32"
        );
        assert_balance(&c, 32);
        assert_eq!(crate::actual_hook::rtFree(outer), 0);
        assert_balance(&c, 0);
    }

    #[test]
    fn nested_other_card_allocation_survives_outer_native_failure() {
        let current = install(64);
        let other = memory_client(64, 1);
        crate::DEVICE_ONE.set(other.clone()).unwrap();
        MODE.store(OTHER_CARD, Ordering::Release);
        OUTER_FAILURE.store(true, Ordering::Release);
        NESTED_SIZE.store(48, Ordering::Release);
        let mut outer = 0;
        assert_ne!(cached(&mut outer, 32), 0);
        assert_eq!(outer, 0);
        assert_eq!(NESTED_RESULT.load(Ordering::Acquire), 0);
        let nested = NESTED_HANDLE.load(Ordering::Acquire);
        assert_ne!(nested, 0);
        assert_eq!(NATIVE_CALLS.load(Ordering::Acquire), 2);
        assert_eq!(crate::externed_api::current_device(), 0);
        assert_balance(&current, 0);
        assert_balance(&other, 48);
        assert!(other.owns_hbm_handle(nested));
        assert!(!current.owns_hbm_handle(nested));
        assert_eq!(crate::actual_hook::rtFree(nested), 0);
        assert_balance(&current, 0);
        assert_balance(&other, 0);
    }

    #[test]
    fn excessive_alias_recursion_is_bounded_rolls_back_and_restores_next_entry() {
        let c = install(64);
        MODE.store(RECURSE_ALIAS, Ordering::Release);
        let mut handle = 0;
        assert_eq!(cached(&mut handle, 32), RT_ERROR_MEMORY_ALLOCATION);
        assert_eq!(handle, 0);
        let native_calls = NATIVE_CALLS.load(Ordering::Acquire);
        assert!(
            native_calls > 1 && native_calls <= 32,
            "native alias recursion must terminate at the documented bound"
        );
        assert_balance(&c, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
        MODE.store(PLAIN, Ordering::Release);
        assert_eq!(cached(&mut handle, 64), 0);
        assert_ne!(handle, 0);
        assert_balance(&c, 64);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&c, 0);
    }

    fn reused_handle_free(kind: AllocationKind) {
        let current = install(256);
        let owner = if kind == AllocationKind::Physical {
            let other = memory_client(256, 1);
            crate::DEVICE_ONE.set(other.clone()).unwrap();
            other
        } else {
            current.clone()
        };
        let mut old = 0;
        assert_eq!(allocate_kind(&mut old, 32, kind), 0);
        MODE.store(FREE_REUSE, Ordering::Release);
        REUSED_HANDLE.store(old, Ordering::Release);
        let freeing = thread::spawn(move || free_kind(old, kind));
        wait_until(|| FREE_ENTERED.load(Ordering::Acquire));
        let old_still_charged = used(&owner);
        let old_claimed = !owner.owns_hbm_handle_kind(old, kind);
        let mut new = 0;
        let new_rc = allocate_kind(&mut new, 96, kind);
        RELEASE_FREE.store(true, Ordering::Release);
        assert_eq!(freeing.join().unwrap(), 0);
        assert_eq!(new_rc, 0);
        assert_eq!(
            new, old,
            "native allocator deliberately reused the numeric handle"
        );
        assert_eq!(
            old_still_charged, 32,
            "pending free cannot refund quota early"
        );
        assert!(old_claimed, "claim must precede the native free");
        assert_balance(&owner, 96);
        assert_eq!(
            owner
                .inner
                .hbm_handle_map
                .lock()
                .unwrap()
                .get(&(kind, new))
                .map(|record| record.size),
            Some(96)
        );
        MODE.store(PLAIN, Ordering::Release);
        assert_eq!(free_kind(new, kind), 0);
        assert_balance(&owner, 0);
        assert!(owner.inner.hbm_handle_map.lock().unwrap().is_empty());
        if kind == AllocationKind::Physical {
            assert_balance(&current, 0);
        }
    }

    #[test]
    fn virtual_free_ticket_preserves_a_concurrently_reused_address() {
        reused_handle_free(AllocationKind::Virtual);
    }

    #[test]
    fn physical_free_ticket_preserves_reused_handle_and_original_card() {
        reused_handle_free(AllocationKind::Physical);
    }

    #[test]
    fn failed_free_restores_the_original_cross_card_record() {
        let current = install(64);
        let owner = memory_client(100, 1);
        crate::DEVICE_ONE.set(owner.clone()).unwrap();
        unsafe { crate::externed_api::rtSetDevice(1) };
        let mut handle = 0;
        assert_eq!(cached(&mut handle, 80), 0);
        unsafe { crate::externed_api::rtSetDevice(0) };
        FREE_RESULT.store(17, Ordering::Release);
        assert_eq!(crate::actual_hook::rtsFree(handle), 17);
        assert_balance(&owner, 80);
        assert_balance(&current, 0);
        assert!(owner.owns_hbm_handle(handle));
        FREE_RESULT.store(0, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&owner, 0);
        assert_balance(&current, 0);
    }

    #[test]
    fn duplicate_native_free_success_refunds_only_one_claimed_record() {
        let c = install(64);
        let mut handle = 0;
        assert_eq!(cached(&mut handle, 32), 0);
        MODE.store(FREE_REUSE, Ordering::Release);
        let first = thread::spawn(move || crate::actual_hook::rtFree(handle));
        wait_until(|| FREE_ENTERED.load(Ordering::Acquire));
        let duplicate_rc = crate::actual_hook::rtsFree(handle);
        let pending = used(&c);
        RELEASE_FREE.store(true, Ordering::Release);
        assert_eq!(first.join().unwrap(), 0);
        assert_eq!(duplicate_rc, 0);
        assert_eq!(pending, 32, "the second free has no ticket to refund");
        assert_eq!(FREE_CALLS.load(Ordering::Acquire), 2);
        assert_balance(&c, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn runtime_free_aliases_refund_once_in_both_directions() {
        let c = install(64);
        for mode in [FREE_ALIAS_RT, FREE_ALIAS_RTS] {
            let mut handle = 0;
            assert_eq!(cached(&mut handle, 32), 0);
            MODE.store(mode, Ordering::Release);
            let rc = if mode == FREE_ALIAS_RT {
                crate::actual_hook::rtFree(handle)
            } else {
                crate::actual_hook::rtsFree(handle)
            };
            assert_eq!(rc, 0);
            assert_balance(&c, 0);
            assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
            MODE.store(PLAIN, Ordering::Release);
        }
        assert_eq!(FREE_CALLS.load(Ordering::Acquire), 4);
    }

    #[test]
    fn failed_free_conflict_keeps_newer_record_and_conservative_charge() {
        let c = install(256);
        let mut old = 0;
        assert_eq!(cached(&mut old, 32), 0);
        MODE.store(FREE_REUSE, Ordering::Release);
        REUSED_HANDLE.store(old, Ordering::Release);
        FREE_RESULT.store(17, Ordering::Release);
        let freeing = thread::spawn(move || crate::actual_hook::rtFree(old));
        wait_until(|| FREE_ENTERED.load(Ordering::Acquire));
        let mut new = 0;
        let rc = cached(&mut new, 96);
        RELEASE_FREE.store(true, Ordering::Release);
        assert_eq!(freeing.join().unwrap(), 17);
        assert_eq!(rc, 0);
        assert_eq!(new, old);
        // An inconsistent native backend reports failure despite reusing the
        // handle. Never overwrite the new record or silently release old bytes.
        assert_eq!(
            c.inner
                .hbm_handle_map
                .lock()
                .unwrap()
                .get(&(AllocationKind::Virtual, new))
                .map(|record| record.size),
            Some(96)
        );
        assert_balance(&c, 128);
        MODE.store(PLAIN, Ordering::Release);
        FREE_RESULT.store(0, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFree(new), 0);
        assert_balance(&c, 32);
    }

    #[test]
    fn physical_retain_twice_keeps_one_charge_until_the_third_successful_free() {
        let (current, owner, handle) = physical_client(32);
        for _ in 0..2 {
            let mut retained = 0;
            let out = &mut retained as *mut u64 as u64;
            assert_eq!(retain(&mut retained), 0);
            assert_eq!(retained, handle);
            assert!(crate::calls().iter().any(|call| matches!(call,
                crate::Call::Native("rtMemRetainAllocationHandle", args, _) if args == &vec![0x789a0000, out])));
            assert_balance(&owner, 32);
        }
        for expected in [32, 32, 0] {
            assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
            assert_balance(&owner, expected);
            assert_balance(&current, 0);
        }
        assert!(owner.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_physical_retain_preserves_native_error_and_adds_no_reference() {
        let (_, owner, handle) = physical_client(32);
        RETAIN_RESULT.store(17, Ordering::Release);
        let mut output = 0xbeef;
        assert_eq!(retain(&mut output), 17);
        assert_eq!(output, 0xbeef);
        assert_balance(&owner, 32);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 0);
    }

    #[test]
    fn failed_physical_free_does_not_consume_a_retained_reference() {
        let (_, owner, handle) = physical_client(32);
        let mut output = 0;
        assert_eq!(retain(&mut output), 0);
        FREE_RESULT.store(17, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 17);
        assert_balance(&owner, 32);
        FREE_RESULT.store(0, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 32);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 0);
    }

    #[test]
    fn physical_retain_uses_original_card_and_ignores_equal_virtual_number() {
        let (current, owner, handle) = physical_client(32);
        assert_eq!(current.check_memory_quota(8), 0);
        current.post_alloc_hbm(handle, 8, 0);
        let mut output = 0;
        assert_eq!(retain(&mut output), 0);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 32);
        assert_balance(&current, 8);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 0);
        assert_balance(&current, 8);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&current, 0);
    }

    #[test]
    fn retaining_foreign_backing_does_not_create_a_charge_or_device_client() {
        let current = install(256);
        RETAIN_HANDLE.store(0xf000, Ordering::Release);
        let mut output = 0;
        assert_eq!(retain(&mut output), 0);
        assert_eq!(output, 0xf000);
        assert_eq!(crate::actual_hook::rtFreePhysical(output), 0);
        assert_balance(&current, 0);
        assert!(crate::DEVICE_ONE.get().is_none());
        assert!(current.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    fn concurrent_physical_frees(second_finishes_first: bool, first_fails: bool) {
        let (_, owner, handle) = physical_client(32);
        let mut output = 0;
        assert_eq!(retain(&mut output), 0);
        MODE.store(FREE_MULTI, Ordering::Release);
        if first_fails {
            FIRST_FREE_RESULT.store(17, Ordering::Release);
        }
        let first = thread::spawn(move || crate::actual_hook::rtFreePhysical(handle));
        wait_until(|| FREE_ENTERED_MASK.load(Ordering::Acquire) == 1);
        let second = thread::spawn(move || crate::actual_hook::rtFreePhysical(handle));
        wait_until(|| FREE_ENTERED_MASK.load(Ordering::Acquire) == 3);
        let before = used(&owner);
        // Allow and join exactly one native operation while the other remains
        // paused: no result order may refund the shared backing prematurely.
        let (first_result, second_result, intermediate) = if second_finishes_first {
            FREE_RELEASE_MASK.store(2, Ordering::Release);
            let second_result = second.join().unwrap();
            let intermediate = used(&owner);
            FREE_RELEASE_MASK.store(3, Ordering::Release);
            (first.join().unwrap(), second_result, intermediate)
        } else {
            FREE_RELEASE_MASK.store(1, Ordering::Release);
            let first_result = first.join().unwrap();
            let intermediate = used(&owner);
            FREE_RELEASE_MASK.store(3, Ordering::Release);
            (first_result, second.join().unwrap(), intermediate)
        };
        assert_eq!(before, 32);
        assert_eq!(intermediate, 32);
        assert_eq!(first_result, if first_fails { 17 } else { 0 });
        assert_eq!(second_result, 0);
        assert_balance(&owner, if first_fails { 32 } else { 0 });
        if first_fails {
            MODE.store(PLAIN, Ordering::Release);
            assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
            assert_balance(&owner, 0);
        }
    }

    #[test]
    fn retained_concurrent_frees_do_not_refund_when_first_finishes_first() {
        concurrent_physical_frees(false, false);
    }

    #[test]
    fn retained_concurrent_frees_do_not_refund_when_second_finishes_first() {
        concurrent_physical_frees(true, false);
    }

    #[test]
    fn retained_concurrent_free_failure_keeps_the_failed_reference_charged() {
        concurrent_physical_frees(true, true);
    }

    fn retain_racing_last_free(fails: bool) {
        let (_, owner, handle) = physical_client(32);
        MODE.store(RETAIN_BLOCK, Ordering::Release);
        if fails {
            RETAIN_RESULT.store(17, Ordering::Release);
        }
        let retaining = thread::spawn(|| {
            let mut output = 0;
            let result = retain(&mut output);
            (result, output)
        });
        wait_until(|| RETAIN_ENTERED.load(Ordering::Acquire));
        let free_result = crate::actual_hook::rtFreePhysical(handle);
        let before_retain_return = used(&owner);
        RELEASE_RETAIN.store(true, Ordering::Release);
        let (retain_result, output) = retaining.join().unwrap();
        assert_eq!(free_result, 0);
        assert_eq!(before_retain_return, 32);
        assert_eq!(retain_result, if fails { 17 } else { 0 });
        assert_balance(&owner, if fails { 0 } else { 32 });
        if !fails {
            assert_eq!(output, handle);
            assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
            assert_balance(&owner, 0);
        }
    }

    #[test]
    fn pending_retain_pins_old_backing_until_its_success_is_accounted() {
        retain_racing_last_free(false);
    }

    #[test]
    fn pending_retain_failure_allows_last_free_to_refund_once() {
        retain_racing_last_free(true);
    }

    #[test]
    fn physical_native_free_alias_consumes_only_one_reference() {
        let (_, owner, handle) = physical_client(32);
        let mut output = 0;
        assert_eq!(retain(&mut output), 0);
        MODE.store(FREE_PHYSICAL_ALIAS, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_eq!(FREE_CALLS.load(Ordering::Acquire), 2);
        assert_balance(&owner, 32);
        MODE.store(PLAIN, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 0);
    }

    #[test]
    fn free_alias_guard_keeps_virtual_and_physical_namespaces_distinct() {
        let (current, owner, handle) = physical_client(32);
        assert_eq!(current.check_memory_quota(8), 0);
        current.post_alloc_hbm(handle, 8, 0);
        MODE.store(FREE_CROSS_KIND, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFree(handle), 0);
        assert_balance(&owner, 0);
        assert_balance(&current, 0);
    }

    #[test]
    fn same_output_native_retain_alias_adds_only_one_reference() {
        let (_, owner, handle) = physical_client(32);
        MODE.store(RETAIN_ALIAS, Ordering::Release);
        let mut output = 0;
        assert_eq!(retain(&mut output), 0);
        assert_eq!(RETAIN_CALLS.load(Ordering::Acquire), 2);
        MODE.store(PLAIN, Ordering::Release);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 32);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 0);
    }

    fn retained_reused_generation(free_new_before_return: bool, retain_fails: bool) {
        let (_, owner, old) = physical_client(32);
        MODE.store(RETAIN_BLOCK, Ordering::Release);
        REUSED_HANDLE.store(old, Ordering::Release);
        if retain_fails {
            RETAIN_RESULT.store(17, Ordering::Release);
        }
        let retaining = thread::spawn(|| {
            let mut output = 0;
            (retain(&mut output), output)
        });
        wait_until(|| RETAIN_ENTERED.load(Ordering::Acquire));
        let old_rc = crate::actual_hook::rtFreePhysical(old);
        let mut new = 0;
        let allocation_rc = allocate_kind(&mut new, 96, AllocationKind::Physical);
        let new_free_rc = if free_new_before_return {
            Some(crate::actual_hook::rtFreePhysical(new))
        } else {
            None
        };
        let pinned_usage = used(&owner);
        RELEASE_RETAIN.store(true, Ordering::Release);
        let (retain_rc, output) = retaining.join().unwrap();
        assert_eq!(old_rc, 0);
        assert_eq!(allocation_rc, 0);
        assert_eq!(new, old);
        assert_eq!(new_free_rc, free_new_before_return.then_some(0));
        assert_eq!(
            pinned_usage, 128,
            "both generations stay pinned until retain returns"
        );
        assert_eq!(retain_rc, if retain_fails { 17 } else { 0 });
        if retain_fails {
            assert_balance(&owner, if free_new_before_return { 0 } else { 96 });
            if !free_new_before_return {
                assert_eq!(crate::actual_hook::rtFreePhysical(new), 0);
            }
        } else {
            assert_eq!(output, new);
            assert_balance(&owner, 96);
            assert_eq!(crate::actual_hook::rtFreePhysical(new), 0);
            if !free_new_before_return {
                assert_balance(&owner, 96);
                assert_eq!(crate::actual_hook::rtFreePhysical(new), 0);
            }
        }
        assert_balance(&owner, 0);
        assert!(owner.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn retain_after_numeric_reuse_updates_current_backing_not_old_generation() {
        retained_reused_generation(false, false);
    }

    #[test]
    fn newly_published_reused_backing_stays_charged_during_pending_retain() {
        retained_reused_generation(true, false);
    }

    #[test]
    fn failed_retain_during_reuse_releases_pins_without_adding_a_reference() {
        retained_reused_generation(true, true);
    }

    #[test]
    fn physical_allocation_on_new_client_joins_an_already_active_retain_scope() {
        let current = install(256);
        MODE.store(RETAIN_BLOCK, Ordering::Release);
        let retaining = thread::spawn(|| {
            let mut output = 0;
            (retain(&mut output), output)
        });
        wait_until(|| RETAIN_ENTERED.load(Ordering::Acquire));
        let owner = memory_client(256, 1);
        crate::DEVICE_ONE.set(owner.clone()).unwrap();
        let mut handle = 0;
        let allocation_rc = allocate_kind(&mut handle, 32, AllocationKind::Physical);
        RETAIN_HANDLE.store(handle, Ordering::Release);
        let free_rc = crate::actual_hook::rtFreePhysical(handle);
        let pinned_usage = used(&owner);
        RELEASE_RETAIN.store(true, Ordering::Release);
        let (retain_rc, output) = retaining.join().unwrap();
        assert_eq!(allocation_rc, 0);
        assert_eq!(free_rc, 0);
        assert_eq!(pinned_usage, 32);
        assert_eq!(retain_rc, 0);
        assert_eq!(output, handle);
        assert_balance(&owner, 32);
        assert_balance(&current, 0);
        assert_eq!(crate::actual_hook::rtFreePhysical(handle), 0);
        assert_balance(&owner, 0);
    }

    #[test]
    fn two_pending_retains_each_pin_and_reference_new_generation_once() {
        let (_, owner, old) = physical_client(32);
        MODE.store(RETAIN_BLOCK, Ordering::Release);
        REUSED_HANDLE.store(old, Ordering::Release);
        let first = thread::spawn(|| {
            let mut output = 0;
            (retain(&mut output), output)
        });
        wait_until(|| RETAIN_CALLS.load(Ordering::Acquire) == 1);
        let second = thread::spawn(|| {
            let mut output = 0;
            (retain(&mut output), output)
        });
        wait_until(|| RETAIN_CALLS.load(Ordering::Acquire) == 2);
        let old_rc = crate::actual_hook::rtFreePhysical(old);
        let mut new = 0;
        let allocation_rc = allocate_kind(&mut new, 96, AllocationKind::Physical);
        let new_free_rc = crate::actual_hook::rtFreePhysical(new);
        let pinned_usage = used(&owner);
        RELEASE_RETAIN.store(true, Ordering::Release);
        let first_result = first.join().unwrap();
        let second_result = second.join().unwrap();
        assert_eq!(old_rc, 0);
        assert_eq!(allocation_rc, 0);
        assert_eq!(new_free_rc, 0);
        assert_eq!(new, old);
        assert_eq!(pinned_usage, 128);
        assert_eq!(first_result, (0, new));
        assert_eq!(second_result, (0, new));
        assert_balance(&owner, 96);
        assert_eq!(crate::actual_hook::rtFreePhysical(new), 0);
        assert_balance(&owner, 96);
        assert_eq!(crate::actual_hook::rtFreePhysical(new), 0);
        assert_balance(&owner, 0);
    }

    #[test]
    fn fork_child_does_not_touch_the_parent_retain_scope_mutex() {
        let _parent = install(256);
        let parent_guard = retain_scopes();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let ticket = prepare_hbm_retain(Vec::new);
            ticket.finish(None);
            unsafe { libc::_exit(0) }
        }
        drop(parent_guard);
        assert!(pid > 0);
        let mut status = -1;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if waited == pid {
                break;
            }
            if waited < 0 || Instant::now() >= deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                panic!("child waited on an inherited retain scope mutex");
            }
            thread::yield_now();
        }
        assert_eq!(status, 0);
    }
}
