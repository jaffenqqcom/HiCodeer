use log::{debug, warn};

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::channel::oneshot;
use openharmony_ability::{
    AvoidAreaType, AxisEventData, AxisToolType, DeviceModifiers, Event, ImeEvent, InputEvent,
    MouseAction, MouseEventData, MouseButton as DeviceMouseButton, OpenHarmonyApp, ScrollPhase,
    xcomponent::{Action, KeyCode, KeyEventData, TouchEvent, TouchEventData},
};
use openharmony_ability_plugin_ime::ImeExt;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use super::display::OhosDisplay;
use super::wgpu_context::WgpuContext;
use super::wgpu_renderer::{WgpuRenderer, WgpuSurfaceConfig};
use crate::{
    Axis, BackgroundExecutor, Bounds, Capslock, DevicePixels, ExternalPaths, FileDropEvent,
    ForegroundExecutor, GpuSpecs, KeyDownEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow,
    Point, PromptButton, PromptLevel, RequestFrameOptions, ResizeEdge, Scene, ScrollDelta,
    ScrollWheelEvent, Size, TouchPhase, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, WindowControls, WindowDecorations, WindowInsets, WindowParams,
    point, px, size,
};

/// Set by the GPUI frame waker when a frame is wanted (wake_platform), and
/// consumed by WindowRedraw handling. If no follow-up request arrives after a
/// frame is consumed, the XComponent frame callback is disarmed so idle
/// windows stop generating redraw events entirely.
static PENDING_REDRAW: AtomicBool = AtomicBool::new(false);

pub(crate) struct OhosWindow {
    handle: crate::AnyWindowHandle,
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    bounds: RefCell<Bounds<Pixels>>,
    scale: RefCell<f32>,
    keyboard_overlap_device_px: Cell<i32>,
    safe_area_avoidance_enabled: Cell<bool>,
    last_emitted_resize: RefCell<Option<ResizeCallbackState>>,
    input_handler: Rc<RefCell<Option<PlatformInputHandler>>>,
    callbacks: Rc<RefCell<WindowCallbacks>>,
    pending_frame_request: Cell<Option<bool>>,
    renderer: RefCell<Option<WgpuRenderer>>,
    gpu_context: Rc<RefCell<Option<Arc<WgpuContext>>>>,
    foreground_executor: ForegroundExecutor,
    background_executor: BackgroundExecutor,
    key_repeat: Rc<RefCell<Option<KeyRepeatState>>>,
    window_alive: Rc<Cell<bool>>,
    pinch_accumulator: Rc<Cell<f32>>,
    keyboard_visible: Rc<Cell<bool>>,
    /// Whether the ArkTS IME session is actually bound. Tracked separately from
    /// `keyboard_visible` (which only drives layout) so a failed attach — the
    /// edit box had not taken focus yet — can be retried instead of being
    /// permanently suppressed.
    ime_attached: Rc<Cell<bool>>,
    /// Guards against overlapping attach attempts. ArkTS retries internally for
    /// about a second, so a concurrent second request adds nothing but load.
    ime_attach_in_flight: Rc<Cell<bool>>,
    last_ime_cursor_rect: RefCell<Option<Bounds<Pixels>>>,
    pending_touch_scroll: RefCell<Option<PendingTouchScroll>>,
    last_dispatched_touch_position: RefCell<Option<Point<Pixels>>>,
    touch_state: Cell<TouchState>,
    touch_down_timestamp: Cell<Option<Duration>>,
    last_touch_timestamp: Cell<Option<Duration>>,
    touch_hit_boundary: Cell<bool>,
    touch_velocity_tracker: RefCell<TouchVelocityTracker>,
    scroll_animation: RefCell<Option<ScrollAnimation>>,
    scroll_frame_rate_boosted: Cell<bool>,
    pending_touch_click_feedback_cancel: Cell<Option<Modifiers>>,
    click_tracker: RefCell<ClickTracker>,
}

impl Drop for OhosWindow {
    fn drop(&mut self) {
        // Invalidate any in-flight key-repeat task captured this flag.
        self.window_alive.set(false);
        // Release the pinch/drop event callbacks that reference this window.
        openharmony_ability_plugin_pinch::clear_pinch_callback();
        openharmony_ability_plugin_filedrop::clear_filedrop_callback();
    }
}

pub(crate) struct OhosWindowHandle {
    inner: Rc<RefCell<OhosWindow>>,
    input_handler: Rc<RefCell<Option<PlatformInputHandler>>>,
}

impl OhosWindowHandle {
    pub(crate) fn new(inner: Rc<RefCell<OhosWindow>>) -> Self {
        let input_handler = inner.borrow().input_handler.clone();
        Self {
            inner,
            input_handler,
        }
    }

    fn with_window<R>(&self, f: impl FnOnce(&OhosWindow) -> R) -> R {
        let window = self.inner.borrow();
        f(&window)
    }

    /// Mutate the window state, returning `None` when the window is already
    /// borrowed. A re-entrant call (e.g. a Zed UI callback triggered during
    /// input dispatch that calls back into this platform window) would
    /// otherwise panic on the `RefCell` double borrow; skipping the mutation
    /// degrades gracefully instead.
    fn with_window_mut<R>(&self, f: impl FnOnce(&mut OhosWindow) -> R) -> Option<R> {
        let Ok(mut window) = self.inner.try_borrow_mut() else {
            log::warn!(
                "with_window_mut: OhosWindow already borrowed; skipping re-entrant mutation"
            );
            return None;
        };
        Some(f(&mut window))
    }
}

struct WindowCallbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    virtual_keyboard_hidden_by_user: Option<Box<dyn FnMut()>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
    hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
}

#[derive(Clone, Copy)]
struct ResizeCallbackState {
    content_size: Size<Pixels>,
    scale: f32,
}

#[derive(Clone, Copy)]
struct PendingTouchScroll {
    position: Point<Pixels>,
    modifiers: Modifiers,
    phase: TouchPhase,
}

#[derive(Clone, Copy, Default)]
enum TouchState {
    #[default]
    Idle,
    Pending(TouchPendingState),
    Scrolling(TouchScrollState),
}

#[derive(Clone, Copy)]
struct TouchPendingState {
    start_position: Point<Pixels>,
    last_position: Point<Pixels>,
    cancel_click: bool,
    mouse_down_sent: bool,
}

#[derive(Clone, Copy)]
struct TouchScrollState {
    last_position: Point<Pixels>,
    locked_axis: Axis,
}

#[derive(Clone, Copy)]
struct ScrollAnimation {
    position: Point<Pixels>,
    modifiers: Modifiers,
    initial_velocity: Point<f32>,
    gamma: f32,
    elapsed: f32,
    last_distance: Point<Pixels>,
    last_frame_timestamp: Option<Duration>,
}

// Double-click detection thresholds, matching gpui_linux's constants
// (DOUBLE_CLICK_INTERVAL / DOUBLE_CLICK_DISTANCE).
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_DISTANCE: Pixels = px(5.0);

/// Tracks consecutive click counts across the mouse, touchpad and touchscreen
/// input paths, mirroring the macOS `clickCount` semantics. The baseline for a
/// double/triple-click is the most recent *completed* click (a mouse release or
/// a confirmed touchscreen tap), so a touchscreen down that later becomes a
/// scroll never pollutes the baseline.
#[derive(Default)]
struct ClickTracker {
    last_click_time: Option<Instant>,
    last_click_position: Option<Point<Pixels>>,
    last_click_button: Option<MouseButton>,
    current_count: usize,
}

impl ClickTracker {
    /// Computes the click count for a button press and records it as the count
    /// for the matching release. Does not update the double-click baseline.
    fn on_button_press(&mut self, button: MouseButton, position: Point<Pixels>) -> usize {
        let is_repeat = self
            .last_click_time
            .is_some_and(|time| time.elapsed() < DOUBLE_CLICK_INTERVAL)
            && self.last_click_button == Some(button)
            && self
                .last_click_position
                .is_some_and(|last| Self::is_within_click_distance(last, position));
        self.current_count = if is_repeat {
            self.current_count.saturating_add(1)
        } else {
            1
        };
        self.current_count
    }

    /// Current click count for the in-flight press, used by the matching release.
    fn current_count(&self) -> usize {
        self.current_count
    }

    /// Records a completed click as the new double/triple-click baseline.
    fn on_click_complete(&mut self, button: MouseButton, position: Point<Pixels>) {
        self.last_click_time = Some(Instant::now());
        self.last_click_position = Some(position);
        self.last_click_button = Some(button);
    }

    fn is_within_click_distance(left: Point<Pixels>, right: Point<Pixels>) -> bool {
        let diff = left - right;
        diff.x.abs() <= DOUBLE_CLICK_DISTANCE && diff.y.abs() <= DOUBLE_CLICK_DISTANCE
    }
}

// Key auto-repeat timing. OHOS key events carry no repeat action (KeyAction has only
// Down/Up), so repeats are synthesized here. Values approximate common desktop defaults
// (Wayland RepeatInfo / X11 autorepeat): 500 ms initial delay, then ~10 cps.
const KEY_REPEAT_DELAY: Duration = Duration::from_millis(500);
const KEY_REPEAT_INTERVAL: Duration = Duration::from_millis(100);

/// A pinch must accumulate at least this much scale change before one zoom step
/// is emitted, keeping the pinch zoom rate gentle (0.15 == 15% scale change).
const PINCH_ZOOM_THRESHOLD: f32 = 0.15;

/// Tracks an in-flight synthesized key-repeat. `generation` increments on every
/// key-down and when the key is released, invalidating any stale repeat task.
#[derive(Clone, Copy)]
struct KeyRepeatState {
    code: KeyCode,
    generation: u64,
}

#[derive(Clone, Copy)]
struct TouchSample {
    position: Point<Pixels>,
    timestamp: Duration,
}

#[derive(Default)]
struct TouchVelocityTracker {
    samples: VecDeque<TouchSample>,
}

impl TouchVelocityTracker {
    fn reset(&mut self) {
        self.samples.clear();
    }

    fn push(
        &mut self,
        position: Point<Pixels>,
        timestamp: Option<Duration>,
        max_sample_count: usize,
    ) {
        let Some(timestamp) = timestamp else {
            return;
        };

        if self
            .samples
            .back()
            .is_some_and(|sample| timestamp < sample.timestamp)
        {
            self.samples.clear();
        }

        self.samples.push_back(TouchSample {
            position,
            timestamp,
        });

        while self.samples.len() > max_sample_count {
            self.samples.pop_front();
        }
    }

    fn velocity(&self, locked_axis: Option<Axis>, sample_window: Duration) -> Point<f32> {
        let Some(last_sample) = self.samples.back() else {
            return point(0.0, 0.0);
        };
        let cutoff = last_sample.timestamp.saturating_sub(sample_window);
        let samples = self
            .samples
            .iter()
            .copied()
            .filter(|sample| sample.timestamp >= cutoff)
            .collect::<Vec<_>>();

        let x = if matches!(locked_axis, Some(Axis::Vertical)) {
            0.0
        } else {
            Self::axis_velocity(&samples, Axis::Horizontal).unwrap_or(0.0) as f32
        };
        let y = if matches!(locked_axis, Some(Axis::Horizontal)) {
            0.0
        } else {
            Self::axis_velocity(&samples, Axis::Vertical).unwrap_or(0.0) as f32
        };

        point(x, y)
    }

    fn axis_velocity(samples: &[TouchSample], axis: Axis) -> Option<f64> {
        if samples.len() < 2 {
            return None;
        }

        Self::quadratic_axis_velocity(samples, axis)
            .or_else(|| Self::linear_axis_velocity(samples, axis))
    }

    fn linear_axis_velocity(samples: &[TouchSample], axis: Axis) -> Option<f64> {
        let first = samples.first()?;
        let last = samples.last()?;
        let elapsed = last.timestamp.checked_sub(first.timestamp)?.as_secs_f64();
        if elapsed <= f64::EPSILON {
            return None;
        }

        Some((Self::axis_position(last, axis) - Self::axis_position(first, axis)) / elapsed)
    }

