pub mod worker;
pub mod manager;
pub mod shmem;
pub mod externed_api;
pub mod reporter;
pub mod memory_report;
use ctor::ctor;

#[ctor]
fn init_logger() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env() 
        .try_init();
}