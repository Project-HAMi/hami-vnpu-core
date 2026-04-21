// reporter.rs
//
// Utilization reporter. One background thread per manager observes the global
// "baton" (scheduling lock) in shared memory. It wakes on every baton pass
// (`signal_counter` increments in `pass_baton`) and on fixed window boundaries
// (monotonic deadline), so we do not poll `lock_owner` on a short sleep loop.
//
// Window timing is independent: baton bookkeeping does not move the next
// flush deadline; if multiple windows are overdue we flush them in sequence.
//
// Per-window utilization is (effective baton time) / (window length). The effective
// baton time is 0 for a window in which no tokens were taken from the local
// bucket, so we do not show compute utilization when nothing actually ran.

use crate::shmem::{futex, GlobalRegistry, LocalContainerShmem};
use log::info;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// Fixed-point scale for utilization (ppm). Percentage = util_fp / UTIL_SCALE * 100.
const UTIL_SCALE: u64 = 1_000_000;

/// Snapshot of the tracker state returned to API consumers.
#[derive(Debug, Clone, Copy)]
pub struct UtilizationSnapshot {
    pub last_interval_percent: f64,
    pub recent_avg_percent: f64,
    pub interval_ms: u64,
    pub history_scale: u64,
    pub tracked: bool,
}

struct ReporterState {
    last_util_fp: u64,
    history: VecDeque<u64>,
    tracked: bool,
}

/// Observes baton ownership in a single `GlobalRegistry` and publishes rolling
/// utilization numbers.
pub struct UtilizationReporter {
    interval_ms: u64,
    history_scale: usize,
    state: Mutex<ReporterState>,
}

