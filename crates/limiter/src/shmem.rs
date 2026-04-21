use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64};
pub mod shm_setup {
    use std::ffi::CString;
    use std::mem;
    use std::ptr;
    use libc::{open, ftruncate, mmap, O_RDWR, O_CREAT, PROT_READ, PROT_WRITE, MAP_SHARED};

    /// For Manager to create SHM
    pub fn create_shmem<T>(name: &str) -> &'static mut T {
        unsafe {
            let c_name = CString::new(name).unwrap();
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o666);
            if fd < 0 { panic!("Manager failed to shm_open {}: {}", name, std::io::Error::last_os_error()); }

            let size = mem::size_of::<T>();
            if libc::ftruncate(fd, size as libc::off_t) < 0 {
                panic!("Failed to ftruncate {}: {}", name, std::io::Error::last_os_error());
            }

            let ptr = libc::mmap(ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            if ptr == libc::MAP_FAILED { panic!("Failed to mmap {}: {}", name, std::io::Error::last_os_error()); }

            libc::close(fd);
            &mut *(ptr as *mut T)
        }
    }

    /// For Worker to open SHM
    pub fn open_shmem<T>(name: &str) -> &'static mut T {
        unsafe {
            let c_name = CString::new(name).unwrap();
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0o666);
            if fd < 0 { panic!("Worker failed to open NPU Manager shmem! Is the Daemon running?"); }

            let ptr = libc::mmap(ptr::null_mut(), mem::size_of::<T>(), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            if ptr == libc::MAP_FAILED { panic!("Worker mmap failed"); }

            libc::close(fd);
            &mut *(ptr as *mut T)
        }
    }

    /// Same as `open_shmem`, but returns `None` if the segment does not exist yet.
    pub fn try_open_shmem<T>(name: &str) -> Option<&'static mut T> {
        unsafe {
            let c_name = CString::new(name).unwrap();
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0o666);
            if fd < 0 {
                return None;
            }
            let size = mem::size_of::<T>();
            let ptr = libc::mmap(ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            libc::close(fd);
            if ptr == libc::MAP_FAILED {
                return None;
            }
            Some(&mut *(ptr as *mut T))
        }
    }

    pub fn open_global_registry<T>(path: &str) -> &'static mut T {

        let c_path = CString::new(path).unwrap();
        println!("open global registry path is {:?}", path);
        let mut fd = unsafe { open(c_path.as_ptr(), O_RDWR) };
        let mut needs_init = false;

        if fd < 0 {
            // File not exist: First Pod
            println!("[Global] Global Registry not exist, now creating...");
            fd = unsafe { open(c_path.as_ptr(), O_RDWR | O_CREAT, 0o666) };
            if fd < 0 { panic!("cannot open: {}", path); }
            needs_init = true;
        }

        let size = std::mem::size_of::<T>();

        if needs_init {
            unsafe { ftruncate(fd, size as i64) };
        }

        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                size,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED { panic!("mmap failed"); }

        let reg = unsafe { &mut *(ptr as *mut T) };

        if needs_init {
            // TODO
            // 这里手动调用初始化逻辑，比如把锁所有权设为某个无效值
            // (reg as *mut GlobalRegistry).initialize_fields();
        }
        println!("connect to global registry");
        reg
    }
}

// =========================================================================================
// 1. FUTEX PRIMITIVES (Linux Only)
// =========================================================================================
// This implementation is now centralized. Both Manager and Worker import this.

pub mod futex {
    use std::sync::atomic::AtomicU32;
    
    // Linux Futex Opcodes
    const FUTEX_WAIT: i32 = 0;
    const FUTEX_WAKE: i32 = 1;

    /// Sleep until the value at `atom` is NOT `expected`.
    /// This is efficient: it only enters the kernel if the value matches.
    pub fn wait(atom: &AtomicU32, expected: u32) {
        unsafe {
            let ptr = atom as *const AtomicU32 as *mut u32;
            // syscall(SYS_futex, uaddr, FUTEX_WAIT, val, timeout, uaddr2, val3)
            // Timeout is NULL (wait forever)
            libc::syscall(libc::SYS_futex, ptr, FUTEX_WAIT, expected, 0, 0, 0);
        }
    }

    /// Sleep with a timeout (microseconds) until the value at `atom` is NOT `expected`.
    /// Keeps retrying on spurious wake-ups, but lets callers re-check state periodically.
    pub fn wait_timeout(atom: &AtomicU32, expected: u32, timeout_us: u64) {
        unsafe {
            let ptr = atom as *const AtomicU32 as *mut u32;
            let ts = libc::timespec {
                tv_sec: (timeout_us / 1_000_000) as libc::time_t,
                tv_nsec: ((timeout_us % 1_000_000) * 1000) as libc::c_long,
            };
            libc::syscall(
                libc::SYS_futex,
                ptr,
                FUTEX_WAIT,
                expected,
                &ts as *const libc::timespec,
                0,
                0,
            );
        }
    }

