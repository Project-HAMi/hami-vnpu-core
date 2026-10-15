//! Compile the actual process cache without linking the CANN runtime.
//! Run with `test/process_local.sh`; fork cases use real child processes.

#[path = "../crates/hook/src/process.rs"]
mod process;

use process::ProcessLocal;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[test]
fn concurrent_first_access_constructs_one_value() {
    let cache = Arc::new(ProcessLocal::new());
    let count = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Barrier::new(16));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let (cache, count, gate) = (cache.clone(), count.clone(), gate.clone());
            std::thread::spawn(move || {
                gate.wait();
                let value = cache.get_or_init(std::process::id(), || {
                    count.fetch_add(1, Ordering::SeqCst);
                    42usize
                });
                assert_eq!(*value, 42);
                value as *const usize as usize
            })
        })
        .collect();
    let pointers: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(pointers.iter().all(|p| *p == pointers[0]));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    cache.get_or_init(std::process::id(), || {
        panic!("must reuse the initialized value")
    });
}

#[test]
fn panic_during_initialization_can_be_retried() {
    let cache = ProcessLocal::new();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_init(std::process::id(), || -> usize { panic!("init failed") });
        }))
        .is_err()
    );
    assert_eq!(*cache.get_or_init(std::process::id(), || 17), 17);
}

struct RetainedValue {
    owner: u32,
    drops: Arc<AtomicUsize>,
}

impl Drop for RetainedValue {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn replacing_pid_retains_the_old_value() {
    let drops = Arc::new(AtomicUsize::new(0));
    let cache = ProcessLocal::new();
    let old = cache.get_or_init(10, || RetainedValue {
        owner: 10,
        drops: drops.clone(),
    });
    let new = cache.get_or_init(20, || RetainedValue {
        owner: 20,
        drops: drops.clone(),
    });
    assert_ne!(old as *const _, new as *const _);
    assert_eq!(old.owner, 10);
    assert_eq!(new.owner, 20);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(cache);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
mod fork_cases {
    use super::*;
    use std::os::raw::c_int;

    unsafe extern "C" {
        fn fork() -> c_int;
        fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
        fn kill(pid: c_int, signal: c_int) -> c_int;
        fn _exit(code: c_int) -> !;
    }

    // The runner limits this test binary to Linux/macOS, where these values
    // are WNOHANG=1 and SIGKILL=9. A stuck child must not hang the test suite.
    fn wait_child(pid: c_int) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut status = 0;
            let result = unsafe { waitpid(pid, &mut status, 1) };
            if result == pid {
                return if status == 0 {
                    Ok(())
                } else {
                    Err(format!("child status {status}"))
                };
            }
            if result < 0 {
                return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
            }
            if Instant::now() >= deadline {
                unsafe {
                    kill(pid, 9);
                    waitpid(pid, &mut status, 0);
                }
                return Err("child timed out (possibly inherited initialization lock)".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn child_gets_its_own_value_without_dropping_the_parent_value() {
        let cache = ProcessLocal::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let parent_pid = std::process::id();
        let parent = cache.get_or_init(parent_pid, || RetainedValue {
            owner: parent_pid,
            drops: drops.clone(),
        });
        let pid = unsafe { fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let child_pid = std::process::id();
            let child = cache.get_or_init(child_pid, || RetainedValue {
                owner: child_pid,
                drops: drops.clone(),
            });
            let valid = child_pid != parent_pid
                && child.owner == child_pid
                && parent.owner == parent_pid
                && !std::ptr::eq(parent, child)
                && drops.load(Ordering::SeqCst) == 0;
            unsafe {
                _exit(if valid { 0 } else { 1 });
            }
        }
        wait_child(pid).unwrap();
        assert!(std::ptr::eq(
            parent,
            cache.get_or_init(parent_pid, || panic!("parent replaced"))
        ));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn child_skips_a_parent_once_lock_that_is_still_initializing() {
        let cache = Arc::new(ProcessLocal::new());
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let parent_pid = std::process::id();
        let initializing = {
            let (cache, entered, release) = (cache.clone(), entered.clone(), release.clone());
            std::thread::spawn(move || {
                *cache.get_or_init(parent_pid, || {
                    entered.store(true, Ordering::Release);
                    while !release.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    parent_pid
                })
            })
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !entered.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "initializer did not start");
            std::thread::yield_now();
        }
        let pid = unsafe { fork() };
        if pid == 0 {
            let child_pid = std::process::id();
            let value = cache.get_or_init(child_pid, || child_pid);
            unsafe {
                _exit(if *value == child_pid && child_pid != parent_pid {
                    0
                } else {
                    2
                });
            }
        }
        let result = if pid > 0 {
            wait_child(pid)
        } else {
            Err("fork failed".into())
        };
        release.store(true, Ordering::Release);
        assert_eq!(initializing.join().unwrap(), parent_pid);
        result.unwrap();
        assert_eq!(
            *cache.get_or_init(parent_pid, || panic!("parent replaced")),
            parent_pid
        );
    }
}
