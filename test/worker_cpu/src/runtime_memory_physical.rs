// Included within worker::audit. The production RTS physical wrapper and
// shared worker are compiled unchanged; only native runtime calls are mocked.
mod runtime_memory_physical {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Attr {
        kind: u32,
        value: [u32; 2],
    }
    #[repr(C)]
    struct Config {
        attrs: *const Attr,
        count: usize,
    }

    static ALLOC_RESULT: AtomicU64 = AtomicU64::new(0);
    static FIXED_HANDLE: AtomicU64 = AtomicU64::new(0);
    static SEEN_TARGET_USED: AtomicU64 = AtomicU64::new(0);

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

    fn install(limit: u64) -> (SchedulerClient, SchedulerClient) {
        let first = memory_client(limit, 6);
        let second = memory_client(limit, 7);
        crate::CLIENT.set(first.clone()).unwrap();
        crate::DEVICE_ONE.set(second.clone()).unwrap();
        assert!(crate::NATIVE_OVERRIDE.set(native_memory).is_ok());
        (first, second)
    }

    fn used(c: &SchedulerClient) -> u64 {
        c.inner.shmem.memory_used.load(Ordering::Acquire)
    }

    fn balance(c: &SchedulerClient, expected: u64) {
        assert_eq!(used(c), expected);
        assert_eq!(
            c.inner.shmem.procs[0].hbm_used[0].load(Ordering::Acquire),
            expected
        );
    }

    fn device(id: u32) -> Attr {
        Attr {
            kind: 2,
            value: [id, 0xdecafbad],
        }
    }

    fn configured(output: &mut u64, size: u64, attrs: &[Attr]) -> u64 {
        let cfg = Config {
            attrs: attrs.as_ptr(),
            count: attrs.len(),
        };
        crate::actual_memory_physical::rtsMemMallocPhysical(
            output as *mut u64 as u64,
            size,
            0x1002,
            &cfg as *const Config as u64,
        )
    }

    fn native_memory(name: &'static str, args: &[u64]) -> Option<u64> {
        if matches!(name, "rtFreePhysical" | "rtFree") {
            return Some(0);
        }
        let target = if name == "rtsMemMallocPhysical" {
            if args[0] == 0 || args[1] == 0 || !matches!(args[2], 0x1001 | 0x1002) {
                return Some(107000);
            }
            let mut target = crate::externed_api::current_device();
            if args[3] != 0 {
                let cfg = unsafe { &*(args[3] as *const Config) };
                if cfg.attrs.is_null() || cfg.count == 0 {
                    return Some(107000);
                }
                for index in 0..cfg.count {
                    let attr = unsafe { *cfg.attrs.add(index) };
                    match attr.kind {
                        0 | 1 => {}
                        2 => target = attr.value[0] as i32,
                        _ => return Some(107000),
                    }
                }
            }
            if !matches!(target, 0 | 1) {
                return Some(107001);
            }
            target
        } else if name == "rtMalloc" {
            crate::externed_api::current_device()
        } else {
            return None;
        };
        SEEN_TARGET_USED.store(
            used(crate::npu_limiter_for_device(target)),
            Ordering::Release,
        );
        let rc = ALLOC_RESULT.load(Ordering::Acquire);
        if rc == 0 {
            let fixed = FIXED_HANDLE.load(Ordering::Acquire);
            let handle = if fixed != 0 {
                fixed
            } else {
                crate::NEXT_MEMORY.fetch_add(1, Ordering::Relaxed)
            };
            unsafe { *(args[0] as *mut u64) = handle };
        }
        Some(rc)
    }

