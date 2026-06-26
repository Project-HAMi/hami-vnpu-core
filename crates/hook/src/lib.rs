use std::sync::Mutex;

use limiter::worker::SchedulerClient;

/// PID-aware factory: detects fork and creates a fresh SchedulerClient for the child.
/// Falls back to a no-op stub if shmem is not available (e.g. in TBE compiler subprocesses).
static LIMITER: Mutex<Option<(i32, SchedulerClient)>> = Mutex::new(None);

pub fn npu_limiter() -> SchedulerClient {
    let pid = std::process::id() as i32;
    let mut guard = LIMITER.lock().unwrap();
    if let Some((old_pid, ref client)) = *guard {
        if old_pid == pid {
            return client.clone();
        }
    }
    let client = std::panic::catch_unwind(std::panic::AssertUnwindSafe(SchedulerClient::new))
        .unwrap_or_else(|e| {
            log::warn!("SchedulerClient init failed (PID {}), using stub: {:?}", pid, e);
            SchedulerClient::stub()
        });
    *guard = Some((pid, client.clone()));
    client
}

macro_rules! passthrough {
    ($name:expr, ($($sig:tt)*), $($arg:expr),*) => {
        {
            static REAL: ::once_cell::sync::Lazy<extern "C" fn($($sig)*) -> u64> = 
                ::once_cell::sync::Lazy::new(|| unsafe {
                    let ptr = libc::dlsym(libc::RTLD_NEXT, concat!($name, "\0").as_ptr() as *const libc::c_char);
                    if ptr.is_null() {
                        panic!("cannot find original function: {}", $name);
                    }
                    std::mem::transmute(ptr)
                });
            // println!("in func {:?}", $name);
            (*REAL)($($arg),*)
        }
    };
}

mod hook;
mod signal_compat;