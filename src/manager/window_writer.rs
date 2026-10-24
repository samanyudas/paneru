//! Moves and resizes windows off the engine frame.
//!
//! An accessibility write is a synchronous round-trip served on the main
//! thread of the window's app, and a busy app holds it for hundreds of
//! milliseconds. Writing inline made every frame wait for the slowest app it
//! touched, freezing the whole strip mid-swipe. Instead the engine asks for a
//! frame through [`FrameWrites`], which records it at once, and
//! [`WindowWriters`] carries the write to the app on a thread for that app.
//!
//! A writer keeps only the newest frame asked for each window. Frames asked
//! for while an earlier write is still in flight replace each other, so a
//! slow app skips positions it could not keep up with instead of replaying
//! them late.

use bevy::math::IRect;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use crate::errors::{Error, Result};
use crate::manager::{Origin, Size};
use crate::platform::Pid;

/// How long reading a window's frame waits for the engine's own write to it
/// to land. Reading before then would see the frame the window is leaving.
pub(crate) const LANDING_TIMEOUT: Duration = Duration::from_millis(250);

/// The calls one app's writer makes, kept apart from the queueing so tests
/// can stand in a fake app.
pub(crate) trait WriteSink: Send + 'static {
    /// What the sink needs to address one window.
    type Window: Send + 'static;
    /// Runs before each batch of writes.
    fn begin(&mut self);
    fn set_position(&mut self, window: &Self::Window, origin: Origin);
    fn set_size(&mut self, window: &Self::Window, size: Size);
    fn read_frame(&mut self, window: &Self::Window) -> Option<IRect>;
    /// Runs after each batch of writes.
    fn finish(&mut self);
}

/// Names the writes of one window handle. A window whose app replaced its AX
/// element gets a new handle and so a new key, and never waits on writes
/// still addressed to the old element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WriteKey(u64);

