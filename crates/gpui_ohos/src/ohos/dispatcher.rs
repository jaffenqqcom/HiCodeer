use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::{
    PlatformDispatcher, Priority, PriorityQueueSender, RunnableVariant, ThreadTaskTimings,
};
use openharmony_ability::{OpenHarmonyTimer, OpenHarmonyWaker};
use worker_pool::{PoolConfig, PoolPriority, WorkerPool};

/// Thread-name prefix for the background pool; each worker is named `{prefix}-{index}`.
/// Kept short so the full OS thread name stays readable in hilog / thread dumps.
const BACKGROUND_THREAD_NAME_PREFIX: &str = "gpui-ohos-bg";
/// The pool never drops below one worker, so queued work always makes progress.
const MIN_BACKGROUND_WORKERS: usize = 1;
/// Cap for the background pool: the UI process stays lean; coarse parallel CPU work is
/// handled by dedicated executor threads, not this pool.
const MAX_BACKGROUND_WORKERS: usize = 4;

/// Resident worker count for the background pool: device parallelism clamped into
/// `MIN..=MAX`, falling back to `MIN` when the value cannot be queried.
fn background_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|it| it.get().clamp(MIN_BACKGROUND_WORKERS, MAX_BACKGROUND_WORKERS))
        .unwrap_or(MIN_BACKGROUND_WORKERS)
}

pub(crate) struct OhosDispatcher {
    main_thread_id: thread::ThreadId,
    main_sender: PriorityQueueSender<RunnableVariant>,
    waker: Arc<Mutex<Option<OpenHarmonyWaker>>>,
    /// Resident worker pool running off-main-thread background tasks (see `dispatch`).
    /// Held via `Arc` so the pool's shared state stays alive for the dispatcher's whole
    /// lifetime; it is drained gracefully only when the dispatcher is dropped.
    _background_pool: Arc<WorkerPool>,
}

impl OhosDispatcher {
    pub(crate) fn new(main_sender: PriorityQueueSender<RunnableVariant>) -> Self {
        let waker: Arc<Mutex<Option<OpenHarmonyWaker>>> = Arc::new(Mutex::new(None));
        // One resident pool, created once and reused for every `dispatch`, replaces the
        // previous per-task `std::thread::spawn` (mirrors the Linux dispatcher's pool).
        let background_pool = Arc::new(WorkerPool::new(PoolConfig {
            thread_name_prefix: BACKGROUND_THREAD_NAME_PREFIX.to_owned(),
            worker_count: background_worker_count(),
        }));
        Self {
            main_thread_id: thread::current().id(),
            main_sender,
            waker,
            _background_pool: background_pool,
        }
    }

    pub(crate) fn set_waker(&self, waker: OpenHarmonyWaker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    pub(crate) fn execute_runnable(runnable: RunnableVariant) {
        runnable.run();
    }
}

impl OhosDispatcher {
    // These two methods were originally trait methods of the zed 1.3 PlatformDispatcher and were removed from the trait starting in 1.17
    // (profiler data is now collected by the top-level gpui::profiler functions). The OHOS dispatcher does not take part
    // in task profiler statistics and returns empty data, but keeps the methods so upper layers can call them as needed, without cutting functionality.
    pub fn get_all_timings(&self) -> Vec<ThreadTaskTimings> {
        Vec::new()
    }

    pub fn get_current_thread_timings(&self) -> ThreadTaskTimings {
        ThreadTaskTimings {
            thread_name: None,
            thread_id: thread::current().id(),
            timings: Vec::new(),
            stats: crate::TaskStatistics::default(),
            total_pushed: 0,
        }
    }
}

impl PlatformDispatcher for OhosDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        // Background tasks run on a resident worker pool (threads created once and reused)
        // instead of spawning a fresh OS thread per task. The pool schedules its three
        // priority lanes with the same weighted-random draw as the Linux dispatcher, so
        // runnables keep their gpui priority. RealtimeAudio never reaches `dispatch` in
        // practice (the executor routes it to `spawn_realtime`); it is handled defensively
        // on a dedicated thread so it is never silently dropped.
        let pool_priority = match priority {
            Priority::High => PoolPriority::High,
            Priority::Medium => PoolPriority::Medium,
            Priority::Low => PoolPriority::Low,
            Priority::RealtimeAudio => {
                log::error!("dispatch received RealtimeAudio; running on a dedicated thread");
                thread::spawn(move || runnable.run());
                return;
            }
        };
        self._background_pool
            .dispatch_with_priority(pool_priority, move || {
                // Discard the `bool` returned by `Runnable::run` (async-task reports whether
                // the future finished); the job closure must evaluate to `()`.
                runnable.run();
            });
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        match self.main_sender.send(priority, runnable) {
            Ok(_) => {
                if let Some(waker) = self.waker.lock().unwrap().as_ref() {
                    waker.wake();
                }
            }
            Err(runnable) => {
                // NOTE: Runnable may wrap a Future that is !Send.
                //
                // This is usually safe because we only poll it on the main thread.
                // However if the send fails, we know that:
                // 1. main_receiver has been dropped (which implies the app is shutting down)
                // 2. we are on a background thread.
                // It is not safe to drop something !Send on the wrong thread, and
                // the app will exit soon anyway, so we must forget the runnable.
                std::mem::forget(runnable);
            }
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        // Schedule the runnable on an FFRT worker thread, matching the off-main-thread execution
        // semantics of the desktop platforms (Linux runs timer runnables on its timer thread).
        // The FFRT callback runs off the ArkTS/N-API main thread, so heavy timer work never
        // blocks UI rendering or input handling.
        let callback: Box<dyn FnOnce() + Send> = Box::new(move || {
            runnable.run();
        });
        match OpenHarmonyTimer::start(duration, callback) {
            Ok(_timer) => {}
            Err(callback) => {
                // FFRT is unavailable; execute the callback inline so the scheduled work is not lost.
                log::error!("dispatch_after: FFRT timer unavailable, executing callback inline");
                callback();
            }
        }
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        thread::spawn(f);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}