    fn quadratic_axis_velocity(samples: &[TouchSample], axis: Axis) -> Option<f64> {
        if samples.len() < 3 {
            return None;
        }

        let first_timestamp = samples.first()?.timestamp;
        let mut sum_t = 0.0;
        let mut sum_t2 = 0.0;
        let mut sum_t3 = 0.0;
        let mut sum_t4 = 0.0;
        let mut sum_position = 0.0;
        let mut sum_t_position = 0.0;
        let mut sum_t2_position = 0.0;
        let mut positions = Vec::with_capacity(samples.len());

        for sample in samples {
            let t = sample.timestamp.checked_sub(first_timestamp)?.as_secs_f64();
            let t2 = t * t;
            let position = Self::axis_position(sample, axis);

            sum_t += t;
            sum_t2 += t2;
            sum_t3 += t2 * t;
            sum_t4 += t2 * t2;
            sum_position += position;
            sum_t_position += t * position;
            sum_t2_position += t2 * position;
            positions.push(position);
        }

        let last_t = samples
            .last()?
            .timestamp
            .checked_sub(first_timestamp)?
            .as_secs_f64();
        if last_t <= f64::EPSILON {
            return None;
        }

        let [a, b, _c] = Self::solve_3x3([
            [sum_t4, sum_t3, sum_t2, sum_t2_position],
            [sum_t3, sum_t2, sum_t, sum_t_position],
            [sum_t2, sum_t, samples.len() as f64, sum_position],
        ])?;

        let velocity = 2.0 * a * last_t + b;
        if let Some(increasing) = Self::monotonic_direction(&positions)
            && ((increasing && velocity < 0.0) || (!increasing && velocity > 0.0))
        {
            return None;
        }

        Some(velocity)
    }

    fn axis_position(sample: &TouchSample, axis: Axis) -> f64 {
        match axis {
            Axis::Horizontal => sample.position.x.as_f32() as f64,
            Axis::Vertical => sample.position.y.as_f32() as f64,
        }
    }

    fn monotonic_direction(values: &[f64]) -> Option<bool> {
        let mut direction = None;
        for pair in values.windows(2) {
            let delta = pair[1] - pair[0];
            if delta.abs() <= f64::EPSILON {
                continue;
            }

            let increasing = delta > 0.0;
            if let Some(direction) = direction {
                if direction != increasing {
                    return None;
                }
            } else {
                direction = Some(increasing);
            }
        }
        direction
    }

    fn solve_3x3(mut matrix: [[f64; 4]; 3]) -> Option<[f64; 3]> {
        for pivot in 0..3 {
            let mut pivot_row = pivot;
            for row in pivot + 1..3 {
                if matrix[row][pivot].abs() > matrix[pivot_row][pivot].abs() {
                    pivot_row = row;
                }
            }

            let pivot_value = matrix[pivot_row][pivot];
            if pivot_value.abs() <= f64::EPSILON {
                return None;
            }

            if pivot_row != pivot {
                matrix.swap(pivot, pivot_row);
            }

            for col in pivot..4 {
                matrix[pivot][col] /= pivot_value;
            }

            for row in 0..3 {
                if row == pivot {
                    continue;
                }

                let factor = matrix[row][pivot];
                for col in pivot..4 {
                    matrix[row][col] -= factor * matrix[pivot][col];
                }
            }
        }

        Some([matrix[0][3], matrix[1][3], matrix[2][3]])
    }
}

impl OhosWindow {
    const TOUCH_SLOP: f32 = 5.0;
    const TAP_MAX_DURATION: Duration = Duration::from_millis(220);
    const MIN_MOMENTUM_VELOCITY: f32 = 240.0;
    const MAX_MOMENTUM_VELOCITY: f32 = 9_000.0;
    const FLING_VELOCITY_SCALE: f32 = 1.5;
    const FLING_FRICTION: f32 = 0.75;
    const SLOW_FLING_FRICTION: f32 = 1.0;
    const SLOW_FLING_THRESHOLD: f32 = 3_000.0;
    const FRICTION_SCALE: f32 = 4.2;
    const TOUCH_VELOCITY_WINDOW: Duration = Duration::from_millis(100);
    const MAX_TOUCH_SAMPLE_COUNT: usize = 16;
    const DEFAULT_FRAME_INTERVAL_SECONDS: f32 = 1.0 / 120.0;
    const MIN_FRAME_INTERVAL_SECONDS: f32 = 1.0 / 240.0;
    const MAX_FRAME_INTERVAL_SECONDS: f32 = 0.05;
    const MIN_SCROLL_DELTA: Pixels = px(0.1);
    const MAX_MOMENTUM_GAP: Duration = Duration::from_millis(100);

    pub(crate) fn new(
        app: Rc<RefCell<Option<OpenHarmonyApp>>>,
        handle: crate::AnyWindowHandle,
        params: WindowParams,
        gpu_context: Rc<RefCell<Option<Arc<WgpuContext>>>>,
        foreground_executor: ForegroundExecutor,
        background_executor: BackgroundExecutor,
    ) -> Result<Self> {
        log::info!("[boot] OhosWindow::new entered, handle {:?}", handle);
        let scale = app
            .borrow()
            .as_ref()
            .map(|a| a.scale() as f32)
            .unwrap_or(1.0);
        let bounds = Bounds::new(point(px(0.0), px(0.0)), params.bounds.size);
        // Don't create renderer immediately - native_window may not be available yet.
        // Renderer will be initialized lazily in draw() or when SurfaceCreate event is received.
        // At that point, native_window from OpenHarmonyApp will be available.

        Ok(Self {
            handle,
            app: app.clone(),
            bounds: RefCell::new(bounds),
            scale: RefCell::new(scale),
            keyboard_overlap_device_px: Cell::new(0),
            safe_area_avoidance_enabled: Cell::new(true),
            last_emitted_resize: RefCell::new(None),
            input_handler: Rc::new(RefCell::new(None)),
            callbacks: Rc::new(RefCell::new(WindowCallbacks {
                request_frame: None,
                input: None,
                active_status_change: None,
                virtual_keyboard_hidden_by_user: None,
                hover_status_change: None,
                resize: None,
                moved: None,
                should_close: None,
                close: None,
                appearance_changed: None,
                hit_test_window_control: None,
            })),
            pending_frame_request: Cell::new(None),
            renderer: RefCell::new(None),
            gpu_context,
            foreground_executor,
            background_executor,
            key_repeat: Rc::new(RefCell::new(None)),
            window_alive: Rc::new(Cell::new(true)),
            pinch_accumulator: Rc::new(Cell::new(0.0)),
            keyboard_visible: Rc::new(Cell::new(false)),
            ime_attached: Rc::new(Cell::new(false)),
            ime_attach_in_flight: Rc::new(Cell::new(false)),
            last_ime_cursor_rect: RefCell::new(None),
            pending_touch_scroll: RefCell::new(None),
            last_dispatched_touch_position: RefCell::new(None),
            touch_state: Cell::new(TouchState::Idle),
            touch_down_timestamp: Cell::new(None),
            last_touch_timestamp: Cell::new(None),
            touch_hit_boundary: Cell::new(false),
            touch_velocity_tracker: RefCell::new(TouchVelocityTracker::default()),
            scroll_animation: RefCell::new(None),
            scroll_frame_rate_boosted: Cell::new(false),
            pending_touch_click_feedback_cancel: Cell::new(None),
            click_tracker: RefCell::new(ClickTracker::default()),
        })
    }

    pub(crate) fn handle(&self) -> crate::AnyWindowHandle {
        self.handle
    }

    fn reset_touch_state(&self) {
        *self.pending_touch_scroll.borrow_mut() = None;
        *self.last_dispatched_touch_position.borrow_mut() = None;
        self.touch_state.set(TouchState::Idle);
        self.touch_down_timestamp.set(None);
        self.last_touch_timestamp.set(None);
        self.touch_hit_boundary.set(false);
    }

    fn size_matches(left: Size<Pixels>, right: Size<Pixels>) -> bool {
        const EPSILON: f32 = 0.01;

        (left.width.as_f32() - right.width.as_f32()).abs() <= EPSILON
            && (left.height.as_f32() - right.height.as_f32()).abs() <= EPSILON
    }

    fn resize_state_matches(left: ResizeCallbackState, right: ResizeCallbackState) -> bool {
        const EPSILON: f32 = 0.01;

        Self::size_matches(left.content_size, right.content_size)
            && (left.scale - right.scale).abs() <= EPSILON
    }

    fn set_bounds_size(&self, new_size: Size<Pixels>) -> bool {
        if Self::size_matches(self.bounds.borrow().size, new_size) {
            return false;
        }

        *self.bounds.borrow_mut() = Bounds::new(point(px(0.0), px(0.0)), new_size);
        true
    }

    fn cancel_momentum(&self) {
        *self.scroll_animation.borrow_mut() = None;
        self.set_scroll_frame_rate_boost(false);
    }

    fn reset_touch_velocity(&self) {
        self.touch_velocity_tracker.borrow_mut().reset();
    }

    fn begin_touch_tracking(
        &self,
        position: Point<Pixels>,
        timestamp: Option<Duration>,
        synthesize_mouse_down: bool,
    ) -> bool {
        let canceled_momentum = self.scroll_animation.borrow().is_some();
        self.cancel_momentum();
        let mouse_down_sent = synthesize_mouse_down && !canceled_momentum;
        *self.pending_touch_scroll.borrow_mut() = None;
        *self.last_dispatched_touch_position.borrow_mut() = Some(position);
        self.touch_state.set(TouchState::Pending(TouchPendingState {
            start_position: position,
            last_position: position,
            cancel_click: canceled_momentum,
            mouse_down_sent,
        }));
        self.touch_down_timestamp.set(timestamp);
        self.last_touch_timestamp.set(timestamp);
        self.touch_hit_boundary.set(false);
        self.reset_touch_velocity();
        self.record_touch_position(position, timestamp);
        mouse_down_sent
    }

    fn movement_exceeds_touch_slop(distance_squared: f32) -> bool {
        distance_squared > Self::TOUCH_SLOP * Self::TOUCH_SLOP
    }

    fn velocity_magnitude(velocity: Point<f32>) -> f32 {
        velocity.x.hypot(velocity.y)
    }

    fn clamp_velocity(velocity: Point<f32>) -> Point<f32> {
        point(
            velocity
                .x
                .clamp(-Self::MAX_MOMENTUM_VELOCITY, Self::MAX_MOMENTUM_VELOCITY),
            velocity
                .y
                .clamp(-Self::MAX_MOMENTUM_VELOCITY, Self::MAX_MOMENTUM_VELOCITY),
        )
    }

    fn touch_timestamp(timestamp: i64) -> Option<Duration> {
        u64::try_from(timestamp).ok().map(Duration::from_nanos)
    }

    fn touch_position(&self, touch_event: &TouchEventData) -> Point<Pixels> {
        let scale = *self.scale.borrow();
        point(px(touch_event.x / scale), px(touch_event.y / scale))
    }

    fn touch_gap_exceeded(&self, now: Option<Duration>) -> bool {
        let (Some(previous_sample_time), Some(now)) = (self.last_touch_timestamp.get(), now) else {
            return false;
        };

        now.saturating_sub(previous_sample_time) > Self::MAX_MOMENTUM_GAP
    }

    fn tap_duration_exceeded(&self, now: Option<Duration>) -> bool {
        let (Some(touch_down_time), Some(now)) = (self.touch_down_timestamp.get(), now) else {
            return true;
        };

        now.saturating_sub(touch_down_time) > Self::TAP_MAX_DURATION
    }

    fn record_touch_position(&self, position: Point<Pixels>, timestamp: Option<Duration>) {
        if let Some(timestamp) = timestamp {
            self.last_touch_timestamp.set(Some(timestamp));
        }
        self.touch_velocity_tracker.borrow_mut().push(
            position,
            timestamp,
            Self::MAX_TOUCH_SAMPLE_COUNT,
        );
    }

    fn tracked_touch_velocity(&self) -> Point<f32> {
        self.touch_velocity_tracker
            .borrow()
            .velocity(self.touch_locked_axis(), Self::TOUCH_VELOCITY_WINDOW)
    }

