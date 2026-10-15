#![allow(dead_code)]
#[path = "../../../crates/limiter/src/config.rs"]
pub mod config;
#[path = "../../../crates/limiter/src/dcmi.rs"]
pub mod dcmi;
#[path = "../../../crates/limiter/src/dcmi_protocol.rs"]
pub mod dcmi_protocol;
#[path = "../../../crates/limiter/src/registry_lock.rs"]
pub mod registry_lock;
#[path = "../../../crates/limiter/src/shmem/mod.rs"]
pub mod shmem;
// The manager tests do not run the periodic process reaper.
pub mod worker {
    pub fn proc_alive(_: i32) -> bool {
        panic!("unexpected process scan");
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
