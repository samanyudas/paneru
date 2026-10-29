//! Window frame writes on one background thread per app.
//!
//! `AXUIElementSetAttributeValue` is a synchronous round trip to the app that
//! owns the window. On the engine thread, which also runs the input event
//! tap, one slow app held up every frame and every other app's windows.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use bevy::math::IRect;
use tracing::{debug, error, warn};

use super::windows::resize_staging_origin;
use crate::errors::{Error, Result};
use crate::manager::{Origin, Size};
use crate::platform::{Pid, WinID};

/// How long a frame read waits for the window's own queued write to land.
/// The same as `AX_MESSAGING_TIMEOUT_SEC`, so a read waits no longer than
/// the synchronous write it replaces could have.
pub(crate) const LANDING_TIMEOUT: Duration = Duration::from_millis(250);

/// A position and a size asked for one window. Requests that queue behind a
/// slow app merge, so the app is only sent the newest frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameWrite {
    pub(crate) origin: Option<Origin>,
    pub(crate) size: Option<Size>,
    /// Where the window was asked to be before this write. A staged resize
    /// without an `origin` puts the window back there.
    pub(crate) origin_before: Origin,
}

impl FrameWrite {
    fn merge(&mut self, newer: FrameWrite) {
        self.origin = newer.origin.or(self.origin);
        self.size = newer.size.or(self.size);
    }
}

/// Applies a [`FrameWrite`] to the app, on the app's writer thread.
pub(crate) trait FrameTarget: Send + Sync + 'static {
    fn write(&self, write: FrameWrite);
}

/// The AX calls a [`FrameWrite`] is made of, for one window, in AX
/// coordinates.
pub(crate) trait AxFrame {
    fn set_position(&self, origin: Origin);
    fn set_size(&self, size: Size);
    fn read_frame(&self) -> Result<IRect>;
}

/// What one window's app last answered.
pub(crate) struct Answers {
    /// The frame the app last reported, to the writer or to the engine. A
    /// staged resize compares against it, not against the size last asked
    /// for, which a fixed-size window never took.
    frame: Mutex<IRect>,
    /// Set when the app last kept a size other than the one asked for, so
    /// the next request for the same size is written again.
    size_limited: AtomicBool,
}

impl Answers {
    pub(crate) fn new(frame: IRect) -> Self {
        Self {
            frame: Mutex::new(frame),
            size_limited: AtomicBool::new(false),
        }
    }

    pub(crate) fn size_limited(&self) -> bool {
        self.size_limited.load(Ordering::Relaxed)
    }

    /// Keeps a frame the engine read from the app.
    pub(crate) fn record(&self, frame: IRect) {
        *self.frame.lock().unwrap_or_else(PoisonError::into_inner) = frame;
    }

    fn reported(&self) -> IRect {
        *self.frame.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `write` through `ax`, on the writer thread.
    pub(crate) fn land(&self, ax: &impl AxFrame, write: FrameWrite) {
        if let Some(origin) = write.origin {
            ax.set_position(origin);
        }
        let Some(size) = write.size else {
            return;
        };
        let target_origin = write.origin.unwrap_or(write.origin_before);
        ax.set_size(size);

        let mut actual = None;
        let mut previous_observed_frame = self.reported();
        let mut staged = false;
        for attempt in 1..=3 {
            let Ok(actual_frame) = ax.read_frame() else {
                break;
            };
            actual = Some(actual_frame);
            let Some(staging_origin) =
                resize_staging_origin(previous_observed_frame, actual_frame, size.x)
            else {
                break;
            };
            debug!(
                attempt,
                requested_width = size.x,
                actual_width = actual_frame.width(),
                staging_x = staging_origin.x,
                "retrying partially constrained AX resize from an offscreen origin"
            );
            staged = true;
            previous_observed_frame = actual_frame;
            ax.set_position(staging_origin);
            ax.set_size(size);
        }

        if staged {
            if let Ok(final_frame) = ax.read_frame() {
                debug!(
                    requested_width = size.x,
                    actual_width = final_frame.width(),
                    "completed staged AX resize"
                );
                actual = Some(final_frame);
            }
            ax.set_position(target_origin);
        }
        if let Some(frame) = actual {
            self.record(frame);
        }
        self.size_limited.store(
            actual.is_some_and(|frame| frame.size() != size),
            Ordering::Relaxed,
        );
    }
}

struct Pending<T> {
    window_id: WinID,
    target: T,
    write: FrameWrite,
}

struct Queue<T> {
    /// At most one entry per window, in the order each was first queued.
    pending: Vec<Pending<T>>,
    in_flight: Option<WinID>,
    closed: bool,
}

impl<T> Queue<T> {
    fn has_window(&self, window_id: WinID) -> bool {
        self.in_flight == Some(window_id) || self.pending.iter().any(|p| p.window_id == window_id)
    }

    fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.pending.is_empty()
    }
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    /// Signals both new work for the thread and finished work for waiters.
    changed: Condvar,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, Queue<T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_until(&self, deadline: Instant, done: impl Fn(&Queue<T>) -> bool) -> bool {
        let mut queue = self.lock();
        while !done(&queue) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            queue = self
                .changed
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// One app's writer thread. Dropping it lets the thread land what is
/// already queued and exit.
pub(crate) struct AppWriter<T: FrameTarget> {
    shared: Arc<Shared<T>>,
    /// Set when the thread could not start, so writes run on the caller.
    inline: bool,
}

impl<T: FrameTarget> AppWriter<T> {
    pub(crate) fn spawn(pid: Pid) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                pending: Vec::new(),
                in_flight: None,
                closed: false,
            }),
            changed: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let inline = thread::Builder::new()
            .name(format!("paneru-ax-writer-{pid}"))
            .spawn(move || write_until_closed(&worker))
            .inspect_err(|err| warn!(pid, "writing window frames inline: {err}"))
            .is_err();
        Self { shared, inline }
    }

    /// Queues a write and returns without waiting for the app.
    pub(crate) fn push(&self, window_id: WinID, target: T, write: FrameWrite) {
        if self.inline {
            target.write(write);
            return;
        }
        let mut queue = self.shared.lock();
        if let Some(pending) = queue.pending.iter_mut().find(|p| p.window_id == window_id) {
            pending.target = target;
            pending.write.merge(write);
        } else {
            queue.pending.push(Pending {
                window_id,
                target,
                write,
            });
        }
        drop(queue);
        self.shared.changed.notify_all();
    }

    /// Whether a write to `window_id` has yet to land.
    pub(crate) fn is_pending(&self, window_id: WinID) -> bool {
        self.shared.lock().has_window(window_id)
    }

    /// Waits up to `timeout` for every write to `window_id` to land.
    pub(crate) fn wait_for_window(&self, window_id: WinID, timeout: Duration) -> bool {
        self.shared.wait_until(Instant::now() + timeout, |queue| {
            !queue.has_window(window_id)
        })
    }

    /// Waits until `deadline` for every queued write to land.
    pub(crate) fn wait_for_all(&self, deadline: Instant) -> bool {
        self.shared.wait_until(deadline, Queue::is_idle)
    }
}

impl<T: FrameTarget> Drop for AppWriter<T> {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
    }
}

fn write_until_closed<T: FrameTarget>(shared: &Shared<T>) {
    let mut queue = shared.lock();
    loop {
        if queue.pending.is_empty() {
            if queue.closed {
                return;
            }
            queue = shared
                .changed
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        }
        let next = queue.pending.remove(0);
        queue.in_flight = Some(next.window_id);
        drop(queue);
        // A plain thread has no run loop to drain autoreleased AX objects. A
        // panic must not end the thread, or the window stays pending forever.
        let landed = panic::catch_unwind(AssertUnwindSafe(|| {
            objc2::rc::autoreleasepool(|_| next.target.write(next.write));
        }));
        if landed.is_err() {
            error!(window_id = next.window_id, "window write panicked");
        }
        drop(next);
        queue = shared.lock();
        queue.in_flight = None;
        shared.changed.notify_all();
    }
}

