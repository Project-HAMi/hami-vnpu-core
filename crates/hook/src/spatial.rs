//! Cooperative CANN core budgets. These are not exclusive physical partitions.
//!
//! ACL declarations match acl_rt.h in CANN 9.1: aclError and the resource enum
//! are C ints, device IDs are int32_t, and contexts/streams are opaque pointers.
//! Cache physical counts separately from application state: a reference-counted
//! device reset need not restore the original resource counts.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use limiter::config::{ManagerConfig, SchedPolicy};

const MAX_DEVICES: i32 = 256;
const CUBE: i32 = 0;
const VECTOR: i32 = 1;

#[derive(Default)]
struct Budget {
    physical: Option<[u32; 2]>,
    limit: [u32; 2],
    applied: bool,
}

struct Policy {
    percent: Option<u32>,
    devices: Mutex<HashMap<i32, Budget>>,
}

static POLICY: crate::process::ProcessLocal<Policy> = crate::process::ProcessLocal::new();
// A child may obtain a new policy without touching inherited locks, but must not
// reuse the parent's initialized CANN context. Fork before device use is allowed.
static DEVICE_PROCESS: AtomicU32 = AtomicU32::new(0);
thread_local! { static INTERNAL: Cell<bool> = const { Cell::new(false) }; }

fn fatal(reason: &str) -> ! {
    eprintln!("[hami-vnpu-core spatial] fatal pid={}: {reason}", std::process::id());
    unsafe { libc::_exit(78) }
}

fn policy() -> &'static Policy {
    let pid = std::process::id();
    let policy = POLICY.get_or_init(pid, || {
        // Other policies must not require any of the optional resource APIs.
        let spatial = SchedPolicy::from_env().unwrap_or_else(|error| fatal(&error))
            == SchedPolicy::Spatial;
        let percent = if spatial {
            let cfg = ManagerConfig::from_env().unwrap_or_else(|error| fatal(&error));
            if cfg.sched_policy != SchedPolicy::Spatial { fatal("inconsistent spatial policy"); }
            Some(cfg.compute_limit)
        } else {
            None
        };
        Policy { percent, devices: Mutex::new(HashMap::new()) }
    });
    if policy.percent.is_some() {
        let owner = DEVICE_PROCESS.load(Ordering::Acquire);
        if owner != 0 && owner != pid {
            fatal("NPU use after fork is unsupported; exec a fresh process before using spatial policy");
        }
    }
    policy
}

pub fn enabled() -> bool { policy().percent.is_some() }

fn internal() -> bool {
    // A CANN callback can fork on this very thread while InternalGuard is live.
    // Check the process before consulting inherited TLS; otherwise `true` would
    // bypass policy() and its PID guard in the child. Owner zero is the ordinary
    // non-spatial path and needs no getpid call here.
    let owner = DEVICE_PROCESS.load(Ordering::Acquire);
    if owner != 0 && owner != std::process::id() {
        fatal("NPU use after fork is unsupported; inherited internal callback");
    }
    INTERNAL.with(Cell::get)
}

struct InternalGuard;
impl InternalGuard {
    fn enter() -> Self {
        INTERNAL.with(|flag| flag.set(true));
        Self
    }
}
impl Drop for InternalGuard {
    fn drop(&mut self) { INTERNAL.with(|flag| flag.set(false)); }
}

fn devices(policy: &Policy) -> MutexGuard<'_, HashMap<i32, Budget>> {
    policy.devices.lock().unwrap_or_else(|_| fatal("resource bookkeeping lock is poisoned"))
}

fn mark_device_use() {
    let pid = std::process::id();
    let owner = DEVICE_PROCESS.load(Ordering::Acquire);
    if owner == 0 {
        // All participating threads have the same PID. Mark before a CANN call
        // so a fork during initialization also fails closed in the child.
        let _ = DEVICE_PROCESS.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire);
    } else if owner != pid {
        fatal("NPU use after fork is unsupported");
    }
}