    fn touch_locked_axis(&self) -> Option<Axis> {
        match self.touch_state.get() {
            TouchState::Scrolling(state) => Some(state.locked_axis),
            TouchState::Idle | TouchState::Pending(..) => None,
        }
    }

    fn touch_is_active(&self) -> bool {
        !matches!(self.touch_state.get(), TouchState::Idle)
    }

    fn touch_is_scrolling(&self) -> bool {
        matches!(self.touch_state.get(), TouchState::Scrolling(..))
    }

    fn touch_axis(delta_from_start: Point<Pixels>) -> Axis {
        if delta_from_start.x.abs() > delta_from_start.y.abs() {
            Axis::Horizontal
        } else {
            Axis::Vertical
        }
    }

    fn filter_touch_delta(&self, delta: Point<Pixels>) -> Point<Pixels> {
        Self::filter_touch_delta_with_axis(delta, self.touch_locked_axis())
    }

    fn filter_touch_delta_with_axis(
        delta: Point<Pixels>,
        locked_axis: Option<Axis>,
    ) -> Point<Pixels> {
        match locked_axis {
            Some(Axis::Vertical) => point(px(0.0), delta.y),
            Some(Axis::Horizontal) => point(delta.x, px(0.0)),
            None => delta,
        }
    }

    fn dispatch_input_with_callbacks(
        callbacks: &Rc<RefCell<WindowCallbacks>>,
        input: PlatformInput,
    ) -> crate::DispatchEventResult {
        let mut callback = callbacks.borrow_mut().input.take();
        let mut result = crate::DispatchEventResult::default();
        if let Some(ref mut cb) = callback {
            result = cb(input);
        }
        callbacks.borrow_mut().input = callback;
        result
    }

    fn queue_pending_touch_scroll(
        &self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        phase: TouchPhase,
    ) {
        let mut pending = self.pending_touch_scroll.borrow_mut();
        if let Some(existing) = pending.as_mut() {
            existing.position = position;
            existing.modifiers = modifiers;
            if matches!(existing.phase, TouchPhase::Moved) && matches!(phase, TouchPhase::Started) {
                existing.phase = TouchPhase::Started;
            }
        } else {
            *pending = Some(PendingTouchScroll {
                position,
                modifiers,
                phase,
            });
        }
    }

    fn cancel_touch_click_feedback(&self, modifiers: Modifiers) {
        // The immediate release cancels pending clicks. The deferred release runs after GPUI has
        // repainted the active-state mouse-up listener that is created in response to mouse-down.
        self.dispatch_touch_click_feedback_cancel(modifiers);
        self.pending_touch_click_feedback_cancel
            .set(Some(modifiers));
    }

