// Use the library crate's name to access the module
use limiter::manager::ContainerManager;
use limiter::shmem::{shm_setup, GlobalRegistry, LocalContainerShmem, local_shmem_name_for};
use std::collections::BTreeSet;
use std::thread;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::os::unix::fs::PermissionsExt;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{transport::Server, Request, Response, Status};

pub mod npu_limiter {
    tonic::include_proto!("npu_limiter");
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("limiter_descriptor");
}

use npu_limiter::limiter_control_server::{LimiterControl, LimiterControlServer};
use npu_limiter::{SetPriorityRequest, SetPriorityResponse};

#[derive(Debug, Clone)]
pub struct LimiterControlService {
    priority_atomic: Arc<AtomicU64>,
}

#[tonic::async_trait]
impl LimiterControl for LimiterControlService {
    async fn set_priority(
        &self,
        request: Request<SetPriorityRequest>,
    ) -> Result<Response<SetPriorityResponse>, Status> {
        let new_priority = request.into_inner().priority;
        if new_priority <= 0.0 {
            return Err(Status::invalid_argument("Priority must be > 0"));
        }
        
        self.priority_atomic.store(new_priority.to_bits(), Ordering::Relaxed);
        log::info!("Updated priority via gRPC to: {}", new_priority);
        
        Ok(Response::new(SetPriorityResponse {
            success: true,
            message: format!("Priority updated to {}", new_priority),
        }))
    }
}

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Shared priority across all devices in this container
    let priority_atomic = Arc::new(AtomicU64::new(0.0f64.to_bits()));

    let mut handles = Vec::new();
    for dev in devices {
        let global_path = format!("{}_dev{}", global_base, dev);
        let local_path = local_shmem_name_for(dev);
        let pid_for_thread = pid;
        let p_atomic = priority_atomic.clone();

        let handle = thread::spawn(move || {
            // 1. 创建共享内存 (拥有内存的绝对控制权)
            // 只有 Manager 有权限使用 create_shmem
            let global_reg = shm_setup::open_global_registry::<GlobalRegistry>(&global_path);
            let local_shm = shm_setup::create_shmem::<LocalContainerShmem>(local_path.as_str());

            // 2. 初始化 Manager (调用 new)
            let mut manager = ContainerManager::new(global_reg, local_shm, pid_for_thread as i32, p_atomic);

            // 3. 进入主循环，开始不断调度和分发 Token
            // 这行代码会阻塞线程，直到进程被 Kill
            manager.run();
        });
        handles.push(handle);
    }

    // Start gRPC server on UDS if configured
    let uds_path = std::env::var("NPU_LIMITER_UDS_PATH").unwrap_or_else(|_| "/tmp/npu_limiter.sock".to_string());
    
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(&uds_path);
    
    let uds = UnixListener::bind(&uds_path)?;
    
    // Soften permissions so host users can access it without sudo
    if let Ok(mut perms) = std::fs::metadata(&uds_path).map(|m| m.permissions()) {
        perms.set_mode(0o777);
        let _ = std::fs::set_permissions(&uds_path, perms);
    }
    
    let uds_stream = UnixListenerStream::new(uds);
    
    log::info!("[Daemon] Starting gRPC control server on UDS: {}", uds_path);
    
    let service = LimiterControlService { priority_atomic };
    
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(npu_limiter::FILE_DESCRIPTOR_SET)
        .build_v1()?;
    
    Server::builder()
        .add_service(LimiterControlServer::new(service))
        .add_service(reflection_service)
        .serve_with_incoming(uds_stream)
        .await?;

    // Block main thread so daemon stays alive even if managers are running on workers.
    for handle in handles {
        let _ = handle.join();
    }
    
    Ok(())
}