unsafe fn symbol<T: Copy>(name: &CStr) -> T {
    let address = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
    if address.is_null() { fatal(name.to_str().unwrap_or("missing CANN symbol")); }
    // Each call site supplies the exact function-pointer type from acl_rt.h.
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&address));
    unsafe { std::mem::transmute_copy(&address) }
}

type DeviceGet = unsafe extern "C" fn(i32, i32, *mut u32) -> i32;
type DeviceSet = unsafe extern "C" fn(i32, i32, u32) -> i32;
type StreamGet = unsafe extern "C" fn(*mut c_void, i32, *mut u32) -> i32;
type StreamSet = unsafe extern "C" fn(*mut c_void, i32, u32) -> i32;

fn current_device() -> i32 {
    let get: unsafe extern "C" fn(*mut i32) -> i32 = unsafe { symbol(c"aclrtGetDevice") };
    let mut device = -1;
    if unsafe { get(&mut device) } != 0 { fatal("cannot obtain current device"); }
    device
}

fn allocation(physical: u32, percent: u32) -> u32 {
    if physical == 0 { fatal("initial physical resource count is zero"); }
    let target = u64::from(physical) * u64::from(percent);
    target.div_ceil(100) as u32
}

fn apply(map: &mut HashMap<i32, Budget>, device: i32, percent: u32) {
    if !(0..MAX_DEVICES).contains(&device) { fatal("device ID is out of range"); }
    let budget = map.entry(device).or_default();
    if budget.applied { return; }
    mark_device_use();
    let get: DeviceGet = unsafe { symbol(c"aclrtGetDeviceResLimit") };
    let set: DeviceSet = unsafe { symbol(c"aclrtSetDeviceResLimit") };
    if budget.physical.is_none() {
        let mut physical = [0; 2];
        for resource in CUBE..=VECTOR {
            if unsafe { get(device, resource, &mut physical[resource as usize]) } != 0 {
                fatal("cannot obtain initial device resource count");
            }
        }
        budget.limit = [allocation(physical[0], percent), allocation(physical[1], percent)];
        budget.physical = Some(physical);
    }
    for resource in CUBE..=VECTOR {
        let cap = budget.limit[resource as usize];
        if unsafe { set(device, resource, cap) } != 0 { fatal("cannot set device resource limit"); }
        let mut readback = 0;
        if unsafe { get(device, resource, &mut readback) } != 0 || readback != cap {
            fatal("device resource limit readback mismatch");
        }
    }
    budget.applied = true;
    let physical = budget.physical.unwrap();
    eprintln!("[hami-vnpu-core spatial] pid={} device={device} requested={percent} cube={}/{} vector={}/{} rounding=ceil cooperative=1",
        std::process::id(), budget.limit[0], physical[0], budget.limit[1], physical[1]);
}

/// Runtime entry can verify initialization if an application bypassed ACL device
/// selection. Applying after a custom operator has already tiled is unsupported.
pub fn ensure_current_device() {
    if internal() { return; }
    let policy = policy();
    let Some(percent) = policy.percent else { return; };
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    apply(&mut map, current_device(), percent);
}

pub fn set_device(device: i32) -> i32 {
    let real: unsafe extern "C" fn(i32) -> i32 = unsafe { symbol(c"aclrtSetDevice") };
    if internal() || !enabled() { return unsafe { real(device) }; }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    mark_device_use();
    let rc = unsafe { real(device) };
    if rc == 0 { apply(&mut map, device, policy.percent.unwrap()); }
    rc
}

pub fn set_context(context: *mut c_void) -> i32 {
    let real: unsafe extern "C" fn(*mut c_void) -> i32 = unsafe { symbol(c"aclrtSetCurrentContext") };
    if internal() || !enabled() { return unsafe { real(context) }; }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    mark_device_use();
    let rc = unsafe { real(context) };
    if rc == 0 { apply(&mut map, current_device(), policy.percent.unwrap()); }
    rc
}