impl WriteKey {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The newest frame asked for one window, and which parts of it to write.
pub(crate) struct PendingWrite<W> {
    pub window: W,
    /// The frame before the oldest request not yet written. Resize staging
    /// compares the app's partial result against it.
    pub from: IRect,
    pub to: IRect,
    pub position: bool,
    pub size: bool,
    /// The size the app last took, if smaller than the engine asked for.
    /// The engine only writes a size when it changes, so the writer asks for
    /// it again after each later move, as the app may allow it at the new
    /// position. The window's handle shares this, so it goes away with the
    /// window.
    pub size_limited: Arc<Mutex<Option<Size>>>,
}

impl<W> PendingWrite<W> {
    /// Takes on a newer request for the same window, keeping the frame the
    /// window had before the older one.
    fn merge(&mut self, newer: Self) {
        self.window = newer.window;
        self.to = newer.to;
        self.position |= newer.position;
        self.size |= newer.size;
    }
}

/// Writes one window's frame. A write of both parts follows the order the
/// engine has always used: the size at the old position, then the position,
/// then the size again if the app limited it at the old position.
/// Returns the frame read back after a size was written, if any read
/// succeeded.
pub(crate) fn apply<S: WriteSink>(sink: &mut S, write: &PendingWrite<S::Window>) -> Option<IRect> {
    let window = &write.window;
    let size = write.to.size();
    match (write.position, write.size) {
        (true, true) => {
            let observed = resize(sink, window, write.from, size, write.from.min);
            sink.set_position(window, write.to.min);
            match observed {
                Some(observed) if observed.size() != size => {
                    let moved = IRect::from_corners(write.to.min, write.to.min + observed.size());
                    resize(sink, window, moved, size, write.to.min)
                }
                observed => observed
                    .map(|frame| IRect::from_corners(write.to.min, write.to.min + frame.size())),
            }
        }
        (true, false) => {
            sink.set_position(window, write.to.min);
            None
        }
        (false, true) => resize(sink, window, write.from, size, write.to.min),
        (false, false) => None,
    }
}

/// Sets `size`, and retries from an offscreen origin while the app only
/// partly applies a wider size, then puts the window back at `origin`.
/// Returns the last frame read back, if any read succeeded.
fn resize<S: WriteSink>(
    sink: &mut S,
    window: &S::Window,
    from: IRect,
    size: Size,
    origin: Origin,
) -> Option<IRect> {
    sink.set_size(window, size);

    let mut observed = from;
    let mut last = None;
    let mut staged = false;
    for attempt in 1..=3 {
        let Some(actual) = sink.read_frame(window) else {
            break;
        };
        last = Some(actual);
        let Some(staging_origin) = resize_staging_origin(observed, actual, size.x) else {
            break;
        };
        debug!(
            attempt,
            requested_width = size.x,
            actual_width = actual.width(),
            staging_x = staging_origin.x,
            "retrying partially constrained AX resize from an offscreen origin"
        );
        staged = true;
        observed = actual;
        sink.set_position(window, staging_origin);
        sink.set_size(window, size);
    }

    if staged {
        if let Some(settled) = sink.read_frame(window) {
            debug!(
                requested_width = size.x,
                actual_width = settled.width(),
                "completed staged AX resize"
            );
            last = Some(settled);
        }
        sink.set_position(window, origin);
        last = last.map(|frame| IRect::from_corners(origin, origin + frame.size()));
    }
    last
}

/// macOS may partially apply an AX width increase when the requested right edge
/// would be far outside the display. Moving the partial result left by the
/// missing width before retrying gives `WindowServer` enough offscreen room.
///
/// Only retry when the first attempt actually grew the window. Fixed-size apps
/// otherwise look identical to this failure mode and must not be moved offscreen.
fn resize_staging_origin(
    previous_frame: IRect,
    actual_frame: IRect,
    target_width: i32,
) -> Option<Origin> {
    let actual_width = actual_frame.width();
    (actual_width > previous_frame.width() && actual_width < target_width).then(|| {
        actual_frame
            .min
            .with_x(actual_frame.min.x - (target_width - actual_width))
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One writer thread per app, each started on its first write and stopped
/// once it has had nothing to do for `idle`. Writes to one app queue on its
/// main thread anyway, and apps must not wait on each other.
pub(crate) struct WindowWriters<S: WriteSink> {
    shared: Arc<Shared<S>>,
}

struct Shared<S: WriteSink> {
    /// Lock order: this map, then a mailbox's queue. A push holds the map
    /// while it queues, and an idle writer holds it while it checks its
    /// queue is still empty and leaves, so a push never lands in a writer
    /// that has already gone.
    writers: Mutex<HashMap<Pid, Arc<Mailbox<S::Window>>>>,
    open_sink: Box<dyn Fn(Option<Pid>) -> S + Send + Sync>,
    idle: Duration,
}

struct Mailbox<W> {
    queue: Mutex<Queue<W>>,
    /// Signalled when a write is queued.
    work: Condvar,
    /// Signalled when a batch has been written.
    landed: Condvar,
}

struct Queue<W> {
    pending: HashMap<WriteKey, PendingWrite<W>>,
    in_flight: HashSet<WriteKey>,
}

impl<W> Queue<W> {
    fn holds(&self, key: WriteKey) -> bool {
        self.pending.contains_key(&key) || self.in_flight.contains(&key)
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_empty()
    }
}

impl<W> Mailbox<W> {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Queue {
                pending: HashMap::new(),
                in_flight: HashSet::new(),
            }),
            work: Condvar::new(),
            landed: Condvar::new(),
        }
    }

    /// Takes every queued write, or `None` once nothing has arrived for
    /// `idle`.
    fn next_batch(&self, idle: Duration) -> Option<HashMap<WriteKey, PendingWrite<W>>> {
        let mut queue = lock(&self.queue);
        while queue.pending.is_empty() {
            let (guard, wait) = self
                .work
                .wait_timeout(queue, idle)
                .unwrap_or_else(PoisonError::into_inner);
            queue = guard;
            if wait.timed_out() && queue.pending.is_empty() {
                return None;
            }
        }
        let batch = std::mem::take(&mut queue.pending);
        queue.in_flight.extend(batch.keys().copied());
        Some(batch)
    }

    fn land(&self) {
        lock(&self.queue).in_flight.clear();
        self.landed.notify_all();
    }

    fn wait_until(&self, deadline: Instant, done: impl Fn(&Queue<W>) -> bool) -> bool {
        let mut queue = lock(&self.queue);
        while !done(&queue) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = self
                .landed
                .wait_timeout(queue, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

impl<S: WriteSink> Shared<S> {
    /// Writes one window's frame through `sink`, asking again for a size
    /// the app limited before.
    fn write(sink: &mut S, write: PendingWrite<S::Window>) {
        let limited = *lock(&write.size_limited);
        let observed = match limited {
            // The app limited the size at the old position, so ask again
            // only at the new one.
            Some(taken) if !write.size => {
                sink.set_position(&write.window, write.to.min);
                let moved = IRect::from_corners(write.to.min, write.to.min + taken);
                resize(sink, &write.window, moved, write.to.size(), write.to.min)
            }
            _ => apply(sink, &write),
        };
        if write.size || limited.is_some() {
            *lock(&write.size_limited) = observed
                .map(|frame| frame.size())
                .filter(|&taken| taken != write.to.size());
        }
    }

    fn mailbox(&self, pid: Pid) -> Option<Arc<Mailbox<S::Window>>> {
        lock(&self.writers).get(&pid).cloned()
    }

    /// Removes an idle writer, unless a write arrived since it gave up
    /// waiting.
    fn retire(&self, pid: Pid, mailbox: &Arc<Mailbox<S::Window>>) -> bool {
        let mut writers = lock(&self.writers);
        if !lock(&mailbox.queue).pending.is_empty() {
            return false;
        }
        if writers
            .get(&pid)
            .is_some_and(|current| Arc::ptr_eq(current, mailbox))
        {
            writers.remove(&pid);
        }
        true
    }

    fn run(&self, pid: Pid, mailbox: &Arc<Mailbox<S::Window>>) {
        let mut sink = (self.open_sink)(Some(pid));
        loop {
            let Some(batch) = mailbox.next_batch(self.idle) else {
                if self.retire(pid, mailbox) {
                    debug!("window writer for pid {pid} exits after idling");
                    return;
                }
                continue;
            };
            objc2::rc::autoreleasepool(|_| {
                sink.begin();
                for write in batch.into_values() {
                    Self::write(&mut sink, write);
                }
                sink.finish();
            });
            mailbox.land();
        }
    }
}

impl<S: WriteSink> WindowWriters<S> {
    pub fn new(
        idle: Duration,
        open_sink: impl Fn(Option<Pid>) -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                writers: Mutex::new(HashMap::new()),
                open_sink: Box::new(open_sink),
                idle,
            }),
        }
    }

    /// Queues `write` for `pid`'s writer, merged into whatever was still
    /// queued for window `key`. Never waits on the app.
    pub fn push(&self, pid: Pid, key: WriteKey, write: PendingWrite<S::Window>) {
        let mut writers = lock(&self.shared.writers);
        let mailbox = if let Some(mailbox) = writers.get(&pid) {
            mailbox.clone()
        } else {
            let mailbox = Arc::new(Mailbox::new());
            let shared = self.shared.clone();
            let thread_mailbox = mailbox.clone();
            let spawned = thread::Builder::new()
                .name(format!("window-writer-{pid}"))
                .spawn(move || shared.run(pid, &thread_mailbox));
            if let Err(err) = spawned {
                drop(writers);
                warn!("cannot start a window writer for pid {pid}, writing inline: {err}");
                self.write_inline(Some(pid), write);
                return;
            }
            writers.insert(pid, mailbox.clone());
            mailbox
        };
        let mut queue = lock(&mailbox.queue);
        match queue.pending.entry(key) {
            Entry::Occupied(mut queued) => queued.get_mut().merge(write),
            Entry::Vacant(slot) => {
                slot.insert(write);
            }
        }
        drop(queue);
        drop(writers);
        mailbox.work.notify_one();
    }

    /// Writes `write` on the calling thread.
    pub fn write_inline(&self, pid: Option<Pid>, write: PendingWrite<S::Window>) {
        let mut sink = (self.shared.open_sink)(pid);
        objc2::rc::autoreleasepool(|_| {
            sink.begin();
            Shared::write(&mut sink, write);
            sink.finish();
        });
    }

    /// Whether a write to window `key` is queued or in flight.
    pub fn is_pending(&self, pid: Pid, key: WriteKey) -> bool {
        self.shared
            .mailbox(pid)
            .is_some_and(|mailbox| lock(&mailbox.queue).holds(key))
    }

    /// Waits up to `timeout` until no write to window `key` is queued or in
    /// flight. Returns whether that happened.
    pub fn wait_landed(&self, pid: Pid, key: WriteKey, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        self.shared
            .mailbox(pid)
            .is_none_or(|mailbox| mailbox.wait_until(deadline, |queue| !queue.holds(key)))
    }

    /// Waits up to `timeout` until every writer has written everything queued
    /// so far. Returns whether that happened.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mailboxes = lock(&self.shared.writers)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        mailboxes
            .iter()
            .all(|mailbox| mailbox.wait_until(deadline, Queue::is_empty))
    }

