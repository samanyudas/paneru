//! Paces a scrolling row at its slowest window.
//!
//! Each app applies a move when its main thread gets to it: Finder and
//! Spotify at once, Helium tens of milliseconds later, and up to 250 ms
//! later right after a focus change. Each window used to ease toward its
//! target on its own, so a window ahead in the direction of motion ran away
//! from a slower window behind it, and the desktop showed between them.
//!
//! Instead, while a row animates, no window gets closer to its target than
//! the window behind it is to its own, as the window server shows that
//! window, at the edge the two windows share. A window that changes width
//! moves its two edges apart, so pacing reads each edge. The slowest window
//! then sets the pace for every window ahead of it. A window that lags into
//! the window ahead only overlaps it, which shows no desktop, so nothing
//! waits on the windows ahead of the slow one.
//! A photo sliding at a shared edge is held the same way, and closes over
//! its window only once the window server shows the window where it rests.
//!
//! The windows ahead of a window whose app has shown every move of a whole
//! motion in time follow where the engine puts it, so a row of quick apps
//! moves together. Any other window paces from where it is shown, and so do
//! the windows of an app that focus moves to or from another app, or that
//! comes to the front or leaves it, which can turn slow.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use bevy::ecs::entity::{Entity, EntityHashMap, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::Added;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Local, Query, Res, ResMut};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use tracing::{debug, warn};

use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::shared_edge::Slide;
use crate::ecs::systems::{
    AnimationStep, LOOP_MAX_TIMEOUT_FRAME_ACTIVE_MS, ease_out_factor, ease_step,
};
use crate::ecs::{
    Bounds, FocusedMarker, Position, PresentedOrigin, RepositionMarker, ResizeMarker,
};
use crate::events::Event;
use crate::manager::window_writer::LANDING_TIMEOUT;
use crate::manager::{Application, Window};
use crate::platform::{ProcessSerialNumber, WinID};

/// How long the sampler rests between reads, a quarter of a frame.
const READ_INTERVAL: Duration = Duration::from_millis(4);
/// How long the sampler waits for windows to watch before its thread exits.
const SAMPLER_IDLE: Duration = Duration::from_secs(1);

type ReadShown = dyn Fn(&[WinID]) -> Vec<(WinID, IRect)> + Send + Sync;

/// Reads where the window server shows the windows of a moving row, on a
/// thread of its own. A read waits on the window server: at frame pace, one
/// for Sam's row of seven windows took 1.6 ms typically, 9 ms at the 99th
/// percentile and up to 26 ms, too long to wait for on the engine frame.
#[derive(Resource, Clone)]
pub(crate) struct ShownFrames {
    shared: Arc<Shared>,
    /// Reads on the sampler's thread, rather than in each watch.
    sampled: bool,
}

struct Shared {
    watch: Mutex<Watch>,
    wake: Condvar,
    read: Box<ReadShown>,
}

#[derive(Default)]
struct Watch {
    ids: Vec<WinID>,
    /// The newest frame read for each watched window. Until the first read,
    /// the frame the engine had asked for when watching began, where a
    /// window at rest is shown.
    shown: HashMap<WinID, IRect>,
    /// The watched windows whose frame in `shown` is still the one asked for
    /// when watching began, not yet read.
    unread: HashSet<WinID>,
    /// Bumped on each stop, so a read started before it is dropped.
    generation: u64,
    sampling: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ShownFrames {
    pub(crate) fn new(
        read: impl Fn(&[WinID]) -> Vec<(WinID, IRect)> + Send + Sync + 'static,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                watch: Mutex::default(),
                wake: Condvar::new(),
                read: Box::new(read),
            }),
            sampled: true,
        }
    }

    /// Reads in each watch, so a test that runs frames back to back sees
    /// every write that has landed.
    #[cfg(test)]
    pub(crate) fn inline(
        read: impl Fn(&[WinID]) -> Vec<(WinID, IRect)> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sampled: false,
            ..Self::new(read)
        }
    }

    /// Watches the windows of `asked`, each with the frame the engine last
    /// asked for, and returns where each is shown as of the newest read.
    /// `None` when no sampler can run.
    fn watch(&self, asked: &[(WinID, IRect)]) -> Option<Shown> {
        let mut watch = lock(&self.shared.watch);
        watch.ids.clear();
        watch.ids.extend(asked.iter().map(|&(id, _)| id));
        for &(id, frame) in asked {
            if let Entry::Vacant(entry) = watch.shown.entry(id) {
                entry.insert(frame);
                watch.unread.insert(id);
            }
        }
        if !self.sampled {
            let frames = (self.shared.read)(&watch.ids);
            watch.take(frames);
        } else if !watch.sampling {
            let shared = self.shared.clone();
            if let Err(err) = thread::Builder::new()
                .name("shown-frames".into())
                .spawn(move || shared.sample())
            {
                warn!("cannot start the shown-frame sampler, not pacing rows: {err}");
                return None;
            }
            watch.sampling = true;
        }
        self.shared.wake.notify_one();
        Some(Shown {
            frames: asked
                .iter()
                .filter_map(|&(id, _)| watch.shown.get(&id).map(|&frame| (id, frame)))
                .collect(),
            unread: asked
                .iter()
                .map(|&(id, _)| id)
                .filter(|id| watch.unread.contains(id))
                .collect(),
        })
    }

    fn stop(&self) {
        let mut watch = lock(&self.shared.watch);
        watch.ids.clear();
        watch.shown.clear();
        watch.unread.clear();
        watch.generation += 1;
    }
}

