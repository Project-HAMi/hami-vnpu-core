// Included within worker::audit; actual_memory_extra includes the production
// memory_extra.rs, with only the native runtime symbols replaced by this mock.
mod runtime_memory_extra {
    use super::*;
    use std::sync::atomic::AtomicU32;

    const PLAIN: u32 = 0;
    const ALIAS: u32 = 1;
    const INDEPENDENT: u32 = 2;
    static MODE: AtomicU32 = AtomicU32::new(PLAIN);
    static ALLOC_RESULT: AtomicU64 = AtomicU64::new(0);
    static FREE_RESULT: AtomicU64 = AtomicU64::new(0);
    static NESTED_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);

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

    fn balance(c: &SchedulerClient, expected: u64) {
        assert_eq!(c.inner.shmem.memory_used.load(Ordering::Acquire), expected);
        assert_eq!(
            c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire),
            expected
        );
    }

    fn alloc(output: &mut u64, size: u64) -> u64 {
        crate::actual_memory_extra::rtDvppMalloc(output as *mut u64 as u64, size, 0xfedc)
    }

    fn flagged(output: &mut u64, size: u64) -> u64 {
        crate::actual_memory_extra::rtDvppMallocWithFlag(
            output as *mut u64 as u64,
            size,
            0x87654321,
            0xfedc,
        )
    }

    fn native_memory(name: &'static str, args: &[u64]) -> Option<u64> {
        if matches!(name, "rtDvppFree" | "rtFree" | "rtsFree") {
            if MODE.load(Ordering::Acquire) == ALIAS {
                if name == "rtDvppFree" {
                    return Some(crate::actual_hook::rtsFree(args[0]));
                }
                if name == "rtsFree" {
                    return Some(crate::actual_hook::rtFree(args[0]));
                }
            }
            return Some(FREE_RESULT.load(Ordering::Acquire));
        }
        if !matches!(name, "rtDvppMalloc" | "rtDvppMallocWithFlag" | "rtMalloc") {
            return None;
        }
        match MODE.load(Ordering::Acquire) {
            ALIAS if name == "rtDvppMalloc" => {
                return Some(crate::actual_memory_extra::rtDvppMallocWithFlag(
                    args[0],
                    args[1],
                    0,
                    args[2] as u16,
                ));
            }
            ALIAS if name == "rtDvppMallocWithFlag" => {
                return Some(crate::actual_hook::rtMalloc(
                    args[0],
                    args[1],
                    args[2] as u32,
                    args[3] as u16,
                ));
            }
            INDEPENDENT if name == "rtDvppMalloc" => {
                let mut nested = 0;
                NESTED_RESULT.store(flagged(&mut nested, 24), Ordering::Release);
            }
            _ => {}
        }
        let rc = ALLOC_RESULT.load(Ordering::Acquire);
        if rc == 0 {
            let handle = crate::NEXT_MEMORY.fetch_add(1, Ordering::Relaxed);
            unsafe { *(args[0] as *mut u64) = handle };
        }
        Some(rc)
    }

    #[test]
    fn dvpp_exact_abi_arguments_and_raw_sizes_are_preserved() {
        let c = install(64);
        let mut plain = 0;
        let mut flagged_pointer = 0;
        assert_eq!(alloc(&mut plain, 17), 0);
        assert_eq!(flagged(&mut flagged_pointer, 23), 0);
        balance(&c, 40);
        let native: Vec<_> = calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Native(name, args, _) => Some((name, args)),
                _ => None,
            })
            .collect();
        assert_eq!(
            native,
            vec![
                (
                    "rtDvppMalloc",
                    vec![&mut plain as *mut u64 as u64, 17, 0xfedc]
                ),
                (
                    "rtDvppMallocWithFlag",
                    vec![
                        &mut flagged_pointer as *mut u64 as u64,
                        23,
                        0x87654321,
                        0xfedc
                    ]
                )
            ]
        );
        assert_eq!(crate::actual_memory_extra::rtDvppFree(plain), 0);
        balance(&c, 23);
        assert_eq!(crate::actual_memory_extra::rtDvppFree(flagged_pointer), 0);
        balance(&c, 0);
    }

    #[test]
    fn dvpp_and_ordinary_allocators_share_one_quota() {
        let c = install(48);
        let mut plain = 0;
        let mut ordinary = 0;
        let mut denied = 0;
        assert_eq!(alloc(&mut plain, 32), 0);
        assert_eq!(
            crate::actual_hook::rtMalloc(&mut ordinary as *mut u64 as u64, 16, 2, 33),
            0
        );
        balance(&c, 48);
        clear_calls();
        assert_eq!(flagged(&mut denied, 1), 207001);
        assert!(
            calls().is_empty(),
            "over-quota allocation must not call native"
        );
        assert_eq!(denied, 0);
        balance(&c, 48);
        assert_eq!(crate::actual_hook::rtFree(plain), 0);
        assert_eq!(crate::actual_memory_extra::rtDvppFree(ordinary), 0);
        balance(&c, 0);
    }

    #[test]
    fn dvpp_native_allocation_errors_roll_back_and_remain_native() {
        let c = install(64);
        ALLOC_RESULT.store(17, Ordering::Release);
        let mut pointer = 0;
        assert_eq!(alloc(&mut pointer, 32), 17);
        balance(&c, 0);
        assert_eq!(flagged(&mut pointer, 32), 17);
        balance(&c, 0);
        c.inner.shmem.memory_limit.store(0, Ordering::Release);
        assert_eq!(alloc(&mut pointer, 32), 17);
        assert_eq!(flagged(&mut pointer, 32), 17);
        balance(&c, 0);
        assert_eq!(pointer, 0);
    }

    #[test]
    fn dvpp_allocation_and_free_alias_chains_charge_and_refund_once() {
        let c = install(32);
        MODE.store(ALIAS, Ordering::Release);
        let mut pointer = 0;
        assert_eq!(alloc(&mut pointer, 32), 0);
        balance(&c, 32);
        assert_eq!(crate::actual_memory_extra::rtDvppFree(pointer), 0);
        balance(&c, 0);
        let names: Vec<_> = calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Native(name, _, _) => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            [
                "rtDvppMalloc",
                "rtDvppMallocWithFlag",
                "rtMalloc",
                "rtDvppFree",
                "rtsFree",
                "rtFree"
            ]
        );
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn dvpp_failed_free_restores_original_card_owner() {
        let c = install(64);
        let other = memory_client(64, 1);
        crate::DEVICE_ONE.set(other.clone()).unwrap();
        let mut pointer = 0;
        assert_eq!(flagged(&mut pointer, 32), 0);
        unsafe { crate::externed_api::rtSetDevice(1) };
        FREE_RESULT.store(17, Ordering::Release);
        assert_eq!(crate::actual_memory_extra::rtDvppFree(pointer), 17);
        balance(&c, 32);
        balance(&other, 0);
        FREE_RESULT.store(0, Ordering::Release);
        assert_eq!(crate::actual_memory_extra::rtDvppFree(pointer), 0);
        balance(&c, 0);
        balance(&other, 0);
        assert!(c.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn dvpp_nested_distinct_output_still_checks_quota() {
        let c = install(48);
        MODE.store(INDEPENDENT, Ordering::Release);
        let mut pointer = 0;
        assert_eq!(alloc(&mut pointer, 32), 0);
        assert_eq!(NESTED_RESULT.load(Ordering::Acquire), 207001);
        balance(&c, 32);
        assert_eq!(crate::actual_memory_extra::rtDvppFree(pointer), 0);
        balance(&c, 0);
    }
}