    fn dispatch_touch_click_feedback_cancel(&self, modifiers: Modifiers) {
        self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: point(px(-1.0), px(-1.0)),
            modifiers,
            click_count: 1,
        }));
    }

    fn flush_pending_touch_click_feedback_cancel(&self) {
        if let Some(modifiers) = self.pending_touch_click_feedback_cancel.take() {
            self.dispatch_touch_click_feedback_cancel(modifiers);
        }
    }

    fn clear_touch_hover_feedback(&self, modifiers: Modifiers) {
        self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
            position: point(px(-1.0), px(-1.0)),
            pressed_button: None,
            modifiers,
        }));
    }

    fn dispatch_touch_scroll_wheel(
        &self,
        scroll_wheel_event: ScrollWheelEvent,
    ) -> crate::DispatchEventResult {
        let modifiers = scroll_wheel_event.modifiers;
        let result = Self::dispatch_input_with_callbacks(
            &self.callbacks,
            PlatformInput::ScrollWheel(scroll_wheel_event),
        );
        self.clear_touch_hover_feedback(modifiers);
        result
    }

    fn flush_pending_scroll(&self) {
        let pending = self.pending_touch_scroll.borrow_mut().take();
        let Some(pending) = pending else {
            return;
        };

        let Some(last_position) = *self.last_dispatched_touch_position.borrow() else {
            *self.last_dispatched_touch_position.borrow_mut() = Some(pending.position);
            return;
        };

        let delta = self.filter_touch_delta(point(
            pending.position.x - last_position.x,
            pending.position.y - last_position.y,
        ));
        *self.last_dispatched_touch_position.borrow_mut() = Some(pending.position);

        if delta.x.as_f32() == 0.0 && delta.y.as_f32() == 0.0 {
            return;
        }

        let result = self.dispatch_touch_scroll_wheel(ScrollWheelEvent {
            position: pending.position,
            delta: ScrollDelta::Pixels(delta),
            modifiers: pending.modifiers,
            touch_phase: pending.phase,
        });

        self.touch_hit_boundary.set(result.propagate);
    }

    fn begin_scroll_animation(
        &self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        velocity: Point<f32>,
        friction: f32,
    ) {
        self.touch_hit_boundary.set(false);
        self.set_scroll_frame_rate_boost(true);
        *self.scroll_animation.borrow_mut() = Some(ScrollAnimation {
            position,
            modifiers,
            initial_velocity: velocity,
            gamma: friction * Self::FRICTION_SCALE,
            elapsed: 0.0,
            last_distance: point(px(0.0), px(0.0)),
            last_frame_timestamp: None,
        });
    }

    fn set_scroll_frame_rate_boost(&self, boosted: bool) {
        if self.scroll_frame_rate_boosted.get() == boosted {
            return;
        }

        if let Some(app) = self.app.borrow().as_ref() {
            if boosted {
                app.set_frame_rate(60, 120, 120);
            } else {
                app.set_frame_rate(30, 120, 60);
            }
        }
        self.scroll_frame_rate_boosted.set(boosted);
    }

    fn scroll_frame_timestamp(event_timestamp: i64) -> Option<Duration> {
        u64::try_from(event_timestamp)
            .ok()
            .map(Duration::from_nanos)
    }

    fn animation_frame_interval(
        animation: &mut ScrollAnimation,
        frame_timestamp: Option<Duration>,
    ) -> f32 {
        let Some(frame_timestamp) = frame_timestamp else {
            return Self::DEFAULT_FRAME_INTERVAL_SECONDS;
        };

        let elapsed = animation
            .last_frame_timestamp
            .and_then(|last_frame_timestamp| frame_timestamp.checked_sub(last_frame_timestamp))
            .map(|elapsed| elapsed.as_secs_f32())
            .filter(|elapsed| *elapsed > 0.0)
            .unwrap_or(Self::DEFAULT_FRAME_INTERVAL_SECONDS);

        animation.last_frame_timestamp = Some(frame_timestamp);
        elapsed.clamp(
            Self::MIN_FRAME_INTERVAL_SECONDS,
            Self::MAX_FRAME_INTERVAL_SECONDS,
        )
    }

    fn scroll_animation_distance(velocity: Point<f32>, gamma: f32, elapsed: f32) -> Point<Pixels> {
        if gamma <= f32::EPSILON {
            return point(px(0.0), px(0.0));
        }

        let coefficient = (1.0 - (-gamma * elapsed).exp()) / gamma;
        point(px(velocity.x * coefficient), px(velocity.y * coefficient))
    }

    fn scroll_animation_velocity(velocity: Point<f32>, gamma: f32, elapsed: f32) -> Point<f32> {
        let decay = (-gamma * elapsed).exp();
        point(velocity.x * decay, velocity.y * decay)
    }

    fn advance_scroll_animation(&self, frame_timestamp: Option<Duration>) {
        let Some(mut animation) = self.scroll_animation.borrow_mut().take() else {
            return;
        };

        let frame_interval = Self::animation_frame_interval(&mut animation, frame_timestamp);
        animation.elapsed += frame_interval;

        let current_distance = Self::scroll_animation_distance(
            animation.initial_velocity,
            animation.gamma,
            animation.elapsed,
        );
        let delta = point(
            current_distance.x - animation.last_distance.x,
            current_distance.y - animation.last_distance.y,
        );
        animation.last_distance = current_distance;

        let current_velocity = Self::scroll_animation_velocity(
            animation.initial_velocity,
            animation.gamma,
            animation.elapsed,
        );
        if Self::velocity_magnitude(current_velocity) < Self::MIN_MOMENTUM_VELOCITY
            || (delta.x.abs() < Self::MIN_SCROLL_DELTA && delta.y.abs() < Self::MIN_SCROLL_DELTA)
        {
            self.dispatch_scroll_end(animation.position, animation.modifiers);
            return;
        }

        let result = self.dispatch_touch_scroll_wheel(ScrollWheelEvent {
            position: animation.position,
            delta: ScrollDelta::Pixels(delta),
            modifiers: animation.modifiers,
            touch_phase: TouchPhase::Moved,
        });

        if result.propagate {
            self.dispatch_scroll_end(animation.position, animation.modifiers);
            return;
        }

        *self.scroll_animation.borrow_mut() = Some(animation);
    }

    fn dispatch_scroll_end(&self, position: Point<Pixels>, modifiers: Modifiers) {
        self.flush_pending_scroll();
        self.touch_hit_boundary.set(false);
        *self.scroll_animation.borrow_mut() = None;
        self.set_scroll_frame_rate_boost(false);
        self.dispatch_touch_scroll_wheel(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.0), px(0.0))),
            modifiers,
            touch_phase: TouchPhase::Ended,
        });
    }

    fn start_momentum_scroll(&self, position: Point<Pixels>, modifiers: Modifiers) {
        let touch_velocity = self.tracked_touch_velocity();
        let friction = if Self::velocity_magnitude(touch_velocity) < Self::SLOW_FLING_THRESHOLD {
            Self::SLOW_FLING_FRICTION
        } else {
            Self::FLING_FRICTION
        };
        let initial_velocity = Self::clamp_velocity(point(
            touch_velocity.x * Self::FLING_VELOCITY_SCALE,
            touch_velocity.y * Self::FLING_VELOCITY_SCALE,
        ));
        if Self::velocity_magnitude(initial_velocity) < Self::MIN_MOMENTUM_VELOCITY {
            self.dispatch_scroll_end(position, modifiers);
            return;
        }
        self.begin_scroll_animation(position, modifiers, initial_velocity, friction);
    }

    /// Binds the ArkTS IME session, retrying from Rust until it succeeds.
    ///
    /// The first attempt typically runs while the edit box has not taken focus
    /// yet, so `attachWithUIContext` rejects it. ArkTS retries internally and
    /// reports the real outcome here, which lets the next focus or caret event
    /// issue a fresh attempt instead of assuming success.
    fn show_keyboard_if_needed(&self) {
        if self.ime_attached.get() || self.ime_attach_in_flight.get() {
            return;
        }
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        self.ime_attach_in_flight.set(true);
        let ime_attached = self.ime_attached.clone();
        let ime_attach_in_flight = self.ime_attach_in_flight.clone();
        let executor = self.foreground_executor.clone();
        executor
            .spawn(async move {
                let attached = match app.ime() {
                    Ok(client) => match client.attach().await {
                        Ok(ack) => ack.accepted,
                        Err(error) => {
                            log::warn!("show_keyboard_if_needed: ime attach failed: {error}");
                            false
                        }
                    },
                    Err(error) => {
                        log::warn!("show_keyboard_if_needed: ime client unavailable: {error}");
                        false
                    }
                };
                ime_attached.set(attached);
                ime_attach_in_flight.set(false);
            })
            .detach();
    }

    fn hide_keyboard_if_needed(&self) {
        if !self.ime_attached.replace(false) {
            return;
        }
        if let Some(app) = self.app.borrow().as_ref() {
            let app = app.clone();
            let executor = self.foreground_executor.clone();
            executor
                .spawn(async move {
                    if let Ok(client) = app.ime() {
                        if let Err(error) = client.detach().await {
                            log::warn!("hide_keyboard_if_needed: ime detach failed: {error}");
                        }
                    }
                })
                .detach();
        }
    }

    fn notify_keyboard_hidden_by_user_if_needed(&self) {
        if self.keyboard_visible.replace(false) {
            let mut callback = self
                .callbacks
                .borrow_mut()
                .virtual_keyboard_hidden_by_user
                .take();
            if let Some(ref mut cb) = callback {
                cb();
            }
            self.callbacks.borrow_mut().virtual_keyboard_hidden_by_user = callback;
        }
    }

    fn keyboard_inset_for_overlap(&self, overlap_device_px: i32) -> Pixels {
        const MIN_CONTENT_HEIGHT: f32 = 64.0;

        let overlap = overlap_device_px.max(0) as f32;
        let scale = self.scale_factor().max(1.0);
        let mut inset = (overlap / scale).max(0.0);
        let bounds_height = self.bounds.borrow().size.height.as_f32().max(0.0);
        let max_inset = (bounds_height - MIN_CONTENT_HEIGHT).max(0.0);
        if inset > max_inset {
            inset = max_inset;
        }
        px(inset)
    }

    fn keyboard_overlap_from_avoid_area_device_px(&self) -> Option<i32> {
        if !self.safe_area_avoidance_enabled.get() {
            return Some(0);
        }

        let app_ref = self.app.borrow();
        let app = app_ref.as_ref()?;

        let content_rect = app.content_rect();
        if content_rect.height <= 0 {
            return Some(0);
        }

        // Use actual XComponent rect as layout basis for keyboard-avoid computation.
        // This keeps behavior correct for embedded/non-fullscreen XComponents.
        let layout_top = content_rect.top;
        let layout_height = content_rect.height.max(0);
        if layout_height <= 0 {
            return Some(0);
        }
        let window_rect = app.window_rect();
        let window_top = window_rect.top;
        let window_bottom = window_rect.top.saturating_add(window_rect.height.max(0));

        let keyboard_area = app.avoid_area(AvoidAreaType::Keyboard);
        let system_area = app.avoid_area(AvoidAreaType::System);
        let system_gesture_area = app.avoid_area(AvoidAreaType::SystemGesture);
        let navigation_indicator_area = app.avoid_area(AvoidAreaType::NavigationIndicator);

        // OHOS avoid-area bottomRect coordinates are in window/screen space.
        // XComponent's content_rect can be reported in safe-content coordinates on some devices.
        // For root full-width layouts, infer top-safe offset so intersection uses a consistent space.
        let root_layout_width_matches_window = content_rect.width > 0
            && window_rect.width > 0
            && (content_rect.width - window_rect.width).abs() <= 1;
        let can_infer_root_safe_top = layout_top == 0
            && layout_height > 0
            && window_rect.height >= layout_height
            && root_layout_width_matches_window;
        let inferred_outside_bottom_safe = if can_infer_root_safe_top {
            let bottom_safe_overlap = |area: Option<openharmony_ability::AvoidArea>| -> i32 {
                let Some(area) = area else {
                    return 0;
                };
                if !area.visible || area.bottom_rect.height <= 0 {
                    return 0;
                }
                let start = area.bottom_rect.top;
                let end = area
                    .bottom_rect
                    .top
                    .saturating_add(area.bottom_rect.height.max(0));
                if end < window_bottom {
                    return 0;
                }
                (window_bottom - start)
                    .max(0)
                    .min(area.bottom_rect.height.max(0))
            };

            bottom_safe_overlap(system_area)
                .max(bottom_safe_overlap(system_gesture_area))
                .max(bottom_safe_overlap(navigation_indicator_area))
        } else {
            0
        };
        let inferred_top_safe = if can_infer_root_safe_top {
            (window_rect.height.max(0) - layout_height - inferred_outside_bottom_safe).max(0)
        } else {
            0
        };
        // Convert GPUI layout bounds to screen space before intersection.
        let layout_top_screen = window_top
            .saturating_add(inferred_top_safe)
            .saturating_add(layout_top);
        let layout_bottom_screen = layout_top_screen.saturating_add(layout_height);

        let keyboard_avoid_visible = keyboard_area.map(|a| a.visible).unwrap_or(false);
        if !(self.keyboard_visible.get() || keyboard_avoid_visible) {
            return Some(0);
        }

        // Keyboard event only determines show/hide state.
        // Actual inset is derived from avoid-area geometry.
        // When keyboard is shown, include bottom occlusion union of:
        // - Keyboard area
        // - System bottom area (3-button navigation etc.)
        // - System gesture area
        // - Navigation indicator area
        // This prevents under-subtraction where keyboard area excludes nav area.
        let mut intervals: Vec<(i32, i32)> = Vec::with_capacity(4);
        let mut push_bottom_overlap_interval =
            |area: openharmony_ability::AvoidArea, require_visible: bool| {
                if area.bottom_rect.height <= 0 {
                    return;
                }
                if require_visible && !area.visible {
                    return;
                }
                let start = area.bottom_rect.top.max(layout_top_screen);
                let end = area
                    .bottom_rect
                    .top
                    .saturating_add(area.bottom_rect.height.max(0))
                    .min(layout_bottom_screen);
                if end > start {
                    intervals.push((start, end));
                }
            };

        if let Some(area) = keyboard_area {
            push_bottom_overlap_interval(area, true);
        }
        if let Some(area) = system_area {
            push_bottom_overlap_interval(area, false);
        }
        if let Some(area) = system_gesture_area {
            push_bottom_overlap_interval(area, false);
        }
        if let Some(area) = navigation_indicator_area {
            push_bottom_overlap_interval(area, false);
        }

        if intervals.is_empty() {
            return Some(0);
        }

        intervals.sort_unstable_by_key(|(start, _)| *start);
        let mut union_overlap = 0i32;
        let mut current = intervals[0];
        for &(start, end) in intervals.iter().skip(1) {
            if start <= current.1 {
                current.1 = current.1.max(end);
            } else {
                union_overlap = union_overlap.saturating_add(current.1 - current.0);
                current = (start, end);
            }
        }
        union_overlap = union_overlap.saturating_add(current.1 - current.0);

        let geometric_overlap = union_overlap.min(layout_height.max(0));
        let clamped_overlap = geometric_overlap;

        Some(clamped_overlap)
    }

    // OHOS 键盘避让开关入口。zed 1.17 PlatformWindow trait 已删除 set_safe_area_avoidance，
    // 该方法无 trait 调用者，但保留以维持键盘避让开关能力，后续上层需要时可接入。
    #[allow(dead_code)]
    fn set_safe_area_avoidance_enabled(&self, enabled: bool) {
        let previous = self.safe_area_avoidance_enabled.replace(enabled);
        if previous != enabled && self.refresh_keyboard_overlap_device_px() {
            self.emit_resize_callback();
        }
    }

    fn refresh_keyboard_overlap_device_px(&self) -> bool {
        let previous_overlap = self.keyboard_overlap_device_px.get();
        let next_overlap = self
            .keyboard_overlap_from_avoid_area_device_px()
            .unwrap_or(0)
            .max(0);
        if previous_overlap != next_overlap {
            self.keyboard_overlap_device_px.set(next_overlap);
            true
        } else {
            false
        }
    }

    fn effective_content_size(&self) -> Size<Pixels> {
        let bounds_size = self.bounds.borrow().size;
        let bounds_height = bounds_size.height.as_f32().max(0.0);
        let keyboard_inset = self
            .keyboard_inset_for_overlap(self.keyboard_overlap_device_px.get())
            .as_f32();
        size(
            bounds_size.width,
            px((bounds_height - keyboard_inset).max(0.0)),
        )
    }

    fn emit_resize_callback(&self) {
        let scale = *self.scale.borrow();
        let content_size = self.effective_content_size();
        let resize_state = ResizeCallbackState {
            content_size,
            scale,
        };

        let mut callback = self.callbacks.borrow_mut().resize.take();
        if callback.is_none() {
            self.callbacks.borrow_mut().resize = callback;
            return;
        }

        {
            let mut last_emitted_resize = self.last_emitted_resize.borrow_mut();
            if last_emitted_resize
                .as_ref()
                .copied()
                .is_some_and(|last_resize| Self::resize_state_matches(last_resize, resize_state))
            {
                self.callbacks.borrow_mut().resize = callback;
                return;
            }
            *last_emitted_resize = Some(resize_state);
        }

        if let Some(ref mut cb) = callback {
            cb(content_size, scale);
        }
        self.callbacks.borrow_mut().resize = callback;
    }

    fn request_frame(&self, force_render: bool) {
        let mut callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(ref mut callback) = callback {
            self.pending_frame_request.set(None);
            callback(RequestFrameOptions {
                require_presentation: force_render,
                force_render,
            });
        } else {
            let force_render = self
                .pending_frame_request
                .take()
                .is_some_and(|pending_force_render| pending_force_render)
                || force_render;
            self.pending_frame_request.set(Some(force_render));
            warn!("OhosWindow: request_frame called before callback was set");
        }
        self.callbacks.borrow_mut().request_frame = callback;
    }

    /// Initialize the renderer when native_window becomes available (after SurfaceCreate event).
    /// This method gets the raw_window_handle from OpenHarmonyApp's native_window.
    pub(crate) fn initialize_renderer(&self) -> Result<()> {
        log::info!("[boot] initialize_renderer entered");
        let mut renderer_guard = self.renderer.borrow_mut();
        if renderer_guard.is_some() {
            // Already initialized
            return Ok(());
        }

        // Get native_window from OpenHarmonyApp - it should be available after SurfaceCreate
        let app = self.app.borrow();
        let app_ref = app.as_ref().ok_or_else(|| {
            anyhow::anyhow!("OpenHarmonyApp not available when initializing renderer")
        })?;

        // Check that native_window is available - this is required for the renderer to work.
        // The actual window handle is obtained via HasWindowHandle trait implementation.
        let _native_window = app_ref.native_window().ok_or_else(|| {
            anyhow::anyhow!(
                "native_window not available yet - SurfaceCreate event may not have been received"
            )
        })?;
        log::info!("[boot] initialize_renderer: native_window available");

        // Get the actual window size from content_rect.
        // Using the correct size is important because mismatched sizes between
        // the surface configuration and the actual native_window can cause
        // rendering issues (stretched/cropped content, black borders, etc.)
        // even though create_platform_window_surface itself won't fail.
        let content_rect = app_ref.content_rect();
        let scale = app_ref.scale() as f32;
        let device_width = if content_rect.width > 0 {
            content_rect.width as u32
        } else {
            // Fallback to bounds if content_rect is not available yet
            self.bounds.borrow().size.width.as_f32() as u32
        };
        let device_height = if content_rect.height > 0 {
            content_rect.height as u32
        } else {
            self.bounds.borrow().size.height.as_f32() as u32
        };
        log::info!(
            "[boot] initialize_renderer: content_rect={:?}, device size {}x{}",
            content_rect,
            device_width,
            device_height
        );

        // Update window bounds to match actual content_rect (convert device px -> logical px)
        if content_rect.width > 0 && content_rect.height > 0 {
            let logical_size = size(
                px(device_width as f32 / scale),
                px(device_height as f32 / scale),
            );
            *self.bounds.borrow_mut() = Bounds::new(point(px(0.0), px(0.0)), logical_size);
        }

        let config = WgpuSurfaceConfig {
            size: Size {
                width: DevicePixels(device_width as i32),
                height: DevicePixels(device_height as i32),
            },
            transparent: true,
        };

        debug!(
            "OhosWindow: Surface config - width: {}, height: {}, transparent: false",
            device_width, device_height
        );

        // Debug: Check window handle before creating renderer
        match self.window_handle() {
            Ok(_) => {}
            Err(e) => {
                warn!("failed to get OHOS window handle: {:?}", e);
                return Err(anyhow::anyhow!("Window handle not available: {:?}", e));
            }
        }

        let gpu_context = if let Some(gpu_context) = self.gpu_context.borrow().clone() {
            gpu_context
        } else {
            log::info!("[boot] initialize_renderer: creating WgpuContext");
            let gpu_context = Arc::new(WgpuContext::new().map_err(|error| {
                warn!("failed to create OHOS GPU context: {error}");
                anyhow::anyhow!("Failed to create GPU context: {error}")
            })?);
            log::info!("[boot] initialize_renderer: WgpuContext created");
            *self.gpu_context.borrow_mut() = Some(gpu_context.clone());
            gpu_context
        };

        // Create renderer using the window's HasWindowHandle and HasDisplayHandle implementation
        // which will get the raw_window_handle from native_window
        log::info!("[boot] initialize_renderer: creating WgpuRenderer");
        let renderer = WgpuRenderer::new(&gpu_context, self, config)
            .map_err(|e| {
                warn!("failed to initialize OHOS renderer: {}", e);
                anyhow::anyhow!("Failed to create Wgpu renderer: {}. Make sure native_window is available from OpenHarmonyApp.", e)
            })?;
        log::info!("[boot] initialize_renderer: WgpuRenderer created");

        *renderer_guard = Some(renderer);
        Ok(())
    }

    pub(crate) fn handle_event(&self, event: &Event) {
        match event {
            Event::SurfaceCreate => {
                // Initialize renderer when SurfaceCreate event is received
                // Note: on_finish_launching is handled at the platform level (OhosPlatform::handle_ohos_event)
                // before windows are created.
                match self.initialize_renderer() {
                    Ok(()) => {}
                    Err(e) => {
                        warn!(
                            "SurfaceCreate failed to initialize OHOS renderer: {}. Make sure native_window is available from OpenHarmonyApp.",
                            e
                        );
                    }
                }
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_resize_callback();
                }
                self.request_frame(true);
                // The XComponent is the edit surface; attach the system IME so it
                // can receive input once the surface gains focus.
                self.show_keyboard_if_needed();
            }
            Event::WindowResize(ohos_size) => {
                let scale = *self.scale.borrow();
                let width = ohos_size.width as f32;
                let height = ohos_size.height as f32;
                let new_size = size(px(width / scale), px(height / scale));
                let bounds_changed = self.set_bounds_size(new_size);
                let keyboard_overlap_changed = self.refresh_keyboard_overlap_device_px();

                // Update renderer's drawable size
                if bounds_changed && let Some(ref mut renderer) = *self.renderer.borrow_mut() {
                    let device_size = Size {
                        width: DevicePixels(width as i32),
                        height: DevicePixels(height as i32),
                    };
                    renderer.update_drawable_size(device_size);
                }
                if bounds_changed || keyboard_overlap_changed {
                    self.emit_resize_callback();
                    self.request_frame(true);
                }
                self.refresh_ime_cursor();
            }
            Event::ContentRectChange(..) => {
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_resize_callback();
                }
                self.refresh_ime_cursor();
            }
            Event::AvoidAreaChange(info) => {
                if matches!(
                    info.area_type,
                    AvoidAreaType::Keyboard
                        | AvoidAreaType::System
                        | AvoidAreaType::SystemGesture
                        | AvoidAreaType::NavigationIndicator
                ) && self.refresh_keyboard_overlap_device_px()
                {
                    self.emit_resize_callback();
                }
                self.refresh_ime_cursor();
            }
            Event::WindowRedraw(info) => {
                self.flush_pending_scroll();
                self.advance_scroll_animation(
                    Self::scroll_frame_timestamp(info.target_time_stamp)
                        .or_else(|| Self::scroll_frame_timestamp(info.time_stamp)),
                );
                self.request_frame(false);
                // Consume the pending-redraw flag: if GPUI requested another frame
                // while this one was being drawn (wake_platform -> frame_waker),
                // keep the frame callback armed; otherwise unregister it so idle
                // windows stop waking the main thread for per-vsync callbacks.
                // enable/disable_frame_callback own the enabled flag, so no
                // explicit set_frame_callback_enabled call is needed here.
                if !PENDING_REDRAW.swap(false, Ordering::AcqRel) {
                    if let Some(app) = self.app.borrow().clone() {
                        app.disable_frame_callback();
                    }
                }
                self.flush_pending_touch_click_feedback_cancel();
            }
            Event::Input(input_event) => {
                self.handle_input_event(input_event);
            }
            Event::GainedFocus => {
                let mut callback = self.callbacks.borrow_mut().active_status_change.take();
                if let Some(ref mut cb) = callback {
                    cb(true);
                }
                self.callbacks.borrow_mut().active_status_change = callback;
                // On gaining focus the active-status change often repaints UI
                // (highlights, cursor), but that demand may arrive without a
                // visibility event. Re-arm the frame callback here so the
                // repaint is drawn instead of flashing stale content.
                if PENDING_REDRAW.load(Ordering::Acquire) {
                    if let Some(app) = self.app.borrow().clone() {
                        app.enable_frame_callback();
                    }
                }
                // Re-attach the IME when the window regains focus (e.g. after the
                // app was minimized and restored); the previous NDK path crashed
                // here because its IME instance was dropped and never re-created.
                self.show_keyboard_if_needed();
            }
            Event::LostFocus => {
                self.cancel_momentum();
                self.reset_touch_velocity();
                self.reset_touch_state();
                let mut callback = self.callbacks.borrow_mut().active_status_change.take();
                if let Some(ref mut cb) = callback {
                    cb(false);
                }
                self.callbacks.borrow_mut().active_status_change = callback;
                self.hide_keyboard_if_needed();
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_resize_callback();
                }
            }
            Event::VisibilityChanged(visible) => {
                // On-demand vsync is driven by window visibility, not by focus:
                // hiding unregisters the frame callback exactly once (idempotent
                // inside disable_frame_callback), so minimize no longer hits the
                // DisplaySync DelFromPipeline path with a null context; restoring
                // re-arms it only when a frame was requested while hidden
                // (PENDING_REDRAW).
                if *visible {
                    if PENDING_REDRAW.load(Ordering::Acquire) {
                        if let Some(app) = self.app.borrow().clone() {
                            app.enable_frame_callback();
                        }
                    }
                } else if let Some(app) = self.app.borrow().clone() {
                    app.disable_frame_callback();
                }
            }
            Event::ConfigChanged(..) => {
                let new_scale = self
                    .app
                    .borrow()
                    .as_ref()
                    .map(|a| a.scale() as f32)
                    .unwrap_or(1.0);
                let scale_changed = (*self.scale.borrow() - new_scale).abs() > f32::EPSILON;
                *self.scale.borrow_mut() = new_scale;
                let keyboard_overlap_changed = self.refresh_keyboard_overlap_device_px();
                if scale_changed || keyboard_overlap_changed {
                    self.emit_resize_callback();
                    self.request_frame(true);
                }
                self.refresh_ime_cursor();
            }
            Event::WindowDestroy => {
                self.cancel_momentum();
                self.reset_touch_velocity();
                self.reset_touch_state();
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_resize_callback();
                }
                // For should_close, we need to call it and check return value
                let mut should_close_callback = self.callbacks.borrow_mut().should_close.take();
                let should_close = if let Some(ref mut cb) = should_close_callback {
                    cb()
                } else {
                    true // Default to allowing close if no callback
                };
                self.callbacks.borrow_mut().should_close = should_close_callback;

                if should_close {
                    // close is FnOnce, so we just take and call it
                    if let Some(callback) = self.callbacks.borrow_mut().close.take() {
                        callback();
                    }
                }
            }
            Event::KeyboardEvent(height) => {
                if *height <= 0 {
                    self.notify_keyboard_hidden_by_user_if_needed();
                } else {
                    self.keyboard_visible.set(true);
                }
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_resize_callback();
                }
            }
            _ => {}
        }
    }

    /// Handles an IME delete-left (Backspace) event.
    ///
    /// When IME composition text is present, delete within it through the IME
    /// text layer. When there is no composition text, the focused view's text
    /// lives outside the IME layer (e.g. the terminal, whose line buffer is in
    /// the PTY), so deliver a real Backspace key press and let the view delete
    /// through its own key handling.
    fn handle_ime_backspace(&self, len: usize) {
        if len == 0 {
            return;
        }
        let deleted_ime_text = {
            let mut handler_guard = self.input_handler.borrow_mut();
            let Some(handler) = handler_guard.as_mut() else {
                return;
            };
            if handler.marked_text_range().is_none() {
                false
            } else {
                if let Some(selection) = handler.selected_text_range(true) {
                    let range = if selection.range.start != selection.range.end {
                        selection.range
                    } else {
                        let caret = if selection.reversed {
                            selection.range.start
                        } else {
                            selection.range.end
                        };
                        let start = caret.saturating_sub(len);
                        start..caret
                    };
                    handler.replace_text_in_range(Some(range), "");
                } else {
                    handler.replace_text_in_range(None, "");
                }
                true
            }
        };
        if deleted_ime_text {
            return;
        }
        let keystroke = Keystroke {
            modifiers: Modifiers::default(),
            key: "backspace".to_string(),
            key_char: None,
        };
        self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }));
    }

    /// Handles the IME delete-forward (Delete) event. Forward delete has no
    /// meaning inside an IME composition, so when composition text is present
    /// it is ignored; otherwise a real Delete key press is delivered so the
    /// focused view can handle it.
    fn handle_ime_delete_forward(&self, len: usize) {
        if len == 0 {
            return;
        }
        let has_composition = {
            let mut handler_guard = self.input_handler.borrow_mut();
            let Some(handler) = handler_guard.as_mut() else {
                return;
            };
            handler.marked_text_range().is_some()
        };
        if has_composition {
            return;
        }
        let keystroke = Keystroke {
            modifiers: Modifiers::default(),
            key: "delete".to_string(),
            key_char: None,
        };
        self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }));
    }

    /// Handles the IME enter / sendFunctionKey (completion) event. With
    /// composition text present the previous behavior is kept (commit the text).
    /// Without composition a real Enter key press is delivered, so each control
    /// applies its own Enter semantics (e.g. completing a settings input,
    /// submitting a terminal line) instead of inserting a literal newline.
    fn handle_ime_enter(&self) {
        let committed_ime_text = {
            let mut handler_guard = self.input_handler.borrow_mut();
            let Some(handler) = handler_guard.as_mut() else {
                return;
            };
            if handler.marked_text_range().is_none() {
                false
            } else {
                handler.replace_text_in_range(None, "\n");
                handler.unmark_text();
                true
            }
        };
        if committed_ime_text {
            return;
        }
        let keystroke = Keystroke {
            modifiers: Modifiers::default(),
            key: "enter".to_string(),
            key_char: None,
        };
        self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }));
    }

    fn handle_input_event(&self, event: &InputEvent) {
        match event {
            InputEvent::ImeEvent(ime_event) => {
                if matches!(
                    ime_event,
                    ImeEvent::ImeStatusEvent(openharmony_ability::ime::KeyboardStatus::Hide)
                ) {
                    self.notify_keyboard_hidden_by_user_if_needed();
                    if self.refresh_keyboard_overlap_device_px() {
                        self.emit_resize_callback();
                    }
                }

                // OHOS reports the physical Backspace/Delete/Enter keys as IME
                // editing-key events (deleteLeft/deleteRight/sendFunctionKey).
                // When there is no composition (marked) text, the focused view
                // (e.g. the terminal, whose line buffer lives in the PTY) has
                // nothing in the IME text layer, so each editing key is delivered
                // as a real key press instead and the view applies its own
                // semantics. See handle_ime_backspace/delete_forward/enter.
                match ime_event {
                    ImeEvent::BackspaceEvent(len) => {
                        self.handle_ime_backspace((*len).max(0) as usize);
                        return;
                    }
                    ImeEvent::DeleteRightEvent(len) => {
                        self.handle_ime_delete_forward((*len).max(0) as usize);
                        return;
                    }
                    ImeEvent::EnterEvent(_key) => {
                        self.handle_ime_enter();
                        return;
                    }
                    _ => {}
                }

                let handler_ref = self.input_handler.clone();
                let ime_event = ime_event.clone();
                let executor = self.foreground_executor.clone();

                executor
                    .spawn(async move {
                        let mut handler_guard = handler_ref.borrow_mut();
                        let Some(handler) = handler_guard.as_mut() else {
                            return;
                        };

                        match ime_event {
                            ImeEvent::TextInputEvent(data) => {
                                handler.replace_text_in_range(None, &data.text);
                                handler.unmark_text();
                            }
                            // Enter is handled synchronously in handle_input_event
                            // (handle_ime_enter); never reached from the closure.
                            ImeEvent::EnterEvent(_action) => {}
                            ImeEvent::ImeStatusEvent(status) => {
                                if matches!(status, openharmony_ability::ime::KeyboardStatus::Hide)
                                {
                                    handler.unmark_text();
                                }
                            }
                            // Backspace/Delete are handled synchronously in
                            // handle_input_event before this closure runs, so
                            // they never reach here; the arm only makes the match
                            // exhaustive.
                            ImeEvent::BackspaceEvent(_) | ImeEvent::DeleteRightEvent(_) => {}
                        }
                    })
                    .detach();
            }
            InputEvent::KeyEvent(key_event) => {
                // Stateless key handling: modifiers and caps-lock are carried by each event.
                // Re-sync the full modifier state to GPUI on every key-down before the
                // keystroke, matching desktop behavior without caching any state.
                match key_event.action {
                    Action::Down => {
                        self.dispatch_input(PlatformInput::ModifiersChanged(
                            ModifiersChangedEvent {
                                modifiers: super::keycodes::modifiers_from_modifier_state(
                                    key_event.modifier_state,
                                ),
                                capslock: Capslock { on: key_event.capslock },
                            },
                        ));

                        // Modifier keys only update the modifier state; they are not
                        // delivered as KeyDown events, matching desktop platforms.
                        // Delivering a lone Alt as KeyDown (key "alt") would let the
                        // terminal encode it as ESC+ascii and echo stray characters
                        // (observed as "lt" in the PTY).
                        if !Self::is_modifier_key(key_event.code) {
                            let keystroke = super::keycodes::key_event_to_keystroke(key_event);
                            let key_down_event = KeyDownEvent {
                                keystroke: keystroke.clone(),
                                is_held: false,
                                prefer_character_input: false,
                            };
                            self.dispatch_input(PlatformInput::KeyDown(key_down_event));
                            // OHOS KeyAction has no Repeat; synthesize auto-repeat with an
                            // initial delay followed by a periodic is_held key-down.
                            self.begin_key_repeat(key_event, keystroke);
                        }
                    }
                    Action::Up => {
                        self.end_key_repeat();
                    }
                    _ => {}
                }
            }
            InputEvent::TouchEvent(touch_event) => {
                let position = self.touch_position(touch_event);
                let modifiers = Modifiers::default();
                let event_timestamp = Self::touch_timestamp(touch_event.timestamp);

                match touch_event.event_type {
                    TouchEvent::Down => {
                        if self.begin_touch_tracking(position, event_timestamp, true) {
                            let click_count = self
                                .click_tracker
                                .borrow_mut()
                                .on_button_press(MouseButton::Left, position);
                            self.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers,
                                click_count,
                                first_mouse: false,
                            }));
                        }
                    }
                    TouchEvent::Up => {
                        match self.touch_state.get() {
                            TouchState::Scrolling(scroll_state) => {
                                let velocity_is_stale = self.touch_gap_exceeded(event_timestamp);
                                self.record_touch_position(position, event_timestamp);
                                let delta = Self::filter_touch_delta_with_axis(
                                    point(
                                        position.x - scroll_state.last_position.x,
                                        position.y - scroll_state.last_position.y,
                                    ),
                                    Some(scroll_state.locked_axis),
                                );
                                if delta.x.as_f32() != 0.0 || delta.y.as_f32() != 0.0 {
                                    self.queue_pending_touch_scroll(
                                        position,
                                        modifiers,
                                        TouchPhase::Moved,
                                    );
                                }

                                self.flush_pending_scroll();
                                if self.touch_hit_boundary.get() {
                                    self.reset_touch_velocity();
                                    self.dispatch_scroll_end(position, modifiers);
                                } else if velocity_is_stale {
                                    self.reset_touch_velocity();
                                    self.dispatch_scroll_end(position, modifiers);
                                } else {
                                    self.start_momentum_scroll(position, modifiers);
                                }
                            }
                            TouchState::Pending(pending_state)
                                if !pending_state.cancel_click
                                    && !self.tap_duration_exceeded(event_timestamp) =>
                            {
                                if pending_state.mouse_down_sent {
                                    let click_count = self.click_tracker.borrow().current_count();
                                    self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position,
                                        modifiers,
                                        click_count,
                                    }));
                                    self.click_tracker.borrow_mut().on_click_complete(
                                        MouseButton::Left,
                                        position,
                                    );
                                }
                            }
                            TouchState::Pending(pending_state) if pending_state.mouse_down_sent => {
                                self.cancel_touch_click_feedback(modifiers);
                            }
                            TouchState::Idle | TouchState::Pending(..) => {}
                        }

                        self.reset_touch_state();
                    }
                    TouchEvent::Move => {
                        let pressed_point_count = touch_event
                            .touch_points
                            .iter()
                            .filter(|point| point.is_pressed)
                            .count();

                        if !self.touch_is_active() {
                            self.begin_touch_tracking(position, event_timestamp, false);
                        }

                        match self.touch_state.get() {
                            TouchState::Idle => {}
                            TouchState::Pending(mut pending_state) => {
                                if pressed_point_count > 1 {
                                    if pending_state.mouse_down_sent {
                                        self.cancel_touch_click_feedback(modifiers);
                                        pending_state.mouse_down_sent = false;
                                    }
                                    pending_state.cancel_click = true;
                                }

                                let from_start = point(
                                    position.x - pending_state.start_position.x,
                                    position.y - pending_state.start_position.y,
                                );
                                let from_start_sq = from_start.x.as_f32() * from_start.x.as_f32()
                                    + from_start.y.as_f32() * from_start.y.as_f32();
                                self.record_touch_position(position, event_timestamp);

                                if Self::movement_exceeds_touch_slop(from_start_sq) {
                                    let locked_axis = Self::touch_axis(from_start);
                                    let delta = Self::filter_touch_delta_with_axis(
                                        point(
                                            position.x - pending_state.last_position.x,
                                            position.y - pending_state.last_position.y,
                                        ),
                                        Some(locked_axis),
                                    );

                                    if pending_state.mouse_down_sent {
                                        self.cancel_touch_click_feedback(modifiers);
                                    }
                                    self.set_scroll_frame_rate_boost(true);
                                    *self.last_dispatched_touch_position.borrow_mut() =
                                        Some(pending_state.last_position);
                                    self.touch_state
                                        .set(TouchState::Scrolling(TouchScrollState {
                                            last_position: position,
                                            locked_axis,
                                        }));

                                    if delta.x.as_f32() != 0.0 || delta.y.as_f32() != 0.0 {
                                        self.queue_pending_touch_scroll(
                                            position,
                                            modifiers,
                                            TouchPhase::Started,
                                        );
                                    }
                                } else {
                                    pending_state.last_position = position;
                                    self.touch_state.set(TouchState::Pending(pending_state));
                                }
                            }
                            TouchState::Scrolling(mut scroll_state) => {
                                let raw_delta = point(
                                    position.x - scroll_state.last_position.x,
                                    position.y - scroll_state.last_position.y,
                                );
                                let delta = Self::filter_touch_delta_with_axis(
                                    raw_delta,
                                    Some(scroll_state.locked_axis),
                                );
                                self.record_touch_position(position, event_timestamp);
                                if delta.x.as_f32() != 0.0 || delta.y.as_f32() != 0.0 {
                                    self.queue_pending_touch_scroll(
                                        position,
                                        modifiers,
                                        TouchPhase::Moved,
                                    );
                                }
                                scroll_state.last_position = position;
                                self.touch_state.set(TouchState::Scrolling(scroll_state));
                            }
                        }
                    }
                    TouchEvent::Cancel | TouchEvent::Unknown => {
                        if self.touch_is_active() {
                            self.cancel_touch_click_feedback(modifiers);
                        }
                        if self.touch_is_scrolling() {
                            self.dispatch_scroll_end(position, modifiers);
                        }
                        self.cancel_momentum();
                        self.reset_touch_velocity();
                        self.reset_touch_state();
                    }
                }
            }
            InputEvent::MouseEvent(data) => {
                self.handle_mouse_input(data);
            }
            InputEvent::HoverEvent(is_hover) => {
                self.handle_hover_event(*is_hover);
            }
            InputEvent::AxisEvent(data) => {
                self.handle_axis_input(data);
            }
        }
    }

    fn handle_hover_event(&self, is_hover: bool) {
        // Mirrors gpui_linux's set_hovered on XinputEnter/Leave: only the pointer
        // enter/leave transition drives the GPUI hovered state.
        let callback = self.callbacks.borrow_mut().hover_status_change.take();
        if let Some(mut callback) = callback {
            callback(is_hover);
            self.callbacks.borrow_mut().hover_status_change = Some(callback);
        }
    }

    /// Starts synthesized auto-repeat for a held non-modifier key. The repeat task
    /// emits `is_held` key-downs on the foreground executor after an initial delay,
    /// until the key is released or a different key is pressed (generation bump).
    fn begin_key_repeat(&self, key_event: &KeyEventData, keystroke: Keystroke) {
        // Modifier keys are never auto-repeated; only their own key events apply.
        if Self::is_modifier_key(key_event.code) {
            return;
        }
        let code = key_event.code;
        let generation =
            self.key_repeat
                .borrow()
                .map_or(0, |state| state.generation)
                + 1;
        *self.key_repeat.borrow_mut() = Some(KeyRepeatState { code, generation });
        let key_repeat = self.key_repeat.clone();
        let callbacks = self.callbacks.clone();
        let window_alive = self.window_alive.clone();
        let background_executor = self.background_executor.clone();
        self.foreground_executor
            .spawn(async move {
                background_executor.timer(KEY_REPEAT_DELAY).await;
                while window_alive.get() && Self::repeat_active(&key_repeat, code, generation) {
                    Self::dispatch_repeat_key_down(&callbacks, &keystroke);
                    background_executor.timer(KEY_REPEAT_INTERVAL).await;
                }
            })
            .detach();
    }

    fn is_modifier_key(code: KeyCode) -> bool {
        matches!(
            code,
            KeyCode::CtrlLeft
                | KeyCode::CtrlRight
                | KeyCode::ShiftLeft
                | KeyCode::ShiftRight
                | KeyCode::AltLeft
                | KeyCode::AltRight
                | KeyCode::MetaLeft
                | KeyCode::MetaRight
                | KeyCode::CapsLock
                | KeyCode::Fn
        )
    }

    /// Stops the in-flight auto-repeat task (generation check fails on next tick).
    fn end_key_repeat(&self) {
        *self.key_repeat.borrow_mut() = None;
    }

    fn repeat_active(
        key_repeat: &Rc<RefCell<Option<KeyRepeatState>>>,
        code: KeyCode,
        generation: u64,
    ) -> bool {
        key_repeat
            .borrow()
            .is_some_and(|state| state.code == code && state.generation == generation)
    }

    fn dispatch_repeat_key_down(
        callbacks: &Rc<RefCell<WindowCallbacks>>,
        keystroke: &Keystroke,
    ) {
        let key_down_event = KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: true,
            prefer_character_input: false,
        };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::KeyDown(key_down_event));
    }

    /// Registers the event-driven pinch and drop handlers. Both plugins invoke
    /// their callback on the Ability main thread (`on_main_thread_event`), so the
    /// handlers dispatch directly to GPUI with no polling.
    pub(crate) fn register_platform_event_handlers(&self) {
        let callbacks = self.callbacks.clone();
        let pinch_accumulator = self.pinch_accumulator.clone();
        openharmony_ability_plugin_pinch::set_pinch_callback(Box::new(move |sample| {
            Self::dispatch_pinch_event(&callbacks, &pinch_accumulator, sample);
        }));
        let drop_callbacks = self.callbacks.clone();
        openharmony_ability_plugin_filedrop::set_filedrop_callback(Box::new(move |data| {
            match data {
                openharmony_ability_plugin_filedrop::FileDropEventData::Enter => {
                    Self::dispatch_filedrop_enter(&drop_callbacks);
                }
                openharmony_ability_plugin_filedrop::FileDropEventData::Move {
                    position_x,
                    position_y,
                } => Self::dispatch_filedrop_move(&drop_callbacks, position_x, position_y),
                openharmony_ability_plugin_filedrop::FileDropEventData::Drop {
                    files,
                    position_x,
                    position_y,
                } => Self::dispatch_drop_files(&drop_callbacks, files, position_x, position_y),
            }
        }));
    }

    fn dispatch_pinch_event(
        callbacks: &Rc<RefCell<WindowCallbacks>>,
        accumulator: &Rc<Cell<f32>>,
        sample: openharmony_ability_plugin_pinch::PinchEventData,
    ) {
        // GPUI editors do not consume `PinchEvent` (only image_viewer does), so a
        // pinch is translated into a control+scroll-wheel event, which is the
        // editor's zoom gesture. The delta is accumulated and one zoom step is
        // emitted only after PINCH_ZOOM_THRESHOLD is crossed. Gesture start/end
        // carry a zero delta; reset the accumulator so a sub-threshold residue
        // from one gesture never bleeds into the next.
        if sample.phase == openharmony_ability_plugin_pinch::PinchPhase::End {
            accumulator.set(0.0);
        }
        let accumulated = accumulator.get() + sample.delta;
        if accumulated.abs() < PINCH_ZOOM_THRESHOLD {
            accumulator.set(accumulated);
            return;
        }
        accumulator.set(0.0);
        let position = point(px(sample.center_x), px(sample.center_y));
        let lines = if accumulated > 0.0 { 1.0 } else { -1.0 };
        let event = ScrollWheelEvent {
            position,
            delta: ScrollDelta::Lines(point(0.0, lines)),
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
            touch_phase: match sample.phase {
                openharmony_ability_plugin_pinch::PinchPhase::Begin => TouchPhase::Started,
                openharmony_ability_plugin_pinch::PinchPhase::Update => TouchPhase::Moved,
                openharmony_ability_plugin_pinch::PinchPhase::End => TouchPhase::Ended,
            },
        };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::ScrollWheel(event));
    }

    fn dispatch_filedrop_enter(callbacks: &Rc<RefCell<WindowCallbacks>>) {
        // Establish the GPUI drag state with an empty ExternalPaths so the
        // `invisible` drop target becomes visible (group_drag_over) and registers
        // its drop listener on the next repaint.
        let position = point(px(0.0), px(0.0));
        let entered = FileDropEvent::Entered {
            position,
            paths: ExternalPaths(Default::default()),
        };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::FileDrop(entered));
    }

    fn dispatch_filedrop_move(
        callbacks: &Rc<RefCell<WindowCallbacks>>,
        position_x: f64,
        position_y: f64,
    ) {
        // Prime the input modality to mouse (a bare FileDrop event never flips
        // Window::last_input_modality), then mirror X11 `Pending`: the MouseMove
        // keeps the now-visible drop target in the hover chain.
        let position = point(px(position_x as f32), px(position_y as f32));
        let move_event = MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::MouseMove(move_event));
        let pending = FileDropEvent::Pending { position };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::FileDrop(pending));
    }

    fn dispatch_drop_files(
        callbacks: &Rc<RefCell<WindowCallbacks>>,
        files: Vec<String>,
        position_x: f64,
        position_y: f64,
    ) {
        let mut paths: Vec<PathBuf> = Vec::new();
        for uri in files {
            match openharmony_ability::path_from_uri(&uri) {
                Some(path) => paths.push(path),
                None => log::warn!("dispatch_drop_files: failed to resolve dropped URI '{uri}'"),
            }
        }
        if paths.is_empty() {
            log::warn!("dispatch_drop_files: no resolvable paths, drop ignored");
            return;
        }
        let position = point(px(position_x as f32), px(position_y as f32));
        // Entered refreshes active_drag with the actual paths, then Submit runs
        // synchronously on the onDrop callback stack — before the system's
        // post-drop MouseUp can clear active_drag.
        let entered = FileDropEvent::Entered {
            position,
            paths: ExternalPaths(paths.into()),
        };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::FileDrop(entered));
        let submit = FileDropEvent::Submit { position };
        Self::dispatch_input_with_callbacks(callbacks, PlatformInput::FileDrop(submit));
    }

    /// Opens the given paths in this window by replaying the same entered+submit drag
    /// sequence that the file-drop path uses. Used by the open-with (file
    /// association) flow as a fallback when no `on_open_urls` callback exists yet;
    /// note that drop handling needs an element drop target under the pointer, so the
    /// primary open-with path delivers through gpui's `on_open_urls` instead.
    pub(crate) fn open_external_paths(&self, paths: Vec<PathBuf>) {
        let position = point(px(0.0), px(0.0));
        let entered = FileDropEvent::Entered {
            position,
            paths: ExternalPaths(paths.into()),
        };
        Self::dispatch_input_with_callbacks(&self.callbacks, PlatformInput::FileDrop(entered));
        let submit = FileDropEvent::Submit { position };
        Self::dispatch_input_with_callbacks(&self.callbacks, PlatformInput::FileDrop(submit));
    }

    fn device_position(&self, x: f32, y: f32) -> Point<Pixels> {
        let scale = *self.scale.borrow();
        point(px(x / scale), px(y / scale))
    }

    fn map_device_button(button: DeviceMouseButton) -> MouseButton {
        match button {
            DeviceMouseButton::LeftButton => MouseButton::Left,
            DeviceMouseButton::RightButton => MouseButton::Right,
            DeviceMouseButton::MiddleButton => MouseButton::Middle,
            DeviceMouseButton::BackButton => MouseButton::Navigate(NavigationDirection::Back),
            DeviceMouseButton::ForwardButton => MouseButton::Navigate(NavigationDirection::Forward),
            DeviceMouseButton::NoneButton => MouseButton::Left,
        }
    }

    fn scroll_phase_to_touch_phase(phase: Option<ScrollPhase>) -> TouchPhase {
        match phase {
            Some(ScrollPhase::Begin) => TouchPhase::Started,
            Some(ScrollPhase::End) => TouchPhase::Ended,
            _ => TouchPhase::Moved,
        }
    }

    fn device_modifiers_to_gpui(modifiers: DeviceModifiers) -> Modifiers {
        Modifiers {
            control: modifiers.control,
            shift: modifiers.shift,
            alt: modifiers.alt,
            platform: false,
            function: false,
        }
    }

    fn modifiers_from_key_mask(mask: u64) -> Modifiers {
        // ArkUI_ModifierKeyName: Ctrl=1<<0, Shift=1<<1, Alt=1<<2, Fn=1<<3.
        const MOD_CTRL: u64 = 1 << 0;
        const MOD_SHIFT: u64 = 1 << 1;
        const MOD_ALT: u64 = 1 << 2;
        Modifiers {
            control: mask & MOD_CTRL != 0,
            shift: mask & MOD_SHIFT != 0,
            alt: mask & MOD_ALT != 0,
            platform: false,
            function: false,
        }
    }

    fn pressed_button_from_mask(mask: u32) -> Option<MouseButton> {
        // OH_NativeXComponent_MouseEventButton: Left=0x01, Right=0x02, Middle=0x04.
        const MOUSE_BUTTON_LEFT: u32 = 0x01;
        const MOUSE_BUTTON_RIGHT: u32 = 0x02;
        const MOUSE_BUTTON_MIDDLE: u32 = 0x04;
        if mask & MOUSE_BUTTON_LEFT != 0 {
            Some(MouseButton::Left)
        } else if mask & MOUSE_BUTTON_RIGHT != 0 {
            Some(MouseButton::Right)
        } else if mask & MOUSE_BUTTON_MIDDLE != 0 {
            Some(MouseButton::Middle)
        } else {
            None
        }
    }

    fn handle_mouse_input(&self, data: &MouseEventData) {
        let position = self.device_position(data.x, data.y);
        let modifiers = Self::modifiers_from_key_mask(data.modifiers);
        match data.action {
            MouseAction::Move => {
                let pressed_button = Self::pressed_button_from_mask(data.button_mask);
                self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button,
                    modifiers,
                }));
            }
            MouseAction::Press => {
                let button = Self::map_device_button(data.button);
                let click_count = self
                    .click_tracker
                    .borrow_mut()
                    .on_button_press(button, position);
                self.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position,
                    modifiers,
                    click_count,
                    first_mouse: false,
                }));
            }
            MouseAction::Release => {
                let button = Self::map_device_button(data.button);
                let click_count = self.click_tracker.borrow().current_count();
                self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position,
                    modifiers,
                    click_count,
                }));
                self.click_tracker
                    .borrow_mut()
                    .on_click_complete(button, position);
            }
            _ => {}
        }
    }

    fn handle_axis_input(&self, data: &AxisEventData) {
        let position = self.device_position(data.x, data.y);
        let modifiers = Self::device_modifiers_to_gpui(data.modifiers);
        let touch_phase = Self::scroll_phase_to_touch_phase(data.scroll_phase);
        // Shift+wheel maps vertical scrolling to horizontal, mirroring gpui_linux.
        let mut scroll_horizontal = data.scroll_horizontal;
        let mut scroll_vertical = data.scroll_vertical;
        if modifiers.shift {
            std::mem::swap(&mut scroll_horizontal, &mut scroll_vertical);
        }
        match data.tool_type {
            AxisToolType::Mouse => {
                // Discrete mouse wheel: convert axis units (a wheel notch is ~120) to
                // lines, scroll three lines per notch, and negate the axis sign so that
                // wheel-up scrolls content up, matching the touchscreen convention.
                const AXIS_WHEEL_UNIT: f64 = 120.0;
                const WHEEL_LINES_PER_NOTCH: f64 = 3.0;
                let delta = point(
                    -(scroll_horizontal / AXIS_WHEEL_UNIT * WHEEL_LINES_PER_NOTCH) as f32,
                    -(scroll_vertical / AXIS_WHEEL_UNIT * WHEEL_LINES_PER_NOTCH) as f32,
                );
                self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Lines(delta),
                    modifiers,
                    touch_phase,
                }));
            }
            AxisToolType::Touchpad => {
                // Two-finger trackpad scroll is a precise pixel delta. Pass the
                // axis values through at 1:1 (no amplification) and negate the
                // axis sign to match the touchscreen convention.
                const TOUCHPAD_SCROLL_SPEEDUP: f64 = 1.0;
                let delta = point(
                    px(-(scroll_horizontal * TOUCHPAD_SCROLL_SPEEDUP) as f32),
                    px(-(scroll_vertical * TOUCHPAD_SCROLL_SPEEDUP) as f32),
                );
                self.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(delta),
                    modifiers,
                    touch_phase,
                }));
            }
        }
    }

    fn dispatch_input(&self, input: PlatformInput) {
        let result = Self::dispatch_input_with_callbacks(&self.callbacks, input.clone());

        // X11-compatible fallback: if GPUI did not consume a KeyDown whose keystroke
        // carries a printable character (and no shortcut modifier is held), deliver
        // that character as text input to the active input handler.
        if result.propagate {
            if let PlatformInput::KeyDown(event) = input {
                // only allow shift modifier when inserting text
                if event.keystroke.modifiers.is_subset_of(&Modifiers::shift()) {
                    let mut handler_ref = self.input_handler.borrow_mut();
                    if let Some(mut input_handler) = handler_ref.take() {
                        drop(handler_ref);
                        if let Some(key_char) = event.keystroke.key_char {
                            input_handler.replace_text_in_range(None, &key_char);
                        }
                        *self.input_handler.borrow_mut() = Some(input_handler);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_sample(tracker: &mut TouchVelocityTracker, x: f32, y: f32, timestamp_ms: u64) {
        tracker.push(
            point(px(x), px(y)),
            Some(Duration::from_millis(timestamp_ms)),
            OhosWindow::MAX_TOUCH_SAMPLE_COUNT,
        );
    }

    #[test]
    fn velocity_tracker_uses_recent_window() {
        let mut tracker = TouchVelocityTracker::default();
        push_sample(&mut tracker, 0.0, 0.0, 0);
        push_sample(&mut tracker, 40.0, 0.0, 40);
        push_sample(&mut tracker, 120.0, 0.0, 120);
        push_sample(&mut tracker, 200.0, 0.0, 200);

        let velocity = tracker.velocity(None, Duration::from_millis(100));

        assert!((velocity.x - 1_000.0).abs() < 0.01);
        assert_eq!(velocity.y, 0.0);
    }

    #[test]
    fn velocity_tracker_respects_axis_lock() {
        let mut tracker = TouchVelocityTracker::default();
        push_sample(&mut tracker, 0.0, 0.0, 0);
        push_sample(&mut tracker, 40.0, 80.0, 40);
        push_sample(&mut tracker, 80.0, 160.0, 80);

        let velocity = tracker.velocity(Some(Axis::Vertical), Duration::from_millis(100));

        assert_eq!(velocity.x, 0.0);
        assert!((velocity.y - 2_000.0).abs() < 0.01);
    }

    #[test]
    fn friction_distance_approaches_arkui_final_position() {
        let gamma = OhosWindow::FLING_FRICTION * OhosWindow::FRICTION_SCALE;
        let velocity = point(1_000.0, 0.0);

        let distance = OhosWindow::scroll_animation_distance(velocity, gamma, 20.0);

        assert!((distance.x.as_f32() - 1_000.0 / gamma).abs() < 0.01);
        assert_eq!(distance.y, px(0.0));
    }

    #[test]
    fn touch_slop_is_shared_by_tap_and_scroll_arbitration() {
        let slop_squared = OhosWindow::TOUCH_SLOP * OhosWindow::TOUCH_SLOP;

        assert!(!OhosWindow::movement_exceeds_touch_slop(slop_squared));
        assert!(OhosWindow::movement_exceeds_touch_slop(slop_squared + 0.01));
    }

    #[test]
    fn touch_axis_prefers_dominant_direction() {
        assert!(matches!(
            OhosWindow::touch_axis(point(px(12.0), px(4.0))),
            Axis::Horizontal
        ));
        assert!(matches!(
            OhosWindow::touch_axis(point(px(4.0), px(12.0))),
            Axis::Vertical
        ));
    }
}

impl HasWindowHandle for OhosWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        self.app
            .borrow()
            .as_ref()
            .and_then(|app| app.native_window())
            .and_then(|native_window| native_window.raw_window_handle())
            .map(|raw_handle| unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_handle) })
            .ok_or(raw_window_handle::HandleError::Unavailable)
    }
}

