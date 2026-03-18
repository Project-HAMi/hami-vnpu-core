// Use the library crate's name to access the module
use limiter::manager::ContainerManager;
use limiter::shmem::{shm_setup, GlobalRegistry, LocalContainerShmem, local_shmem_name_for};
use std::collections::BTreeSet;
use std::thread;

fn parse_visible_devices() -> Vec<u32> {
    let raw = std::env::var("ASCEND_RT_VISIBLE_DEVICES").unwrap_or_else(|_| "0".to_string());
    let mut set = BTreeSet::new();
    for v in raw.split(',').filter_map(|s| s.trim().parse::<u32>().ok()) {
        set.insert(v);
    }
    if set.is_empty() {
        vec![0]
    } else {
        set.into_iter().collect()
    }
}

fn main() {
    println!("[Daemon] Starting NPU Virtualization Manager...");
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env() 
        .try_init();
    let pid = std::process::id() as i32;

    let global_base = match std::env::var("NPU_GLOBAL_SHM_PATH") {
        Ok(path) => path,
        Err(_) => {
            eprintln!("\n[ERROR] Missing ENV Var: NPU_GLOBAL_SHM_PATH");
            std::process::exit(1);
        }
    };

    let devices = parse_visible_devices();
    if devices.len() > 1 {
        println!("[Daemon] Launching managers for devices: {:?}", devices);
    }

    let mut handles = Vec::new();
    for dev in devices {
        let global_path = format!("{}_dev{}", global_base, dev);
        let local_path = local_shmem_name_for(dev);
        let pid_for_thread = pid;

        let handle = thread::spawn(move || {
            // 1. 创建共享内存 (拥有内存的绝对控制权)
            // 只有 Manager 有权限使用 create_shmem
            let global_reg = shm_setup::open_global_registry::<GlobalRegistry>(&global_path);
            let local_shm = shm_setup::create_shmem::<LocalContainerShmem>(local_path.as_str());

            // 2. 初始化 Manager (调用 new)
            let mut manager = ContainerManager::new(global_reg, local_shm, pid_for_thread as i32);

            // 3. 进入主循环，开始不断调度和分发 Token
            // 这行代码会阻塞线程，直到进程被 Kill
            manager.run();
        });
        handles.push(handle);
    }

    // Block main thread so daemon stays alive even if managers are running on workers.
    for handle in handles {
        let _ = handle.join();
    }
}