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

#[allow(dead_code)]
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
