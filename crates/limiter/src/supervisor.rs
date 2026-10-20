//! In-process manager supervisor.
//!
//! The manager daemon no longer runs as a separate binary. Instead every process
//! that loads `libvnpu.so` spawns a lightweight supervisor thread (see the ctor in
//! the `hook` crate). All supervisors in the same pod (same `NPU_LOCAL_SHM_PATH`)
//! race to become the *single* manager via a CAS on `LocalContainerShmem::manager_pid`.
//! The winner runs `ContainerManager::run()`; the losers stand by and watch, ready to
//! take over if the current manager dies. This guarantees exactly one manager per pod
//! while surviving crashes of whichever process happened to host it.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{info, warn};

use crate::config::ManagerConfig;
use crate::manager::ContainerManager;
use crate::shmem::setup;
use crate::shmem::{GlobalRegistry, LocalContainerShmem, MAX_MANAGERS, STATE_IDLE, futex};
use crate::worker::proc_alive;

/// A manager is declared dead if its PID is gone OR its heartbeat is older than this.
/// Kept comfortably above the global watchdog (1s) so the two mechanisms don't fight,
/// and well above the sub-millisecond window between winning election and the first
/// heartbeat write.
const TAKEOVER_TIMEOUT_US: u64 = 3_000_000;
/// How long to sleep between election attempts / watch polls.
const POLL_MS: u64 = 500;

/// A process must live at least this long before it may contend for the manager role.
/// `ld.so.preload` loads libvnpu into every process — including the short-lived helper
/// subprocesses CANN/torch spawn (TBE compilers etc.). Without this gate each one won the
/// election then exited, thrashing the manager (~45 takeovers/s). Real workers clear it easily.
const MANAGER_MIN_AGE_SECS: u64 = 3;

/// Entry point for the supervisor thread. Never returns under normal operation
/// (loops forever electing/watching); returns early only when this process cannot or
/// should not host a manager, in which case the process runs worker-only.
pub fn run() {
    block_all_signals();

    let my_pid = std::process::id() as i32;

    let cfg = match ManagerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            warn!("[Supervisor] {e}; manager disabled, running worker-only.");
            return;
        }
    };

    thread::sleep(Duration::from_secs(MANAGER_MIN_AGE_SECS));
    if std::process::id() as i32 != my_pid {
        return;
    }

    let local = setup::create_shmem::<LocalContainerShmem>(&cfg.local_shm_path);
    let mut global: Option<&'static GlobalRegistry> = None;

    loop {
        if std::process::id() as i32 != my_pid {
            return;
        }

        if let Some(old_pid) = try_become_manager(local, my_pid) {
            info!("[Supervisor PID:{}] won manager election", my_pid);
            let g =
                *global.get_or_insert_with(|| setup::open_global_registry(&cfg.global_shm_path));

            reclaim_stale_global_slot(g, local, old_pid);
            local.overdraft_pool.close();
            local.state.store(STATE_IDLE, Ordering::Release);
            futex::wake_all(&local.state);

            match ContainerManager::new(g, local, my_pid, cfg.clone()) {
                Some(mut mgr) => mgr.run(),
                None => warn!(
                    "[Supervisor PID:{}] global registry full; cannot manage.",
                    my_pid
                ),
            }

            let _ =
                local
                    .manager_pid
                    .compare_exchange(my_pid, 0, Ordering::AcqRel, Ordering::Relaxed);
        } else {
            watch(local);
        }

        thread::sleep(Duration::from_millis(POLL_MS));
    }
}

/// Attempt to claim the pod's single manager role. Returns `Some(predecessor_pid)` iff we
/// now own it — `0` for a fast-path win (no dead predecessor), or the dead manager's pid we
/// took over from (so the caller reclaims exactly that manager's global slot). `None` on loss.
fn try_become_manager(local: &LocalContainerShmem, my_pid: i32) -> Option<i32> {
    if local
        .manager_pid
        .compare_exchange(0, my_pid, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        local
            .manager_heartbeat
            .store(get_time_us(), Ordering::Release);
        return Some(0);
    }

    let owner = local.manager_pid.load(Ordering::Acquire);
    if owner == my_pid {
        return Some(0);
    }
    if owner != 0 && !manager_alive(local, owner) {
        if local
            .manager_pid
            .compare_exchange(owner, my_pid, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            local
                .manager_heartbeat
                .store(get_time_us(), Ordering::Release);
            warn!(
                "[Supervisor PID:{}] took over from dead/stale manager {}",
                my_pid, owner
            );
            return Some(owner);
        }
    }
    None
}

/// A manager is alive only if its PID is alive AND its heartbeat is fresh. The
/// heartbeat guards against PID recycling: a recycled PID can pass `proc_alive` while
/// the real manager is long gone.
fn manager_alive(local: &LocalContainerShmem, owner_pid: i32) -> bool {
    if !proc_alive(owner_pid) {
        return false;
    }
    let hb = local.manager_heartbeat.load(Ordering::Acquire);
    let now = get_time_us();
    now <= hb || now - hb <= TAKEOVER_TIMEOUT_US
}

/// Stand by while another process holds the manager role; return once it vacates or
/// looks dead, so the caller re-enters election.
fn watch(local: &LocalContainerShmem) {
    loop {
        let owner = local.manager_pid.load(Ordering::Acquire);
        if owner == 0 || !manager_alive(local, owner) {
            return;
        }
        thread::sleep(Duration::from_millis(POLL_MS));
    }
}

/// On takeover, clear the dead predecessor's global registry slot so other pods don't
/// count this pod's priority more than once. `old_pid` is the manager we took over from;
/// `0` (fast-path win) means there is no predecessor, so nothing is reclaimed. Clearing is
/// gated on the slot still belonging to `old_pid`: this prevents a fresh pod — whose
/// `manager_global_idx` is still the zeroed default 0 — from wiping another live pod's
/// `global.slots[0]`, and leaves a since-reused slot untouched.
fn reclaim_stale_global_slot(global: &GlobalRegistry, local: &LocalContainerShmem, old_pid: i32) {
    if old_pid == 0 {
        return;
    }
    let idx = local.manager_global_idx.load(Ordering::Acquire);
    if idx < 0 || idx as usize >= MAX_MANAGERS {
        return;
    }
    let slot = &global.slots[idx as usize];
    if slot.pid.load(Ordering::Acquire) == old_pid {
        slot.is_active.store(0, Ordering::Release);
        warn!(
            "[Supervisor] cleared stale global slot {} of dead manager {}",
            idx, old_pid
        );
    }
}

/// Block every signal on the calling thread so signals are delivered to the AI app's
/// threads, not this background daemon thread.
fn block_all_signals() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut set);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn get_time_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}
