//! Process-local lifecycle records for opaque CANN stream and context handles.
//!
//! CANN 8.5.2 has no general stream-to-device query. Record ownership at creation
//! rather than interpreting a non-default stream using the calling thread's card.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub device: i32,
    pub context: usize,
}

#[derive(Clone, Copy)]
struct StreamRecord {
    owner: Owner,
    context_generation: u64,
    generation: u64,
}

#[derive(Clone, Copy)]
struct ContextRecord {
    device: i32,
    generation: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct ContextSnapshot {
    context: usize,
    record: Option<ContextRecord>,
}

#[derive(Default)]
struct Records {
    streams: HashMap<usize, StreamRecord>,
    contexts: HashMap<usize, ContextRecord>,
    generation: u64,
}

impl Records {
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.checked_add(1).unwrap_or_else(|| {
            crate::spatial::fatal("CANN ownership generation counter exhausted")
        });
        self.generation
    }

    fn context_generation(&mut self, owner: Owner) -> u64 {
        if let Some(record) = self.contexts.get(&owner.context)
            && record.device == owner.device
        {
            return record.generation;
        }
        let generation = self.next_generation();
        self.contexts.insert(
            owner.context,
            ContextRecord {
                device: owner.device,
                generation,
            },
        );
        generation
    }
}

static RECORDS: crate::process::ProcessLocal<Mutex<Records>> = crate::process::ProcessLocal::new();
static CONTEXT_LIFECYCLE: crate::process::ProcessLocal<Mutex<()>> =
    crate::process::ProcessLocal::new();
thread_local! { static NATIVE_DEPTH: Cell<usize> = const { Cell::new(0) }; }

// Serialize generation publication and timing retirement, without holding the
// ownership lock while entering a worker or holding either lock across native calls.
fn context_lifecycle() -> MutexGuard<'static, ()> {
    CONTEXT_LIFECYCLE
        .get_or_init(std::process::id(), || Mutex::new(()))
        .lock()
        .unwrap_or_else(|_| crate::spatial::fatal("CANN context lifecycle lock is poisoned"))
}

fn records() -> MutexGuard<'static, Records> {
    RECORDS
        .get_or_init(std::process::id(), || Mutex::new(Records::default()))
        .lock()
        .unwrap_or_else(|_| crate::spatial::fatal("CANN ownership lock is poisoned"))
}

unsafe fn symbol<T: Copy>(name: &CStr) -> T {
    let address = limiter::dynamic::sdk_symbol(name)
        .unwrap_or_else(|| crate::spatial::fatal(name.to_str().unwrap_or("missing CANN symbol")));
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&address));
    unsafe { std::mem::transmute_copy(&address) }
}

pub(crate) fn current_owner() -> Owner {
    let get_device: unsafe extern "C" fn(*mut i32) -> i32 = unsafe { symbol(c"rtGetDevice") };
    let get_context: unsafe extern "C" fn(*mut *mut c_void) -> i32 =
        unsafe { symbol(c"rtCtxGetCurrent") };
    let mut device = -1;
    let mut context = std::ptr::null_mut();
    if unsafe { get_device(&mut device) } != 0 || device < 0 {
        crate::spatial::fatal("cannot obtain the current stream device");
    }
    if unsafe { get_context(&mut context) } != 0 || context.is_null() {
        crate::spatial::fatal("cannot obtain the current stream context");
    }
    Owner {
        device,
        context: context as usize,
    }
}

pub(crate) fn owner_for_stream(stream: *mut c_void) -> Option<Owner> {
    if stream.is_null() {
        Some(current_owner())
    } else {
        records()
            .streams
            .get(&(stream as usize))
            .map(|record| record.owner)
    }
}

