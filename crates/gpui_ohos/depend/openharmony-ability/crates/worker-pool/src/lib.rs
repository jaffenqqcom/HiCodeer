//! A small, std-only worker thread pool with three priority levels.
//!
//! [`WorkerPool`] runs fire-and-forget closures on a fixed set of resident worker
//! threads, draining them through a shared priority queue whose pop order mirrors the
//! desktop GPUI dispatcher (`crates/gpui/src/queue.rs`): three FIFO lanes (high /
//! medium / low) selected by a weighted "loaded die" draw, so higher priority work is
//! more likely to run first while low priority work is never starved.
//!
//! The pool is built only on `std` primitives plus the `log` facade — no OHOS, gpui or
//! FFRT dependency — so it can be unit-tested on the host and reused by any OHOS
//! adapter that needs a background queue (e.g. a future `util::command` OHOS path).
//! Thread lifecycle mirrors the Linux dispatcher's background pool
//! (`crates/gpui_linux/src/linux/dispatcher.rs`): workers block on the shared queue;
//! when the pool is dropped its send side is closed, workers drain whatever is left
//! and then exit, so dropping a [`WorkerPool`] is a graceful shutdown.
//!
//! # Anti-pattern warning
//!
//! Do not submit a job that itself blocks waiting on another job submitted to the
//! *same* pool — with one worker that deadlocks, and with several it can starve the
//! pool. This pool is fire-and-forget; any "submit and await the result" scenario
//! should use a dedicated thread or a different pool.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

/// A boxed, `Send`, fire-and-forget job.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Scheduling weights for the three priority lanes. Values match gpui's
/// `Priority::weight` in `crates/scheduler/src/scheduler.rs` (High=60, Medium=30, Low=10).
const HIGH_WEIGHT: u32 = 60;
const MEDIUM_WEIGHT: u32 = 30;
const LOW_WEIGHT: u32 = 10;

/// Priority level for a job submitted to a [`WorkerPool`].
///
/// Higher priority jobs are more likely to be picked before lower priority ones, but
/// never strictly ordered — matching the weighted-random scheduling of desktop GPUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolPriority {
    High,
    Medium,
    Low,
}

/// Configuration for a [`WorkerPool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// Prefix used to name each worker thread: `"{thread_name_prefix}-{index}"`.
    pub thread_name_prefix: String,
    /// Number of resident worker threads in the pool.
    pub worker_count: usize,
}

impl PoolConfig {
    /// Prefix used when the caller does not provide one.
    pub const DEFAULT_THREAD_NAME_PREFIX: &'static str = "worker-pool";
    /// Worker count used when the hardware parallelism cannot be queried.
    pub const FALLBACK_WORKERS: usize = 2;
    /// Upper bound for the worker count chosen by [`Self::for_available_parallelism`].
    pub const MAX_WORKERS: usize = 4;
    /// Priority used by the [`WorkerPool::dispatch`] convenience method, aligning with
    /// gpui's `Priority::default()` (Medium).
    pub const DEFAULT_PRIORITY: PoolPriority = PoolPriority::Medium;

    /// Builds a config with an explicit worker count.
    ///
    /// # Panics
    /// Panics if `worker_count == 0` (a zero-worker pool would queue work forever).
    pub fn new(thread_name_prefix: impl Into<String>, worker_count: usize) -> Self {
        assert!(worker_count > 0, "WorkerPool requires at least one worker thread");
        Self {
            thread_name_prefix: thread_name_prefix.into(),
            worker_count,
        }
    }

    /// Picks a worker count from the available parallelism, clamped to
    /// `1..=MAX_WORKERS`, falling back to `FALLBACK_WORKERS` when the value cannot be
    /// queried.
    pub fn for_available_parallelism(thread_name_prefix: impl Into<String>) -> Self {
        let worker_count = thread::available_parallelism()
            .map(|it| it.get().clamp(1, Self::MAX_WORKERS))
            .unwrap_or(Self::FALLBACK_WORKERS);
        Self::new(thread_name_prefix, worker_count)
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::for_available_parallelism(Self::DEFAULT_THREAD_NAME_PREFIX)
    }
}

/// The three FIFO lanes plus their shared synchronisation primitives.
struct PriorityQueues {
    high_priority: VecDeque<Job>,
    medium_priority: VecDeque<Job>,
    low_priority: VecDeque<Job>,
}

impl PriorityQueues {
    fn is_empty(&self) -> bool {
        self.high_priority.is_empty()
            && self.medium_priority.is_empty()
            && self.low_priority.is_empty()
    }
}

/// State shared between the pool and its workers via `Arc`.
struct State {
    queues: Mutex<PriorityQueues>,
    condvar: Condvar,
    closed: AtomicBool,
}

