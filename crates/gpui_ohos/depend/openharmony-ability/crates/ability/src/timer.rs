//! FFRT (Function Flow Runtime) one-shot timer capability.
//!
//! Wraps OHOS FFRT's `ffrt_timer_start` so Rust can schedule a callback to run once on an FFRT
//! worker thread after a timeout. The callback runs off the ArkTS/N-API main thread, so heavy work
//! here does not block the UI event loop. This is the OHOS counterpart of the timer facility other
//! zed platforms get from their OS (calloop timers on Linux, GCD `dispatch_after` on macOS).

use std::{ffi::c_void, time::Duration};

/// Default QoS for the FFRT worker that runs timer callbacks (`ffrt_qos_default`).
const FFRT_QOS_DEFAULT: i32 = 2;

// The `.z` suffix follows the OHOS shared-library naming convention (`libffrt.z.so`).
#[link(name = "ffrt.z")]
unsafe extern "C" {
    /// Starts a one-shot timer on an FFRT worker. Returns a handle (>= 0) or -1 on failure.
    fn ffrt_timer_start(
        qos: i32,
        timeout: u64,
        data: *mut c_void,
        cb: Option<unsafe extern "C" fn(*mut c_void)>,
        repeat: bool,
    ) -> i32;
    /// Stops a running timer. Returns 0 on success, -1 otherwise.
    fn ffrt_timer_stop(qos: i32, handle: i32) -> i32;
}

/// Rust side of the `data` pointer handed to FFRT.
type TimerCallback = Box<dyn FnOnce() + Send>;

/// Invoked by FFRT on a worker thread when the timer fires.
unsafe extern "C" fn ffrt_timer_callback(data: *mut c_void) {
    let callback = Box::from_raw(data as *mut TimerCallback);
    // Never unwind across the C boundary.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback();
    }));
}

/// Handle to a running one-shot FFRT timer.
///
/// The timer is fire-and-forget: it executes its callback once and is gone. [`OpenHarmonyTimer::stop`]
/// is offered for emergency cancellation; a stopped timer never runs its callback, and the boxed
/// callback is held by FFRT and cannot be reclaimed (a deliberate, rare trade-off). Callers that do
/// not cancel never leak.
#[derive(Debug)]
pub struct OpenHarmonyTimer {
    handle: i32,
}

impl OpenHarmonyTimer {
    /// Schedules `callback` to run once on an FFRT worker thread after `timeout`.
    ///
    /// Returns `Ok` with a timer handle, or `Err` with the original callback if FFRT failed to arm
    /// the timer so the caller can fall back instead of losing the scheduled work.
    pub fn start(
        timeout: Duration,
        callback: Box<dyn FnOnce() + Send>,
    ) -> Result<Self, Box<dyn FnOnce() + Send>> {
        let timeout_ms = timeout.as_millis().max(1) as u64;
        let boxed: TimerCallback = Box::new(callback);
        let data = Box::into_raw(boxed) as *mut c_void;
        let handle = unsafe {
            ffrt_timer_start(
                FFRT_QOS_DEFAULT,
                timeout_ms,
                data,
                Some(ffrt_timer_callback),
                false,
            )
        };
        if handle < 0 {
            // FFRT declined to arm the timer; hand the callback back so the caller can fall back.
            let recovered = unsafe { Box::from_raw(data as *mut TimerCallback) };
            return Err(recovered);
        }
        Ok(Self { handle })
    }

    /// Cancels the timer. See the type-level docs for the cancellation leak trade-off.
    pub fn stop(&self) {
        unsafe {
            ffrt_timer_stop(FFRT_QOS_DEFAULT, self.handle);
        }
    }
}
