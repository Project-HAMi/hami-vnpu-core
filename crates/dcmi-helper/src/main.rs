//! Executes DCMI in a fresh process, never in an inference process's fork child.
//! This binary deliberately does not depend on the limiter or CANN runtime.

#[path = "../../limiter/src/dcmi_protocol.rs"]
mod protocol;

use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessMemory {
    pid: i32,
    bytes: u64,
}

// Only async-signal-safe syscalls: the helper has its own process group, so a
// parent-death signal terminates its driver-created descendants as well.
extern "C" fn parent_gone(_: i32) {
    unsafe {
        libc::kill(-libc::getpgrp(), libc::SIGKILL);
    }
}

fn prepare(parent: i32, query: bool) -> io::Result<()> {
    unsafe {
        if libc::getpgrp() != libc::getpid() {
            return Err(io::Error::other(
                "DCMI helper requires its own process group",
            ));
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = if query {
            parent_gone as *const () as usize
        } else {
            stop as *const () as usize
        };
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0
            || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut child_action: libc::sigaction = std::mem::zeroed();
        child_action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut child_action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &child_action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        // The spawning supervisor blocks signals; exec preserves that mask.
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        if libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::getppid() != parent {
            if query {
                parent_gone(0);
            } else {
                STOP.store(true, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

unsafe fn symbol<T: Copy>(handle: *mut libc::c_void, name: &CStr) -> io::Result<T> {
    let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if address.is_null() {
        return Err(io::Error::other(format!("missing DCMI symbol {name:?}")));
    }
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&address));
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

fn snapshot() -> io::Result<Vec<protocol::Record>> {
    let driver =
        std::env::var("ASCEND_DRIVER_HOME").unwrap_or_else(|_| "/usr/local/Ascend/driver".into());
    let path = CString::new(format!("{driver}/lib64/driver/libdcmi.so"))?;
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            return Err(io::Error::other("cannot load libdcmi.so"));
        }
        // Keep the handle until _exit: do not run vendor library destructors.
        let init: unsafe extern "C" fn() -> i32 = symbol(handle, c"dcmi_init")?;
        let memory: unsafe extern "C" fn(i32, i32, *mut ProcessMemory, *mut i32) -> i32 =
            symbol(handle, c"dcmi_get_device_resource_info")?;
        if init() != 0 {
            return Err(io::Error::other("dcmi_init failed"));
        }
        let locations = if let Some(value) = std::env::var_os(protocol::DEVICE_ENV) {
            let physical = value
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| io::Error::other("invalid physical device ID"))?;
            let locate: unsafe extern "C" fn(*mut i32, *mut i32, u32) -> i32 =
                symbol(handle, c"dcmi_get_card_id_device_id_from_phyid")?;
            let (mut card, mut chip) = (-1, -1);
            if locate(&mut card, &mut chip, physical) != 0 || card < 0 || chip < 0 {
                return Err(io::Error::other("cannot locate physical device"));
            }
            // This snapshot belongs only to the selected card's local domain.
            vec![(card, chip)]
        } else {
            let cards: unsafe extern "C" fn(*mut i32, *mut i32, i32) -> i32 =
                symbol(handle, c"dcmi_get_card_list")?;
            let mut count = 0;
            let mut ids = [0; protocol::MAX_DEVICES];
            if cards(&mut count, ids.as_mut_ptr(), ids.len() as i32) != 0
                || count < 0
                || count as usize > ids.len()
            {
                return Err(io::Error::other("invalid DCMI card list"));
            }
            ids[..count as usize]
                .iter()
                .map(|&card| (card, 0))
                .collect()
        };
        let mut records = Vec::new();
        for (device, (card, chip)) in locations.into_iter().enumerate() {
            let mut processes = [ProcessMemory { pid: 0, bytes: 0 }; protocol::MAX_PROCESSES];
            let mut count = processes.len() as i32;
            // A failed query never falls back to another physical card.
            if memory(card, chip, processes.as_mut_ptr(), &mut count) != 0 {
                return Err(io::Error::other("DCMI process query failed"));
            }
            if count < 0 || count as usize > processes.len() {
                return Err(io::Error::other("invalid DCMI process count"));
            }
            for process in &processes[..count as usize] {
                if process.pid > 0 {
                    records.push(protocol::Record {
                        device,
                        pid: process.pid,
                        bytes: process.bytes,
                    });
                }
            }
        }
        Ok(records)
    }
}

fn run() -> io::Result<()> {
    let parent = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&pid| pid > 0)
        .ok_or_else(|| io::Error::other("usage: vnpu-dcmi PARENT_PID"))?;
    if std::env::var_os(protocol::HELPER_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Err(io::Error::other("DCMI helper must be launched by libvnpu"));
    }
    let query = std::env::args().nth(2).as_deref() == Some("--query");
    prepare(parent, query)?;
    if !query {
        return supervise();
    }
    // Driver printf output must not be confused with the snapshot protocol.
    let output = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if output < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut output = unsafe { std::fs::File::from_raw_fd(output) };
    if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let records = snapshot()?;
    let mut data = format!("{}\n", protocol::HEADER);
    for record in records {
        data.push_str(&format!(
            "{} {} {}\n",
            record.device, record.pid, record.bytes
        ));
    }
    protocol::decode(data.as_bytes()).map_err(io::Error::other)?;
    output.write_all(data.as_bytes())
}

/// Remain outside vendor code so timeout/parent death can always be handled.
/// Reap orphaned driver grandchildren here, rather than relying on container PID 1.
fn supervise() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if STOP.load(Ordering::Relaxed) {
        return Err(io::Error::other("query parent exited"));
    }
    io::stdout().write_all(protocol::READY.as_bytes())?;
    io::stdout().flush()?;
    let query = std::process::Command::new(std::env::current_exe()?)
        .arg(std::process::id().to_string())
        .arg("--query")
        .process_group(0)
        .spawn()?;
    let query_pid = query.id() as i32;
    let start = Instant::now();
    let mut cleanup = false;
    let mut success = false;
    loop {
        if !cleanup {
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    query.id(),
                    &mut status,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let exited = rc == 0 && unsafe { status.si_pid() } != 0;
            let cancelled =
                STOP.load(Ordering::Relaxed) || start.elapsed() >= Duration::from_millis(1500);
            if exited || cancelled || rc < 0 {
                success = exited
                    && !cancelled
                    && unsafe { status.si_status() } == 0
                    && status.si_code == libc::CLD_EXITED;
                // The leader is not reaped yet, so this process-group ID cannot
                // be recycled before the signal. Cleanup also runs after success
                // in case a driver command left descendants behind.
                unsafe {
                    libc::kill(-query_pid, libc::SIGKILL);
                }
                cleanup = true;
            }
        }
        if cleanup {
            // A descendant can create a new session. Once its parent is killed,
            // subreaper adoption makes it our child and it is terminated here too.
            // Discovery is best-effort, but waitpid must always run. In
            // particular, CONFIG_PROC_CHILDREN is optional on Linux.
            for pid in
                child_pids(std::path::Path::new("/proc"), std::process::id()).unwrap_or_default()
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            loop {
                let mut status = 0;
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid > 0 {
                    continue;
                }
                if pid == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ECHILD) {
                    return if success {
                        Ok(())
                    } else {
                        Err(io::Error::other("DCMI worker failed or timed out"))
                    };
                }
                return Err(error);
            }
        }
        // If a driver task remains in uninterruptible I/O, keep its parent alive
        // to reap it later. The interceptor has already bounded its own wait and
        // will not start another helper until this one exits.
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn child_pids(proc_root: &std::path::Path, parent: u32) -> io::Result<Vec<i32>> {
    if let Ok(children) =
        std::fs::read_to_string(proc_root.join(format!("self/task/{parent}/children")))
    {
        return children
            .split_whitespace()
            .map(|pid| {
                pid.parse::<i32>()
                    .ok()
                    .filter(|pid| *pid > 0)
                    .ok_or_else(|| io::Error::other("invalid child PID"))
            })
            .collect();
    }
    let mut children = Vec::new();
    for entry in std::fs::read_dir(proc_root)?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        // Tasks can exit between listing /proc and reading their stat file.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // comm can contain spaces and parentheses; PPid follows the state field
        // after the final ')', not a fixed whitespace position in the whole line.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(1)?.parse::<u32>().ok());
        if ppid == Some(parent) {
            children.push(pid);
        }
    }
    Ok(children)
}

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("vnpu-dcmi: {error}");
            1
        }
    };
    // Do not enter CANN/driver atexit handlers, including when initialization failed.
    unsafe {
        libc::_exit(code);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn proc_scan_finds_only_direct_children_even_with_parentheses_in_comm() {
        let root = std::env::temp_dir().join(format!("vnpu-proc-test-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        for (pid, stat) in [
            (101, "101 (driver (query) worker)) S 42 0 0"),
            (102, "102 (unrelated) S 99 0 0"),
            (103, "103 (exited task)"),
        ] {
            std::fs::create_dir(root.join(pid.to_string())).unwrap();
            std::fs::write(root.join(format!("{pid}/stat")), stat).unwrap();
        }
        // A vanished stat file must not stop the rest of the scan.
        std::fs::create_dir(root.join("104")).unwrap();
        assert_eq!(super::child_pids(&root, 42).unwrap(), vec![101]);
        let task = root.join("self/task/42");
        std::fs::create_dir_all(&task).unwrap();
        std::fs::write(task.join("children"), "101 104 ").unwrap();
        assert_eq!(super::child_pids(&root, 42).unwrap(), vec![101, 104]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
