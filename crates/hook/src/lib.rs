use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use limiter::worker::{AllocationKind, HbmFreeTicket, HbmRetainTicket, SchedulerClient};

#[derive(Default)]
struct DeviceClient {
    supervisor_started: AtomicBool,
    client: OnceLock<SchedulerClient>,
}

#[derive(Default)]
struct Clients {
    devices: Mutex<HashMap<u32, &'static DeviceClient>>,
}

// PID-aware storage avoids inherited locks after fork. Each physical card owns
// its own quota, manager election, timing contexts and allocation handles.
static CLIENTS: process::ProcessLocal<Clients> = process::ProcessLocal::new();
static STUB: process::ProcessLocal<SchedulerClient> = process::ProcessLocal::new();

fn fatal(reason: &str) -> ! {
    eprintln!("[hami-vnpu-core] {reason}");
    unsafe { libc::_exit(78) }
}

fn clients() -> &'static Clients {
    CLIENTS.get_or_init(std::process::id(), Clients::default)
}

fn physical_device(logical: i32) -> u32 {
    let mut physical = 0;
    if logical < 0
        || unsafe { limiter::externed_api::rtGetDevicePhyIdByIndex(logical as u32, &mut physical) }
            != 0
    {
        fatal("cannot identify the physical NPU");
    }
    physical
}

fn current_physical_device() -> u32 {
    let mut logical = -1;
    // These are runtime APIs, so time-sliced policies do not require ACL.
    if unsafe { limiter::externed_api::rtGetDevice(&mut logical) } != 0 {
        fatal("cannot obtain the current NPU");
    }
    physical_device(logical)
}

fn device_entry(physical: u32) -> &'static DeviceClient {
    let mut devices = clients()
        .devices
        .lock()
        .unwrap_or_else(|_| fatal("client registry lock poisoned"));
    devices
        .entry(physical)
        .or_insert_with(|| Box::leak(Box::new(DeviceClient::default())))
}

fn ensure_supervisor(entry: &DeviceClient, physical: u32) {
    if entry
        .supervisor_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let config = limiter::config::ManagerConfig::from_env()
        .unwrap_or_else(|error| fatal(&format!("invalid resource configuration: {error}")));
    limiter::config::require_distinct_shmem_paths(&config.global_shm_path, &config.local_shm_path);
    let spawned = std::thread::Builder::new()
        .name(format!("vnpu-device-{physical}"))
        .spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                limiter::supervisor::run_for_device(physical);
            }));
        });
    if spawned.is_err() {
        entry.supervisor_started.store(false, Ordering::Release);
        fatal("cannot start per-device supervisor");
    }
}

pub fn npu_limiter() -> &'static SchedulerClient {
    if !limiter::config::vnpu_configuration_requested() {
        return STUB.get_or_init(std::process::id(), SchedulerClient::stub);
    }
    // Apply spatial budgets before resource creation or operator submission.
    spatial::ensure_current_device();
    client_for_physical(current_physical_device())
}

/// Physical-memory properties can target a card other than the current card.
/// Resolve the target without changing the application's current context.
pub fn npu_limiter_for_device(logical: i32) -> &'static SchedulerClient {
    if !limiter::config::vnpu_configuration_requested() {
        return STUB.get_or_init(std::process::id(), SchedulerClient::stub);
    }
    // Physical memory can target a device before it has a compute context.
    // Apply core budgets when the application selects that context or launches.
    if spatial::enabled() {
        spatial::mark_device_use();
    }
    client_for_physical(physical_device(logical))
}

fn client_for_physical(physical: u32) -> &'static SchedulerClient {
    let entry = device_entry(physical);
    ensure_supervisor(entry, physical);
    entry.client.get_or_init(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            SchedulerClient::for_device(physical)
        }))
        .unwrap_or_else(|_| fatal("cannot initialize per-device resource limiter"))
    })
}

/// Claim the allocation on its original card before native code may free/reuse
/// its handle. Never initialize another card's client during a free.
pub(crate) fn claim_hbm_free(handle: u64, kind: AllocationKind) -> Option<HbmFreeTicket> {
    let devices: Vec<_> = clients()
        .devices
        .lock()
        .unwrap_or_else(|_| fatal("client registry lock poisoned"))
        .values()
        .copied()
        .collect();
    for entry in devices {
        if let Some(client) = entry.client.get()
            && let Some(ticket) = client.claim_hbm_free(handle, kind)
        {
            return Some(ticket);
        }
    }
    None
}

/// Pin only this process's already charged physical backings across a retain.
pub(crate) fn prepare_hbm_retain() -> HbmRetainTicket {
    limiter::worker::prepare_hbm_retain(|| {
        let devices: Vec<_> = clients()
            .devices
            .lock()
            .unwrap_or_else(|_| fatal("client registry lock poisoned"))
            .values()
            .copied()
            .collect();
        devices
            .into_iter()
            .filter_map(|entry| entry.client.get())
            .cloned()
            .collect()
    })
}

pub(crate) fn retire_context(context: u64) {
    let devices: Vec<_> = clients()
        .devices
        .lock()
        .unwrap_or_else(|_| fatal("client registry lock poisoned"))
        .values()
        .copied()
        .collect();
    for entry in devices {
        if let Some(client) = entry.client.get() {
            client.retire_context(context);
        }
    }
}

fn missing_runtime_symbol(name: &str) -> u64 {
    if limiter::config::vnpu_configuration_requested() {
        fatal(&format!("cannot find original function: {name}"));
    }
    log::debug!("optional Ascend runtime symbol `{name}` is unavailable");
    u64::MAX
}

macro_rules! passthrough {
    ($name:expr, ($($sig:tt)*), $($arg:expr),*) => {
        {
            type Real = unsafe extern "C" fn($($sig)*) -> u64;
            static REAL: ::std::sync::atomic::AtomicPtr<libc::c_void> =
                ::std::sync::atomic::AtomicPtr::new(::std::ptr::null_mut());
            let name = ::std::ffi::CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes())
                .expect("static hook symbol");
            match limiter::dynamic::resolve_sdk_symbol(&REAL, name) {
                Some(address) => {
                    let real: Real = unsafe { ::std::mem::transmute(address) };
                    unsafe { real($($arg),*) }
                }
                None => crate::missing_runtime_symbol($name),
            }
        }
    };
}

mod hook;
mod host_args;
mod memory;
mod memory_extra;
mod memory_physical;
mod ownership;
mod process;
pub mod spatial;
