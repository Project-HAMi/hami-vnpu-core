// Use the library crate's name to access the module
use limiter::manager::ContainerManager;
use limiter::shmem::{shm_setup, GlobalRegistry, LocalContainerShmem, local_shmem_name};

fn main() {
    println!("[Daemon] Starting NPU Virtualization Manager...");
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env() 
        .try_init();
    let pid = std::process::id() as i32;

   let global_path = match std::env::var("NPU_GLOBAL_SHM_PATH") {
            Ok(path) => path,
            Err(_) => {
                eprintln!("\n[ERROR] Missing ENV Var: NPU_GLOBAL_SHM_PATH");
                std::process::exit(1);
            }
        };
        
    let local_path = local_shmem_name();
    // 1.Create SHM
    let global_reg = shm_setup::open_global_registry::<GlobalRegistry>(&global_path);
    let local_shm = shm_setup::create_shmem::<LocalContainerShmem>(local_path.as_str());

    // Initialize Manager
    let mut manager = ContainerManager::new(global_reg, local_shm, pid);

    manager.run(); 
}