//! Moves and resizes windows on behalf of the engine.
//!
//! An accessibility write is a synchronous round-trip served on the main
//! thread of the window's app, and a busy app holds it for hundreds of
//! milliseconds. The engine asks for a frame through [`FrameWrites`], and
//! [`WindowWriters`] carries the write to the app through a [`WriteSink`].

use bevy::math::IRect;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, trace};

use crate::errors::Result;
use crate::manager::{Origin, Size};
use crate::platform::Pid;

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
}

/// Writes one window's frame. A write of both parts follows the order the
/// engine has always used: the size at the old position, then the position,
/// then the size again if the app limited it at the old position.
pub(crate) fn apply<S: WriteSink>(sink: &mut S, write: &PendingWrite<S::Window>) {
    let window = &write.window;
    let size = write.to.size();
    match (write.position, write.size) {
        (true, true) => {
            let observed = resize(sink, window, write.from, size, write.from.min);
            sink.set_position(window, write.to.min);
            if let Some(observed) = observed
                && observed.size() != size
            {
                let moved = IRect::from_corners(write.to.min, write.to.min + observed.size());
                resize(sink, window, moved, size, write.to.min);
            }
        }
        (true, false) => sink.set_position(window, write.to.min),
        (false, true) => {
            resize(sink, window, write.from, size, write.to.min);
        }
        (false, false) => {}
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

/// Carries window writes to their apps.
pub(crate) struct WindowWriters<S: WriteSink> {
    open_sink: Box<dyn Fn(Option<Pid>) -> S + Send + Sync>,
}

impl<S: WriteSink> WindowWriters<S> {
    pub fn new(
        _idle: Duration,
        open_sink: impl Fn(Option<Pid>) -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            open_sink: Box::new(open_sink),
        }
    }

    /// Writes `write` to `pid`'s window `key`.
    pub fn push(&self, pid: Pid, _key: WriteKey, write: PendingWrite<S::Window>) {
        self.write_inline(Some(pid), &write);
    }

    /// Writes `write` on the calling thread.
    pub fn write_inline(&self, pid: Option<Pid>, write: &PendingWrite<S::Window>) {
        let mut sink = (self.open_sink)(pid);
        objc2::rc::autoreleasepool(|_| {
            sink.begin();
            apply(&mut sink, write);
            sink.finish();
        });
    }
}

/// The engine's side of one window's writes: the frame it last asked for,
/// and the writers that carry it to the app.
pub(crate) struct FrameWrites<S: WriteSink> {
    writers: Arc<WindowWriters<S>>,
    key: WriteKey,
    frame: IRect,
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
        }
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
        };
        self.frame = to;
        match pid {
            Some(pid) => self.writers.push(pid, self.key, write),
            // No app to queue behind: write it here.
            None => self.writers.write_inline(None, &write),
        }
    }

    /// Reads the window's frame with `read`, and records it.
    ///
    /// # Errors
    ///
    /// Fails if `read` fails.
    pub fn update_frame(&mut self, read: impl FnOnce() -> Result<IRect>) -> Result<IRect> {
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