impl State {
    fn send(&self, priority: PoolPriority, job: Job) {
        let mut queues = self.queues.lock().unwrap();
        match priority {
            PoolPriority::High => queues.high_priority.push_back(job),
            PoolPriority::Medium => queues.medium_priority.push_back(job),
            PoolPriority::Low => queues.low_priority.push_back(job),
        };
        self.condvar.notify_one();
    }

    /// Closes the send side: workers drain any remaining jobs and then exit.
    fn close(&self) {
        let _queues = self.queues.lock().unwrap();
        self.closed.store(true, Ordering::Release);
        self.condvar.notify_all();
    }
}

/// A fixed pool of resident worker threads running fire-and-forget jobs.
///
/// `WorkerPool` is deliberately not `Clone`; share one pool through an `Arc<WorkerPool>`.
/// All methods take `&self`, and the type is `Send + Sync`.
pub struct WorkerPool {
    // `None` only while the pool is being torn down; dispatch is unreachable then
    // because `drop` needs `&mut self` (or, via `Arc`, exclusive ownership).
    state: Option<Arc<State>>,
    // `JoinHandle` is not `Sync`; the `Mutex` (only touched on drop / diagnostics) is
    // what makes `WorkerPool: Sync`, so an `Arc<WorkerPool>` can be shared across threads.
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl WorkerPool {
    /// Spawns `worker_count` resident workers and returns the pool.
    ///
    /// # Panics
    /// Panics if a worker thread cannot be spawned (fail-fast, matching the Linux
    /// dispatcher which unwraps its worker spawns).
    pub fn new(config: PoolConfig) -> Self {
        let PoolConfig {
            thread_name_prefix,
            worker_count,
        } = config;
        assert!(worker_count > 0, "WorkerPool requires at least one worker thread");

        let state = Arc::new(State {
            queues: Mutex::new(PriorityQueues {
                high_priority: VecDeque::new(),
                medium_priority: VecDeque::new(),
                low_priority: VecDeque::new(),
            }),
            condvar: Condvar::new(),
            closed: AtomicBool::new(false),
        });

        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let worker_state = Arc::clone(&state);
            // A distinct fixed seed per worker keeps runs deterministic (so the tests
            // are reproducible) while avoiding identical sequences across workers. This
            // is scheduling randomness, not cryptographic randomness.
            let seed = (index as u64) + 1;
            let thread_name = format!("{thread_name_prefix}-{index}");
            let handle = match thread::Builder::new().name(thread_name).spawn(move || {
                worker_loop(worker_state, XorShift64Star::new(seed))
            }) {
                Ok(handle) => handle,
                Err(error) => {
                    // Thread creation failed (e.g. out of resources) is an abnormal
                    // path and must be logged. Fail fast afterwards: a pool with fewer
                    // workers than requested would silently degrade concurrency, so
                    // mirror the Linux dispatcher's unwrap semantics.
                    log::error!("WorkerPool failed to spawn worker {index}: {error}");
                    panic!("failed to spawn worker thread: {error}");
                }
            };
            workers.push(handle);
        }

        log::info!(
            "WorkerPool started: name_prefix=\"{thread_name_prefix}\", workers={worker_count}"
        );
        Self {
            state: Some(state),
            workers: Mutex::new(workers),
        }
    }

    /// Queues `job` at [`PoolConfig::DEFAULT_PRIORITY`] (Medium) to run on a worker.
    /// Fire-and-forget.
    pub fn dispatch(&self, job: impl FnOnce() + Send + 'static) {
        self.dispatch_with_priority(PoolConfig::DEFAULT_PRIORITY, job);
    }

    /// Queues `job` at `priority` to run on a worker. Fire-and-forget; jobs in the same
    /// lane run in FIFO order.
    pub fn dispatch_with_priority(&self, priority: PoolPriority, job: impl FnOnce() + Send + 'static) {
        match self.state.as_ref() {
            Some(state) => state.send(priority, Box::new(job)),
            None => {
                // Defensive: unreachable while the pool is alive (see field comment).
                log::error!("WorkerPool::dispatch_with_priority on a shutting-down pool; dropping job");
            }
        }
    }

    /// Number of resident worker threads (diagnostics).
    pub fn worker_count(&self) -> usize {
        self.workers.lock().unwrap().len()
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // 1. Close the send side. Workers drain jobs that were already queued, then
        //    observe the closed flag and exit their loop.
        if let Some(state) = self.state.take() {
            state.close();
        }
        // 2. Join every worker so `drop` returns only after queued/in-flight jobs
        //    finish. A worker inside a long task delays drop until it completes; this
        //    is the intended graceful-shutdown semantic (no task is abandoned).
        let workers = std::mem::take(&mut *self.workers.get_mut().unwrap());
        for handle in workers {
            let _ = handle.join();
        }
    }
}

