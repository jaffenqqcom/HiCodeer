use std::sync::{Arc, LazyLock, RwLock};

use napi_ohos::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

type WakerType = LazyLock<RwLock<Option<Arc<ThreadsafeFunction<(), ()>>>>>;

pub(crate) static WAKER: WakerType = LazyLock::new(|| RwLock::new(None));

pub struct OpenHarmonyWaker {
    waker: Option<Arc<ThreadsafeFunction<(), ()>>>,
}

// Safety: ThreadsafeFunction can be called from any thread.
unsafe impl Send for OpenHarmonyWaker {}
unsafe impl Sync for OpenHarmonyWaker {}

impl OpenHarmonyWaker {
    pub fn new(waker: Option<Arc<ThreadsafeFunction<(), ()>>>) -> Self {
        Self { waker }
    }

    pub fn wake(&self) {
        // Read the TSFN live from the global WAKER on each wake, instead of using a snapshot taken at creation.
        // Reason: set_ohos_app -> create_waker runs before the ArkTS-side init -> create_lifecycle_handle
        // (which writes the global WAKER). With a snapshot, wake would always get None, UserEvent would never
        // be delivered, the foreground executor would not be driven, and the GPUI window creation task would never run (black screen).
        let guard = (*WAKER).read().expect("Failed to read WAKER");
        if let Some(waker) = guard.as_ref() {
            waker.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

impl Clone for OpenHarmonyWaker {
    fn clone(&self) -> Self {
        Self {
            waker: self.waker.clone(),
        }
    }
}
