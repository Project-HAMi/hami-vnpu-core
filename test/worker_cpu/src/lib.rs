#![allow(dead_code, non_snake_case)]
mod wait_observer;
extern crate self as limiter;
#[path = "../../../crates/limiter/src/dynamic.rs"]
pub mod dynamic;
#[macro_export]
macro_rules! check_rts { ($call:expr) => {{ ($call) as u32 }}; }
macro_rules! passthrough {
    ($name:literal, ($($kind:ty),*), $($arg:expr),*) => {
        crate::native($name, &[$($arg as u64),*])
    };
}
include!(concat!(env!("OUT_DIR"), "/source.rs"));
pub mod shmem { include!(concat!(env!("OUT_DIR"), "/shared.rs")); }

#[derive(Clone, Debug, PartialEq)]
pub enum Call { StreamCreate(u64), EventCreate(u64), Record(u64,u64), Wait(u64,u64),
    StreamSync(u64), DeviceSync, Elapsed(u64,u64), Capture(u64), Native(&'static str, Vec<u64>,u64) }
pub static CALLS: std::sync::Mutex<Vec<Call>> = std::sync::Mutex::new(Vec::new());
pub fn trace(call: Call) { CALLS.lock().unwrap().push(call); }
pub fn calls() -> Vec<Call> { CALLS.lock().unwrap().clone() }
pub fn clear_calls() { CALLS.lock().unwrap().clear(); }
static DEVICE_ONE: std::sync::OnceLock<worker::SchedulerClient> = std::sync::OnceLock::new();
static NATIVE_MEMORY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static NEXT_MEMORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(10000);
static CLIENT: std::sync::OnceLock<worker::SchedulerClient> = std::sync::OnceLock::new();
static NATIVE_OVERRIDE: std::sync::OnceLock<fn(&'static str, &[u64]) -> Option<u64>> = std::sync::OnceLock::new();
pub fn npu_limiter() -> &'static worker::SchedulerClient {
    npu_limiter_for_device(externed_api::current_device())
}
pub fn npu_limiter_for_device(logical: i32) -> &'static worker::SchedulerClient {
    match logical {
        0 => CLIENT.get().expect("test client not installed"),
        1 => DEVICE_ONE.get().expect("target card client not installed"),
        _ => panic!("invalid test logical device"),
    }
}
pub(crate) fn claim_hbm_free(handle: u64, kind: worker::AllocationKind) -> Option<worker::HbmFreeTicket> {
    for client in [CLIENT.get(), DEVICE_ONE.get()].into_iter().flatten() {
        if let Some(ticket) = client.claim_hbm_free(handle, kind) {
            return Some(ticket);
        }
    }
    None
}
pub(crate) fn prepare_hbm_retain() -> worker::HbmRetainTicket {
    worker::prepare_hbm_retain(|| [CLIENT.get(), DEVICE_ONE.get()].into_iter().flatten().cloned().collect())
}
pub(crate) fn retire_context(context: u64) {
    if let Some(client) = CLIENT.get() { client.retire_context(context); }
}
fn native(name: &'static str, args: &[u64]) -> u64 {
    #[cfg(test)] let remaining = worker::audit::remaining(npu_limiter());
    #[cfg(not(test))] let remaining = 0;
    trace(Call::Native(name, args.to_vec(), remaining));
    if let Some(handler) = NATIVE_OVERRIDE.get()
        && let Some(result) = handler(name, args)
    {
        return result;
    }
    if NATIVE_MEMORY.load(std::sync::atomic::Ordering::Acquire) {
        if name == "rtMalloc" || name == "rtMallocPhysical" {
            let handle = NEXT_MEMORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            unsafe { *(args[0] as *mut u64) = handle; }
        }
        return 0;
    }
    89
}
pub mod externed_api {
    use super::{Call,trace};
    use std::sync::atomic::{AtomicU64,Ordering};
    static OWNERS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u64, u64>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    fn track_resource(handle: u64) { OWNERS.lock().unwrap().insert(handle, CONTEXT.get()); }
    fn check_resource(handle: u64) {
        if let Some(owner) = OWNERS.lock().unwrap().get(&handle) {
            assert_eq!(*owner, CONTEXT.get(), "timing resource reused in another context");
        }
    }
    static NEXT: AtomicU64 = AtomicU64::new(100);
    pub const RT_ERROR_NONE: u64 = 0;
    pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;
    pub unsafe fn rtStreamCreate(out: *mut u64, _: i32) -> i32 { unsafe { *out = NEXT.fetch_add(1,Ordering::Relaxed); } track_resource(unsafe { *out }); trace(Call::StreamCreate(CONTEXT.get())); 0 }
    pub unsafe fn rtEventCreate(out: *mut u64) -> usize { unsafe { *out = NEXT.fetch_add(1,Ordering::Relaxed); } track_resource(unsafe { *out }); trace(Call::EventCreate(CONTEXT.get())); 0 }
    pub unsafe fn rtEventRecord(event: u64, stream: u64) -> usize { check_resource(event); check_resource(stream); trace(Call::Record(event,stream)); 0 }
    pub unsafe fn rtStreamWaitEvent(stream: u64, event: u64) -> usize { check_resource(stream); check_resource(event); trace(Call::Wait(stream,event)); 0 }
    pub unsafe fn rtStreamSynchronize(stream: u64) -> i32 { check_resource(stream); trace(Call::StreamSync(stream)); 0 }
    pub unsafe fn rtDeviceSynchronize() -> i32 { trace(Call::DeviceSync); 0 }
    pub unsafe fn rtEventElapsedTime(out: *mut f32, first: u64,last:u64) -> usize { check_resource(first); check_resource(last); trace(Call::Elapsed(first,last)); unsafe { *out = 2.5; } 0 }
    std::thread_local! { static CONTEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
    pub fn set_context(context: u64) { CONTEXT.set(context); }
    pub unsafe fn rtCtxGetCurrent(out: *mut u64) -> i32 { unsafe { *out = CONTEXT.get() }; 0 }
    pub unsafe fn rtGetDevicePhyIdByIndex(logical: u32, out: *mut u32) -> i32 {
        if logical > 1 { return 107001; }
        unsafe { *out = logical }; 0
    }
    std::thread_local! { static DEVICE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) }; }
    pub fn current_device() -> i32 { DEVICE.get() }
    pub unsafe fn rtSetDevice(device: i32) -> i32 { DEVICE.set(device); 0 }
    pub unsafe fn rtGetDevice(out: *mut i32) -> i32 { unsafe { *out=DEVICE.get(); } 0 }
    pub unsafe fn rtStreamGetCaptureInfo(stream: u64, status: *mut u32,_: *mut u64) -> i32 { trace(Call::Capture(stream)); unsafe { *status=0; } 0 }
}