impl Watch {
    fn take(&mut self, frames: Vec<(WinID, IRect)>) {
        for (id, frame) in frames {
            self.unread.remove(&id);
            self.shown.insert(id, frame);
        }
    }
}

impl Shared {
    fn sample(&self) {
        loop {
            let mut watch = lock(&self.watch);
            while watch.ids.is_empty() {
                let (woken, wait) = self
                    .wake
                    .wait_timeout(watch, SAMPLER_IDLE)
                    .unwrap_or_else(PoisonError::into_inner);
                watch = woken;
                if wait.timed_out() && watch.ids.is_empty() {
                    watch.sampling = false;
                    return;
                }
            }
            let (ids, generation) = (watch.ids.clone(), watch.generation);
            drop(watch);

            let frames = (self.read)(&ids);
            let mut watch = lock(&self.watch);
            if watch.generation == generation {
                watch.take(frames);
            }
            drop(watch);
            thread::sleep(READ_INTERVAL);
        }
    }
}

/// Where the window server shows watched windows, as of one moment.
struct Shown {
    frames: HashMap<WinID, IRect>,
    /// The windows whose frame is still the one the engine asked for when
    /// watching began, as no read has come since.
    unread: HashSet<WinID>,
}

/// How far each sliding photo may go this frame, as [`pace_rows`] holds it
/// with the rest of its row, and which landing windows the window server
/// shows where they rest. The photos step after that.
#[derive(Resource, Default)]
pub(crate) struct SlideHolds {
    bounds: EntityHashMap<i32>,
    landed: EntityHashSet,
}

impl SlideHolds {
    /// `next`, the step of the photo of `entity` from `at`, held where
    /// pacing stops it this frame.
    pub(crate) fn limit(&self, entity: Entity, at: IVec2, next: IVec2) -> IVec2 {
        let Some(&bound) = self.bounds.get(&entity) else {
            return next;
        };
        next.with_x(if next.x < at.x {
            next.x.max(bound).min(at.x)
        } else {
            next.x.min(bound).max(at.x)
        })
    }

    /// Whether the window server shows the landing window `entity` where the
    /// engine put it.
    pub(crate) fn landed(&self, entity: Entity) -> bool {
        self.landed.contains(&entity)
    }
}

/// Where a sliding window or its photo shows now, and where it heads.
/// `None` while it lands, at rest under its photo.
fn heading(slide: &Slide, presented: IVec2) -> Option<(IVec2, IVec2)> {
    match *slide {
        Slide::Moving { at, .. } => Some((at, presented)),
        Slide::Leaving { at, to, .. } | Slide::Tucking { at, to, .. } => Some((at, to)),
        Slide::Landing { .. } => None,
    }
}

/// The left and right edges of a window along its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edges {
    left: i32,
    right: i32,
}

impl From<IRect> for Edges {
    fn from(frame: IRect) -> Self {
        Self {
            left: frame.min.x,
            right: frame.max.x,
        }
    }
}

