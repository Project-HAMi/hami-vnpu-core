//! Test seams around real std locks and Linux futex; production expressions
//! remain unchanged. Each case runs in its own process to isolate the flags.
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, PoisonError, RwLockReadGuard, RwLockWriteGuard, TryLockResult};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use crate::shmem::{LocalContainerShmem, STATE_RUNNING};

pub static REGION: std::sync::OnceLock<&'static LocalContainerShmem> = std::sync::OnceLock::new();
pub static CHANGE_AFTER_UNLOCK: AtomicBool = AtomicBool::new(false);
pub static CHANGE_BEFORE_SYSCALL: AtomicBool = AtomicBool::new(false);
pub static WAIT_CALLS: AtomicU64 = AtomicU64::new(0);
pub static WAIT_ERRNO: AtomicI32 = AtomicI32::new(-1);
pub static WAIT_TID: AtomicI32 = AtomicI32::new(0);

pub fn publish_next_batch() {
    let region = REGION.get().unwrap();
    region.batch_id.store(43, Ordering::Relaxed);
    region.active_workers.store(0, Ordering::Relaxed);
    region.tokens_remaining.store(1, Ordering::Relaxed);
    region.state.store(STATE_RUNNING, Ordering::Release);
    crate::raw_futex::wake_all(&region.state);
}

#[derive(Debug)]
pub struct RwLock<T>(std::sync::RwLock<T>);
impl<T> RwLock<T> {
    pub fn new(value: T) -> Self { Self(std::sync::RwLock::new(value)) }
    pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> { self.0.read() }
    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> { self.0.try_read() }
    pub fn write(&self) -> LockResult<WriteGuard<'_, T>> {
        match self.0.write() {
            Ok(guard) => Ok(WriteGuard(Some(guard))),
            Err(error) => Err(PoisonError::new(WriteGuard(Some(error.into_inner())))),
        }
    }
}
pub struct WriteGuard<'a, T>(Option<RwLockWriteGuard<'a, T>>);
impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { self.0.as_ref().unwrap() }
}
impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T { self.0.as_mut().unwrap() }
}
impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.0.take());
        if CHANGE_AFTER_UNLOCK.swap(false, Ordering::AcqRel) { publish_next_batch(); }
    }
}

pub mod futex {
    pub use crate::raw_futex::{wake, wake_all, wait_timeout};
    use super::*;
    pub fn wait(atom: &std::sync::atomic::AtomicU32, expected: u32) {
        WAIT_CALLS.fetch_add(1, Ordering::Relaxed);
        WAIT_TID.store(unsafe { libc::syscall(libc::SYS_gettid) } as i32, Ordering::Release);
        if CHANGE_BEFORE_SYSCALL.swap(false, Ordering::AcqRel) { publish_next_batch(); }
        crate::raw_futex::wait(atom, expected);
        WAIT_ERRNO.store(std::io::Error::last_os_error().raw_os_error().unwrap_or(0), Ordering::Release);
    }
}