impl HasWindowHandle for OhosWindowHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        self.inner
            .borrow()
            .app
            .borrow()
            .as_ref()
            .and_then(|app| app.native_window())
            .and_then(|native_window| native_window.raw_window_handle())
            .map(|raw_handle| unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_handle) })
            .ok_or(raw_window_handle::HandleError::Unavailable)
    }
}

impl HasDisplayHandle for OhosWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::ohos())
    }
}

impl HasDisplayHandle for OhosWindowHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::ohos())
    }
}

impl PlatformWindow for OhosWindowHandle {
    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        // GPUI calls this waker (wake_platform) when it needs another frame.
        // Always record the demand so a frame requested while hidden is not
        // lost; arm the frame callback only while the window is visible. The
        // accumulated PENDING_REDRAW is rendered once on return to foreground
        // (GainedFocus).
        let waker: Rc<dyn Fn()> = Rc::new(|| {
            PENDING_REDRAW.store(true, Ordering::Release);
            if openharmony_ability::window_visibility() {
                if let Some(app) = openharmony_ability::global_app() {
                    app.enable_frame_callback();
                }
            }
        });
        Some(waker)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.with_window(|window| window.bounds())
    }

    fn is_maximized(&self) -> bool {
        self.with_window(|window| window.is_maximized())
    }

    fn window_bounds(&self) -> WindowBounds {
        self.with_window(|window| window.window_bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.with_window(|window| window.content_size())
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let _ = self.with_window_mut(|window| window.resize(size));
    }

    fn scale_factor(&self) -> f32 {
        self.with_window(|window| window.scale_factor())
    }

    fn appearance(&self) -> WindowAppearance {
        self.with_window(|window| window.appearance())
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.with_window(|window| window.display())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.with_window(|window| window.mouse_position())
    }

    fn modifiers(&self) -> Modifiers {
        self.with_window(|window| window.modifiers())
    }

    fn capslock(&self) -> Capslock {
        self.with_window(|window| window.capslock())
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        self.with_window(|window| window.prompt(level, msg, detail, answers))
    }

    fn activate(&self) {
        self.with_window(|window| window.activate())
    }

    fn is_active(&self) -> bool {
        self.with_window(|window| window.is_active())
    }

    fn is_hovered(&self) -> bool {
        self.with_window(|window| window.is_hovered())
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.with_window(|window| window.background_appearance())
    }

    fn set_title(&mut self, title: &str) {
        let _ = self.with_window_mut(|window| window.set_title(title));
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        self.with_window(|window| window.set_background_appearance(background_appearance))
    }

    fn minimize(&self) {
        self.with_window(|window| window.minimize())
    }

    fn zoom(&self) {
        self.with_window(|window| window.zoom())
    }

    fn toggle_fullscreen(&self) {
        self.with_window(|window| window.toggle_fullscreen())
    }

    fn is_fullscreen(&self) -> bool {
        self.with_window(|window| window.is_fullscreen())
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.with_window(|window| window.on_request_frame(callback))
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>) {
        self.with_window(|window| window.on_input(callback))
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.with_window(|window| window.on_active_status_change(callback))
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.with_window(|window| window.on_hover_status_change(callback))
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.with_window(|window| window.on_resize(callback))
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_moved(callback))
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.with_window(|window| window.on_should_close(callback))
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.with_window(|window| window.on_hit_test_window_control(callback))
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.with_window(|window| window.on_close(callback))
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_appearance_changed(callback))
    }

    fn draw(&self, scene: &Scene) {
        self.with_window(|window| window.draw(scene))
    }

    fn completed_frame(&self) {
        if self.input_handler.borrow().is_none() {
            self.with_window(|window| window.hide_keyboard_if_needed());
        }
        self.with_window(|window| window.completed_frame())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.with_window(|window| window.sprite_atlas())
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.with_window(|window| window.gpu_specs())
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.with_window(|window| window.is_subpixel_rendering_supported())
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        self.with_window(|window| window.update_ime_position(bounds))
    }
}

