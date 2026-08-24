use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::{
    PlatformDispatcher, Priority, PriorityQueueSender, RunnableVariant, ThreadTaskTimings,
};
use openharmony_ability::{OpenHarmonyTimer, OpenHarmonyWaker};

pub(crate) struct OhosDispatcher {
    main_thread_id: thread::ThreadId,
    main_sender: PriorityQueueSender<RunnableVariant>,
    waker: Arc<Mutex<Option<OpenHarmonyWaker>>>,
}

impl OhosDispatcher {
    pub(crate) fn new(main_sender: PriorityQueueSender<RunnableVariant>) -> Self {
        let waker: Arc<Mutex<Option<OpenHarmonyWaker>>> = Arc::new(Mutex::new(None));
        Self {
            main_thread_id: thread::current().id(),
            main_sender,
            waker,
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

    fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
        // On OHOS, run background tasks off the main thread to avoid UI stalls.
        std::thread::spawn(move || runnable.run());
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
