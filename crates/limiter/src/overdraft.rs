//! A bounded, refillable, process-shared overdraft bucket.
//!
//! The round tag and remaining allowance share a single atomic word. A stale
//! worker therefore cannot decrement or activate a later round's allowance.
//! Only the manager starts rounds and opens their IDLE gate. An empty, open
//! bucket can be refilled by a timed-out worker, including after exhaustion in
//! the same round. Workers recheck scheduler state after a failed CAS.
//!
//! Admission is counted here, not completed kernels or device execution time.
//! The manager publishes a stable limit before opening each round's IDLE gate.
//! Callers load that limit after the pool snapshot and check that the scheduler
//! is still IDLE in that round before admission.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Largest bucket capacity; two payload values are reserved for gate states.
pub const MAX_LIMIT: u32 = u32::MAX - 2;

// Zero is the initial/terminated closed state. Exhaustion returns to DORMANT,
// which is open but empty; only a timed-out worker can refill it. Closing must
// not restore ARMED or DORMANT: neither opening nor timeout may undo a close.
const CLOSED: u32 = 0;
const ARMED: u32 = u32::MAX;
const DORMANT: u32 = u32::MAX - 1;
// Never reuse tags: the final value is a permanent closed tombstone.
const RETIRED_ROUND: u32 = u32::MAX;

const fn pack(round: u32, payload: u32) -> u64 {
    ((round as u64) << 32) | payload as u64
}

const fn round_of(word: u64) -> u32 {
    (word >> 32) as u32
}

const fn payload_of(word: u64) -> u32 {
    word as u32
}

#[repr(C)]
#[derive(Debug)]
pub struct OverdraftPool {
    word: AtomicU64,
}

