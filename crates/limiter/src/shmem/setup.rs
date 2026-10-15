use std::ffi::CString;
use std::mem;
use std::ptr;
use std::path::Path;

use libc::{close, fstat, ftruncate, mmap, open, MAP_FAILED, MAP_SHARED, O_CREAT, O_EXCL, O_RDWR, PROT_READ, PROT_WRITE, EEXIST, c_uint, off_t};

use std::sync::atomic::{AtomicU64, Ordering};

use crate::shmem::{GlobalRegistry, MAX_MANAGERS};

static GLOBAL_REGISTRY_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Create a file-backed shared memory region.
/// `path` is a full filesystem path; parent directories are created if missing.
pub fn create_shmem<T>(path: &str) -> &'static T {
    // Ensure parent directory exists
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    unsafe {
        let c_path = CString::new(path).unwrap();
        let fd = open(c_path.as_ptr(), O_CREAT | O_RDWR, 0o666 as c_uint);
        if fd < 0 { panic!("Manager failed to create shmem file {}: {}", path, std::io::Error::last_os_error()); }

        let size = mem::size_of::<T>();
        if ftruncate(fd, size as off_t) < 0 {
            panic!("Failed to ftruncate {}: {}", path, std::io::Error::last_os_error());
        }

        let ptr = mmap(ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if ptr == MAP_FAILED { panic!("Failed to mmap {}: {}", path, std::io::Error::last_os_error()); }

        close(fd);
        &*(ptr as *const T)
    }
}

/// Open an existing file-backed shared memory (panics on failure).
pub fn open_shmem<T>(path: &str) -> &'static T {
    try_open_shmem(path).expect("Worker failed to open NPU Manager shmem! Is the Daemon running?")
}

/// Non-panicking version: returns None if the shmem file is not available
/// or not yet fully sized.
pub fn try_open_shmem<T>(path: &str) -> Option<&'static T> {
    unsafe {
        let c_path = CString::new(path).unwrap();
        let fd = open(c_path.as_ptr(), O_RDWR);
        if fd < 0 { return None; }

        // The manager creates the file with open(O_CREAT) then ftruncate()s it; a
        // worker may open it in between. Guard against mapping a too-small file,
        // which would mmap fine but SIGBUS on later access.
        let mut st: libc::stat = mem::zeroed();
        if fstat(fd, &mut st) < 0 || (st.st_size as usize) < mem::size_of::<T>() {
            close(fd);
            return None;
        }

        let ptr = mmap(ptr::null_mut(), mem::size_of::<T>(), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        close(fd);
        if ptr == MAP_FAILED { return None; }
        Some(&*(ptr as *const T))
    }
}

/// Open or atomically create the host-wide registry and map its complete layout.
///
/// The creator sizes and initializes a same-directory temporary file before publishing
/// it with a hard link; concurrent openers never observe a partial final path.
pub fn open_global_registry(path: &str) -> &'static GlobalRegistry {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("cannot create global registry directory {}: {e}", parent.display()));
        }
    }

    let size = std::mem::size_of::<GlobalRegistry>();

    let c_path = CString::new(path).unwrap();
    loop {
        let existing_fd = unsafe { open(c_path.as_ptr(), O_RDWR) };
        if existing_fd >= 0 {
            return map_existing_registry(existing_fd, size, path);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
            panic!("cannot open global registry {}: {}", path, std::io::Error::last_os_error());
        }

        // Never publish the final pathname until the file has its complete size and
        // initialized header. If this process crashes, only the temporary pathname is
        // left behind and a later creator can safely retry.
        let sequence = GLOBAL_REGISTRY_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = format!(
            "{}.tmp.{}.{}",
            path,
            std::process::id(),
            sequence,
        );
        let c_temp_path = CString::new(temp_path.as_str()).unwrap();
        let temp_fd = unsafe {
            open(
                c_temp_path.as_ptr(),
                O_RDWR | O_CREAT | O_EXCL,
                0o666 as c_uint,
            )
        };
        if temp_fd < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(EEXIST) {
                continue;
            }
            panic!("cannot create temporary global registry {}: {}", temp_path, std::io::Error::last_os_error());
        }

        if unsafe { ftruncate(temp_fd, size as off_t) } < 0 {
            unsafe {
                close(temp_fd);
                libc::unlink(c_temp_path.as_ptr());
            }
            panic!("failed to size global registry {}: {}", path, std::io::Error::last_os_error());
        }

        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                size,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                temp_fd,
                0,
            )
        };
        if ptr == MAP_FAILED {
            unsafe {
                close(temp_fd);
                libc::unlink(c_temp_path.as_ptr());
            }
            panic!("mmap failed for global registry {}: {}", path, std::io::Error::last_os_error());
        }

        let reg = unsafe { &*(ptr as *const GlobalRegistry) };
        reg.lock_owner.store(MAX_MANAGERS as u32, Ordering::Release);
        if unsafe { libc::msync(ptr, size, libc::MS_SYNC) } < 0 {
            unsafe {
                libc::munmap(ptr, size);
                close(temp_fd);
                libc::unlink(c_temp_path.as_ptr());
            }
            panic!("failed to publish global registry {}: {}", path, std::io::Error::last_os_error());
        }

        let linked = unsafe { libc::link(c_temp_path.as_ptr(), c_path.as_ptr()) } == 0;
        let link_error = if linked {
            None
        } else {
            Some(std::io::Error::last_os_error())
        };
        unsafe {
            libc::unlink(c_temp_path.as_ptr());
            close(temp_fd);
        }
        if linked {
            return reg;
        }

        unsafe { libc::munmap(ptr, size); }
        if link_error.as_ref().and_then(std::io::Error::raw_os_error) == Some(EEXIST) {
            // Another creator won the publish race. Open its complete file on the
            // next iteration and discard this fully initialized temporary mapping.
            continue;
        }
        panic!("cannot publish global registry {}: {}", path, link_error.unwrap());
    }
}

fn map_existing_registry(fd: libc::c_int, size: usize, path: &str) -> &'static GlobalRegistry {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let sized = unsafe { fstat(fd, &mut stat) == 0 && stat.st_size as usize >= size };
        if sized {
            break;
        }
        if std::time::Instant::now() >= deadline {
            unsafe { close(fd); }
            panic!("timed out waiting for global registry {} to be sized", path);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
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
    unsafe { close(fd); }
    if ptr == MAP_FAILED {
        panic!("mmap failed for global registry {}: {}", path, std::io::Error::last_os_error());
    }
    unsafe { &*(ptr as *const GlobalRegistry) }
}
