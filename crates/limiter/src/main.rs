// Use the library crate's name to access the module
use limiter::config::ManagerConfig;
use limiter::manager::ContainerManager;
use limiter::shmem::{setup, LocalContainerShmem};

fn main() {
    println!("[Daemon] Starting NPU Virtualization Manager...");

    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env() 
        .try_init();

    let config = ManagerConfig::from_env().unwrap_or_else(|err| {
        eprintln!("\n[ERROR] {}", err);
        std::process::exit(1);
    });

    // 1.Create SHM
    let global_reg = setup::open_global_registry(&config.global_shm_path);

    let local_shm = setup::create_shmem::<LocalContainerShmem>(&config.local_shm_name);

    // Initialize Manager
    let mut manager = ContainerManager::new(global_reg, local_shm, std::process::id() as _, config);

    manager.run(); 
}