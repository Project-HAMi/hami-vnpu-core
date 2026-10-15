#![allow(dead_code, non_snake_case)]
extern crate self as limiter;
#[macro_export]
macro_rules! check_rts { ($call:expr) => {{ (unsafe { $call }) as u32 }}; }
macro_rules! passthrough {
    ($name:literal, ($($kind:ty),*), $($arg:expr),*) => {
        crate::native($name, &[$($arg as u64),*])
    };
}
include!(concat!(env!("OUT_DIR"), "/source.rs"));
pub mod shmem { include!(concat!(env!("OUT_DIR"), "/shared.rs")); }

#[derive(Clone, Debug, PartialEq)]
pub enum Call { StreamCreate, EventCreate, Record(u64,u64), Wait(u64,u64),
    StreamSync(u64), DeviceSync, Elapsed(u64,u64), Capture(u64), Native(&'static str, Vec<u64>,u64) }
pub static CALLS: std::sync::Mutex<Vec<Call>> = std::sync::Mutex::new(Vec::new());
pub fn trace(call: Call) { CALLS.lock().unwrap().push(call); }
pub fn calls() -> Vec<Call> { CALLS.lock().unwrap().clone() }
pub fn clear_calls() { CALLS.lock().unwrap().clear(); }
static CLIENT: std::sync::OnceLock<worker::SchedulerClient> = std::sync::OnceLock::new();
pub fn npu_limiter() -> &'static worker::SchedulerClient { CLIENT.get().expect("test client not installed") }
fn native(name: &'static str, args: &[u64]) -> u64 {
    #[cfg(test)] let remaining = worker::audit::remaining(npu_limiter());
    #[cfg(not(test))] let remaining = 0;
    trace(Call::Native(name, args.to_vec(), remaining));
    89
}
pub mod externed_api {
    use super::{Call,trace};
    use std::sync::atomic::{AtomicU64,Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(100);
    pub const RT_ERROR_NONE: u64 = 0;
    pub const RT_ERROR_MEMORY_ALLOCATION: u64 = 207001;
    pub unsafe fn rtStreamCreate(out: *mut u64, _: i32) -> i32 { unsafe { *out = NEXT.fetch_add(1,Ordering::Relaxed); } trace(Call::StreamCreate); 0 }
    pub unsafe fn rtEventCreate(out: *mut u64) -> usize { unsafe { *out = NEXT.fetch_add(1,Ordering::Relaxed); } trace(Call::EventCreate); 0 }
    pub unsafe fn rtEventRecord(event: u64, stream: u64) -> usize { trace(Call::Record(event,stream)); 0 }
    pub unsafe fn rtStreamWaitEvent(stream: u64, event: u64) -> usize { trace(Call::Wait(stream,event)); 0 }
    pub unsafe fn rtStreamSynchronize(stream: u64) -> i32 { trace(Call::StreamSync(stream)); 0 }
    pub unsafe fn rtDeviceSynchronize() -> i32 { trace(Call::DeviceSync); 0 }
    pub unsafe fn rtEventElapsedTime(out: *mut f32, first: u64,last:u64) -> usize { trace(Call::Elapsed(first,last)); unsafe { *out = 2.5; } 0 }
    pub unsafe fn rtSetDevice(_: i32) -> i32 { 0 }
    pub unsafe fn rtGetDevice(out: *mut i32) -> i32 { unsafe { *out=0; } 0 }
    pub unsafe fn rtStreamGetCaptureInfo(stream: u64, status: *mut u32,_: *mut u64) -> i32 { trace(Call::Capture(stream)); unsafe { *status=0; } 0 }
}
