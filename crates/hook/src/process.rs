//! A retained client per process, without a mutex on the initialized path.
//!
//! The caller supplies a fresh PID (never a cached parent PID). A fork child
//! checks that tag before touching the inherited OnceLock or client. Published
//! objects are never freed: dropping an inherited client could clear its parent's
//! shared registration, and its locks may belong to vanished parent threads.
//! Keeping one getpid per hook also avoids an atfork-registration/OnceLock race.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

pub(crate) struct ProcessLocal<T: Send + Sync + 'static>(AtomicPtr<Entry<T>>);

struct Entry<T> {
    pid: u32,
    value: OnceLock<T>,
}

impl<T: Send + Sync + 'static> ProcessLocal<T> {
    pub(crate) const fn new() -> Self {
        Self(AtomicPtr::new(std::ptr::null_mut()))
    }

    pub(crate) fn get_or_init(&self, pid: u32, initialize: impl FnOnce() -> T) -> &T {
        loop {
            let old = self.0.load(Ordering::Acquire);
            // Entries are immutable except for their OnceLock and are retained
            // forever, so inspecting the tag cannot race with deallocation.
            if !old.is_null() && unsafe { (*old).pid } == pid {
                return unsafe { &*old }.value.get_or_init(initialize);
            }
            let fresh = Box::into_raw(Box::new(Entry {
                pid,
                value: OnceLock::new(),
            }));
            if self
                .0
                .compare_exchange(old, fresh, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return unsafe { &*fresh }.value.get_or_init(initialize);
            }
            // This losing allocation was never published or initialized.
            unsafe {
                drop(Box::from_raw(fresh));
            }
        }
    }
}
