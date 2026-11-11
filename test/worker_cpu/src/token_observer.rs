//! Pause one real admission attempt after its state/batch checks, before its CAS.
use std::sync::{Barrier, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub static PAUSE_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static PAUSE: AtomicBool = AtomicBool::new(false);
pub static AFTER_CAS: AtomicBool = AtomicBool::new(false);
pub static BARRIERS: OnceLock<(Barrier, Barrier)> = OnceLock::new();

pub fn after_snapshot() {
    if !AFTER_CAS.load(Ordering::Acquire) {
        pause();
    }
}

pub fn after_acquire(acquired: bool) {
    if acquired && AFTER_CAS.load(Ordering::Acquire) {
        pause();
    }
}

fn pause() {
    if PAUSE.swap(false, Ordering::AcqRel)
        || PAUSE_COUNT.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
    {
        let (reached, resume) = BARRIERS.get().unwrap();
        reached.wait();
        resume.wait();
    }
}