impl OverdraftPool {
    /// A zero-initialized pool denies every admission until a round is opened.
    pub const fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
        }
    }

    /// Close the previous round and prepare a fresh, initially closed IDLE gate.
    ///
    /// The manager calls this before publishing the new normal batch. The
    /// returned tag belongs to this pool and need not equal the normal batch ID.
    /// When tags are exhausted, the pool stays permanently closed rather than
    /// wrapping and accepting a worker's old snapshot.
    pub fn begin_round(&self) -> Option<u32> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let round = round_of(current);
            let next_round = round.saturating_add(1);
            let next = if next_round == RETIRED_ROUND {
                pack(RETIRED_ROUND, CLOSED)
            } else {
                pack(next_round, ARMED)
            };
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return (next_round != RETIRED_ROUND).then_some(next_round),
                Err(observed) => current = observed,
            }
        }
    }

    /// Open exactly this round's IDLE gate, once, after normal measurement ends.
    ///
    /// This does not issue a token: the first timed-out worker activates and
    /// consumes the first token atomically. Exhaustion returns to the dormant
    /// gate and permits another timed-out refill. A closed round cannot be
    /// reopened; only begin_round() can prepare a fresh gate.
    pub fn open_idle(&self, round: u32) -> bool {
        if round == 0 || round == RETIRED_ROUND {
            return false;
        }
        self.word
            .compare_exchange(
                pack(round, ARMED),
                pack(round, DORMANT),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Read an opaque snapshot for one admission attempt.
    pub fn snapshot(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// Consume one token if the current round and bucket match this snapshot.
    ///
    /// A dormant pool needs a timed-out worker to activate it. Its activating
    /// worker is included in `limit`; later callers may use the remaining
    /// allowance immediately. Exhaustion returns to dormant, so a later timed-
    /// out worker may refill in the same round. Repeated refills can repeat a
    /// payload within that round; this bounds each pool fill, not total round
    /// admissions. A closed bucket never refills. The supplied limit must be
    /// loaded from shared configuration AFTER acquiring this pool snapshot;
    /// caching it before a round switch could apply an old, larger limit to the
    /// new round. Prefer try_acquire_idle() for scheduler admission. A false
    /// result does not grant permission to launch.
    pub fn try_acquire(&self, snapshot: u64, limit: u32, timed_out: bool) -> bool {
        let round = round_of(snapshot);
        if round == 0 || round == RETIRED_ROUND || limit == 0 || limit > MAX_LIMIT {
            return false;
        }
        let remaining = match payload_of(snapshot) {
            ARMED | CLOSED => return false,
            DORMANT if timed_out => limit - 1,
            DORMANT => return false,
            remaining => remaining - 1,
        };
        let remaining = if remaining == 0 { DORMANT } else { remaining };
        self.word
            .compare_exchange(
                snapshot,
                pack(round, remaining),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Admit only while the scheduler remains IDLE in the observed normal batch.
    ///
    /// The manager must close the old pool with begin_round() before publishing
    /// a new batch/RUNNING state, and publish IDLE before open_idle(). Read the
    /// pool first: an acquired new IDLE gate publishes its batch and configured
    /// limit to subsequent reads; an old pool snapshot cannot survive a round
    /// switch in the CAS. The manager must publish the limit before open_idle().
    ///
    /// The checks after the CAS deliberately do not refund a token on failure.
    /// Refunding could credit a later fill or round, while losing an admission
    /// cannot increase the bucket capacity. As with every admission check, this
    /// does not hold the scheduler state fixed after this method returns.
    pub fn try_acquire_idle(
        &self,
        state: &AtomicU32,
        batch: &AtomicU64,
        idle_state: u32,
        limit: &AtomicU32,
        timed_out: bool,
    ) -> bool {
        let snapshot = self.snapshot();
        let current_limit = limit.load(Ordering::Acquire);
        let expected_batch = batch.load(Ordering::Acquire);
        if state.load(Ordering::Acquire) != idle_state
            || batch.load(Ordering::Acquire) != expected_batch
        {
            return false;
        }
        if !self.try_acquire(snapshot, current_limit, timed_out) {
            return false;
        }
        state.load(Ordering::Acquire) == idle_state
            && batch.load(Ordering::Acquire) == expected_batch
            && round_of(self.snapshot()) == round_of(snapshot)
    }

    /// Terminate the current allowance, preserving its tag and preventing reopen.
    ///
    /// A successful admission racing with close is counted before close. After
    /// close linearizes, that admission's old snapshot can no longer succeed.
    /// The scheduler still rechecks its own state before launching a kernel.
    pub fn close(&self) {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let closed = pack(round_of(current), CLOSED);
            if current == closed {
                return;
            }
            match self.word.compare_exchange_weak(
                current,
                closed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

impl Default for OverdraftPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn take(pool: &OverdraftPool, limit: u32, timed_out: bool) -> bool {
        pool.try_acquire(pool.snapshot(), limit, timed_out)
    }

    fn idle_pool() -> (OverdraftPool, u32) {
        let pool = OverdraftPool::new();
        let round = pool.begin_round().unwrap();
        assert!(pool.open_idle(round));
        (pool, round)
    }

    #[test]
    fn all_zero_is_closed_even_after_timeout() {
        let pool = OverdraftPool::new();
        assert_eq!(pool.snapshot(), 0);
        assert!(!take(&pool, 10, true));
        assert!(!pool.open_idle(0));
        pool.close();
        assert_eq!(pool.snapshot(), 0);
    }

    #[test]
    fn beginning_a_round_does_not_allow_running_or_measuring_admission() {
        let pool = OverdraftPool::new();
        let round = pool.begin_round().unwrap();
        for _ in 0..100 {
            assert!(!take(&pool, 10, true));
        }
        assert!(pool.open_idle(round));
        assert!(take(&pool, 10, true));
    }

    #[test]
    fn first_worker_must_time_out_and_consumes_one_of_the_limit() {
        let (pool, _) = idle_pool();
        assert!(!take(&pool, 3, false));
        assert!(take(&pool, 3, true));
        assert_eq!(payload_of(pool.snapshot()), 2);
        assert!(take(&pool, 3, false));
        assert!(take(&pool, 3, false));
        assert_eq!(payload_of(pool.snapshot()), DORMANT);
        assert!(!take(&pool, 3, false));
    }

    #[test]
    fn a_one_token_pool_counts_the_triggering_worker() {
        let (pool, _) = idle_pool();
        for _ in 0..10 {
            assert!(!take(&pool, 1, false));
            assert!(take(&pool, 1, true));
            assert_eq!(payload_of(pool.snapshot()), DORMANT);
        }
        pool.close();
        assert!(!take(&pool, 1, true));
    }

    #[test]
    fn zero_and_reserved_limits_never_allow_admission() {
        let (pool, _) = idle_pool();
        let initial = pool.snapshot();
        for limit in [0, MAX_LIMIT + 1, u32::MAX] {
            assert!(!take(&pool, limit, true));
            assert_eq!(pool.snapshot(), initial);
        }
        assert!(take(&pool, 2, true));
        assert!(!take(&pool, 0, true));
        assert_eq!(payload_of(pool.snapshot()), 1);
    }

    #[test]
    fn maximum_limit_is_distinct_from_gate_sentinels() {
        let (pool, _) = idle_pool();
        assert!(take(&pool, MAX_LIMIT, true));
        assert_eq!(payload_of(pool.snapshot()), MAX_LIMIT - 1);
        assert!(take(&pool, MAX_LIMIT, false));
        assert_eq!(payload_of(pool.snapshot()), MAX_LIMIT - 2);
    }

    #[test]
    fn exhausted_pool_refills_only_after_timeout_without_reopening_the_round() {
        let (pool, round) = idle_pool();
        for _ in 0..10 {
            assert!(!take(&pool, 10, false));
            assert!(!pool.open_idle(round));
            assert!(take(&pool, 10, true));
            for _ in 1..10 {
                assert!(take(&pool, 10, false));
            }
            assert_eq!(pool.snapshot(), pack(round, DORMANT));
            assert!(!take(&pool, 10, false));
        }
    }

    #[test]
    fn changed_snapshot_rejects_cas_until_the_same_open_bucket_state_reappears() {
        let (pool, _) = idle_pool();
        let dormant = pool.snapshot();
        assert!(pool.try_acquire(dormant, 3, true));
        assert!(!pool.try_acquire(dormant, 3, true));
        let active = pool.snapshot();
        assert!(pool.try_acquire(active, 3, false));
        assert!(!pool.try_acquire(active, 3, false));
        assert_eq!(payload_of(pool.snapshot()), 1);
        assert!(take(&pool, 3, false));
        // The same open/empty state can recur in one round. Consuming a new
        // fill is valid; only close or a new round permanently invalidates it.
        assert!(pool.try_acquire(dormant, 3, true));
        assert!(pool.try_acquire(active, 3, false));
    }

    #[test]
    fn close_invalidates_snapshots_and_does_not_permit_reopen() {
        let (pool, round) = idle_pool();
        let dormant = pool.snapshot();
        pool.close();
        assert!(!pool.try_acquire(dormant, 10, true));
        assert!(!pool.open_idle(round));
        assert!(!take(&pool, 10, true));
        pool.close();
        assert_eq!(pool.snapshot(), pack(round, CLOSED));
    }

    #[test]
    fn closing_an_active_or_not_yet_opened_round_is_terminal() {
        let pool = OverdraftPool::new();
        let first = pool.begin_round().unwrap();
        pool.close();
        assert!(!pool.open_idle(first));
        let second = pool.begin_round().unwrap();
        assert!(pool.open_idle(second));
        assert!(take(&pool, 10, true));
        let active = pool.snapshot();
        pool.close();
        assert!(!pool.try_acquire(active, 10, false));
        assert!(!pool.open_idle(second));
    }

    #[test]
    fn closing_an_exhausted_pool_prevents_timeout_refills() {
        let (pool, round) = idle_pool();
        assert!(take(&pool, 2, true));
        assert!(take(&pool, 2, false));
        let exhausted = pool.snapshot();
        assert_eq!(payload_of(exhausted), DORMANT);
        pool.close();
        assert!(!pool.try_acquire(exhausted, 2, true));
        assert!(!take(&pool, 2, true));
        assert!(!pool.open_idle(round));
        assert_eq!(pool.snapshot(), pack(round, CLOSED));
    }

    #[test]
    fn new_round_discards_unused_allowance_and_rejects_old_snapshots() {
        let (pool, old_round) = idle_pool();
        let old_dormant = pool.snapshot();
        assert!(take(&pool, 10, true));
        let old_active = pool.snapshot();
        let round = pool.begin_round().unwrap();
        assert_eq!(round, old_round + 1);
        assert!(!pool.open_idle(old_round));
        assert!(!pool.try_acquire(old_dormant, 10, true));
        assert!(!pool.try_acquire(old_active, 10, false));
        assert!(!take(&pool, 10, true));
        assert!(pool.open_idle(round));
        assert!(!pool.try_acquire(old_dormant, 10, true));
        assert!(!pool.try_acquire(old_active, 10, false));
        assert!(take(&pool, 1, true));
        assert!(!take(&pool, 1, false));
        assert!(!pool.try_acquire(old_dormant, 10, true));
    }

    #[test]
    fn concurrent_workers_consume_exactly_each_refill_including_its_trigger() {
        const THREADS: usize = 32;
        const LIMIT: u32 = 212;
        let (pool, _) = idle_pool();
        let pool = Arc::new(pool);
        for _ in 0..3 {
            assert!(!take(&pool, LIMIT, false));
            // The triggering admission is part of this fill, not an extra one.
            assert!(take(&pool, LIMIT, true));
            let start = Arc::new(Barrier::new(THREADS));
            let threads: Vec<_> = (0..THREADS)
                .map(|_| {
                    let pool = Arc::clone(&pool);
                    let start = Arc::clone(&start);
                    thread::spawn(move || {
                        start.wait();
                        let mut granted = 0;
                        // Fresh snapshots retry contention without activating
                        // another fill once the shared bucket becomes empty.
                        for _ in 0..LIMIT {
                            if take(&pool, LIMIT, false) {
                                granted += 1;
                            }
                        }
                        granted
                    })
                })
                .collect();
            let granted: u32 = threads
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum();
            assert_eq!(granted + 1, LIMIT);
            assert_eq!(payload_of(pool.snapshot()), DORMANT);
            assert!(!take(&pool, LIMIT, false));
        }
    }

    #[test]
    fn concurrent_first_workers_only_activate_the_pool_once() {
        const THREADS: usize = 16;
        let (pool, _) = idle_pool();
        let dormant = pool.snapshot();
        let pool = Arc::new(pool);
        let start = Arc::new(Barrier::new(THREADS));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let pool = Arc::clone(&pool);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    pool.try_acquire(dormant, 10, true) as usize
                })
            })
            .collect();
        let granted: usize = threads
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum();
        assert_eq!(granted, 1);
        assert_eq!(payload_of(pool.snapshot()), 9);
    }

    #[test]
    fn worker_paused_across_round_switch_cannot_touch_the_new_pool() {
        let (pool, old_round) = idle_pool();
        let pool = Arc::new(pool);
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let pool = Arc::clone(&pool);
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            thread::spawn(move || {
                let old = pool.snapshot();
                ready.wait();
                resume.wait();
                pool.try_acquire(old, 10, true)
            })
        };
        ready.wait();
        let new_round = pool.begin_round().unwrap();
        assert_ne!(new_round, old_round);
        assert!(pool.open_idle(new_round));
        let new_snapshot = pool.snapshot();
        resume.wait();
        assert!(!worker.join().unwrap());
        assert_eq!(pool.snapshot(), new_snapshot);
    }

    #[test]
    fn close_wins_against_a_worker_paused_with_an_active_snapshot() {
        let (pool, _) = idle_pool();
        assert!(take(&pool, 10, true));
        let pool = Arc::new(pool);
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let pool = Arc::clone(&pool);
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            thread::spawn(move || {
                let active = pool.snapshot();
                ready.wait();
                resume.wait();
                pool.try_acquire(active, 10, false)
            })
        };
        ready.wait();
        pool.close();
        resume.wait();
        assert!(!worker.join().unwrap());
        assert_eq!(payload_of(pool.snapshot()), 0);
    }

    #[test]
    fn tag_exhaustion_permanently_closes_instead_of_wrapping() {
        let pool = OverdraftPool {
            word: AtomicU64::new(pack(RETIRED_ROUND - 2, CLOSED)),
        };
        let last_round = pool.begin_round().unwrap();
        assert_eq!(last_round, RETIRED_ROUND - 1);
        assert!(pool.open_idle(last_round));
        let last_snapshot = pool.snapshot();
        assert_eq!(pool.begin_round(), None);
        for _ in 0..10 {
            assert_eq!(pool.begin_round(), None);
            assert!(!pool.open_idle(last_round));
            assert!(!pool.open_idle(RETIRED_ROUND));
            assert!(!pool.try_acquire(last_snapshot, 10, true));
            assert!(!take(&pool, 10, true));
            pool.close();
            assert_eq!(pool.snapshot(), pack(RETIRED_ROUND, CLOSED));
        }
    }

    #[test]
    fn shared_layout_is_one_u64_atomic_and_default_is_zero() {
        assert_eq!(
            std::mem::size_of::<OverdraftPool>(),
            std::mem::size_of::<AtomicU64>()
        );
        assert_eq!(
            std::mem::align_of::<OverdraftPool>(),
            std::mem::align_of::<AtomicU64>()
        );
        assert_eq!(OverdraftPool::default().snapshot(), 0);
    }

    #[test]
    fn scheduler_running_and_measuring_states_cannot_consume_idle_allowance() {
        const IDLE: u32 = 0;
        const RUNNING: u32 = 1;
        const MEASURING: u32 = 2;
        let (pool, _) = idle_pool();
        let state = AtomicU32::new(RUNNING);
        let batch = AtomicU64::new(7);
        let limit = AtomicU32::new(10);
        let dormant = pool.snapshot();
        for scheduler_state in [RUNNING, MEASURING] {
            state.store(scheduler_state, Ordering::Release);
            assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
            assert_eq!(pool.snapshot(), dormant);
        }
        state.store(IDLE, Ordering::Release);
        assert!(pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        let active = pool.snapshot();
        state.store(MEASURING, Ordering::Release);
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, false));
        assert_eq!(pool.snapshot(), active);
    }

    #[test]
    fn idle_scheduler_still_respects_timeout_zero_limit_exhaustion_and_close() {
        const IDLE: u32 = 4;
        let (pool, _) = idle_pool();
        let state = AtomicU32::new(IDLE);
        let batch = AtomicU64::new(8);
        let limit = AtomicU32::new(2);
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, false));
        limit.store(0, Ordering::Release);
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        limit.store(2, Ordering::Release);
        assert!(pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        assert!(pool.try_acquire_idle(&state, &batch, IDLE, &limit, false));
        for _ in 0..100 {
            assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, false));
        }
        assert!(pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        assert!(pool.try_acquire_idle(&state, &batch, IDLE, &limit, false));
        let round = pool.begin_round().unwrap();
        assert!(pool.open_idle(round));
        pool.close();
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        let never_enabled = OverdraftPool::new();
        assert!(!never_enabled.try_acquire_idle(&state, &batch, IDLE, &limit, true));
    }

    #[test]
    fn a_futex_wakeup_into_running_or_measuring_does_not_grant_overdraft() {
        const IDLE: u32 = 0;
        const RUNNING: u32 = 1;
        const MEASURING: u32 = 2;
        let (pool, _) = idle_pool();
        let state = AtomicU32::new(IDLE);
        let batch = AtomicU64::new(9);
        let limit = AtomicU32::new(10);
        // A worker went to sleep after seeing IDLE. A wakeup only tells it to
        // recheck: it is not permission to return from token admission.
        assert_eq!(state.load(Ordering::Acquire), IDLE);
        for after_wakeup in [RUNNING, MEASURING] {
            state.store(after_wakeup, Ordering::Release);
            assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        }
        assert_eq!(payload_of(pool.snapshot()), DORMANT);
    }

    #[test]
    fn changed_batch_before_running_publication_does_not_bind_to_a_fresh_pool() {
        const IDLE: u32 = 0;
        const RUNNING: u32 = 1;
        let (pool, old_round) = idle_pool();
        let state = AtomicU32::new(IDLE);
        let batch = AtomicU64::new(10);
        let limit = AtomicU32::new(10);
        let new_round = pool.begin_round().unwrap();
        assert_ne!(new_round, old_round);
        batch.store(11, Ordering::Release);
        // Manager has closed the old pool and advanced batch, but has not yet
        // published RUNNING. The old IDLE state cannot activate this new pool.
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        state.store(RUNNING, Ordering::Release);
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        // Only a complete normal round and a newly opened IDLE gate allow it.
        state.store(IDLE, Ordering::Release);
        assert!(!pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
        assert!(pool.open_idle(new_round));
        assert!(pool.try_acquire_idle(&state, &batch, IDLE, &limit, true));
    }

    #[test]
    fn paused_worker_uses_the_new_rounds_smaller_shared_limit() {
        const IDLE: u32 = 0;
        const RUNNING: u32 = 1;
        let (pool, _) = idle_pool();
        let pool = Arc::new(pool);
        let state = Arc::new(AtomicU32::new(IDLE));
        let batch = Arc::new(AtomicU64::new(20));
        let limit = Arc::new(AtomicU32::new(10));
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let pool = Arc::clone(&pool);
            let state = Arc::clone(&state);
            let batch = Arc::clone(&batch);
            let limit = Arc::clone(&limit);
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            thread::spawn(move || {
                let cached_old_limit = limit.load(Ordering::Acquire);
                ready.wait();
                resume.wait();
                let mut admitted = 0;
                for _ in 0..cached_old_limit {
                    // Admission accepts shared configuration, never the cached
                    // value that belonged to the old manager/round.
                    if pool.try_acquire_idle(&state, &batch, IDLE, &limit, admitted == 0) {
                        admitted += 1;
                    }
                }
                (cached_old_limit, admitted)
            })
        };
        ready.wait();
        let new_round = pool.begin_round().unwrap();
        batch.store(21, Ordering::Release);
        state.store(RUNNING, Ordering::Release);
        limit.store(2, Ordering::Release);
        state.store(IDLE, Ordering::Release);
        assert!(pool.open_idle(new_round));
        resume.wait();
        assert_eq!(worker.join().unwrap(), (10, 2));
        assert_eq!(payload_of(pool.snapshot()), DORMANT);
    }
}