/// What the engine has seen of one row window's app taking its moves.
#[derive(Default)]
pub(super) struct Tracked {
    /// Where the engine had asked for the window as of the previous frame.
    asked: Option<Edges>,
    /// The app showed each of the engine's moves by the next frame through
    /// a whole motion, and has not been late since, nor gained or lost focus
    /// to another app, nor come to the front or left it, so it takes this
    /// frame's move about as soon as the windows ahead take theirs. Kept
    /// across motions, so a slow app is paced from the first step of the
    /// next one.
    prompt: bool,
    /// This motion, the app has shown a move by the next frame.
    on_time: bool,
    /// This motion, the window has shown away from where the engine asked.
    late_this_motion: bool,
    /// Where the window shows away from where the engine asked for it, and
    /// since when it has shown no progress.
    late: Option<(Edges, Duration)>,
    /// Where the engine first asked for the window this motion.
    start: Option<Edges>,
    /// Where pacing held the window last frame, short of its slide.
    held: Option<Held>,
    /// Where the window's photo was when it began to slide this motion.
    slid_from: Option<i32>,
}

/// A window that pacing held back for a frame.
#[derive(Clone, Copy)]
struct Held {
    /// Where the window's slide would have put it.
    free: i32,
    /// Where pacing put it instead.
    at: i32,
    target: i32,
}

impl Tracked {
    /// Takes in that the engine asked for the window at `asked` last frame
    /// and the window server shows it at `shown`, if it lists it.
    fn see(&mut self, asked: Edges, shown: Option<Edges>, now: Duration) -> Pacer {
        self.start.get_or_insert(asked);
        let before = self.asked.replace(asked);
        match shown {
            Some(edges) if edges != asked => {
                self.prompt = false;
                self.late_this_motion = true;
                let since = match self.late {
                    Some((at, since)) if at == edges => since,
                    _ => {
                        self.late = Some((edges, now));
                        now
                    }
                };
                if now.saturating_sub(since) >= LANDING_TIMEOUT {
                    Pacer::Asked
                } else {
                    Pacer::Shown(edges)
                }
            }
            _ => {
                self.late = None;
                if before.is_some_and(|before| before != asked) {
                    self.on_time = true;
                }
                if self.prompt {
                    Pacer::Asked
                } else {
                    Pacer::Shown(asked)
                }
            }
        }
    }

    /// Ends a motion: the app counts as prompt from now on if it showed
    /// every move of the motion in time. A window the motion did not move
    /// keeps what it was.
    fn rest(&mut self) {
        self.prompt |= self.on_time && !self.late_this_motion;
        self.on_time = false;
        self.late_this_motion = false;
        self.late = None;
        self.start = None;
        self.held = None;
        self.slid_from = None;
    }

    /// Where the window's slide puts it this frame had pacing not held it
    /// last frame: one step on from where it would have been then. `at`,
    /// where the animation stepped it to from where pacing held it, if
    /// pacing did not hold it or something else has moved it since. Pacing
    /// writes where it holds a window back into its slide, so a window held
    /// for a slow window behind it would otherwise ease on from where it
    /// waited, and trail that window for the rest of the motion (miri#500).
    fn free(&mut self, at: i32, target: i32, t: f32) -> i32 {
        let step = |from: i32| ease_step(IVec2::X * from, IVec2::X * target, t).0.x;
        match self.held.take() {
            Some(held) if held.target == target && step(held.at) == at => {
                let free = step(held.free);
                if (target - free).abs() <= (target - at).abs() {
                    free
                } else {
                    at
                }
            }
            _ => at,
        }
    }

    /// Where the window started this motion, while the window server still
    /// shows it there.
    fn unmoved(&self, shown: Option<Edges>) -> Option<Edges> {
        self.start.filter(|&start| shown == Some(start))
    }
}

/// How a row window paces the windows ahead of it.
#[derive(Clone, Copy, Debug)]
enum Pacer {
    /// From where the engine puts it this frame: its app is prompt, it is a
    /// photo, which shows at once, or it has stalled and pacing on it would
    /// stall the row.
    Asked,
    /// From where the window server shows it, or where its photo shows.
    Shown(Edges),
}