    #[test]
    fn rts_physical_null_config_uses_current_logical_card() {
        let (first, second) = install(64);
        let function: extern "C" fn(u64, u64, u32, u64) -> u64 =
            crate::actual_memory_physical::rtsMemMallocPhysical;
        let mut a = 0;
        let mut b = 0;
        assert_eq!(function(&mut a as *mut u64 as u64, 17, 0x1001, 0), 0);
        unsafe { crate::externed_api::rtSetDevice(1) };
        assert_eq!(function(&mut b as *mut u64 as u64, 23, 0x1002, 0), 0);
        balance(&first, 17);
        balance(&second, 23);
        assert_eq!(crate::actual_hook::rtFreePhysical(a), 0);
        assert_eq!(crate::actual_hook::rtFreePhysical(b), 0);
        balance(&first, 0);
        balance(&second, 0);
    }

    #[test]
    fn rts_physical_explicit_target_can_differ_from_current_card() {
        let (first, second) = install(64);
        let mut a = 0;
        let mut b = 0;
        assert_eq!(configured(&mut b, 23, &[device(1)]), 0);
        assert_eq!(crate::externed_api::current_device(), 0);
        balance(&first, 0);
        balance(&second, 23);
        unsafe { crate::externed_api::rtSetDevice(1) };
        assert_eq!(configured(&mut a, 17, &[device(0)]), 0);
        assert_eq!(crate::externed_api::current_device(), 1);
        balance(&first, 17);
        balance(&second, 23);
        assert_eq!(crate::actual_hook::rtFreePhysical(a), 0);
        unsafe { crate::externed_api::rtSetDevice(0) };
        assert_eq!(crate::actual_hook::rtFreePhysical(b), 0);
        balance(&first, 0);
        balance(&second, 0);
    }

    #[test]
    fn rts_physical_last_device_attribute_wins_without_rewriting_config() {
        let (first, second) = install(64);
        assert_eq!(std::mem::size_of::<Attr>(), 12);
        assert_eq!(std::mem::offset_of!(Attr, value), 4);
        assert_eq!(std::mem::size_of::<Config>(), 16);
        let attrs = [
            device(6),
            Attr {
                kind: 1,
                value: [0xfedc, 0x12345678],
            },
            device(1),
        ];
        let original = attrs;
        let cfg = Config {
            attrs: attrs.as_ptr(),
            count: attrs.len(),
        };
        let mut pointer = 0;
        let output = &mut pointer as *mut u64 as u64;
        let config = &cfg as *const Config as u64;
        assert_eq!(
            crate::actual_memory_physical::rtsMemMallocPhysical(output, 19, 0x1002, config),
            0
        );
        assert_eq!(attrs, original);
        assert_eq!(cfg.attrs, attrs.as_ptr());
        assert_eq!(cfg.count, attrs.len());
        assert!(calls().iter().any(|call| matches!(call,
            Call::Native("rtsMemMallocPhysical", args, _) if args == &vec![output, 19, 0x1002, config]
        )));
        balance(&first, 0);
        balance(&second, 19);
        assert_eq!(crate::actual_hook::rtFreePhysical(pointer), 0);
        balance(&second, 0);
    }

    #[test]
    fn rts_physical_config_without_device_uses_current_and_preserves_opaque_values() {
        let (first, second) = install(64);
        unsafe { crate::externed_api::rtSetDevice(1) };
        let attrs = [
            Attr {
                kind: 0,
                value: [u32::MAX, 0x76543210],
            },
            Attr {
                kind: 1,
                value: [0x1234fedc, 0xabcdef01],
            },
        ];
        let original = attrs;
        let mut pointer = 0;
        assert_eq!(configured(&mut pointer, 17, &attrs), 0);
        assert_eq!(attrs, original);
        balance(&first, 0);
        balance(&second, 17);
        assert_eq!(crate::actual_hook::rtFreePhysical(pointer), 0);
        balance(&second, 0);
    }