struct NativeGuard;
impl NativeGuard {
    fn enter() -> (Self, bool) {
        // Check the PID before inherited TLS: a fork inside a native callback
        // must not bypass spatial's post-initialization fork guard.
        if crate::spatial::enabled() {
            crate::spatial::mark_device_use();
        }
        let outer = NATIVE_DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(current + 1);
            current == 0
        }) && !crate::spatial::internal();
        (Self, outer)
    }
}
impl Drop for NativeGuard {
    fn drop(&mut self) {
        NATIVE_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

unsafe fn created_stream(output: *mut *mut c_void) {
    // A successful native create must have supplied a live non-default handle.
    let stream = unsafe { *output };
    if stream.is_null() {
        crate::spatial::fatal("successful stream creation returned a null handle");
    }
    let owner = current_owner();
    let mut records = records();
    let context_generation = records.context_generation(owner);
    let generation = records.next_generation();
    records.streams.insert(
        stream as usize,
        StreamRecord {
            owner,
            context_generation,
            generation,
        },
    );
}

fn stream_record(stream: *mut c_void) -> Option<StreamRecord> {
    records().streams.get(&(stream as usize)).copied()
}

fn destroyed_stream(stream: *mut c_void, before: Option<StreamRecord>) {
    let Some(before) = before else {
        return;
    };
    let mut records = records();
    if records
        .streams
        .get(&(stream as usize))
        .is_some_and(|record| record.generation == before.generation)
    {
        records.streams.remove(&(stream as usize));
    }
}

unsafe fn created_context(output: *mut *mut c_void, device: i32) {
    let context = unsafe { *output };
    if context.is_null() {
        crate::spatial::fatal("successful context creation returned a null handle");
    }
    {
        let _lifecycle = context_lifecycle();
        // A successful create begins a new lifetime. Retire the former timing
        // before publishing its replacement; a late old destroy cannot retire
        // timing belonging to the new generation.
        crate::retire_context(context as u64);
        let mut records = records();
        let generation = records.next_generation();
        records
            .contexts
            .insert(context as usize, ContextRecord { device, generation });
        // A reused context address cannot inherit streams from its former life.
        records
            .streams
            .retain(|_, record| record.owner.context != context as usize);
    }
    crate::spatial::ensure_device(device);
}

fn context_record(context: *mut c_void) -> Option<ContextRecord> {
    records().contexts.get(&(context as usize)).copied()
}

pub(crate) fn snapshot_context(context: *mut c_void) -> ContextSnapshot {
    ContextSnapshot {
        context: context as usize,
        record: context_record(context),
    }
}

pub(crate) fn retire_context_snapshot(snapshot: ContextSnapshot) {
    destroyed_context(snapshot.context as *mut c_void, snapshot.record);
}

fn destroyed_context(context: *mut c_void, before: Option<ContextRecord>) {
    let _lifecycle = context_lifecycle();
    let retire = {
        let mut records = records();
        match before {
            Some(before) => {
                records.streams.retain(|_, record| {
                    record.owner.context != context as usize
                        || record.context_generation != before.generation
                });
                if records
                    .contexts
                    .get(&(context as usize))
                    .is_some_and(|record| record.generation == before.generation)
                {
                    records.contexts.remove(&(context as usize));
                    true
                } else {
                    false
                }
            }
            None => !records.contexts.contains_key(&(context as usize)),
        }
    };
    if retire {
        crate::retire_context(context as u64);
    }
}

// ACL may delegate to the exported runtime API. Only the outer lifecycle call
// records its returned handle; no bookkeeping mutex is held across native code.
#[unsafe(no_mangle)]
unsafe extern "C" fn aclrtCreateStream(stream: *mut *mut c_void) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void) -> i32 =
        unsafe { symbol(c"aclrtCreateStream") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(stream) };
    if rc == 0 && outer {
        unsafe { created_stream(stream) };
    }
    rc
}

#[unsafe(no_mangle)]
unsafe extern "C" fn aclrtCreateStreamWithConfig(
    stream: *mut *mut c_void,
    priority: u32,
    flag: u32,
) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, u32, u32) -> i32 =
        unsafe { symbol(c"aclrtCreateStreamWithConfig") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(stream, priority, flag) };
    if rc == 0 && outer {
        unsafe { created_stream(stream) };
    }
    rc
}

#[unsafe(no_mangle)]
unsafe extern "C" fn aclrtCreateStreamV2(stream: *mut *mut c_void, handle: *const c_void) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> i32 =
        unsafe { symbol(c"aclrtCreateStreamV2") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(stream, handle) };
    if rc == 0 && outer {
        unsafe { created_stream(stream) };
    }
    rc
}

