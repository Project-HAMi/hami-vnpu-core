//! Bounded queries in an exec-isolated process. No DCMI function is called here.

use std::ffi::CStr;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::dcmi_protocol::{self, HELPER_ENV, MAX_OUTPUT, Record};

pub fn helper_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NPU_DCMI_HELPER_PATH") {
        return Some(path.into());
    }
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(helper_path as *const () as _, &mut info) == 0 || info.dli_fname.is_null() {
            return None;
        }
        let library = PathBuf::from(
            CStr::from_ptr(info.dli_fname)
                .to_string_lossy()
                .into_owned(),
        );
        library.parent().map(|dir| dir.join("vnpu-dcmi"))
    }
}

#[derive(Default)]
pub struct Collector {
    // A helper can remain alive while reaping a child in uninterruptible kernel I/O. Retain it and
    // refuse another spawn until it is reaped, rather than leaking more helpers.
    child: Option<Child>,
}

impl Collector {
    pub fn snapshot(
        &mut self,
        path: &Path,
        timeout: Duration,
        stopped: &AtomicBool,
    ) -> io::Result<Vec<Record>> {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return Err(io::Error::other("previous DCMI helper has not exited")),
                Ok(Some(_)) => {}
                // The application's SIGCHLD handler may have reaped a cancelled
                // helper after snapshot() returned. Release that stale handle.
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {}
                Err(error) => return Err(error),
            }
            self.child = None;
        }
        let child = Command::new(path)
            .arg(std::process::id().to_string())
            .env(HELPER_ENV, "1")
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        self.child = Some(child);
        let result = self.read_snapshot(timeout, stopped);
        if result.is_err() {
            self.terminate();
        }
        result
    }

    fn read_snapshot(
        &mut self,
        timeout: Duration,
        stopped: &AtomicBool,
    ) -> io::Result<Vec<Record>> {
        let child = self.child.as_mut().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let start = Instant::now();
        let mut output = Vec::new();
        let mut ready = false;
        loop {
            drain_output(&mut stdout, &mut output)?;
            if !ready && output.starts_with(dcmi_protocol::READY.as_bytes()) {
                output.drain(..dcmi_protocol::READY.len());
                ready = true;
            }
            // Keep Child ownership until its exit status is consumed below.
            // The helper itself reaps its driver descendants before exiting.
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut status,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ECHILD) {
                    // An application SIGCHLD handler may have reaped it. Do not
                    // signal a process whose numeric ID can now be reused.
                    self.child = None;
                }
                return Err(error);
            }
            if unsafe { status.si_pid() } != 0 {
                let status = child.wait()?;
                self.child = None;
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "DCMI helper exited with {status}"
                    )));
                }
                // The final write could race the previous nonblocking read.
                drain_output(&mut stdout, &mut output)?;
                if !ready && output.starts_with(dcmi_protocol::READY.as_bytes()) {
                    output.drain(..dcmi_protocol::READY.len());
                }
                return dcmi_protocol::decode(&output).map_err(io::Error::other);
            }
            if stopped.load(Ordering::Acquire) || start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "DCMI helper deadline exceeded",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.child = None;
                    return;
                }
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    self.child = None;
                    return;
                }
                _ => {}
            }
            // The helper is a subreaper, separate from the vendor query worker.
            // Let it kill and reap descendants before exiting. SIGKILL here
            // would orphan those descendants under a possibly non-reaping PID 1.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
        }
    }
}

fn drain_output(stdout: &mut impl Read, output: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; 4096];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                if output.len() > MAX_OUTPUT {
                    return Err(io::Error::other("oversized DCMI output"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.terminate();
        if let Some(mut child) = self.child.take() {
            // At most one deferred reap per collector; never block scheduling on
            // a task stuck in a driver syscall after SIGKILL.
            let _ = std::thread::Builder::new()
                .name("vnpu-dcmi-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}
