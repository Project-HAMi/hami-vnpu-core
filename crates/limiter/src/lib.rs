pub mod worker;
pub mod manager;
pub mod shmem;
pub mod externed_api;
use ctor::ctor;
use once_cell::sync::Lazy;
use std::sync::Once;
use std::thread;
use log::info;
use crate::shmem::*;
use crate::manager::ContainerManager;

#[ctor]
fn init_logger() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env() 
        .try_init();
}

static START_MANAGER: Once = Once::new();

pub fn ensure_manager_initialized() {
    START_MANAGER.call_once(|| {
        let pid = std::process::id() as i32;
        let global_path = std::env::var("NPU_GLOBAL_SHM_PATH").expect("Missing NPU_GLOBAL_SHM_PATH");
        let local_path = local_shmem_name();
       
        let global_reg = shm_setup::open_global_registry::<GlobalRegistry>(&global_path);
        let local_shm = shm_setup::create_shmem::<LocalContainerShmem>(local_path.as_str());

        thread::spawn(move || {
            let mut manager = ContainerManager::new(global_reg, local_shm, pid);
            info!("[Background-Manager] Thread started for PID {}", pid);
            manager.run(); // loop
        });
    });
}