/// One window's writes, queued on its app's writer thread, or written on the
/// caller when the window has none.
pub(crate) struct WindowWrites<T: FrameTarget + Clone> {
    window_id: WinID,
    target: T,
    writer: Option<Arc<AppWriter<T>>>,
}

impl<T: FrameTarget + Clone> WindowWrites<T> {
    pub(crate) fn new(window_id: WinID, target: T, writer: Option<Arc<AppWriter<T>>>) -> Self {
        Self {
            window_id,
            target,
            writer,
        }
    }

    pub(crate) fn target(&self) -> &T {
        &self.target
    }

    /// Queues the write, or writes it at once when there is no writer.
    pub(crate) fn push(&self, write: FrameWrite) {
        match &self.writer {
            Some(writer) => writer.push(self.window_id, self.target.clone(), write),
            None => self.target.write(write),
        }
    }

    /// Whether a write to the window has yet to land.
    pub(crate) fn pending(&self) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|writer| writer.is_pending(self.window_id))
    }

    /// Waits up to [`LANDING_TIMEOUT`] for the window's writes to land.
    pub(crate) fn wait_for_landing(&self) -> Result<()> {
        match &self.writer {
            Some(writer) if !writer.wait_for_window(self.window_id, LANDING_TIMEOUT) => {
                Err(Error::WriteNotLanded(self.window_id))
            }
            _ => Ok(()),
        }
    }
}

impl<T: FrameTarget + Clone> std::fmt::Debug for WindowWrites<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowWrites")
            .field("window_id", &self.window_id)
            .field("threaded", &self.writer.is_some())
            .finish_non_exhaustive()
    }
}

/// Hands out one [`AppWriter`] per app, for as long as a window holds it.
pub(crate) struct WindowWriters<T: FrameTarget> {
    apps: Mutex<HashMap<Pid, Weak<AppWriter<T>>>>,
}