/// One window of a moving row, in engine frame coordinates.
struct Member {
    entity: Entity,
    id: WinID,
    /// Where the animation puts the window this frame.
    eased: i32,
    /// Where the window's slide puts it this frame had pacing never held
    /// it, or the first step a window that leads the row in gets until its
    /// app shows a move.
    free: i32,
    /// The first step of the window's slide, the most it gets while a window
    /// of its column has yet to show a move and no column behind it has gone
    /// further. `None` for a window that changes width, is on its way
    /// nowhere, or slides as a photo.
    first: Option<FirstStep>,
    /// Where pacing lets the window go this frame.
    x: i32,
    /// How wide the animation makes the window this frame.
    width: i32,
    /// Where pacing lets the right edge of a window that changes width go
    /// this frame. A window that only moves keeps its width.
    held_right: Option<i32>,
    target: i32,
    target_right: i32,
    /// Where the engine asked for the window last frame.
    asked: i32,
    /// The edges of the frame the engine last asked the window's app for.
    asked_edges: Edges,
    pacer: Pacer,
    paced: bool,
    /// The window's photo slides it at a shared edge: pacing holds the
    /// photo, not the window.
    slides: bool,
    /// How far the window or its photo is from where it was when this
    /// motion began: where its app shows it, or where the engine puts it
    /// this frame if that shows at once.
    travelled: i32,
}

/// Where the first step of a window's slide puts it.
#[derive(Clone, Copy)]
struct FirstStep {
    /// Where it puts the window, counted from where the window started this
    /// motion.
    at: i32,
    /// Which way the window heads from where it started this motion: -1
    /// left, 1 right.
    heading: i32,
    /// How far one step takes the window from where it started this motion.
    reach: i32,
    /// The window's app has yet to show a move sideways this motion, so the
    /// window gets only its first step.
    unshown: bool,
}

/// Where the first step of a slide from `start` to `target` puts a window:
/// at `asked`, where the engine has it, if that is further along, and no
/// further than `free`, where its slide has it this frame. `first_t` is the
/// ease of a first step. Comes with how far one step takes it from `start`.
fn first_step(start: i32, asked: i32, free: i32, target: i32, first_t: f32) -> (i32, i32) {
    let (step, _) = ease_step(IVec2::X * start, IVec2::X * target, first_t);
    let along = |x: i32| (x - start) * (target - start).signum();
    let first = if along(asked) > along(step.x) {
        asked
    } else {
        step.x
    };
    let at = if along(free) > along(first) {
        first
    } else {
        free
    };
    (at, along(step.x))
}

impl Member {
    /// How far each edge of the window is from its target, as the windows
    /// next to it see it.
    fn lag(&self) -> Edges {
        let edges = match self.pacer {
            Pacer::Asked => Edges {
                left: self.x,
                right: self.right(),
            },
            // A write that widens sets the new size at the old position
            // before the new position, so the edge that a width change moves
            // shows ahead of where it lands until the position follows. Each
            // edge counts as no further along than with the shown width and
            // the other edge where the engine asked for it.
            Pacer::Shown(edges) => {
                let width = edges.right - edges.left;
                Edges {
                    left: edges.left.max(self.asked_edges.right - width),
                    right: edges.right.min(self.asked_edges.left + width),
                }
            }
        };
        Edges {
            left: edges.left - self.target,
            right: edges.right - self.target_right,
        }
    }

    /// Where pacing lets the window's right edge go this frame.
    fn right(&self) -> i32 {
        self.held_right.unwrap_or(self.x + self.width)
    }

    /// Keeps the right edge of a window that narrows from the right at
    /// `bound` or right of it, but never further right than where the engine
    /// last asked for it, so the window keeps its width a while.
    fn hold_right_edge_right_of(&mut self, bound: i32) {
        let asked = self.asked_edges.right;
        if let Some(right) = &mut self.held_right
            && self.eased + self.width < asked
        {
            *right = (*right).max(bound).min(asked);
        }
    }

    /// Keeps a window that moves left at `bound` or right of it, but never
    /// further right than where the engine last asked for it.
    fn hold_right_of(&mut self, bound: i32) {
        if self.paced && self.x < self.asked {
            self.x = self.x.max(bound).min(self.asked);
        }
    }

    /// Keeps a window that moves right at `bound` or left of it, but never
    /// further left than where the engine last asked for it.
    fn hold_left_of(&mut self, bound: i32) {
        if self.paced && self.x > self.asked {
            self.x = self.x.min(bound).max(self.asked);
        }
    }
}

type RowWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static Window,
        &'static mut Position,
        &'static mut Bounds,
        Option<&'static RepositionMarker>,
        Option<&'static ResizeMarker>,
        Option<(&'static Slide, &'static PresentedOrigin)>,
    ),
>;

