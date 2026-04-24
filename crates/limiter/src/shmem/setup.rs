use std::ffi::CString;
use std::mem;
use std::ptr;

use libc::{open, close, ftruncate, mmap, shm_open, O_RDWR, O_CREAT, PROT_READ, PROT_WRITE, MAP_SHARED, MAP_FAILED, off_t};

use crate::shmem::GlobalRegistry;
/// For Manager to create SHM
pub fn create_shmem<T>(name: &str) -> &'static T {
    unsafe {
        let c_name = CString::new(name).unwrap();
        let fd = shm_open(c_name.as_ptr(), O_CREAT | O_RDWR, 0o666);
        if fd < 0 { panic!("Manager failed to shm_open {}: {}", name, std::io::Error::last_os_error()); }

        let size = mem::size_of::<T>();
        if ftruncate(fd, size as off_t) < 0 {
            panic!("Failed to ftruncate {}: {}", name, std::io::Error::last_os_error());
        }

        let ptr = mmap(ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if ptr == MAP_FAILED { panic!("Failed to mmap {}: {}", name, std::io::Error::last_os_error()); }

        close(fd);
        &*(ptr as *const T)
    }
}

/// For Worker to open SHM
pub fn open_shmem<T>(name: &str) -> &'static T {
    unsafe {
        let c_name = CString::new(name).unwrap();
        let fd = shm_open(c_name.as_ptr(), O_RDWR, 0o666);
        if fd < 0 { panic!("Worker failed to open NPU Manager shmem! Is the Daemon running?"); }

        let ptr = mmap(ptr::null_mut(), mem::size_of::<T>(), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if ptr == MAP_FAILED { panic!("Worker mmap failed"); }

        close(fd);
        &*(ptr as *const T)
    }
}

pub fn open_global_registry(path: &str) -> &'static GlobalRegistry {
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

    let size = std::mem::size_of::<GlobalRegistry>();

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

    if ptr == MAP_FAILED { panic!("mmap failed"); }

    let reg = unsafe { &*(ptr as *const GlobalRegistry) };

    if needs_init {
        // TODO
        // 这里手动调用初始化逻辑，比如把锁所有权设为某个无效值
        // (reg as *mut GlobalRegistry).initialize_fields();
    }
    println!("connect to global registry");
    reg
}