impl<T: FrameTarget> Default for WindowWriters<T> {
    fn default() -> Self {
        Self {
            apps: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: FrameTarget> WindowWriters<T> {
    pub(crate) fn for_app(&self, pid: Pid) -> Arc<AppWriter<T>> {
        let mut apps = self.apps.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(writer) = apps.get(&pid).and_then(Weak::upgrade) {
            return writer;
        }
        apps.retain(|_, writer| writer.strong_count() > 0);
        let writer = Arc::new(AppWriter::spawn(pid));
        apps.insert(pid, Arc::downgrade(&writer));
        writer
    }

    /// Waits up to `timeout` for every app's queued writes to land, and
    /// returns whether they all did.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let writers: Vec<_> = self
            .apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        writers.iter().all(|writer| writer.wait_for_all(deadline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::IVec2;

    /// Records each write, after waiting while the gate is held.
    #[derive(Clone)]
    struct Recorder {
        window_id: WinID,
        log: Arc<Mutex<Vec<(WinID, FrameWrite)>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl FrameTarget for Recorder {
        fn write(&self, write: FrameWrite) {
            let (held, released) = &*self.gate;
            let held = held.lock().unwrap();
            let _held = released
                .wait_timeout_while(held, Duration::from_secs(2), |held| *held)
                .unwrap();
            self.log.lock().unwrap().push((self.window_id, write));
        }
    }

    struct App {
        writer: AppWriter<Recorder>,
        log: Arc<Mutex<Vec<(WinID, FrameWrite)>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl App {
        fn new() -> Self {
            Self {
                writer: AppWriter::spawn(1),
                log: Arc::default(),
                gate: Arc::default(),
            }
        }

        fn push(&self, window_id: WinID, write: FrameWrite) {
            let target = Recorder {
                window_id,
                log: Arc::clone(&self.log),
                gate: Arc::clone(&self.gate),
            };
            self.writer.push(window_id, target, write);
        }

        fn hold(&self) {
            *self.gate.0.lock().unwrap() = true;
        }

        fn release(&self) {
            *self.gate.0.lock().unwrap() = false;
            self.gate.1.notify_all();
        }

        /// The thread takes a write without signalling anyone, so poll.
        fn wait_in_flight(&self, window_id: WinID) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.writer.shared.lock().in_flight != Some(window_id) {
                assert!(
                    Instant::now() < deadline,
                    "window {window_id} never went in flight"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn log(&self) -> Vec<(WinID, FrameWrite)> {
            self.log.lock().unwrap().clone()
        }
    }

    fn origin(x: i32) -> FrameWrite {
        FrameWrite {
            origin: Some(IVec2::new(x, 0)),
            size: None,
            origin_before: IVec2::new(x - 1, 0),
        }
    }

    fn size(width: i32) -> FrameWrite {
        FrameWrite {
            origin: None,
            size: Some(IVec2::new(width, 100)),
            origin_before: IVec2::new(width - 1, 0),
        }
    }

    #[test]
    fn a_push_returns_before_a_stuck_app_takes_it() {
        let app = App::new();
        app.hold();
        app.push(1, origin(10));
        app.wait_in_flight(1);
        assert_eq!(app.log(), Vec::new());

        app.release();
        assert!(app.writer.wait_for_window(1, Duration::from_secs(2)));
        assert_eq!(app.log(), vec![(1, origin(10))]);
    }

    #[test]
    fn writes_queued_behind_a_stuck_app_merge_into_the_newest() {
        let app = App::new();
        app.hold();
        app.push(1, origin(10));
        app.wait_in_flight(1);
        app.push(1, origin(20));
        app.push(2, origin(5));
        app.push(1, size(300));
        app.push(1, origin(30));

        app.release();
        assert!(app.writer.wait_for_window(1, Duration::from_secs(2)));
        assert!(app.writer.wait_for_window(2, Duration::from_secs(2)));
        let merged = FrameWrite {
            origin: Some(IVec2::new(30, 0)),
            size: Some(IVec2::new(300, 100)),
            origin_before: origin(20).origin_before,
        };
        assert_eq!(
            app.log(),
            vec![(1, origin(10)), (1, merged), (2, origin(5))],
            "one write in flight, then each window's newest frame in the order first queued"
        );
    }

    #[test]
    fn waiting_on_a_stuck_window_gives_up_at_the_timeout() {
        let app = App::new();
        app.hold();
        app.push(1, origin(10));
        assert!(!app.writer.wait_for_window(1, Duration::from_millis(20)));
        assert!(app.writer.wait_for_window(2, Duration::ZERO));
        app.release();
    }

    #[test]
    fn a_stuck_app_does_not_delay_another_app() {
        let stuck = App::new();
        let other = App::new();
        stuck.hold();
        stuck.push(1, origin(10));
        other.push(2, origin(20));

        assert!(other.writer.wait_for_window(2, Duration::from_secs(2)));
        assert_eq!(other.log(), vec![(2, origin(20))]);
        assert_eq!(stuck.log(), Vec::new());
        stuck.release();
    }

    #[test]
    fn a_dropped_writer_lands_its_queue_then_exits() {
        let app = App::new();
        app.hold();
        app.push(1, origin(10));
        app.push(2, origin(20));
        let shared = Arc::clone(&app.writer.shared);
        let App { writer, log, gate } = app;
        drop(writer);
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();

        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&shared) > 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(Arc::strong_count(&shared), 1, "writer thread has exited");
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[derive(Clone)]
    struct Panicking {
        log: Arc<Mutex<Vec<FrameWrite>>>,
    }

    impl FrameTarget for Panicking {
        fn write(&self, write: FrameWrite) {
            assert_ne!(write, origin(13), "deliberate panic in a test write");
            self.log.lock().unwrap().push(write);
        }
    }

    #[test]
    fn a_write_that_panics_does_not_stop_later_writes() {
        let writer = AppWriter::spawn(1);
        let target = Panicking {
            log: Arc::default(),
        };
        writer.push(1, target.clone(), origin(13));
        assert!(
            writer.wait_for_window(1, Duration::from_secs(2)),
            "the write that panicked still counts as pending"
        );
        writer.push(1, target.clone(), origin(20));
        assert!(writer.wait_for_window(1, Duration::from_secs(2)));
        assert_eq!(*target.log.lock().unwrap(), vec![origin(20)]);
    }

    /// An app window that takes any width up to `max_width`.
    struct AppWindow {
        frame: Mutex<IRect>,
        max_width: i32,
        positions: Mutex<Vec<Origin>>,
    }

    impl AppWindow {
        fn new(width: i32, max_width: i32) -> Self {
            Self {
                frame: Mutex::new(IRect::new(100, 0, 100 + width, 100)),
                max_width,
                positions: Mutex::default(),
            }
        }

        fn frame(&self) -> IRect {
            *self.frame.lock().unwrap()
        }

        fn moves(&self) -> usize {
            self.positions.lock().unwrap().len()
        }
    }

    impl AxFrame for AppWindow {
        fn set_position(&self, origin: Origin) {
            self.positions.lock().unwrap().push(origin);
            let mut frame = self.frame.lock().unwrap();
            *frame = IRect::from_corners(origin, origin + frame.size());
        }

        fn set_size(&self, size: Size) {
            let mut frame = self.frame.lock().unwrap();
            frame.max = frame.min + size.with_x(size.x.min(self.max_width));
        }

        fn read_frame(&self) -> Result<IRect> {
            Ok(self.frame())
        }
    }

    fn width(width: i32) -> FrameWrite {
        FrameWrite {
            origin: None,
            size: Some(IVec2::new(width, 100)),
            origin_before: IVec2::new(100, 0),
        }
    }

    #[test]
    fn a_resize_is_staged_only_when_it_grew_the_window() {
        let window = AppWindow::new(400, 450);
        let answers = Answers::new(window.frame());
        answers.land(&window, width(470));
        assert!(
            window.moves() > 0,
            "a resize that grew the window but fell short is staged"
        );
        assert_eq!(window.frame(), IRect::new(100, 0, 550, 100));
        assert!(answers.size_limited());

        let moves = window.moves();
        answers.land(&window, width(490));
        assert_eq!(
            window.moves(),
            moves,
            "the writer's own read-back showed the window at its widest"
        );

        let window = AppWindow::new(400, 450);
        let answers = Answers::new(window.frame());
        window.set_size(Size::new(450, 100));
        answers.record(window.frame());
        answers.land(&window, width(490));
        assert_eq!(
            window.moves(),
            0,
            "the engine's read showed the window at its widest"
        );
    }

    #[test]
    fn flush_waits_for_every_app_and_reports_a_stuck_one() {
        let writers = WindowWriters::<Recorder>::default();
        let log = Arc::default();
        let gate: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let recorder = |window_id| Recorder {
            window_id,
            log: Arc::clone(&log),
            gate: Arc::clone(&gate),
        };
        let first = writers.for_app(1);
        let second = writers.for_app(2);
        assert!(Arc::ptr_eq(&first, &writers.for_app(1)));

        first.push(1, recorder(1), origin(10));
        second.push(2, recorder(2), origin(20));
        assert!(writers.flush(Duration::from_secs(2)));
        assert_eq!(log.lock().unwrap().len(), 2);

        *gate.0.lock().unwrap() = true;
        first.push(1, recorder(1), origin(30));
        assert!(!writers.flush(Duration::from_millis(20)));
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();
        assert!(writers.flush(Duration::from_secs(2)));
    }
}
