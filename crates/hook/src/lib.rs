use std::sync::atomic::{AtomicI32, Ordering};

use limiter::worker::SchedulerClient;

/// PID that already started a supervisor thread — one per process, re-armed after fork.
static SUPERVISOR_PID: AtomicI32 = AtomicI32::new(0);

/// Lazily start this process's manager supervisor on the first intercepted NPU call.
/// NOT started from the ctor: `ld.so.preload` loads libvnpu into every process (incl. the
/// short-lived TBE-compiler subprocesses CANN/torch spawn), and a supervisor in each made
/// them all thrash the single manager role. Gating on "actually used the NPU" excludes them.
/// Only spawns a thread (returns immediately) to stay off the dynamic-linker load lock.
fn ensure_supervisor(me: i32) {
    let cur = SUPERVISOR_PID.load(Ordering::Acquire);
    if cur == me {
        return;
    }
    if let Err(error) = limiter::config::validate_requested_limits() {
        eprintln!("[hami-vnpu-core] invalid resource configuration: {error}");
        unsafe { libc::_exit(78) };
    }
    if SUPERVISOR_PID
        .compare_exchange(cur, me, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        let spawned = std::panic::catch_unwind(|| {
            std::thread::Builder::new()
                .name("vnpu-supervisor".into())
                .spawn(|| {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        limiter::supervisor::run,
                    ));
                })
        });
        if !matches!(spawned, Ok(Ok(_))) {
            let _ = SUPERVISOR_PID.compare_exchange(me, cur, Ordering::AcqRel, Ordering::Relaxed);
            log::warn!(
                "[hook] failed to spawn vnpu-supervisor thread; will retry on next NPU call"
            );
        }
    }
}

/// PID-aware factory: detects fork and creates a fresh SchedulerClient for the child.
/// Falls back to a no-op stub if shmem is not available (e.g. in TBE compiler subprocesses).
static LIMITER: process::ProcessLocal<SchedulerClient> = process::ProcessLocal::new();

pub fn npu_limiter() -> &'static SchedulerClient {
    let pid = std::process::id();
    // /etc/ld.so.preload is also applied to the fresh query executable. Its
    // runtime callbacks must never register workers or elect another manager.
    if limiter::is_dcmi_helper() {
        return LIMITER.get_or_init(pid, SchedulerClient::stub);
    }
    ensure_supervisor(pid as i32);
    LIMITER.get_or_init(pid, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(SchedulerClient::new)).unwrap_or_else(
            |e| {
                log::warn!(
                    "SchedulerClient init failed (PID {}), using stub: {:?}",
                    pid,
                    e
                );
                SchedulerClient::stub()
            },
        )
    })
}

macro_rules! passthrough {
    ($name:expr, ($($sig:tt)*), $($arg:expr),*) => {
        {
            static REAL: ::std::sync::LazyLock<extern "C" fn($($sig)*) -> u64> =
                ::std::sync::LazyLock::new(|| unsafe {
                    let name = std::ffi::CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap();
                    let ptr = limiter::externed_api::runtime_symbol(name);
                    std::mem::transmute(ptr)
                });
            // println!("in func {:?}", $name);
            (*REAL)($($arg),*)
        }
    };
}

mod hook;
mod process;
mod signal_compat;
