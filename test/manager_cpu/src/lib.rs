#![allow(dead_code)]
#[path = "../../../crates/limiter/src/dynamic.rs"]
pub mod dynamic;
#[path = "../../../crates/limiter/src/config.rs"]
pub mod config;
#[path = "../../../crates/limiter/src/overdraft.rs"]
pub mod overdraft;
#[path = "../../../crates/limiter/src/shmem/mod.rs"]
pub mod shmem;
// None of these arithmetic tests calls the DCMI/reaper paths.
pub mod worker { pub fn proc_alive(_: i32) -> bool { panic!("unexpected process scan"); } }
pub mod manager {
    include!("../../../crates/limiter/src/manager.rs");
    #[cfg(test)] mod tests;
}
