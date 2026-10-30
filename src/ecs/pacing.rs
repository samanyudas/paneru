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
//! window. The slowest window then sets the pace for every window ahead of
//! it. A window that lags into the window ahead only overlaps it, which
//! shows no desktop, so nothing waits on the windows ahead of the slow one.
//! A photo sliding at a shared edge is held the same way.
//!
//! The windows ahead of a window whose app has shown every move of a whole
//! motion in time follow where the engine puts it, so a row of quick apps
//! moves together. Any other window paces from where it is shown, and so do
//! the windows of an app that focus moves to or from another app, which
//! can turn slow.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use bevy::ecs::entity::{Entity, EntityHashMap};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::query::Added;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Local, Query, Res, ResMut};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use tracing::{debug, warn};

use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::shared_edge::Slide;
use crate::ecs::systems::{ease_out_factor, ease_step};
use crate::ecs::{FocusedMarker, Position, PresentedOrigin, RepositionMarker};
use crate::manager::Window;
use crate::manager::window_writer::LANDING_TIMEOUT;
use crate::platform::WinID;

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
    fn watch(&self, asked: &[(WinID, IRect)]) -> Option<HashMap<WinID, IRect>> {
        let mut watch = lock(&self.shared.watch);
        watch.ids.clear();
        watch.ids.extend(asked.iter().map(|&(id, _)| id));
        for &(id, frame) in asked {
            watch.shown.entry(id).or_insert(frame);
        }
        if !self.sampled {
            let frames = (self.shared.read)(&watch.ids);
            watch.shown.extend(frames);
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
        Some(
            asked
                .iter()
                .filter_map(|&(id, _)| watch.shown.get(&id).map(|&frame| (id, frame)))
                .collect(),
        )
    }

    fn stop(&self) {
        let mut watch = lock(&self.shared.watch);
        watch.ids.clear();
        watch.shown.clear();
        watch.generation += 1;
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
                watch.shown.extend(frames);
            }
            drop(watch);
            thread::sleep(READ_INTERVAL);
        }
    }
}

/// How far each sliding photo may go this frame, as [`pace_rows`] holds it
/// with the rest of its row. The photo steps after that.
#[derive(Resource, Default)]
pub(crate) struct SlideHolds(EntityHashMap<i32>);

