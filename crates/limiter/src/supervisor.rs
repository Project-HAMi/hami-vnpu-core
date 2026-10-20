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
use crate::manager::{ContainerManager, REGISTRY_PROTOCOL};
use crate::registry_lock::RegistryLock;
use crate::shmem::setup;
use crate::shmem::{GlobalRegistry, LocalContainerShmem, MAX_MANAGERS, STATE_IDLE, futex};
use crate::worker::proc_alive;
use std::sync::Arc;

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

    crate::config::require_distinct_shmem_paths(&cfg.global_shm_path, &cfg.local_shm_path);
    let local = setup::create_shmem::<LocalContainerShmem>(&cfg.local_shm_path);
    let global = setup::open_global_registry(&cfg.global_shm_path);
    let registry = match RegistryLock::open(&cfg.global_shm_path) {
        Ok(lock) => Arc::new(lock),
        Err(error) => {
            warn!("[Supervisor] cannot open registry lock: {error}");
            return;
        }
    };
    {
        let Ok(_guard) = registry.lock() else {
            return;
        };
        let protocol = global.queue_head.load(Ordering::Acquire);
        if protocol != REGISTRY_PROTOCOL {
            // The layout remains compatible with device-plugin memory monitoring,
            // but old managers do not implement the fenced scheduling protocol.
            if protocol != 0
                || global
                    .slots
                    .iter()
                    .any(|s| s.is_active.load(Ordering::Acquire) != 0)
            {
                // Returning would eventually let the historical worker-startup
                // fallback run the application without quota enforcement.
                eprintln!(
                    "[Supervisor] incompatible scheduler registry: stop all sharing workloads and recreate the registry before upgrading"
                );
                unsafe {
                    libc::_exit(78);
                }
            }
            global.queue_tail.store(0, Ordering::Relaxed);
            global
                .queue_head
                .store(REGISTRY_PROTOCOL, Ordering::Release);
        }
    }
    loop {
        if std::process::id() as i32 != my_pid {
            return;
        }
        let manager = {
            let Ok(_guard) = registry.lock() else {
                return;
            };
            let predecessor_generation = local.manager_generation.load(Ordering::Acquire);
            if let Some(old_pid) = try_become_manager(local, my_pid) {
                reclaim_stale_global_slot(global, local, old_pid, predecessor_generation);
                // Former queue_tail is now a registry-wide lease sequence. Zero
                // remains reserved for uninitialized/legacy registrations.
                let generation = global
                    .queue_tail
                    .load(Ordering::Relaxed)
                    .wrapping_add(1)
                    .max(1);
                global.queue_tail.store(generation, Ordering::Relaxed);
                local
                    .manager_generation
                    .store(generation, Ordering::Release);
                local.overdraft_pool.close();
                local.tokens_remaining.store(0, Ordering::Relaxed);
                local.state.store(STATE_IDLE, Ordering::Release);
                futex::wake_all(&local.state);
                let manager = ContainerManager::new(
                    global,
                    local,
                    my_pid,
                    cfg.clone(),
                    registry.clone(),
                    generation,
                );
                if manager.is_none() {
                    local.manager_pid.store(0, Ordering::Release);
                    local.manager_global_idx.store(-1, Ordering::Release);
                }
                manager
            } else {
                None
            }
        };
        if let Some(mut manager) = manager {
            info!("[Supervisor PID:{}] won manager election", my_pid);
            manager.run();
            // Drop fences cleanup against the current generation while holding
            // the same transaction as takeover; an old manager cannot reset it.
        }
        thread::sleep(Duration::from_millis(POLL_MS));
    }
}

/// Caller must hold the registry transaction throughout election and publication.
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

/// On takeover, clear the dead predecessor's global registry slot so other pods don't
/// count this pod's priority more than once. `old_pid` is the manager we took over from;
/// `0` (fast-path win) means there is no predecessor, so nothing is reclaimed. Clearing is
/// gated on the slot still belonging to `old_pid`: this prevents a fresh pod — whose
/// `manager_global_idx` is still the zeroed default 0 — from wiping another live pod's
/// `global.slots[0]`, and leaves a since-reused slot untouched.
fn reclaim_stale_global_slot(
    global: &GlobalRegistry,
    local: &LocalContainerShmem,
    old_pid: i32,
    old_generation: u32,
) {
    if old_pid == 0 {
        return;
    }
    let idx = local.manager_global_idx.load(Ordering::Acquire);
    if idx < 0 || idx as usize >= MAX_MANAGERS {
        return;
    }
    let slot = &global.slots[idx as usize];
    if slot.pid.load(Ordering::Acquire) == old_pid
        && slot.generation.load(Ordering::Acquire) == old_generation
    {
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
