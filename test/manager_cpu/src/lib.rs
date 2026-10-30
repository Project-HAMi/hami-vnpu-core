#![allow(dead_code)]
#[path = "../../../crates/limiter/src/dynamic.rs"]
pub mod dynamic;
#[path = "../../../crates/limiter/src/config.rs"]
pub mod config;
#[path = "../../../crates/limiter/src/overdraft.rs"]
pub mod overdraft;
#[path = "../../../crates/limiter/src/dcmi.rs"]
pub mod dcmi;
#[path = "../../../crates/limiter/src/dcmi_protocol.rs"]
pub mod dcmi_protocol;
#[path = "../../../crates/limiter/src/registry_lock.rs"]
pub mod registry_lock;
#[path = "../../../crates/limiter/src/shmem/mod.rs"]
pub mod shmem;
#[path = "../../../crates/limiter/src/supervisor.rs"]
pub mod supervisor;
// These tests do not run periodic process scanning or slot reclamation.
pub mod worker {
    pub fn proc_alive(_: i32) -> bool {
        panic!("unexpected process scan");
    }
    pub fn reclaim_dead_process_slots(_: &crate::shmem::LocalContainerShmem) {
        panic!("unexpected process reclamation");
    }
}
pub mod manager {
    include!("../../../crates/limiter/src/manager.rs");
    #[cfg(test)]
    mod tests;
}

#[cfg(test)]
mod dcmi_tests;
#[cfg(test)]
mod registry_tests;

// Older std::process implementations may fork while another test is writing a
// temporary executable. A briefly inherited writable fd makes exec fail with
// ETXTBSY. Serialize fixture lifetimes; each case still runs its own concurrent
// threads/processes and tests the real production locking/timeout behavior.
#[cfg(test)]
static SUBPROCESS_FIXTURES: std::sync::Mutex<()> = std::sync::Mutex::new(());