pub fn reset_device(device: i32, force: bool) -> i32 {
    let real: unsafe extern "C" fn(i32) -> i32 = unsafe {
        symbol(if force { c"aclrtResetDeviceForce" } else { c"aclrtResetDevice" })
    };
    if internal() || !enabled() { return unsafe { real(device) }; }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    let rc = unsafe { real(device) };
    if rc == 0 {
        if let Some(budget) = map.get_mut(&device) { budget.applied = false; }
    }
    rc
}

pub fn set_device_resource(device: i32, resource: i32, value: u32) -> i32 {
    let real: DeviceSet = unsafe { symbol(c"aclrtSetDeviceResLimit") };
    if internal() || !enabled() || !(CUBE..=VECTOR).contains(&resource) {
        return unsafe { real(device, resource, value) };
    }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    apply(&mut map, device, policy.percent.unwrap());
    let cap = map[&device].limit[resource as usize];
    let rc = unsafe { real(device, resource, value.min(cap)) };
    if rc == 0 {
        let get: DeviceGet = unsafe { symbol(c"aclrtGetDeviceResLimit") };
        let mut readback = 0;
        if unsafe { get(device, resource, &mut readback) } != 0 || readback > cap {
            fatal("device override exceeds the resource budget");
        }
    }
    rc
}

pub fn reset_device_resources(device: i32) -> i32 {
    let real: unsafe extern "C" fn(i32) -> i32 = unsafe { symbol(c"aclrtResetDeviceResLimit") };
    if internal() || !enabled() { return unsafe { real(device) }; }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    apply(&mut map, device, policy.percent.unwrap());
    let rc = unsafe { real(device) };
    if rc == 0 {
        map.get_mut(&device).unwrap().applied = false;
        apply(&mut map, device, policy.percent.unwrap());
    }
    rc
}

fn stream_device(map: &HashMap<i32, Budget>, stream: *mut c_void) -> i32 {
    // CANN 9.1 exposes no public general stream-to-device query. Do not apply a
    // larger current-device cap to a stream belonging to a different device.
    let device = current_device();
    if !stream.is_null()
        && (map.len() > 1 || (!map.is_empty() && !map.contains_key(&device)))
    {
        fatal("explicit stream resource overrides with multiple devices are unsupported");
    }
    device
}

pub fn set_stream_resource(stream: *mut c_void, resource: i32, value: u32) -> i32 {
    let real: StreamSet = unsafe { symbol(c"aclrtSetStreamResLimit") };
    if internal() || !enabled() || !(CUBE..=VECTOR).contains(&resource) {
        return unsafe { real(stream, resource, value) };
    }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    let device = stream_device(&map, stream);
    apply(&mut map, device, policy.percent.unwrap());
    let cap = map[&device].limit[resource as usize];
    let rc = unsafe { real(stream, resource, value.min(cap)) };
    if rc == 0 {
        let get: StreamGet = unsafe { symbol(c"aclrtGetStreamResLimit") };
        let mut readback = 0;
        if unsafe { get(stream, resource, &mut readback) } != 0 || readback > cap {
            fatal("stream override exceeds the resource budget");
        }
    }
    rc
}

pub fn reset_stream_resources(stream: *mut c_void) -> i32 {
    let real: unsafe extern "C" fn(*mut c_void) -> i32 = unsafe { symbol(c"aclrtResetStreamResLimit") };
    if internal() || !enabled() { return unsafe { real(stream) }; }
    let policy = policy();
    let _internal = InternalGuard::enter();
    let mut map = devices(policy);
    let device = stream_device(&map, stream);
    apply(&mut map, device, policy.percent.unwrap());
    let rc = unsafe { real(stream) };
    if rc == 0 {
        let set: StreamSet = unsafe { symbol(c"aclrtSetStreamResLimit") };
        let get: StreamGet = unsafe { symbol(c"aclrtGetStreamResLimit") };
        for resource in CUBE..=VECTOR {
            let cap = map[&device].limit[resource as usize];
            if unsafe { set(stream, resource, cap) } != 0 { fatal("cannot restore stream resource budget"); }
            let mut readback = 0;
            if unsafe { get(stream, resource, &mut readback) } != 0 || readback != cap {
                fatal("stream resource reset readback mismatch");
            }
        }
    }
    rc
}
