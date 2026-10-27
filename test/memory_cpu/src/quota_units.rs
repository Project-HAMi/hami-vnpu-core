use crate::{config::{ManagerConfig, OverdraftConfig, SchedPolicy}, manager::ContainerManager, shmem::{GlobalRegistry, LocalContainerShmem}};
use std::sync::atomic::Ordering;

#[test]
fn quota_conversion_preserves_normal_values_and_never_wraps_to_unlimited() {
    for (mb, expected) in [(0, 0), (19456, 19456 * 1024 * 1024), (u64::MAX, u64::MAX)] {
        let global = Box::leak(Box::new(unsafe { std::mem::zeroed::<GlobalRegistry>() }));
        let local = Box::leak(Box::new(unsafe { std::mem::zeroed::<LocalContainerShmem>() }));
        let config = ManagerConfig {
            physical_device_id: None, global_shm_path: String::new(), local_shm_path: String::new(),
            compute_limit: 50, memory_limit_mb: mb, sched_policy: SchedPolicy::Dynamic,
            overdraft: OverdraftConfig { limit: 10, timeout_us: 5000, poll_interval_us: 1000 },
        };
        let manager = ContainerManager::new(global, local, std::process::id() as i32, config).unwrap();
        assert_eq!(local.memory_limit.load(Ordering::Acquire), expected);
        drop(manager);
    }
}
