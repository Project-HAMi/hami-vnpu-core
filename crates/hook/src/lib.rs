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
    if let Err(error) = limiter::config::ManagerConfig::from_env() {
        fatal(&format!("invalid resource configuration: {error}"));
    }
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
    // Apply spatial budgets before resource creation or operator submission.
    spatial::ensure_current_device();
    client_for_physical(current_physical_device())
}

/// Physical-memory properties can target a card other than the current card.
/// Resolve the target without changing the application's current context.
pub fn npu_limiter_for_device(logical: i32) -> &'static SchedulerClient {
    spatial::ensure_device(logical);
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

macro_rules! passthrough {
    ($name:expr, ($($sig:tt)*), $($arg:expr),*) => {
        {
            static REAL: ::std::sync::LazyLock<extern "C" fn($($sig)*) -> u64> =
                ::std::sync::LazyLock::new(|| unsafe {
                    let ptr = libc::dlsym(libc::RTLD_NEXT, concat!($name, "\0").as_ptr() as *const libc::c_char);
                    if ptr.is_null() {
                        panic!("cannot find original function: {}", $name);
                    }
                    std::mem::transmute(ptr)
                });
            // println!("in func {:?}", $name);
            (*REAL)($($arg),*)
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
mod signal_compat;
pub mod spatial;
