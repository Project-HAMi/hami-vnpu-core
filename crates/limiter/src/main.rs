// Use the library crate's name to access the module
use limiter::manager::ContainerManager;
use limiter::memory_report;
use limiter::reporter::UtilizationReporter;
use limiter::shmem::{shm_setup, GlobalRegistry, LocalContainerShmem, local_shmem_name_for};
use std::collections::BTreeSet;
use std::thread;
use std::time::{Duration, Instant};
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
use npu_limiter::{
    DeviceUtilization, GetUtilizationRequest, GetUtilizationResponse, MemorySnapshot,
    SetPriorityRequest, SetPriorityResponse,
};

/// gRPC service. Keeps a handle to the shared priority atomic plus one
/// utilization reporter per device managed by this daemon.
#[derive(Clone)]
pub struct LimiterControlService {
    priority_atomic: Arc<AtomicU64>,
    reporters: Arc<Vec<(u32, Arc<UtilizationReporter>)>>,
    /// Read-only mapping of each device's local shmem (same names as managers).
    local_shmems: Arc<Vec<&'static LocalContainerShmem>>,
}

#[tonic::async_trait]
impl LimiterControl for LimiterControlService {
    async fn set_priority(
        &self,
        request: Request<SetPriorityRequest>,
    ) -> Result<Response<SetPriorityResponse>, Status> {
        let new_priority = request.into_inner().priority;
        if !new_priority.is_finite() || new_priority <= 0.0 {
            return Err(Status::invalid_argument(
                "Priority must be a finite value > 0",
            ));
        }
        
        self.priority_atomic.store(new_priority.to_bits(), Ordering::Relaxed);
        log::info!("Updated priority via gRPC to: {}", new_priority);
        
        Ok(Response::new(SetPriorityResponse {
            success: true,
            message: format!("Priority updated to {}", new_priority),
        }))
    }

    async fn get_utilization(
        &self,
        _request: Request<GetUtilizationRequest>,
    ) -> Result<Response<GetUtilizationResponse>, Status> {
        // All reporters on this daemon share the same interval/history config,
        // so any of them can supply the header. Fall back to defaults if the
        // daemon was launched without any devices (which shouldn't happen).
        let (interval_ms, history_scale) = self
            .reporters
            .first()
            .map(|(_, r)| (r.interval_ms(), r.history_scale()))
            .unwrap_or((0, 0));

        let devices = self
            .reporters
            .iter()
            .enumerate()
            .map(|(idx, (dev, r))| {
                let snap = r.snapshot();
                let m = memory_report::memory_metrics(self.local_shmems[idx], idx as i32);
                DeviceUtilization {
                    device_id: *dev,
                    tracked: snap.tracked,
                    utilization_last_interval_percent: snap.last_interval_percent,
                    utilization_recent_windows_avg_percent: snap.recent_avg_percent,
                    memory: Some(MemorySnapshot {
                        limit_enforced: m.limit_enforced,
                        total_mb: m.total_mb,
                        used_mb: m.used_mb,
                        free_mb: m.free_mb,
                    }),
                }
            })
            .collect();

        Ok(Response::new(GetUtilizationResponse {
            interval_ms,
            history_scale,
            devices,
        }))
    }
}

/// How long the main thread waits for each manager to `shm_open` local shmem after spawn.
const OPEN_LOCAL_SHMEM_TIMEOUT: Duration = Duration::from_secs(120);
const OPEN_LOCAL_SHMEM_POLL: Duration = Duration::from_millis(20);

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

    // One utilization reporter per device. The gRPC handler reads snapshots
    // from these Arcs; each reporter thread is started inside its device's
    // manager thread (where we open the shared memory).
    let mut reporters: Vec<(u32, Arc<UtilizationReporter>)> = Vec::with_capacity(devices.len());
    for dev in &devices {
        reporters.push((*dev, UtilizationReporter::from_env()));
    }

    let mut handles = Vec::new();
    for (dev_idx, dev) in devices.iter().enumerate() {
        let global_path = format!("{}_dev{}", global_base, dev);
        let local_path = local_shmem_name_for(*dev);
        let pid_for_thread = pid;
        let p_atomic = priority_atomic.clone();
        let reporter = reporters[dev_idx].1.clone();

        let handle = thread::spawn(move || {
            // 1. Open shared memory. `open_global_registry` returns a
            //    `&'static mut GlobalRegistry`; the reporter needs a shared
            //    `&'static GlobalRegistry` view of the same mapping, and the
            //    manager (further below) consumes the same shared reference.
            //    All fields are atomics, so aliasing as `&` is sound.
            let global_reg_mut: &'static mut GlobalRegistry =
                shm_setup::open_global_registry::<GlobalRegistry>(&global_path);
            let global_reg: &'static GlobalRegistry =
                unsafe { &*(global_reg_mut as *const GlobalRegistry) };
            let local_shm = shm_setup::create_shmem::<LocalContainerShmem>(local_path.as_str());

            // 2. Start the utilization reporter. It locates its slot by
            //    matching on pid, which is written during `ContainerManager::new`
            //    just below, so a brief retry inside the reporter is expected.
            reporter.start(
                global_reg,
                unsafe { &*(local_shm as *const LocalContainerShmem) },
                pid_for_thread,
            );

            // 3. Initialize the manager and enter its scheduling loop.
            let mut manager =
                ContainerManager::new(global_reg, local_shm, pid_for_thread as i32, p_atomic);
            manager.run();
        });
        handles.push(handle);
    }

    // Map each device's local shmem for quota / used reads (managers created it above).
    let mut local_shmems: Vec<&'static LocalContainerShmem> = Vec::with_capacity(devices.len());
    for dev in &devices {
        let path = local_shmem_name_for(*dev);
        let deadline = Instant::now() + OPEN_LOCAL_SHMEM_TIMEOUT;
        let ptr = loop {
            if let Some(p) = shm_setup::try_open_shmem::<LocalContainerShmem>(&path) {
                break p;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out after {:?} waiting for local shmem {} (manager thread may have failed to create it)",
                    OPEN_LOCAL_SHMEM_TIMEOUT, path
                )
                .into());
            }
            thread::sleep(OPEN_LOCAL_SHMEM_POLL);
        };
        local_shmems.push(unsafe { &*(ptr as *const LocalContainerShmem) });
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

    let service = LimiterControlService {
        priority_atomic,
        reporters: Arc::new(reporters),
        local_shmems: Arc::new(local_shmems),
    };

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