/// Holds each animating row window back so that it stays at least as far
/// from its target as the column behind it, in the direction of motion, is
/// from its own. Runs between the animation step and the frame write, and
/// before the photos at shared edges step.
#[allow(clippy::too_many_arguments)]
pub(super) fn pace_rows(
    focused: Query<Entity, Added<FocusedMarker>>,
    mut unfocused: RemovedComponents<FocusedMarker>,
    mut front_switches: MessageReader<Event>,
    applications: Query<(Entity, &Application)>,
    mut front: Local<Option<ProcessSerialNumber>>,
    apps: Query<&ChildOf>,
    shown_frames: Option<Res<ShownFrames>>,
    strips: Query<&LayoutStrip>,
    mut windows: RowWindows,
    time: Res<Time>,
    step: Res<AnimationStep>,
    config: Res<Config>,
    mut slide_holds: ResMut<SlideHolds>,
    mut tracked: Local<EntityHashMap<Tracked>>,
    mut watching: Local<bool>,
) {
    *slide_holds = SlideHolds::default();
    forget_switched_apps(&mut tracked, focused.iter(), unfocused.read(), &apps);
    distrust_fronted_apps(
        &mut tracked,
        &mut front,
        front_switches.read(),
        &applications,
        &apps,
    );
    let Some(shown_frames) = shown_frames else {
        return;
    };
    let rows = moving_rows(&strips, &windows);
    if rows.is_empty() {
        if *watching {
            shown_frames.stop();
            tracked.retain(|&entity, window| {
                window.rest();
                windows.contains(entity)
            });
            *watching = false;
        }
        return;
    }

    let asked = rows
        .iter()
        .flatten()
        .flatten()
        .filter_map(|&entity| windows.get(entity).ok())
        .map(|(window, ..)| (window.id(), window.margins().shown(window.frame())))
        .collect::<Vec<_>>();
    let Some(shown) = shown_frames.watch(&asked) else {
        return;
    };
    *watching = true;
    slide_holds.landed = landed(&rows, &windows, &shown);

    let now = time.elapsed();
    let t = ease_out_factor(config.animation_speed(), step.secs());
    let first_t = ease_out_factor(
        config.animation_speed(),
        f64::from(LOOP_MAX_TIMEOUT_FRAME_ACTIVE_MS) / 1000.0,
    );
    for row in rows {
        let mut row = row
            .into_iter()
            .map(|column| {
                column
                    .into_iter()
                    .filter_map(|entity| {
                        member(entity, &windows, &shown, &mut tracked, (t, first_t), now)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        start_on_first_steps(&mut row);
        pace_row(&mut row);
        for member in row.iter().flatten().filter(|member| !member.slides) {
            if let Some(record) = tracked.get_mut(&member.entity) {
                record.held = (member.paced && member.x != member.free).then_some(Held {
                    free: member.free,
                    at: member.x,
                    target: member.target,
                });
            }
        }

        for member in row.iter().flatten().filter(|member| {
            member.x != member.eased || member.right() != member.eased + member.width
        }) {
            if member.slides {
                debug!(
                    "pacing holds the photo of window {} at x {} on its way to {}",
                    member.id, member.x, member.target
                );
                slide_holds.bounds.insert(member.entity, member.x);
            } else if let Ok((_, mut position, mut bounds, ..)) = windows.get_mut(member.entity) {
                debug!(
                    "pacing puts window {} at x {} right {}, not at {} right {}, on its way to {} right {}",
                    member.id,
                    member.x,
                    member.right(),
                    member.eased,
                    member.eased + member.width,
                    member.target,
                    member.target_right
                );
                position.0.x = member.x;
                let width = member.right() - member.x;
                if bounds.0.x != width {
                    bounds.0.x = width;
                }
            }
        }
    }
}

/// The row window `entity` as pacing sees it this frame, with `t` the
/// row's ease this frame, `first_t` the ease of a slide's first step, and
/// `tracked` what the engine has seen of each app.
fn member(
    entity: Entity,
    windows: &RowWindows,
    shown: &Shown,
    tracked: &mut EntityHashMap<Tracked>,
    (t, first_t): (f32, f32),
    now: Duration,
) -> Option<Member> {
    let (window, position, bounds, to, resize, slide) = windows.get(entity).ok()?;
    let id = window.id();
    let asked = Edges::from(window.frame());
    let shown_edges = shown
        .frames
        .get(&id)
        .map(|&frame| Edges::from(window.margins().tracked(frame)));
    let width = bounds.0.x;
    if let Some((at, heading_to)) = slide.and_then(|(slide, presented)| heading(slide, presented.0))
    {
        let (step, _) = ease_step(at, heading_to, t);
        let record = tracked.entry(entity).or_default();
        let from = *record.slid_from.get_or_insert(at.x);
        // A photo shows at once where it steps to this frame. A window
        // leaving for its tuck moves itself, as late as its app takes the
        // move.
        let pacer = match slide {
            Some((Slide::Leaving { .. }, _)) => record.see(asked, shown_edges, now),
            _ => Pacer::Asked,
        };
        return Some(Member {
            entity,
            id,
            eased: step.x,
            free: step.x,
            first: None,
            x: step.x,
            width,
            held_right: None,
            target: heading_to.x,
            target_right: heading_to.x + width,
            asked: at.x,
            asked_edges: asked,
            pacer,
            paced: true,
            slides: true,
            travelled: (step.x - from).abs(),
        });
    }
    let record = tracked.entry(entity).or_default();
    let pacer = record.see(asked, shown_edges, now);
    let unmoved = record.unmoved(shown_edges);
    // A window landing under its photo shows where its photo rests, however
    // far its app has yet to take it, and has no move of its own yet but
    // still heads where layout puts it.
    let (target, pacer) = match slide {
        Some((_, presented)) => (presented.0.x, Pacer::Shown(asked)),
        None => (to.map_or(position.0.x, |to| to.0.x), pacer),
    };
    let target_width = resize.map_or(width, |resize| resize.0.x);
    let in_motion = to.is_some() && slide.is_none();
    let (free, first) = if in_motion && resize.is_none() {
        let led = record.held.is_none();
        let free = record.free(position.0.x, target, t);
        let start = record.start.map_or(asked.left, |start| start.left);
        let (at, reach) = first_step(start, asked.left, free, target, first_t);
        let first = FirstStep {
            at,
            heading: (target - start).signum(),
            reach,
            // A window that pacing held last frame waits for the window
            // behind it instead. One that only moves up or down has no move
            // sideways to show, and neither has one whose first step rounds
            // to no move at all.
            unshown: led
                && start != target
                && at != start
                && matches!(pacer, Pacer::Shown(_))
                && unmoved.is_some(),
        };
        (free, Some(first))
    } else {
        (position.0.x, None)
    };
    // A window at rest counts from the frame the engine asked for: its
    // position can stand a few points off that frame while it is tucked.
    let place = match pacer {
        Pacer::Asked if in_motion => free,
        Pacer::Asked => asked.left,
        Pacer::Shown(edges) => edges.left,
    };
    let travelled = record.start.map_or(0, |start| (place - start.left).abs());
    Some(Member {
        entity,
        id,
        eased: position.0.x,
        free,
        first,
        x: free,
        width,
        // With animation off the resize lands in one step, so pacing holds no
        // width.
        held_right: (resize.is_some() && slide.is_none() && t < 1.0)
            .then_some(position.0.x + width),
        target,
        target_right: target + target_width,
        asked: asked.left,
        asked_edges: asked,
        pacer,
        paced: in_motion,
        slides: false,
        travelled,
    })
}

/// Gives each column that leads the row in only the first step of its
/// slide while one of its windows has yet to show a move.
///
/// Asked on, such a window would show its first move however far the ease
/// had got by then, which right after a focus change is half the way, over
/// the window beside it (miri#500). The windows ahead wait for where it
/// shows, so the row starts together.
///
/// A column that follows one already further along than its own first step
/// keeps its slide, and [`pace_row`] holds it to where that column is. A
/// photo shows each step at once, and so does a window whose app pacing
/// trusts, so neither waits for the column ahead of it: held to its first
/// step, that column would fall behind and stay covered for the rest of the
/// slide.
fn start_on_first_steps(row: &mut [Vec<Member>]) {
    let follows = (0..row.len())
        .map(|index| follows(row, index))
        .collect::<Vec<_>>();
    for (column, follows) in row.iter_mut().zip(follows) {
        if !follows {
            start_together(column);
        }
    }
}

/// Whether column `index` of `row` has a window yet to show a move, and a
/// window of the column behind it, in the direction of motion, has gone
/// further this motion than that window's first step would take it. A
/// tucked column behind that only shifts a few points does not count.
fn follows(row: &[Vec<Member>], index: usize) -> bool {
    row[index]
        .iter()
        .filter_map(|member| member.first.filter(|first| first.unshown))
        .any(|first| {
            let behind = if first.heading < 0 {
                row.get(index + 1)
            } else {
                index.checked_sub(1).and_then(|behind| row.get(behind))
            };
            behind.is_some_and(|column| column.iter().any(|member| member.travelled > first.reach))
        })
}

/// Gives every window of a column only the first step of its slide while
/// one of them has yet to show a move, so a stack starts as one. On its own
/// slide, a window stacked with a late one would go on ahead, and the late
/// one would trail it until the slide ended (miri#500). Each window takes
/// its own first step, from where it started this motion towards where it
/// heads.
fn start_together(column: &mut [Member]) {
    if !column
        .iter()
        .any(|member| member.first.is_some_and(|first| first.unshown))
    {
        return;
    }
    for member in column {
        if let Some(first) = member.first {
            member.free = first.at;
            member.x = first.at;
        }
    }
}

/// Forgets what the engine has seen of the apps that gained focus from
/// another app or lost it to one taking their moves. Such an app can turn
/// slow, as Helium and Beeper do, so its windows pace the row from where
/// they show until they keep up again. Focus moving between the windows of
/// one app neither activates nor deactivates it.
fn forget_switched_apps(
    tracked: &mut EntityHashMap<Tracked>,
    focused: impl Iterator<Item = Entity>,
    unfocused: impl Iterator<Item = Entity>,
    apps: &Query<&ChildOf>,
) {
    let app = |entity: &Entity| apps.get(*entity).ok().map(ChildOf::parent);
    let gained = focused
        .filter_map(|entity| app(&entity))
        .collect::<Vec<_>>();
    let lost = unfocused
        .filter_map(|entity| app(&entity))
        .collect::<Vec<_>>();
    let switched = gained
        .iter()
        .filter(|app| !lost.contains(app))
        .chain(lost.iter().filter(|app| !gained.contains(app)))
        .collect::<Vec<_>>();
    if !switched.is_empty() {
        tracked.retain(|entity, _| app(entity).is_none_or(|app| !switched.contains(&&app)));
    }
}

/// Stops trusting the apps that each front switch in `events` brings to the
/// front or takes from it, `front`, to take their moves in time. Focus on an
/// empty display brings Finder to the front with only its desktop window,
/// which the engine does not track, and focus back on the same window brings
/// its app back, though no tracked window gains or loses focus, so
/// [`forget_switched_apps`] sees neither switch.
///
/// Only [`Tracked::prompt`] is cleared, so a motion in flight keeps what it
/// has seen. On an ordinary switch, the focus move has already forgotten
/// both apps before macOS reports it, so clearing changes nothing unless a
/// motion ended in between. Forgetting them a second time, as
/// paneru#93 did, made the windows ahead wait more often and the row land
/// later. The first switch after launch clears only the app it brings to the
/// front, because the engine has not yet heard which app was there.
fn distrust_fronted_apps<'a>(
    tracked: &mut EntityHashMap<Tracked>,
    front: &mut Option<ProcessSerialNumber>,
    events: impl Iterator<Item = &'a Event>,
    applications: &Query<(Entity, &Application)>,
    apps: &Query<&ChildOf>,
) {
    for event in events {
        let Event::ApplicationFrontSwitched { psn } = *event else {
            continue;
        };
        let left = front.replace(psn);
        if left == Some(psn) {
            continue;
        }
        let switched = applications
            .iter()
            .filter(|(_, app)| app.psn() == psn || Some(app.psn()) == left)
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>();
        let mut distrusted = 0;
        for (entity, window) in tracked.iter_mut() {
            if window.prompt
                && apps
                    .get(*entity)
                    .is_ok_and(|app| switched.contains(&app.parent()))
            {
                window.prompt = false;
                distrusted += 1;
            }
        }
        if distrusted > 0 {
            debug!(
                "a front switch ends pacing's trust in {distrusted} windows of the apps that came to or left the front"
            );
        }
    }
}

/// The windows of `rows` landing under their photos that the window server
/// was read to show where the engine put them.
fn landed(rows: &[Vec<Vec<Entity>>], windows: &RowWindows, shown: &Shown) -> EntityHashSet {
    rows.iter()
        .flatten()
        .flatten()
        .copied()
        .filter(|&entity| {
            windows.get(entity).is_ok_and(|(window, .., slide)| {
                matches!(slide, Some((Slide::Landing { .. }, _)))
                    && !shown.unread.contains(&window.id())
                    && shown
                        .frames
                        .get(&window.id())
                        .map(|&frame| window.margins().tracked(frame).min)
                        == Some(window.frame().min)
            })
        })
        .collect()
}

/// The windows of each strip that has a window on its way somewhere,
/// changing width, or landing under its photo, column by column, left to
/// right.
fn moving_rows(strips: &Query<&LayoutStrip>, windows: &RowWindows) -> Vec<Vec<Vec<Entity>>> {
    strips
        .iter()
        .map(|strip| {
            strip
                .columns()
                .map(|column| column.window_iter().collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .filter(|row| {
            row.iter().flatten().any(|&entity| {
                windows
                    .get(entity)
                    .is_ok_and(|(_, _, _, to, resize, slide)| {
                        to.is_some()
                            || resize.is_some()
                            || slide
                                .is_some_and(|(slide, _)| matches!(slide, Slide::Landing { .. }))
                    })
            })
        })
        .collect()
}

/// Holds the columns of `row`, left to right, from behind each one in the
/// direction of motion to the front, so each column follows where the edge
/// of the column behind it that faces it is this frame.
fn pace_row(row: &mut [Vec<Member>]) {
    // A column whose left edge is right of its target has yet to move left,
    // so the column left of it, ahead, may get no closer to its own.
    for index in (0..row.len().saturating_sub(1)).rev() {
        let lag = row[index + 1].iter().map(|member| member.lag().left).max();
        if let Some(lag) = lag.filter(|&lag| lag > 0) {
            for member in &mut row[index] {
                member.hold_right_of(member.target + lag);
                member.hold_right_edge_right_of(member.target_right + lag);
            }
        }
    }
    for index in 1..row.len() {
        let lag = row[index - 1].iter().map(|member| member.lag().right).min();
        if let Some(lag) = lag.filter(|&lag| lag < 0) {
            for member in &mut row[index] {
                member.hold_left_of(member.target + lag);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    /// The sampler reads on its own thread, so a watch never waits on the
    /// window server: until the first read lands, a window counts as shown
    /// where the engine asked for it.
    #[test]
    fn a_watch_returns_the_asked_frame_until_the_sampler_has_read() {
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let shown = IRect::new(70, 0, 170, 100);
        let frames = ShownFrames::new(move |ids| {
            counted.fetch_add(1, Ordering::Relaxed);
            ids.iter().map(|&id| (id, shown)).collect()
        });
        let asked = IRect::new(0, 0, 100, 100);

        let first = frames.watch(&[(7, asked)]).expect("a sampler runs");
        assert_eq!(first.frames[&7], asked, "seeded with the asked frame");

        let deadline = Instant::now() + Duration::from_secs(5);
        while frames.watch(&[(7, asked)]).expect("a sampler runs").frames[&7] != shown {
            assert!(Instant::now() < deadline, "the sampler read window 7");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(reads.load(Ordering::Relaxed) > 0);

        frames.stop();
        let after_stop = reads.load(Ordering::Relaxed);
        thread::sleep(READ_INTERVAL * 10);
        assert!(
            reads.load(Ordering::Relaxed) <= after_stop + 1,
            "the sampler stops reading once nothing is watched"
        );
    }

    /// Until the sampler has read a window, the frame a watch returns for it
    /// is only the one the engine asked for, and does not count as read.
    #[test]
    fn a_window_counts_as_read_only_once_the_sampler_has_read_it() {
        let answers = Arc::new(AtomicBool::new(false));
        let answering = answers.clone();
        let frames = ShownFrames::new(move |ids| {
            if answering.load(Ordering::Relaxed) {
                ids.iter()
                    .map(|&id| (id, IRect::new(0, 0, 100, 100)))
                    .collect()
            } else {
                Vec::new()
            }
        });
        let asked = [(7, IRect::new(0, 0, 100, 100))];
        let watch = |frames: &ShownFrames| frames.watch(&asked).expect("a sampler runs");
        watch(&frames);
        thread::sleep(READ_INTERVAL * 10);
        assert!(
            watch(&frames).unread.contains(&7),
            "the asked frame does not count as read"
        );

        answers.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while watch(&frames).unread.contains(&7) {
            assert!(Instant::now() < deadline, "the sampler read window 7");
            thread::sleep(Duration::from_millis(1));
        }
    }
}