/// Blocking worker loop: run one job at a time; a panicking job is caught so the worker
/// survives and keeps serving the queue.
fn worker_loop(state: Arc<State>, mut rng: XorShift64Star) {
    loop {
        let mut queues = state.queues.lock().unwrap();
        // Wait while the queue is empty and the send side is still open. Re-checking the
        // predicate under the lock (in a loop) handles spurious wakeups correctly.
        while queues.is_empty() && !state.closed.load(Ordering::Acquire) {
            queues = state.condvar.wait(queues).unwrap();
        }
        if queues.is_empty() {
            // Send side closed and everything queued has been drained.
            return;
        }
        let job = pop_one(&mut queues, &mut rng);
        drop(queues);

        let result = panic::catch_unwind(AssertUnwindSafe(|| job()));
        if result.is_err() {
            // A task panic must never kill the worker thread (mirrors the FFI guard in
            // openharmony-ability's timer module).
            log::error!("WorkerPool worker task panicked; worker continues");
        }
    }
}

/// Pops one job using the "loaded die from a biased coin" weighted draw
/// (https://www.keithschwarz.com/darts-dice-coins), the same algorithm gpui uses in
/// `crates/gpui/src/queue.rs`. Each non-empty lane is attempted in priority order with
/// probability `weight / remaining mass`; the low lane is the guaranteed fallback.
fn pop_one(queues: &mut PriorityQueues, rng: &mut XorShift64Star) -> Job {
    let high_mass = HIGH_WEIGHT * !queues.high_priority.is_empty() as u32;
    let medium_mass = MEDIUM_WEIGHT * !queues.medium_priority.is_empty() as u32;
    let low_mass = LOW_WEIGHT * !queues.low_priority.is_empty() as u32;
    let mut mass = high_mass + medium_mass + low_mass; // > 0: the caller holds a non-empty queue

    if !queues.high_priority.is_empty() {
        if rng.random_ratio(HIGH_WEIGHT, mass) {
            return queues.high_priority.pop_front().unwrap();
        }
        mass -= HIGH_WEIGHT;
    }
    if !queues.medium_priority.is_empty() {
        if rng.random_ratio(MEDIUM_WEIGHT, mass) {
            return queues.medium_priority.pop_front().unwrap();
        }
    }
    // Fallback: the low lane is non-empty and the remaining mass equals LOW_WEIGHT, so
    // the weighted draw would always succeed here; pop it directly.
    queues.low_priority.pop_front().unwrap()
}

