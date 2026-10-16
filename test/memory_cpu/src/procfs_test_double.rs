//! Counts deterministic test-double calls, never claims to count real syscalls.
use std::{cell::RefCell, collections::HashMap, io, path::Path};

#[derive(Clone, Copy)]
pub enum Answer { Live, Zombie, Unreadable }
#[derive(Default)]
struct Script { answers: HashMap<i32, Answer>, calls: Vec<i32>, replacement: Option<(i32, usize, i32)> }
thread_local! { static SCRIPT: RefCell<Script> = RefCell::new(Script::default()); }

pub fn set(answers: &[(i32, Answer)]) {
    SCRIPT.with(|state| *state.borrow_mut() = Script { answers: answers.iter().copied().collect(), calls: Vec::with_capacity(128), replacement: None });
}
pub fn calls() -> Vec<i32> { SCRIPT.with(|state| state.borrow().calls.clone()) }
// Simulate the slot owner changing between the actual worker's PID load and CAS.
// The pointer is only used synchronously inside read_to_string by the owning test.
pub fn replace_pid_on_read(pid: i32, field: &std::sync::atomic::AtomicI32, replacement: i32) {
    SCRIPT.with(|state| state.borrow_mut().replacement = Some((pid, field as *const _ as usize, replacement)));
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref().to_str().expect("test expects UTF-8 proc path");
    let pid: i32 = path.strip_prefix("/proc/").and_then(|p| p.strip_suffix("/stat"))
        .expect("unexpected worker fs operation: only /proc/<pid>/stat is modeled").parse().unwrap();
    SCRIPT.with(|state| {
        let mut state = state.borrow_mut();
        state.calls.push(pid);
        if let Some((target, address, replacement)) = state.replacement {
            if target == pid {
                unsafe { &*(address as *const std::sync::atomic::AtomicI32) }
                    .store(replacement, std::sync::atomic::Ordering::Release);
                state.replacement = None;
            }
        }
        match *state.answers.get(&pid).expect("unscripted proc PID read") {
            Answer::Live => Ok(format!("{pid} (test process) S 0 0 0\n")),
            Answer::Zombie => Ok(format!("{pid} (test process) Z 0 0 0\n")),
            Answer::Unreadable => Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected proc read failure")),
        }
    })
}
