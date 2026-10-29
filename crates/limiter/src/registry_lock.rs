//! Short, process-shared scheduler transactions. Never hold a guard across a
//! device call, subprocess operation, sleep, or wait for a scheduling turn.
//!
//! POSIX record locks belong to the process: a fork child does not inherit the
//! lock and cannot keep it held after its parent dies. The mutex additionally
//! serializes threads sharing this process's single supervisor/registry handle.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::sync::{Mutex, MutexGuard};

pub struct RegistryLock(Mutex<File>);

pub struct RegistryGuard<'a>(MutexGuard<'a, File>);

impl RegistryLock {
    pub fn open(path: &str) -> io::Result<Self> {
        Ok(Self(Mutex::new(
            OpenOptions::new().read(true).write(true).open(path)?,
        )))
    }

    pub fn lock(&self) -> io::Result<RegistryGuard<'_>> {
        let file = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_lock(&file, libc::F_WRLCK as libc::c_short, libc::F_SETLKW)?;
        Ok(RegistryGuard(file))
    }
}

fn set_lock(file: &File, kind: libc::c_short, command: i32) -> io::Result<()> {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_len = 1;
    loop {
        if unsafe { libc::fcntl(file.as_raw_fd(), command, &lock) } != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

impl Drop for RegistryGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = set_lock(&self.0, libc::F_UNLCK as libc::c_short, libc::F_SETLK) {
            log::error!("cannot unlock scheduler registry: {error}");
        }
    }
}