    /// How many writer threads are running.
    #[cfg(test)]
    pub fn writers(&self) -> usize {
        lock(&self.shared.writers).len()
    }
}

/// The engine's side of one window's writes: the frame it last asked for,
/// and the writers that carry it to the app.
pub(crate) struct FrameWrites<S: WriteSink> {
    writers: Arc<WindowWriters<S>>,
    key: WriteKey,
    frame: IRect,
    size_limited: Arc<Mutex<Option<Size>>>,
}

impl<S: WriteSink> fmt::Debug for FrameWrites<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameWrites")
            .field("key", &self.key)
            .field("frame", &self.frame)
            .finish_non_exhaustive()
    }
}

impl<S: WriteSink> FrameWrites<S> {
    pub fn new(writers: Arc<WindowWriters<S>>) -> Self {
        Self {
            writers,
            key: WriteKey::next(),
            frame: IRect::default(),
            size_limited: Arc::default(),
        }
    }

    /// Starts from `frame` rather than from the first read.
    #[cfg(test)]
    pub fn starting_at(mut self, frame: IRect) -> Self {
        self.frame = frame;
        self
    }

    /// The frame last asked for or read back.
    pub fn frame(&self) -> IRect {
        self.frame
    }

    pub fn reposition(
        &mut self,
        pid: Option<Pid>,
        window: impl FnOnce() -> S::Window,
        origin: Origin,
    ) {
        if self.frame.min == origin {
            trace!("already in position.");
            return;
        }
        let to = IRect::from_corners(origin, origin + self.frame.size());
        self.request(pid, window(), to, true, false);
    }

