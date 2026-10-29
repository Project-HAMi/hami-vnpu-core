//! Ascend libraries are optional until an intercepted NPU call actually needs them.

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

struct Library {
    name: &'static CStr,
    handle: AtomicPtr<c_void>,
}

impl Library {
    const fn new(name: &'static CStr) -> Self {
        Self {
            name,
            handle: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn symbol(&self, name: &CStr) -> Option<*mut c_void> {
        let mut handle = self.handle.load(Ordering::Acquire);
        if handle.is_null() {
            let opened =
                unsafe { libc::dlopen(self.name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if opened.is_null() {
                return None;
            }
            handle = match self.handle.compare_exchange(
                ptr::null_mut(),
                opened,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => opened,
                Err(existing) => {
                    unsafe { libc::dlclose(opened) };
                    existing
                }
            };
        }
        // Retain the handle: the driver/runtime may keep callbacks and threads alive.
        let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
        (!address.is_null()).then_some(address)
    }
}

static RUNTIME: Library = Library::new(c"libruntime.so");
static ACL: Library = Library::new(c"libascendcl.so");
static DCMI: Library = Library::new(c"libdcmi.so");

/// Prefer the application's backend, including fake backends used by CPU tests.
/// An explicit local handle also supports SDKs loaded with `RTLD_LOCAL`.
pub fn sdk_symbol(name: &CStr) -> Option<*mut c_void> {
    let address = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
    if !address.is_null() {
        return Some(address);
    }
    if name.to_bytes().starts_with(b"acl") {
        ACL.symbol(name)
    } else {
        RUNTIME.symbol(name)
    }
}

/// Take a reference to the actual backend before caching one of its addresses.
/// `RTLD_NEXT` itself does not keep an application's dlopen handle alive.
fn retain_symbol_library(address: *mut c_void) -> *mut c_void {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    if unsafe { libc::dladdr(address, &mut info) } == 0 || info.dli_fname.is_null() {
        return ptr::null_mut();
    }
    // NOLOAD avoids initializing another SDK or selecting a different backend.
    // A main-executable symbol needs no extra reference and may return null here.
    unsafe {
        libc::dlopen(
            info.dli_fname,
            libc::RTLD_LAZY | libc::RTLD_LOCAL | libc::RTLD_NOLOAD,
        )
    }
}

fn cached_symbol(
    cache: &AtomicPtr<c_void>,
    resolve: impl FnOnce() -> Option<*mut c_void>,
) -> Option<*mut c_void> {
    let address = cache.load(Ordering::Acquire);
    if !address.is_null() {
        return Some(address);
    }
    // Do not cache a miss: a later dlopen can make an optional symbol available.
    let address = resolve()?;
    let retained = retain_symbol_library(address);
    Some(
        match cache.compare_exchange(
            ptr::null_mut(),
            address,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => address,
            Err(existing) => {
                if !retained.is_null() {
                    unsafe { libc::dlclose(retained) };
                }
                existing
            }
        },
    )
}

/// Cache successful SDK lookups and retain their backend for the process lifetime.
/// Retry a missing backend on the next call.
pub fn resolve_sdk_symbol(cache: &AtomicPtr<c_void>, name: &CStr) -> Option<*mut c_void> {
    cached_symbol(cache, || sdk_symbol(name))
}

pub(crate) fn resolve_dcmi_symbol(cache: &AtomicPtr<c_void>, name: &CStr) -> Option<*mut c_void> {
    cached_symbol(cache, || DCMI.symbol(name))
}