impl PlatformWindow for OhosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        *self.bounds.borrow()
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(*self.bounds.borrow())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.effective_content_size()
    }

    fn resize(&mut self, size: Size<Pixels>) {
        *self.bounds.borrow_mut() = Bounds::new(point(px(0.0), px(0.0)), size);
    }

    fn scale_factor(&self) -> f32 {
        *self.scale.borrow()
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        if let Some(app) = self.app.borrow().clone() {
            Some(Rc::new(OhosDisplay::new(app)))
        } else {
            None
        }
    }

    fn mouse_position(&self) -> Point<Pixels> {
        point(px(0.0), px(0.0))
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        // Not supported on OHOS
    }

    fn is_active(&self) -> bool {
        true
    }

    fn is_hovered(&self) -> bool {
        // OHOS is a single-window platform: the input device only ever reports
        // events for this window, so the window is always considered hovered.
        // Returning true is what keeps GPUI's cursor-style pipeline running
        // (reset_cursor_style gates on Window::is_window_hovered).
        true
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, _title: &str) {
        // Not supported on OHOS
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {
        // Not supported on OHOS
    }

    fn minimize(&self) {
        // Not supported on OHOS
    }

    fn zoom(&self) {
        // Not supported on OHOS
    }

    fn toggle_fullscreen(&self) {
        // Not supported on OHOS
    }

    fn is_fullscreen(&self) -> bool {
        false
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.callbacks.borrow_mut().request_frame = Some(callback);
        if let Some(force_render) = self.pending_frame_request.take() {
            self.request_frame(force_render);
        }
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>) {
        self.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        // Initialize renderer lazily if not already initialized
        // This ensures native_window is available (after SurfaceCreate event)
        if self.renderer.borrow().is_none() {
            if let Err(e) = self.initialize_renderer() {
                warn!("failed to initialize OHOS renderer in draw(): {}", e);
                return;
            }
        }

        // Use WGPU renderer to render the scene.
        if let Some(ref mut renderer) = *self.renderer.borrow_mut() {
            renderer.draw(scene);
        } else {
            warn!("draw called but OHOS renderer is not available");
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        if let Some(ref renderer) = *self.renderer.borrow() {
            renderer.sprite_atlas().clone()
        } else {
            if let Err(error) = self.initialize_renderer() {
                panic!("OhosWindow: renderer must be initialized before sprite_atlas: {error}");
            }
            self.renderer
                .borrow()
                .as_ref()
                .expect("renderer should be initialized after initialize_renderer")
                .sprite_atlas()
                .clone()
        }
    }

    fn request_decorations(&self, _decorations: WindowDecorations) {
        // Not supported on OHOS
    }

    fn show_window_menu(&self, _position: Point<Pixels>) {
        // Not supported on OHOS
    }

    fn start_window_move(&self) {
        // Not supported on OHOS
    }

    fn start_window_resize(&self, _edge: ResizeEdge) {
        // Not supported on OHOS
    }

    fn window_decorations(&self) -> crate::Decorations {
        crate::Decorations::Server
    }

    fn set_app_id(&mut self, _app_id: &str) {
        // Not supported on OHOS
    }

    fn map_window(&mut self) -> Result<()> {
        Ok(())
    }

    fn window_controls(&self) -> WindowControls {
        WindowControls {
            fullscreen: false,
            maximize: false,
            minimize: false,
            window_menu: false,
        }
    }

    fn set_client_inset(&self, _inset: Pixels) {
        // Keyboard avoidance is driven by content_size updates from avoid-area overlap.
        // client_inset is intentionally ignored on OHOS.
    }

    fn insets(&self) -> WindowInsets {
        // OHOS 键盘/系统区域避让由 content_size 驱动的 resize 机制处理
        // （keyboard_overlap_device_px -> effective_content_size -> emit_resize_callback），
        // 不走 insets 机制，故返回默认（safe_area=0, ime=0）。
        WindowInsets::default()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        // Return GPU specs from the WGPU renderer.
        self.renderer
            .borrow()
            .as_ref()
            .map(|renderer| renderer.gpu_specs())
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        *self.last_ime_cursor_rect.borrow_mut() = Some(bounds);
        // A caret position exists only while the edit box holds the focus, which
        // makes this the earliest reliable moment to bind the IME. The attempt
        // fired on surface creation runs well before focus hand-off completes
        // and is rejected, so without this the keyboard stays dead until the
        // window is minimized and restored.
        self.show_keyboard_if_needed();
        self.push_ime_cursor_rect(bounds);
    }

}

impl OhosWindow {
    /// Re-sends the cached IME cursor rectangle to ArkTS after a window
    /// geometry change (resize / move / keyboard avoidance). GPUI does not
    /// re-push the cursor on those transitions, so the candidate box must be
    /// repositioned from the cache, mirroring warp-ohos `refreshCursorWithLatest`.
    fn refresh_ime_cursor(&self) {
        if let Some(bounds) = *self.last_ime_cursor_rect.borrow() {
            self.push_ime_cursor_rect(bounds);
        }
    }

    /// Converts a cursor rectangle (logical px, relative to the XComponent
    /// surface) into **window-relative** physical px and asks the ArkTS IME
    /// plugin to move the candidate box there.
    ///
    /// `content_rect` is the XComponent's own offset inside the window, reported
    /// by the system (see the `offset` read in `on_surface_created` /
    /// `on_surface_changed`). It covers the status bar / safe-area insets that
    /// sit *within* the window, but NOT the system title bar, which lives above
    /// the XComponent's content area. The title bar offset is added on the ArkTS
    /// side (`ImePlugin.titleBarHeightPx`), which is also where the window's own
    /// screen position (`windowRect`) is added.
    fn push_ime_cursor_rect(&self, bounds: Bounds<Pixels>) {
        let scale = f64::from(*self.scale.borrow());
        let (offset_left, offset_top) = self
            .app
            .borrow()
            .as_ref()
            .map(|app| {
                let rect = app.content_rect();
                (f64::from(rect.left), f64::from(rect.top))
            })
            .unwrap_or((0.0, 0.0));
        let x = f64::from(bounds.origin.x) * scale + offset_left;
        let y = f64::from(bounds.origin.y) * scale + offset_top;
        let width = f64::from(bounds.size.width) * scale;
        let height = f64::from(bounds.size.height) * scale;
        if let Some(app) = self.app.borrow().as_ref() {
            let app = app.clone();
            let executor = self.foreground_executor.clone();
            executor
                .spawn(async move {
                    if let Ok(client) = app.ime() {
                        let _ = client.update_cursor(x, y, width, height).await;
                    }
                })
                .detach();
        }
    }
}