    /// Wake up `n` threads waiting on this atomic.
    pub fn wake(atom: &AtomicU32, n: i32) {
        unsafe {
            let ptr = atom as *const AtomicU32 as *mut u32;
            libc::syscall(libc::SYS_futex, ptr, FUTEX_WAKE, n, 0, 0, 0);
        }
    }

    /// Wake up ALL threads waiting on this atomic.
    pub fn wake_all(atom: &AtomicU32) {
        unsafe {
            let ptr = atom as *const AtomicU32 as *mut u32;
            // i32::MAX tells the kernel to wake everyone
            libc::syscall(libc::SYS_futex, ptr, FUTEX_WAKE, i32::MAX, 0, 0, 0);
        }
    }
}

// =========================================================================================
// 2. GLOBAL SHARED MEMORY (The "Stadium")
// =========================================================================================
// This is mapped by every Container Manager.
pub const MAX_MANAGERS: usize = 64;

#[repr(C)]
pub struct GlobalManagerSlot {
    pub pid: AtomicI32,             // OS PID of the Manager Process
    pub avg_kernel_time: AtomicU64, // The "Score" used for anchor calculation
    pub last_heartbeat: AtomicU64,  // Timestamp for Zombie Detection
    pub is_active: AtomicU32,       // 1 = Active, 0 = Dead/Empty
}

#[repr(C)]
pub struct GlobalRegistry {
    // --- The Baton (Mutex) ---
    pub lock_owner: AtomicU32,      // Index of current owner in `slots`
    pub lock_timestamp: AtomicU64,  // Timestamp of when lock was acquired (or heartbeat)
    
    // --- The Queue (Ring Buffer) ---
    pub queue_head: AtomicU32,
    pub queue_tail: AtomicU32,
    pub queue: [AtomicU32; MAX_MANAGERS], // Stores Manager Indices (not PIDs)
    
    // --- The Scoreboard ---
    pub slots: [GlobalManagerSlot; MAX_MANAGERS],
    
    // --- Synchronization Signal ---
    // Waiters sleep on this. Owner increments this to wake next.
    pub signal_counter: AtomicU32,
}

// =========================================================================================
// 3. LOCAL SHARED MEMORY (The "Locker Room")
// =========================================================================================
// This is mapped by 1 Manager + N Workers inside a single container.

/// Default base for `shm_open`; must be a single path segment with a leading `/`
/// (POSIX shared memory object names).
pub const LOCAL_SHMEM_NAME: &str = "/vnpu_local_session";
pub const LOCAL_SHMEM_ENV: &str = "NPU_LOCAL_SHM_NAME";

fn posix_shm_base_name(raw: &str) -> String {
    let name = raw.trim();
    if name.is_empty() {
        return LOCAL_SHMEM_NAME.to_string();
    }
    if name.starts_with('/') {
        name.to_string()
    } else {
        format!("/{}", name)
    }
}

/// Resolve the local shmem name for a specific physical device id.
/// We suffix the base name with the physical id so managers/workers on different
/// devices do not collide.
pub fn local_shmem_name_for(device_phy_id: u32) -> String {
    let base = std::env::var(LOCAL_SHMEM_ENV).unwrap_or_else(|_| LOCAL_SHMEM_NAME.to_string());
    let base = posix_shm_base_name(&base);
    format!("{}_{}", base, device_phy_id)
}

/// Backwards-compatible helper (defaults to device 0). Prefer `local_shmem_name_for`.
pub fn local_shmem_name() -> String {
    local_shmem_name_for(0)
}
pub const MAX_WORKERS: usize = 32;

// Local State Constants
pub const STATE_IDLE: u32 = 0;      // Manager waiting for Global
pub const STATE_RUNNING: u32 = 1;   // Tokens available
pub const STATE_MEASURING: u32 = 2; // Stop & Report

#[repr(C)]
#[derive(Debug)]
pub struct LocalWorkerReport {
    pub batch_id: AtomicU64,      // Generation ID this report belongs to
    pub cpu_start_us: AtomicU64,  // Wall clock start (Anchor)
    pub duration_us: AtomicU64,   // GPU Duration (Event Elapsed Time)
    pub occupied: AtomicU32,      // 1 = Slot taken by thread, 0 = Free
}

#[repr(C)]
#[derive(Debug)]
pub struct LocalContainerShmem {
    pub memory_limit: AtomicU64,
    pub memory_used: AtomicU64,
    // --- Control Flags ---
    pub state: AtomicU32,         // IDLE / RUNNING / MEASURING (Futex here)
    pub batch_id: AtomicU64,      // "Generation" ID (Incremented on every round)
    
    // --- Token Bucket ---
    pub tokens_remaining: AtomicU64,
    
    // --- Sync Barriers ---
    pub active_workers: AtomicU32,    // How many threads took a token?
    pub reported_count: AtomicU32,    // How many threads finished reporting?

    /// Successful token `fetch_sub` count (cumulative, process-wide). The utilization
    /// reporter uses the per-window delta: if it is 0, baton time does not count as busy.
    pub tokens_consumed_cumulative: AtomicU64,
    
    // --- Data Slots ---
    pub reports: [LocalWorkerReport; MAX_WORKERS],
}