impl SlideHolds {
    /// `next`, the step of the photo of `entity` from `at`, held where
    /// pacing stops it this frame.
    pub(crate) fn limit(&self, entity: Entity, at: IVec2, next: IVec2) -> IVec2 {
        let Some(&bound) = self.0.get(&entity) else {
            return next;
        };
        next.with_x(if next.x < at.x {
            next.x.max(bound).min(at.x)
        } else {
            next.x.min(bound).max(at.x)
        })
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

/// What the engine has seen of one row window's app taking its moves.
#[derive(Default)]
pub(super) struct Tracked {
    /// Where the engine had asked for the window as of the previous frame.
    asked: Option<i32>,
    /// The app showed each of the engine's moves by the next frame through
    /// a whole motion, and has not been late since, nor gained or lost focus
    /// to another app, so it takes this frame's move about as soon as the
    /// windows ahead take theirs. Kept across motions, so a slow app is
    /// paced from the first step of the next one.
    prompt: bool,
    /// This motion, the app has shown a move by the next frame.
    on_time: bool,
    /// This motion, the window has shown away from where the engine asked.
    late_this_motion: bool,
    /// Where the window shows away from where the engine asked for it, and
    /// since when it has shown no progress.
    late: Option<(i32, Duration)>,
}

impl Tracked {
    /// Takes in that the engine asked for the window at `asked` last frame
    /// and the window server shows it at `shown`, if it lists it.
    fn see(&mut self, asked: i32, shown: Option<i32>, now: Duration) -> Pacer {
        let before = self.asked.replace(asked);
        match shown {
            Some(x) if x != asked => {
                self.prompt = false;
                self.late_this_motion = true;
                let since = match self.late {
                    Some((at, since)) if at == x => since,
                    _ => {
                        self.late = Some((x, now));
                        now
                    }
                };
                if now.saturating_sub(since) >= LANDING_TIMEOUT {
                    Pacer::Asked
                } else {
                    Pacer::Shown(x)
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
    Shown(i32),
}

/// One window of a moving row, in engine frame coordinates.
struct Member {
    entity: Entity,
    id: WinID,
    /// Where the animation puts the window this frame.
    eased: i32,
    /// Where pacing lets the window go this frame.
    x: i32,
    target: i32,
    /// Where the engine asked for the window last frame.
    asked: i32,
    pacer: Pacer,
    paced: bool,
    /// The window's photo slides it at a shared edge: pacing holds the
    /// photo, not the window.
    slides: bool,
}

impl Member {
    /// How far the window is from its target, as the windows next to it
    /// see it.
    fn lag(&self) -> i32 {
        match self.pacer {
            Pacer::Asked => self.x - self.target,
            Pacer::Shown(x) => x - self.target,
        }
    }

    /// Keeps a window that moves left at `bound` or right of it, but never
    /// further right than where the engine last asked for it.
    fn hold_right_of(&mut self, bound: i32) {
        if self.paced && self.eased < self.asked {
            self.x = self.x.max(bound).min(self.asked);
        }
    }

    /// Keeps a window that moves right at `bound` or left of it, but never
    /// further left than where the engine last asked for it.
    fn hold_left_of(&mut self, bound: i32) {
        if self.paced && self.eased > self.asked {
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
        Option<&'static RepositionMarker>,
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
    apps: Query<&ChildOf>,
    shown_frames: Option<Res<ShownFrames>>,
    strips: Query<&LayoutStrip>,
    mut windows: RowWindows,
    time: Res<Time>,
    config: Res<Config>,
    mut slide_holds: ResMut<SlideHolds>,
    mut tracked: Local<EntityHashMap<Tracked>>,
    mut watching: Local<bool>,
) {
    slide_holds.0.clear();
    let focused = focused.iter().collect::<Vec<_>>();
    let unfocused = unfocused.read().collect::<Vec<_>>();
    forget_switched_apps(&mut tracked, &focused, &unfocused, &apps);
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

    let now = time.elapsed();
    let t = ease_out_factor(config.animation_speed(), time.delta_secs_f64());
    for row in rows {
        let mut row = row
            .into_iter()
            .map(|column| {
                column
                    .into_iter()
                    .filter_map(|entity| {
                        let (window, position, to, slide) = windows.get(entity).ok()?;
                        let id = window.id();
                        if let Some((at, heading_to)) =
                            slide.and_then(|(slide, presented)| heading(slide, presented.0))
                        {
                            // A photo shows at once where it steps to this frame.
                            let (step, _) = ease_step(at, heading_to, t);
                            return Some(Member {
                                entity,
                                id,
                                eased: step.x,
                                x: step.x,
                                target: heading_to.x,
                                asked: at.x,
                                pacer: Pacer::Asked,
                                paced: true,
                                slides: true,
                            });
                        }
                        let asked = window.frame().min.x;
                        let shown_x = shown
                            .get(&id)
                            .map(|&frame| window.margins().tracked(frame).min.x);
                        Some(Member {
                            entity,
                            id,
                            eased: position.0.x,
                            x: position.0.x,
                            target: to.map_or(position.0.x, |to| to.0.x),
                            asked,
                            pacer: tracked.entry(entity).or_default().see(asked, shown_x, now),
                            paced: to.is_some() && slide.is_none(),
                            slides: false,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        pace_row(&mut row);

        for member in row
            .iter()
            .flatten()
            .filter(|member| member.x != member.eased)
        {
            if member.slides {
                debug!(
                    "pacing holds the photo of window {} at x {} on its way to {}",
                    member.id, member.x, member.target
                );
                slide_holds.0.insert(member.entity, member.x);
            } else if let Ok((_, mut position, ..)) = windows.get_mut(member.entity) {
                debug!(
                    "pacing holds window {} at x {} short of {} on its way to {}",
                    member.id, member.x, member.eased, member.target
                );
                position.0.x = member.x;
            }
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
    focused: &[Entity],
    unfocused: &[Entity],
    apps: &Query<&ChildOf>,
) {
    let app = |entity: &Entity| apps.get(*entity).ok().map(ChildOf::parent);
    let gained = focused.iter().filter_map(app).collect::<Vec<_>>();
    let lost = unfocused.iter().filter_map(app).collect::<Vec<_>>();
    let switched = gained
        .iter()
        .filter(|app| !lost.contains(app))
        .chain(lost.iter().filter(|app| !gained.contains(app)))
        .collect::<Vec<_>>();
    if !switched.is_empty() {
        tracked.retain(|entity, _| app(entity).is_none_or(|app| !switched.contains(&&app)));
    }
}

/// The windows of each strip that has a window on its way somewhere, column
/// by column, left to right.
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
            row.iter()
                .flatten()
                .any(|&entity| windows.get(entity).is_ok_and(|(_, _, to, _)| to.is_some()))
        })
        .collect()
}

/// Holds the columns of `row`, left to right, from behind each one in the
/// direction of motion to the front, so each column follows where the
/// column behind it is this frame.
fn pace_row(row: &mut [Vec<Member>]) {
    // A column right of its target has yet to move left, so the column left
    // of it, ahead, may get no closer to its own.
    for index in (0..row.len().saturating_sub(1)).rev() {
        let lag = row[index + 1].iter().map(Member::lag).max();
        if let Some(lag) = lag.filter(|&lag| lag > 0) {
            for member in &mut row[index] {
                member.hold_right_of(member.target + lag);
            }
        }
    }
    for index in 1..row.len() {
        let lag = row[index - 1].iter().map(Member::lag).min();
        if let Some(lag) = lag.filter(|&lag| lag < 0) {
            for member in &mut row[index] {
                member.hold_left_of(member.target + lag);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        assert_eq!(first[&7], asked, "seeded with the asked frame");

        let deadline = Instant::now() + Duration::from_secs(5);
        while frames.watch(&[(7, asked)]).expect("a sampler runs")[&7] != shown {
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
}
