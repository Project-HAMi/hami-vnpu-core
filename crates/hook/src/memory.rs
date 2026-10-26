//! Shared accounting for virtual device allocation entry points.

use std::cell::Cell;

use limiter::externed_api::{RT_ERROR_MEMORY_ALLOCATION, RT_ERROR_NONE};

const MAX_NATIVE_DEPTH: usize = 32;

#[derive(Clone, Copy)]
struct State {
    pid: u32,
    initializing: bool,
    depth: usize,
    outputs: [u64; MAX_NATIVE_DEPTH],
}

impl State {
    const fn new(pid: u32) -> Self {
        Self {
            pid,
            initializing: false,
            depth: 0,
            outputs: [0; MAX_NATIVE_DEPTH],
        }
    }
}

thread_local! { static STATE: Cell<State> = const { Cell::new(State::new(0)) }; }

fn state() -> State {
    let pid = std::process::id();
    STATE.with(|cell| {
        let mut state = cell.get();
        if state.pid != pid {
            // A fork inside a native callback must not inherit a quota bypass.
            // The normal per-device/spatial process checks still run below.
            state = State::new(pid);
            cell.set(state);
        }
        state
    })
}

struct Initializing(u32);

impl Initializing {
    fn enter() -> Self {
        let mut state = state();
        state.initializing = true;
        STATE.with(|cell| cell.set(state));
        Self(state.pid)
    }
}

impl Drop for Initializing {
    fn drop(&mut self) {
        let mut state = state();
        if state.pid == self.0 {
            state.initializing = false;
            STATE.with(|cell| cell.set(state));
        }
    }
}

struct NativeCall(u32);

impl NativeCall {
    fn enter(output: u64) -> Option<Self> {
        let mut state = state();
        if state.depth == MAX_NATIVE_DEPTH {
            return None;
        }
        state.outputs[state.depth] = output;
        state.depth += 1;
        STATE.with(|cell| cell.set(state));
        Some(Self(state.pid))
    }
}

impl Drop for NativeCall {
    fn drop(&mut self) {
        let mut state = state();
        if state.pid == self.0 {
            state.depth -= 1;
            STATE.with(|cell| cell.set(state));
        }
    }
}

fn invoke(output: u64, native: impl FnOnce() -> u64) -> u64 {
    let Some(_call) = NativeCall::enter(output) else {
        return RT_ERROR_MEMORY_ALLOCATION;
    };
    native()
}

pub(crate) fn allocate_virtual(
    output: u64,
    size: u64,
    legacy_malloc: bool,
    native: impl FnOnce() -> u64,
) -> u64 {
    let current = state();
    if current.initializing || current.outputs[..current.depth].contains(&output) {
        // Cached/configured allocators may delegate to another exported entry
        // point with the same output pointer. Only that outer allocation owns
        // the reservation. A nested allocation to another output is independent.
        return invoke(output, native);
    }
    if output == 0 || size == 0 {
        // Keep native validation without dereferencing an invalid output or
        // starting a device client for an invalid/empty allocation.
        return invoke(output, native);
    }
    let client = {
        // Runtime resources created while initializing this client can reenter
        // an allocation hook; do not recursively enter its OnceLock.
        let _initializing = Initializing::enter();
        crate::npu_limiter()
    };
    if !client.is_hbm_limited() {
        return invoke(output, native);
    }
    if client.check_memory_quota(size) != RT_ERROR_NONE {
        return RT_ERROR_MEMORY_ALLOCATION;
    }
    let result = invoke(output, native);
    if result == RT_ERROR_NONE {
        // The successful runtime allocation published its pointer at output.
        let pointer = unsafe { std::ptr::read_unaligned(output as *const u64) };
        client.post_alloc_hbm(pointer, size, result);
    } else {
        client.post_alloc_hbm(0, size, result);
        if legacy_malloc {
            // Preserve rtMalloc's existing error translation only after its
            // own quota reservation. Native aliases and unlimited calls return
            // above, so their native errors remain untouched.
            return RT_ERROR_MEMORY_ALLOCATION;
        }
    }
    result
}

// Free/retain aliases use a separate stack from allocation output pointers.
// Include the operation kind so equal virtual/physical numbers cannot collide.
#[derive(Clone, Copy)]
struct LifetimeState {
    pid: u32,
    depth: usize,
    keys: [(u8, u64); MAX_NATIVE_DEPTH],
}

impl LifetimeState {
    const fn new(pid: u32) -> Self {
        Self {
            pid,
            depth: 0,
            keys: [(0, 0); MAX_NATIVE_DEPTH],
        }
    }
}

thread_local! {
    static LIFETIME_STATE: Cell<LifetimeState> = const { Cell::new(LifetimeState::new(0)) };
}

struct LifetimeCall(u32);

impl LifetimeCall {
    fn enter(key: (u8, u64)) -> Option<(Self, bool)> {
        LIFETIME_STATE.with(|cell| {
            let mut state = cell.get();
            let pid = std::process::id();
            if state.pid != pid {
                state = LifetimeState::new(pid);
            }
            if state.depth == MAX_NATIVE_DEPTH {
                return None;
            }
            let alias = state.keys[..state.depth].contains(&key);
            state.keys[state.depth] = key;
            state.depth += 1;
            cell.set(state);
            Some((Self(pid), alias))
        })
    }
}

impl Drop for LifetimeCall {
    fn drop(&mut self) {
        LIFETIME_STATE.with(|cell| {
            let mut state = cell.get();
            if state.pid == self.0 {
                state.depth -= 1;
                cell.set(state);
            }
        });
    }
}

pub(crate) fn free_with_accounting(
    handle: u64,
    kind: limiter::worker::AllocationKind,
    native: impl FnOnce() -> u64,
) -> u64 {
    use limiter::worker::AllocationKind;
    let key = (
        if kind == AllocationKind::Virtual {
            0
        } else {
            1
        },
        handle,
    );
    let Some((_call, alias)) = LifetimeCall::enter(key) else {
        return RT_ERROR_MEMORY_ALLOCATION;
    };
    let ticket = if alias {
        None
    } else {
        crate::claim_hbm_free(handle, kind)
    };
    let result = native();
    if let Some(ticket) = ticket {
        ticket.finish(result);
    }
    result
}

pub(crate) fn retain_with_accounting(output: u64, native: impl FnOnce() -> u64) -> u64 {
    let Some((_call, alias)) = LifetimeCall::enter((2, output)) else {
        return RT_ERROR_MEMORY_ALLOCATION;
    };
    let ticket = if alias {
        None
    } else {
        Some(crate::prepare_hbm_retain())
    };
    let result = native();
    let handle = if result == RT_ERROR_NONE && output != 0 {
        Some(unsafe { *(output as *const u64) })
    } else {
        None
    };
    if let Some(ticket) = ticket {
        ticket.finish(handle);
    }
    result
}