/// A tiny xorshift64* PRNG with zero dependencies, used only for weighted scheduling.
struct XorShift64Star(u64);

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        // The generator requires a non-zero state.
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Returns `true` with probability `weight / mass`. Callers guarantee `mass > 0`.
    fn random_ratio(&mut self, weight: u32, mass: u32) -> bool {
        let sample = self.next_u64() % mass as u64;
        sample < weight as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// Returns a one-shot receiver that fires once `N` jobs have all completed. Each
    /// job must call `JobCompletion::done()`; the last one sends the completion signal.
    struct JobCompletion {
        remaining: AtomicUsize,
        done_tx: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl JobCompletion {
        fn new(total: usize) -> (Arc<Self>, mpsc::Receiver<()>) {
            let (done_tx, done_rx) = mpsc::channel();
            (
                Arc::new(Self {
                    remaining: AtomicUsize::new(total),
                    done_tx: Mutex::new(Some(done_tx)),
                }),
                done_rx,
            )
        }

        fn done(&self) {
            if self.remaining.fetch_sub(1, AtomicOrdering::Relaxed) == 1 {
                if let Some(tx) = self.done_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
        }
    }

    fn test_pool(workers: usize) -> WorkerPool {
        WorkerPool::new(PoolConfig::new("test-pool", workers))
    }

    #[test]
    fn dispatched_jobs_all_run() {
        let pool = test_pool(2);
        const TOTAL: usize = 100;
        let (completion, done_rx) = JobCompletion::new(TOTAL);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..TOTAL {
            let completion = Arc::clone(&completion);
            let counter = Arc::clone(&counter);
            pool.dispatch(move || {
                counter.fetch_add(1, AtomicOrdering::Relaxed);
                completion.done();
            });
        }

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("all jobs completed within timeout");
        assert_eq!(counter.load(AtomicOrdering::Relaxed), TOTAL);
    }

    #[test]
    fn single_worker_same_priority_fifo() {
        let pool = test_pool(1);
        const TOTAL: usize = 50;
        let (completion, done_rx) = JobCompletion::new(TOTAL);
        let order = Arc::new(Mutex::new(Vec::new()));

        for index in 0..TOTAL {
            let completion = Arc::clone(&completion);
            let order = Arc::clone(&order);
            pool.dispatch_with_priority(PoolPriority::Medium, move || {
                order.lock().unwrap().push(index);
                completion.done();
            });
        }

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("all jobs completed within timeout");
        // A single worker drains a single lane strictly in FIFO order.
        let order = order.lock().unwrap();
        let expected: Vec<usize> = (0..TOTAL).collect();
        assert_eq!(*order, expected);
    }

    #[test]
    fn all_three_priorities_run() {
        let pool = test_pool(4);
        const PER_LANE: usize = 20;
        let total = 3 * PER_LANE;
        let (completion, done_rx) = JobCompletion::new(total);
        let counts: Arc<[AtomicUsize; 3]> = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));

        for _ in 0..PER_LANE {
            for (lane_index, priority) in
                [PoolPriority::High, PoolPriority::Medium, PoolPriority::Low]
                    .into_iter()
                    .enumerate()
            {
                let completion = Arc::clone(&completion);
                let counts = Arc::clone(&counts);
                pool.dispatch_with_priority(priority, move || {
                    counts[lane_index].fetch_add(1, AtomicOrdering::Relaxed);
                    completion.done();
                });
            }
        }

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("all jobs completed within timeout");
        // Every lane is fully executed; nothing is dropped or permanently starved.
        for count in counts.iter() {
            assert_eq!(count.load(AtomicOrdering::Relaxed), PER_LANE);
        }
    }

    #[test]
    fn multiple_workers_run_concurrently() {
        let pool = test_pool(4);
        const TOTAL: usize = 8;
        let (completion, done_rx) = JobCompletion::new(TOTAL);
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        for _ in 0..TOTAL {
            let completion = Arc::clone(&completion);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            pool.dispatch(move || {
                let now = active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                max_active.fetch_max(now, AtomicOrdering::SeqCst);
                thread::sleep(Duration::from_millis(30));
                active.fetch_sub(1, AtomicOrdering::SeqCst);
                completion.done();
            });
        }

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("all jobs completed within timeout");
        assert!(
            max_active.load(AtomicOrdering::SeqCst) >= 2,
            "expected at least two workers to overlap"
        );
    }

    #[test]
    fn panicking_job_does_not_kill_worker() {
        let pool = test_pool(1);
        let (completion, done_rx) = JobCompletion::new(1);

        // Panic first...
        pool.dispatch(|| panic!("intentional test panic"));
        // ...then a healthy job that must still run on the same single worker.
        let completion = Arc::clone(&completion);
        pool.dispatch(move || completion.done());

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker survived the panicking job");
    }

    #[test]
    fn drop_waits_for_queued_and_running_jobs() {
        const TOTAL: usize = 20;
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let pool = test_pool(2);
            for _ in 0..TOTAL {
                let counter = Arc::clone(&counter);
                pool.dispatch(move || {
                    thread::sleep(Duration::from_millis(5));
                    counter.fetch_add(1, AtomicOrdering::Relaxed);
                });
            }
            // Pool dropped here: must drain queued + in-flight jobs before returning.
        }
        assert_eq!(counter.load(AtomicOrdering::Relaxed), TOTAL);
    }

    #[test]
    fn high_frequency_dispatch_stays_stable() {
        const TOTAL: usize = 50_000;
        let (completion, done_rx) = JobCompletion::new(TOTAL);
        let counter = Arc::new(AtomicUsize::new(0));

        let pool = test_pool(4);
        for _ in 0..TOTAL {
            let completion = Arc::clone(&completion);
            let counter = Arc::clone(&counter);
            pool.dispatch(move || {
                counter.fetch_add(1, AtomicOrdering::Relaxed);
                completion.done();
            });
        }

        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("50k jobs completed within timeout");
        assert_eq!(counter.load(AtomicOrdering::Relaxed), TOTAL);
        assert_eq!(pool.worker_count(), 4);
    }

    #[test]
    fn worker_threads_are_named_with_prefix() {
        let pool = test_pool(2);
        let names = Arc::new(Mutex::new(Vec::new()));
        let (completion, done_rx) = JobCompletion::new(6);

        for _ in 0..6 {
            let names = Arc::clone(&names);
            let completion = Arc::clone(&completion);
            pool.dispatch(move || {
                if let Some(name) = thread::current().name() {
                    names.lock().unwrap().push(name.to_owned());
                }
                completion.done();
            });
        }

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("jobs completed within timeout");
        let names = names.lock().unwrap();
        assert!(!names.is_empty(), "captured at least one worker thread name");
        for name in names.iter() {
            assert!(
                name.starts_with("test-pool-"),
                "worker thread name {name} does not carry the pool prefix"
            );
        }
    }
}