    #[test]
    fn rts_physical_invalid_targets_and_config_preserve_native_errors() {
        let (first, second) = install(64);
        let mut pointer = 0;
        for target in [6, 7, u32::MAX] {
            assert_eq!(configured(&mut pointer, 17, &[device(target)]), 107001);
        }
        assert_eq!(
            configured(&mut pointer, 17, &[device(1), device(6)]),
            107001
        );
        let empty = Config {
            attrs: std::ptr::null(),
            count: 0,
        };
        assert_eq!(
            crate::actual_memory_physical::rtsMemMallocPhysical(
                &mut pointer as *mut u64 as u64,
                17,
                0x1002,
                &empty as *const Config as u64,
            ),
            107000
        );
        let unknown = [
            Attr {
                kind: 99,
                value: [1, 2],
            },
            device(1),
        ];
        assert_eq!(configured(&mut pointer, 17, &unknown), 107000);
        assert_eq!(
            crate::actual_memory_physical::rtsMemMallocPhysical(
                &mut pointer as *mut u64 as u64,
                17,
                0x87654321,
                0,
            ),
            107000
        );
        assert!(calls().iter().any(|call| matches!(call,
            Call::Native("rtsMemMallocPhysical", args, _) if args[2] == 0x87654321
        )));
        balance(&first, 0);
        balance(&second, 0);
        assert_eq!(pointer, 0);
    }

    #[test]
    fn rts_physical_target_quota_rejects_before_native_and_failure_rolls_back() {
        let (first, second) = install(16);
        let mut pointer = 0;
        let mut denied = 0;
        assert_eq!(configured(&mut pointer, 12, &[device(1)]), 0);
        balance(&second, 12);
        clear_calls();
        assert_eq!(configured(&mut denied, 8, &[device(1)]), 207001);
        assert!(calls().is_empty());
        balance(&first, 0);
        balance(&second, 12);
        assert_eq!(crate::actual_hook::rtFreePhysical(pointer), 0);
        ALLOC_RESULT.store(37, Ordering::Release);
        assert_eq!(configured(&mut denied, 8, &[device(1)]), 37);
        assert_eq!(SEEN_TARGET_USED.load(Ordering::Acquire), 8);
        balance(&first, 0);
        balance(&second, 0);
        assert!(second.inner.hbm_handle_map.lock().unwrap().is_empty());
    }

    #[test]
    fn rts_physical_handles_stay_separate_from_equal_virtual_addresses() {
        let (first, second) = install(64);
        FIXED_HANDLE.store(0x12345678, Ordering::Release);
        let mut physical = 0;
        let mut virtual_pointer = 0;
        assert_eq!(configured(&mut physical, 12, &[device(0)]), 0);
        assert_eq!(
            crate::actual_hook::rtMalloc(&mut virtual_pointer as *mut u64 as u64, 7, 2, 33),
            0
        );
        assert_eq!(physical, virtual_pointer);
        balance(&first, 19);
        assert_eq!(crate::actual_hook::rtFreePhysical(physical), 0);
        balance(&first, 7);
        assert_eq!(crate::actual_hook::rtFree(virtual_pointer), 0);
        balance(&first, 0);
        balance(&second, 0);
    }

    #[test]
    fn rts_physical_unlimited_and_invalid_output_keep_native_validation() {
        let (first, second) = install(0);
        let mut pointer = 0;
        assert_eq!(configured(&mut pointer, 17, &[device(1)]), 0);
        balance(&first, 0);
        balance(&second, 0);
        assert!(second.inner.hbm_handle_map.lock().unwrap().is_empty());
        assert_eq!(crate::actual_hook::rtFreePhysical(pointer), 0);
        ALLOC_RESULT.store(37, Ordering::Release);
        assert_eq!(configured(&mut pointer, 17, &[device(1)]), 37);
        assert_eq!(
            crate::actual_memory_physical::rtsMemMallocPhysical(0, 17, 0x1002, 0),
            107000
        );
        assert_eq!(
            crate::actual_memory_physical::rtsMemMallocPhysical(
                &mut pointer as *mut u64 as u64,
                0,
                0x1002,
                0
            ),
            107000
        );
        balance(&first, 0);
        balance(&second, 0);
    }
}