macro_rules! destroy_stream {
    ($function:ident, $symbol:literal) => {
        #[unsafe(no_mangle)]
        unsafe extern "C" fn $function(stream: *mut c_void) -> i32 {
            let real: unsafe extern "C" fn(*mut c_void) -> i32 = unsafe { symbol($symbol) };
            let (_guard, outer) = NativeGuard::enter();
            let before = outer.then(|| stream_record(stream)).flatten();
            let rc = unsafe { real(stream) };
            if rc == 0 && outer {
                destroyed_stream(stream, before);
            }
            rc
        }
    };
}
destroy_stream!(aclrtDestroyStream, c"aclrtDestroyStream");
destroy_stream!(aclrtDestroyStreamForce, c"aclrtDestroyStreamForce");

#[unsafe(no_mangle)]
unsafe extern "C" fn aclrtCreateContext(context: *mut *mut c_void, device: i32) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, i32) -> i32 =
        unsafe { symbol(c"aclrtCreateContext") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(context, device) };
    if rc == 0 && outer {
        unsafe { created_context(context, device) };
    }
    rc
}

macro_rules! destroy_context {
    ($function:ident, $symbol:literal) => {
        #[unsafe(no_mangle)]
        unsafe extern "C" fn $function(context: *mut c_void) -> i32 {
            let real: unsafe extern "C" fn(*mut c_void) -> i32 = unsafe { symbol($symbol) };
            let (_guard, outer) = NativeGuard::enter();
            let before = outer.then(|| context_record(context)).flatten();
            let rc = unsafe { real(context) };
            if rc == 0 && outer {
                destroyed_context(context, before);
            }
            rc
        }
    };
}
destroy_context!(aclrtDestroyContext, c"aclrtDestroyContext");

#[unsafe(no_mangle)]
unsafe extern "C" fn rtStreamCreate(stream: *mut *mut c_void, priority: i32) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, i32) -> i32 =
        unsafe { symbol(c"rtStreamCreate") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(stream, priority) };
    if rc == 0 && outer {
        unsafe { created_stream(stream) };
    }
    rc
}

#[unsafe(no_mangle)]
unsafe extern "C" fn rtStreamCreateWithFlags(
    stream: *mut *mut c_void,
    priority: i32,
    flags: u32,
) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, i32, u32) -> i32 =
        unsafe { symbol(c"rtStreamCreateWithFlags") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(stream, priority, flags) };
    if rc == 0 && outer {
        unsafe { created_stream(stream) };
    }
    rc
}

#[unsafe(no_mangle)]
unsafe extern "C" fn rtStreamCreateWithConfig(
    stream: *mut *mut c_void,
    handle: *mut c_void,
) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, *mut c_void) -> i32 =
        unsafe { symbol(c"rtStreamCreateWithConfig") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(stream, handle) };
    if rc == 0 && outer {
        unsafe { created_stream(stream) };
    }
    rc
}

destroy_stream!(rtStreamDestroy, c"rtStreamDestroy");
destroy_stream!(rtStreamDestroyForce, c"rtStreamDestroyForce");

#[unsafe(no_mangle)]
unsafe extern "C" fn rtCtxCreate(context: *mut *mut c_void, flags: u32, device: i32) -> i32 {
    let real: unsafe extern "C" fn(*mut *mut c_void, u32, i32) -> i32 =
        unsafe { symbol(c"rtCtxCreate") };
    let (_guard, outer) = NativeGuard::enter();
    let rc = unsafe { real(context, flags, device) };
    if rc == 0 && outer {
        unsafe { created_context(context, device) };
    }
    rc
}

destroy_context!(rtCtxDestroy, c"rtCtxDestroy");

macro_rules! reset_device {
    ($function:ident, $symbol:literal, $force:literal) => {
        #[unsafe(no_mangle)]
        unsafe extern "C" fn $function(device: i32) -> i32 {
            let real: unsafe extern "C" fn(i32) -> i32 = unsafe { symbol($symbol) };
            crate::spatial::runtime_reset(device, $force, || unsafe { real(device) })
        }
    };
}
reset_device!(rtDeviceReset, c"rtDeviceReset", false);
reset_device!(rtDeviceResetEx, c"rtDeviceResetEx", false);
reset_device!(rtDeviceResetWithoutTsd, c"rtDeviceResetWithoutTsd", false);
reset_device!(rtDeviceResetForce, c"rtDeviceResetForce", true);