    pub fn resize(&mut self, pid: Option<Pid>, window: impl FnOnce() -> S::Window, size: Size) {
        if self.frame.size() == size {
            trace!("already correct size.");
            return;
        }
        let to = IRect::from_corners(self.frame.min, self.frame.min + size);
        self.request(pid, window(), to, false, true);
    }

    /// Moves and resizes in one write, in the order [`apply`] describes.
    pub fn place(&mut self, pid: Option<Pid>, window: impl FnOnce() -> S::Window, frame: IRect) {
        let position = self.frame.min != frame.min;
        let size = self.frame.size() != frame.size();
        if !position && !size {
            trace!("already in place.");
            return;
        }
        self.request(pid, window(), frame, position, size);
    }

    fn request(
        &mut self,
        pid: Option<Pid>,
        window: S::Window,
        to: IRect,
        position: bool,
        size: bool,
    ) {
        let write = PendingWrite {
            window,
            from: self.frame,
            to,
            position,
            size,
            size_limited: self.size_limited.clone(),
        };
        self.frame = to;
        match pid {
            Some(pid) => self.writers.push(pid, self.key, write),
            // No app to queue behind: write it here.
            None => self.writers.write_inline(None, write),
        }
    }

    /// Whether a move or resize the engine asked for has yet to reach the
    /// window.
    pub fn write_pending(&self, pid: Option<Pid>) -> bool {
        pid.is_some_and(|pid| self.writers.is_pending(pid, self.key))
    }