impl UtilizationReporter {
    /// Build a reporter using the standard environment variables:
    /// - `NPU_REPORT_INTERVAL_MS` (default 1000). Set to 0 to disable the
    ///   background thread (snapshots will report 0% and `tracked=false`).
    /// - `NPU_REPORT_HISTORY_SCALE` (default 10, min 1).
    pub fn from_env() -> Arc<Self> {
        let interval_ms = std::env::var("NPU_REPORT_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1000);
        let history_scale = std::env::var("NPU_REPORT_HISTORY_SCALE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10)
            .max(1);
        Arc::new(Self {
            interval_ms,
            history_scale,
            state: Mutex::new(ReporterState {
                last_util_fp: 0,
                history: VecDeque::with_capacity(history_scale),
                tracked: false,
            }),
        })
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    pub fn history_scale(&self) -> u64 {
        self.history_scale as u64
    }

    /// Read the current utilization snapshot. Cheap: takes a short mutex.
    pub fn snapshot(&self) -> UtilizationSnapshot {
        let st = self.state.lock().unwrap();
        let last =
            ((st.last_util_fp as f64) / (UTIL_SCALE as f64) * 100.0).clamp(0.0, 100.0);
        let avg_fp = if st.history.is_empty() {
            0
        } else {
            st.history.iter().copied().sum::<u64>() / (st.history.len() as u64)
        };
        let avg = ((avg_fp as f64) / (UTIL_SCALE as f64) * 100.0).clamp(0.0, 100.0);
        UtilizationSnapshot {
            last_interval_percent: last,
            recent_avg_percent: avg,
            interval_ms: self.interval_ms,
            history_scale: self.history_scale as u64,
            tracked: st.tracked,
        }
    }

    /// Spawn the background thread. Wakes on baton handoffs (`signal_counter`)
    /// and on window boundaries; no fixed-interval polling of `lock_owner`.
    pub fn start(
        self: &Arc<Self>,
        global: &'static GlobalRegistry,
        local: &'static LocalContainerShmem,
        my_pid: i32,
    ) {
        if self.interval_ms == 0 {
            info!("[Reporter] NPU_REPORT_INTERVAL_MS=0, utilization reporting disabled");
            return;
        }
        let this = self.clone();
        let _ = thread::Builder::new()
            .name(format!("npu_util_reporter_{}", my_pid))
            .spawn(move || this.run(global, local, my_pid));
    }

    /// Wait until the manager registers its slot, waking on baton activity or
    /// a short timeout instead of a tight sleep loop.
    fn locate_slot(&self, global: &'static GlobalRegistry, my_pid: i32) -> u32 {
        loop {
            for (i, slot) in global.slots.iter().enumerate() {
                if slot.is_active.load(Ordering::Relaxed) == 1
                    && slot.pid.load(Ordering::Relaxed) == my_pid
                {
                    return i as u32;
                }
            }
            let sig = global.signal_counter.load(Ordering::Relaxed);
            futex::wait_timeout(&global.signal_counter, sig, 20_000);
        }
    }

    fn run(
        self: Arc<Self>,
        global: &'static GlobalRegistry,
        local: &'static LocalContainerShmem,
        my_pid: i32,
    ) {
        let my_idx = self.locate_slot(global, my_pid);
        {
            let mut st = self.state.lock().unwrap();
            st.tracked = true;
        }
        info!(
            "[Reporter] tracking global slot {} (pid {}), interval_ms={}, history_scale={} (baton + window futex)",
            my_idx, my_pid, self.interval_ms, self.history_scale
        );

        let window_dur = Duration::from_millis(self.interval_ms);
        let max_history = self.history_scale;

        let mut window_start = Instant::now();
        let mut window_deadline = window_start + window_dur;

        let mut busy_us: u64 = 0;
        let mut last_tokens = local
            .tokens_consumed_cumulative
            .load(Ordering::Relaxed);
        let mut owned = global.lock_owner.load(Ordering::Acquire) == my_idx;
        let mut segment_start: Option<Instant> = if owned {
            Some(window_start)
        } else {
            None
        };

        loop {
            let iteration_sig = global.signal_counter.load(Ordering::Relaxed);

            // 1) Baton transitions at current time (same ordering as the
            //    reference busy/idle tracker: apply events before closing a window).
            let now = Instant::now();
            let cur_owner = global.lock_owner.load(Ordering::Acquire);
            let currently_owned = cur_owner == my_idx;
            if currently_owned != owned {
                if owned {
                    if let Some(s) = segment_start.take() {
                        if now > s {
                            busy_us = busy_us
                                .saturating_add(now.saturating_duration_since(s).as_micros() as u64);
                        }
                    }
                } else {
                    segment_start = Some(now);
                }
                owned = currently_owned;
            }

            // 2) Flush every completed window. Deadline is fixed-phase; baton work
            //    above does not shift `window_deadline`.
            while window_deadline <= Instant::now() {
                let flush_at = window_deadline;

                if owned {
                    if let Some(s) = segment_start {
                        if flush_at > s {
                            busy_us = busy_us.saturating_add(
                                flush_at.saturating_duration_since(s).as_micros() as u64,
                            );
                        }
                        segment_start = Some(flush_at);
                    }
                }

                let interval_us = flush_at.saturating_duration_since(window_start).as_micros() as u64;
                let cur_tokens = local
                    .tokens_consumed_cumulative
                    .load(Ordering::Relaxed);
                let tokens_in_window = cur_tokens.saturating_sub(last_tokens);
                last_tokens = cur_tokens;
                let util_fp = if interval_us == 0 {
                    0
                } else if tokens_in_window == 0 {
                    0
                } else {
                    busy_us.saturating_mul(UTIL_SCALE) / interval_us
                }
                .min(UTIL_SCALE);

                {
                    let mut st = self.state.lock().unwrap();
                    st.last_util_fp = util_fp;
                    if st.history.len() == max_history {
                        st.history.pop_front();
                    }
                    st.history.push_back(util_fp);
                }

                busy_us = 0;
                window_start = flush_at;
                window_deadline = flush_at + window_dur;
            }

            // If another baton pass happened while we were working, drain without
            // sleeping so we do not miss rapid handoffs.
            if global.signal_counter.load(Ordering::Relaxed) != iteration_sig {
                continue;
            }

            // 3) Sleep until next window end or next baton event (whichever first).
            let now = Instant::now();
            let until_window = window_deadline.saturating_duration_since(now);
            let timeout_us = until_window.as_micros().min(u128::from(u64::MAX)) as u64;

            let sig = global.signal_counter.load(Ordering::Relaxed);
            futex::wait_timeout(&global.signal_counter, sig, timeout_us);
        }
    }
}
