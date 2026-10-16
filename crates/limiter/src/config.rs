use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const ENV_GLOBAL_SHM_PATH: &str = "NPU_GLOBAL_SHM_PATH";
pub const ENV_LOCAL_SHM_PATH: &str = "NPU_LOCAL_SHM_PATH";
pub const ENV_PRIORITY: &str = "NPU_PRIORITY";
pub const ENV_MEM_QUOTA_MB: &str = "NPU_MEM_QUOTA";

pub const DEFAULT_LOCAL_SHMEM_PATH: &str = "/tmp/vnpu_local_shmem";
pub const DEFAULT_PRIORITY: f64 = 1.0;
pub const DEFAULT_MEM_QUOTA_MB: u64 = 0;

pub const VIRTUAL_OVERHEAD_MB: usize = 128; // Reserve 128MB as HBM overhead

#[derive(Debug, Clone)]
pub struct ManagerConfig {
    pub global_shm_path: String,
    pub local_shm_path: String,
    pub priority: f64,
    pub memory_limit_mb: u64,
}

impl ManagerConfig {
    pub fn from_env() -> Result<Self, String> {
        let global_shm_path = std::env::var(ENV_GLOBAL_SHM_PATH)
            .map_err(|_| format!("Missing environment variable: {}", ENV_GLOBAL_SHM_PATH))?;

        let local_shm_path = std::env::var(ENV_LOCAL_SHM_PATH)
            .unwrap_or_else(|_| DEFAULT_LOCAL_SHMEM_PATH.to_string());

        let priority = parse_f64(ENV_PRIORITY, DEFAULT_PRIORITY);
        let memory_limit_mb = parse_u64(ENV_MEM_QUOTA_MB, DEFAULT_MEM_QUOTA_MB);

        Ok(Self {
            global_shm_path,
            local_shm_path,
            priority,
            memory_limit_mb,
        })
    }
}

pub fn local_shmem_path() -> String {
    std::env::var(ENV_LOCAL_SHM_PATH).unwrap_or_else(|_| DEFAULT_LOCAL_SHMEM_PATH.to_string())
}

/// Validate without opening either file: closing an alias would release this
/// process's POSIX record lock on the global registry.
pub fn validate_shmem_paths(global: &str, local: &str) -> io::Result<()> {
    let same_inode = match (std::fs::metadata(global), std::fs::metadata(local)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if same_inode
        || resolve_shmem_path(&std::path::absolute(global)?, 0)?
            == resolve_shmem_path(&std::path::absolute(local)?, 0)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NPU_LOCAL_SHM_PATH and NPU_GLOBAL_SHM_PATH must refer to different files",
        ));
    }
    Ok(())
}

// canonicalize requires the file to exist. Resolve existing ancestors and
// symlinks too, since the manager may not have created the files/directories yet.
fn resolve_shmem_path(path: &Path, links: usize) -> io::Result<PathBuf> {
    if links >= 40 {
        return Err(io::Error::other("too many shared-memory path symlinks"));
    }
    if let Ok(target) = std::fs::read_link(path) {
        let target = path.parent().unwrap_or(Path::new("/")).join(target);
        return resolve_shmem_path(&target, links + 1);
    }
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let parent = resolve_shmem_path(parent, links)?;
            match path.components().next_back() {
                Some(std::path::Component::Normal(name)) => Ok(parent.join(name)),
                Some(std::path::Component::ParentDir) => {
                    Ok(parent.parent().unwrap_or(Path::new("/")).to_path_buf())
                }
                _ => Err(io::Error::other("invalid shared-memory path")),
            }
        }
        Err(error) => Err(error),
    }
}

pub fn require_distinct_shmem_paths(global: &str, local: &str) {
    if let Err(error) = validate_shmem_paths(global, local) {
        eprintln!("[vnpu] invalid shared-memory configuration: {error}");
        // A panic is caught by the hook's historical worker-only fallback and
        // would allow the workload to continue without quota enforcement.
        unsafe { libc::_exit(78) }
    }
}

fn parse_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}

fn parse_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}