    /// Reads the window's frame with `read`, after waiting up to
    /// [`LANDING_TIMEOUT`] for the engine's own writes to it to land.
    ///
    /// # Errors
    ///
    /// Fails if `read` fails, or if the writes did not land in time.
    pub fn update_frame(
        &mut self,
        pid: Option<Pid>,
        read: impl FnOnce() -> Result<IRect>,
    ) -> Result<IRect> {
        if let Some(pid) = pid
            && !self.writers.wait_landed(pid, self.key, LANDING_TIMEOUT)
        {
            return Err(Error::Generic(
                "the window's app has not applied the last move yet".to_string(),
            ));
        }
        self.frame = read()?;
        Ok(self.frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Op {
        Position(Origin),
        Size(Size),
    }

    /// A window whose app limits each size it is given, and records each
    /// write.
    struct Constrained<F> {
        frame: IRect,
        constrain: F,
        ops: Vec<Op>,
    }

    impl<F: Fn(IRect, Size) -> Size + Send + 'static> WriteSink for Constrained<F> {
        type Window = ();

        fn begin(&mut self) {}

        fn set_position(&mut self, (): &(), origin: Origin) {
            self.ops.push(Op::Position(origin));
            self.frame = IRect::from_corners(origin, origin + self.frame.size());
        }

        fn set_size(&mut self, (): &(), size: Size) {
            self.ops.push(Op::Size(size));
            let size = (self.constrain)(self.frame, size);
            self.frame.max = self.frame.min + size;
        }

        fn read_frame(&mut self, (): &()) -> Option<IRect> {
            Some(self.frame)
        }

        fn finish(&mut self) {}
    }

    fn place(
        from: IRect,
        to: IRect,
        constrain: impl Fn(IRect, Size) -> Size + Send + 'static,
    ) -> (IRect, Vec<Op>) {
        let mut app = Constrained {
            frame: from,
            constrain,
            ops: Vec::new(),
        };
        apply(
            &mut app,
            &PendingWrite {
                window: (),
                from,
                to,
                position: true,
                size: true,
                size_limited: Arc::default(),
            },
        );
        (app.frame, app.ops)
    }

    /// An app that limits a height by where the window sits gets the size
    /// again once the window has moved.
    #[test]
    fn a_combined_write_sets_the_size_again_after_the_move_when_the_app_limited_it() {
        let target = IRect::new(640, 30, 1280, 1440);
        let (frame, ops) = place(
            IRect::new(640, 735, 1280, 1440),
            target,
            |frame, mut size| {
                size.y = size.y.min(1440 - frame.min.y);
                size
            },
        );
        assert_eq!(frame, target);
        assert_eq!(
            ops,
            vec![
                Op::Size(target.size()),
                Op::Position(target.min),
                Op::Size(target.size()),
            ]
        );
    }

    /// An app that takes the size at once is not asked a second time.
    #[test]
    fn a_combined_write_sets_the_size_before_the_position() {
        let target = IRect::new(100, 50, 600, 450);
        let (frame, ops) = place(IRect::new(0, 0, 400, 400), target, |_, size| size);
        assert_eq!(frame, target);
        assert_eq!(ops, vec![Op::Size(target.size()), Op::Position(target.min)]);
    }

    #[test]
    fn a_combined_write_shrinks_a_full_height_window_into_a_lower_slot() {
        let target = IRect::new(640, 735, 1280, 1440);
        let (frame, _) = place(
            IRect::new(640, 30, 1280, 1440),
            target,
            |frame, mut size| {
                if frame.max.y > 1440 && size.y < frame.height() {
                    size.y = size.y.max(775);
                }
                size
            },
        );
        assert_eq!(frame, target);
    }

    #[test]
    fn a_combined_write_shrinks_height_and_grows_width_across_opposing_limits() {
        let target = IRect::new(1280, 735, 2560, 1440);
        let (frame, _) = place(
            IRect::new(1920, 30, 2560, 1440),
            target,
            |frame, mut size| {
                size.x = size.x.min(2560 - frame.min.x);
                if frame.max.y > 1440 && size.y < frame.height() {
                    size.y = size.y.max(775);
                }
                size
            },
        );
        assert_eq!(frame, target);
    }

    /// A window that cannot take a wider size in one step is staged
    /// offscreen until it does, then put back where it was.
    #[test]
    fn a_resize_the_app_only_partly_applies_is_staged_offscreen() {
        struct Partial {
            reads: Mutex<Vec<IRect>>,
            ops: Vec<Op>,
        }
        impl WriteSink for Partial {
            type Window = ();
            fn begin(&mut self) {}
            fn set_position(&mut self, (): &(), origin: Origin) {
                self.ops.push(Op::Position(origin));
            }
            fn set_size(&mut self, (): &(), size: Size) {
                self.ops.push(Op::Size(size));
            }
            fn read_frame(&mut self, (): &()) -> Option<IRect> {
                let mut reads = self.reads.lock().unwrap();
                (!reads.is_empty()).then(|| reads.remove(0))
            }
            fn finish(&mut self) {}
        }

        let mut app = Partial {
            reads: Mutex::new(vec![
                IRect::new(-400, 40, 2416, 640),
                IRect::new(-2056, 40, 2016, 640),
                IRect::new(-2096, 40, 2016, 640),
            ]),
            ops: Vec::new(),
        };
        apply(
            &mut app,
            &PendingWrite {
                window: (),
                from: IRect::new(-400, 40, 400, 640),
                to: IRect::new(-400, 40, 3712, 640),
                position: false,
                size: true,
                size_limited: Arc::default(),
            },
        );

        let size = Size::new(4112, 600);
        assert_eq!(
            app.ops,
            vec![
                Op::Size(size),
                Op::Position(Origin::new(-1696, 40)),
                Op::Size(size),
                Op::Position(Origin::new(-2096, 40)),
                Op::Size(size),
                Op::Position(Origin::new(-400, 40)),
            ]
        );
    }

    /// An app whose limits a later write can lift: the height it allows
    /// depends on how far the window sits above the display's bottom edge.
    struct Shelf(Arc<Mutex<(IRect, Vec<Op>)>>);

    impl WriteSink for Shelf {
        type Window = ();
        fn begin(&mut self) {}
        fn set_position(&mut self, (): &(), origin: Origin) {
            let mut app = self.0.lock().unwrap();
            app.1.push(Op::Position(origin));
            app.0 = IRect::from_corners(origin, origin + app.0.size());
        }
        fn set_size(&mut self, (): &(), mut size: Size) {
            let mut app = self.0.lock().unwrap();
            app.1.push(Op::Size(size));
            size.y = size.y.min(1440 - app.0.min.y);
            app.0.max = app.0.min + size;
        }
        fn read_frame(&mut self, (): &()) -> Option<IRect> {
            Some(self.0.lock().unwrap().0)
        }
        fn finish(&mut self) {}
    }

    /// A window animates up from a low slot to full height. The app limits
    /// the first frame's height because the window is still low. Later
    /// frames ask for the same size at higher positions, and the window
    /// must still reach full height.
    #[test]
    fn a_size_the_app_limited_is_written_again_with_the_next_move() {
        let start = IRect::new(640, 735, 1280, 1440);
        let app = Arc::new(Mutex::new((start, Vec::new())));
        let sink = app.clone();
        let writers = Arc::new(WindowWriters::new(Duration::from_secs(1), move |_| {
            Shelf(sink.clone())
        }));
        let mut window = FrameWrites::new(writers.clone()).starting_at(start);
        for y in [500, 250, 30] {
            window.place(Some(1), || (), IRect::new(640, y, 1280, y + 1410));
            assert!(writers.flush(Duration::from_secs(1)));
        }
        let app = app.lock().unwrap();
        assert_eq!(app.0, IRect::new(640, 30, 1280, 1440));
        // The app limited the size at the old position, so a retry asks for
        // it once, after the move.
        assert_eq!(
            app.1[3..],
            [
                Op::Position(Origin::new(640, 250)),
                Op::Size(Size::new(640, 1410)),
                Op::Position(Origin::new(640, 30)),
                Op::Size(Size::new(640, 1410)),
            ]
        );
    }

    #[test]
    fn stages_partially_applied_width_growth() {
        let previous = IRect::new(-400, 40, 400, 640);
        let actual = IRect::new(-400, 40, 2416, 640);

        assert_eq!(
            resize_staging_origin(previous, actual, 4112),
            Some(Origin::new(-1696, 40))
        );

        let nearly_complete = IRect::new(-2056, 40, 2016, 640);
        assert_eq!(
            resize_staging_origin(actual, nearly_complete, 4112),
            Some(Origin::new(-2096, 40))
        );
    }

    #[test]
    fn does_not_stage_fixed_size_or_completed_resizes() {
        let fixed = IRect::new(0, 40, 230, 448);
        assert_eq!(resize_staging_origin(fixed, fixed, 4112), None);

        let previous = IRect::new(0, 40, 800, 640);
        let completed = IRect::new(0, 40, 4112, 640);
        assert_eq!(resize_staging_origin(previous, completed, 4112), None);
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Condvar, Mutex};
    use std::thread;
    use std::time::Instant;

    /// Longer than any test holds an app, so a hold never ends by itself
    /// while a test still expects it, yet short enough that a writer which
    /// waits on the caller's thread fails the test instead of hanging it.
    const HOLD_LIMIT: Duration = Duration::from_secs(1);
    const SECOND: Duration = Duration::from_secs(1);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Op {
        Begin,
        Position(u32, Origin),
        Size(u32, Size),
        Finish,
    }

    /// What every fake app recorded, and a switch that makes an app hang
    /// inside its next write until the test lets it go.
    struct Probe {
        log: Mutex<HashMap<Pid, Vec<Op>>>,
        held: Mutex<HashSet<Pid>>,
        released: Condvar,
        entered: Mutex<Sender<Pid>>,
        opened: AtomicUsize,
    }

    impl Probe {
        fn new() -> (Arc<Self>, Receiver<Pid>) {
            let (entered, receiver) = channel();
            let probe = Arc::new(Self {
                log: Mutex::new(HashMap::new()),
                held: Mutex::new(HashSet::new()),
                released: Condvar::new(),
                entered: Mutex::new(entered),
                opened: AtomicUsize::new(0),
            });
            (probe, receiver)
        }

        fn hold(&self, pid: Pid) {
            self.held.lock().unwrap().insert(pid);
        }

        fn release(&self, pid: Pid) {
            self.held.lock().unwrap().remove(&pid);
            self.released.notify_all();
        }

        fn log(&self, pid: Pid) -> Vec<Op> {
            self.log
                .lock()
                .unwrap()
                .get(&pid)
                .cloned()
                .unwrap_or_default()
        }

        fn record(&self, pid: Pid, op: Op) {
            self.log.lock().unwrap().entry(pid).or_default().push(op);
        }

        fn hang_if_held(&self, pid: Pid) {
            let held = self.held.lock().unwrap();
            if !held.contains(&pid) {
                return;
            }
            let _ = self.entered.lock().unwrap().send(pid);
            let _ = self
                .released
                .wait_timeout_while(held, HOLD_LIMIT, |held| held.contains(&pid))
                .unwrap();
        }
    }

    struct FakeApp {
        pid: Pid,
        probe: Arc<Probe>,
    }

    impl WriteSink for FakeApp {
        type Window = u32;

        fn begin(&mut self) {
            self.probe.record(self.pid, Op::Begin);
        }

        fn set_position(&mut self, window: &u32, origin: Origin) {
            self.probe.hang_if_held(self.pid);
            self.probe.record(self.pid, Op::Position(*window, origin));
        }

        fn set_size(&mut self, window: &u32, size: Size) {
            self.probe.hang_if_held(self.pid);
            self.probe.record(self.pid, Op::Size(*window, size));
        }

        fn read_frame(&mut self, _window: &u32) -> Option<IRect> {
            None
        }

        fn finish(&mut self) {
            self.probe.record(self.pid, Op::Finish);
        }
    }

    fn writers(probe: &Arc<Probe>, idle: Duration) -> WindowWriters<FakeApp> {
        let probe = probe.clone();
        WindowWriters::new(idle, move |pid| {
            probe.opened.fetch_add(1, Ordering::SeqCst);
            FakeApp {
                pid: pid.unwrap_or_default(),
                probe: probe.clone(),
            }
        })
    }

    /// One key per fake window, so tests can name windows by number.
    fn key(window: u32) -> WriteKey {
        static KEYS: Mutex<Vec<WriteKey>> = Mutex::new(Vec::new());
        let mut keys = KEYS.lock().unwrap();
        while keys.len() <= window as usize {
            keys.push(WriteKey::next());
        }
        keys[window as usize]
    }

    fn moved(window: u32, x: i32) -> PendingWrite<u32> {
        PendingWrite {
            window,
            from: IRect::new(0, 0, 100, 100),
            to: IRect::new(x, 0, x + 100, 100),
            position: true,
            size: false,
            size_limited: Arc::default(),
        }
    }

    fn resized(window: u32, frame: IRect) -> PendingWrite<u32> {
        PendingWrite {
            window,
            from: IRect::new(0, 0, 100, 100),
            to: frame,
            position: false,
            size: true,
            size_limited: Arc::default(),
        }
    }

    #[test]
    fn a_push_does_not_wait_for_a_hung_app() {
        let (probe, entered) = Probe::new();
        let writers = writers(&probe, SECOND);
        probe.hold(1);
        let started = Instant::now();
        writers.push(1, key(7), moved(7, 10));
        assert_eq!(entered.recv_timeout(SECOND), Ok(1));
        writers.push(1, key(7), moved(7, 20));
        let pushes_took = started.elapsed();

        assert!(
            pushes_took < Duration::from_millis(50),
            "the pushes took {pushes_took:?}"
        );
        assert!(writers.is_pending(1, key(7)));
        assert!(!writers.wait_landed(1, key(7), Duration::from_millis(20)));
        assert_eq!(probe.log(1), vec![Op::Begin]);

        probe.release(1);
        assert!(writers.wait_landed(1, key(7), SECOND));
        assert!(!writers.is_pending(1, key(7)));
        assert_eq!(
            probe.log(1),
            vec![
                Op::Begin,
                Op::Position(7, Origin::new(10, 0)),
                Op::Finish,
                Op::Begin,
                Op::Position(7, Origin::new(20, 0)),
                Op::Finish,
            ]
        );
    }

    #[test]
    fn a_hung_app_does_not_hold_up_another_app() {
        let (probe, entered) = Probe::new();
        let writers = writers(&probe, SECOND);
        probe.hold(1);
        writers.push(1, key(7), moved(7, 10));
        assert_eq!(entered.recv_timeout(SECOND), Ok(1));

        let started = Instant::now();
        writers.push(2, key(8), moved(8, 50));
        assert!(writers.wait_landed(2, key(8), SECOND));
        let landed_after = started.elapsed();

        assert!(
            landed_after < Duration::from_millis(100),
            "the other app's write landed after {landed_after:?}"
        );
        assert_eq!(
            probe.log(2),
            vec![Op::Begin, Op::Position(8, Origin::new(50, 0)), Op::Finish]
        );
        assert_eq!(probe.log(1), vec![Op::Begin]);

        probe.release(1);
        assert!(writers.flush(SECOND));
        assert_eq!(
            probe.log(1),
            vec![Op::Begin, Op::Position(7, Origin::new(10, 0)), Op::Finish]
        );
    }

    /// Frames asked for while an earlier write is in flight replace each
    /// other, so the app gets only the newest once it answers. A position
    /// and a later size merge into one write of both.
    #[test]
    fn requests_queued_behind_a_hung_write_collapse_to_the_newest() {
        let (probe, entered) = Probe::new();
        let writers = writers(&probe, SECOND);
        probe.hold(1);
        writers.push(1, key(7), moved(7, 10));
        assert_eq!(entered.recv_timeout(SECOND), Ok(1));

        writers.push(1, key(7), moved(7, 20));
        writers.push(1, key(7), moved(7, 30));
        writers.push(1, key(7), moved(7, 40));
        writers.push(1, key(7), resized(7, IRect::new(40, 0, 240, 100)));
        probe.release(1);

        assert!(writers.flush(SECOND));
        assert_eq!(
            probe.log(1),
            vec![
                Op::Begin,
                Op::Position(7, Origin::new(10, 0)),
                Op::Finish,
                Op::Begin,
                Op::Size(7, Size::new(200, 100)),
                Op::Position(7, Origin::new(40, 0)),
                Op::Finish,
            ]
        );
    }

    #[test]
    fn flush_waits_for_the_write_in_flight() {
        let (probe, entered) = Probe::new();
        let writers = writers(&probe, SECOND);
        probe.hold(1);
        writers.push(1, key(7), moved(7, 10));
        assert_eq!(entered.recv_timeout(SECOND), Ok(1));

        assert!(!writers.flush(Duration::from_millis(30)));

        let releaser = {
            let probe = probe.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                probe.release(1);
            })
        };
        assert!(writers.flush(SECOND));
        assert_eq!(
            probe.log(1),
            vec![Op::Begin, Op::Position(7, Origin::new(10, 0)), Op::Finish]
        );
        releaser.join().unwrap();
    }

    #[test]
    fn an_idle_writer_exits_and_comes_back_on_demand() {
        let (probe, _entered) = Probe::new();
        let writers = writers(&probe, Duration::from_millis(20));
        writers.push(1, key(7), moved(7, 10));
        assert!(writers.flush(SECOND));
        assert_eq!(writers.writers(), 1);

        let deadline = Instant::now() + SECOND;
        while writers.writers() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(writers.writers(), 0);

        writers.push(1, key(7), moved(7, 20));
        assert!(writers.flush(SECOND));
        assert_eq!(probe.opened.load(Ordering::SeqCst), 2);
        assert_eq!(
            probe.log(1),
            vec![
                Op::Begin,
                Op::Position(7, Origin::new(10, 0)),
                Op::Finish,
                Op::Begin,
                Op::Position(7, Origin::new(20, 0)),
                Op::Finish,
            ]
        );
    }

    /// A writer that never waits gives up after every batch, so these pushes
    /// keep landing right as their writer is leaving, and the pauses let it
    /// actually leave. A push that lost the race against the exit would be
    /// missing from the log.
    #[test]
    fn a_push_racing_an_idle_exit_is_never_lost() {
        const PUSHES: u32 = 20_000;
        let (probe, _entered) = Probe::new();
        let writers = writers(&probe, Duration::ZERO);
        let keys: Vec<_> = (0..PUSHES).map(|_| WriteKey::next()).collect();
        for (window, key) in (0..PUSHES).zip(&keys) {
            writers.push(1, *key, moved(window, window.cast_signed()));
            if window % 1000 == 999 {
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(writers.flush(Duration::from_secs(5)));

        let mut landed = probe
            .log(1)
            .into_iter()
            .filter_map(|op| match op {
                Op::Position(window, origin) => Some((window, origin.x)),
                _ => None,
            })
            .collect::<Vec<_>>();
        landed.sort_unstable();
        assert_eq!(
            landed,
            (0..PUSHES)
                .map(|window| (window, window.cast_signed()))
                .collect::<Vec<_>>()
        );
        assert!(probe.opened.load(Ordering::SeqCst) > 1);
    }
}
