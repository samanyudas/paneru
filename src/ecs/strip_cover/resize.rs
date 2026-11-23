//! Column resizes under the picture cover (miri#509).
//!
//! A resize changes a column's width, and the columns beside it move to make
//! room. Apps take the new size and the new positions on their own time, so
//! live windows overlap, or show the desktop between them, while they
//! change. Under the cover, the real windows take their new frames at once,
//! out of sight, and each window's frame on the cover moves from the
//! window's old frame to its new one on the strip's ease, so the strip moves
//! as one sheet with its gaps kept.
//!
//! A picture never stretches. It stays at its own size from its frame's top
//! left corner: a frame that narrows crops it, and one that widens opens a
//! strip right of it in the colour of the window's surface there. Once the
//! window server shows a widened window at its new size, the engine takes a
//! fresh picture of it, which replaces the old one while the frame still
//! moves. Once the frames land and the windows rest at their new frames, the
//! cover comes down at once, with no fade, over the real windows.

use std::collections::HashMap;
use std::time::Duration;

use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Local, NonSendMut, Query, Res, ResMut};
use bevy::ecs::world::{Mut, Ref};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::debug;

use super::{
    ASK_TIMEOUT, Access, Asked, CoverPhotos, CoverScreenHandle, CoverSlide, CoverSubject,
    CoverSwipe, CoverWindows, LANDED_FRAMES, LANDING_TIMEOUT, PATH_STEP, SHOWN_GRACE, ShownStrips,
    StartedCovers, StripWindow, apply_cover_reply, close, cover_enabled, end_guard, scene_of,
    shows, strip_windows,
};
use crate::config::Config;
use crate::ecs::drag::DragState;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::pacing::ShownFrames;
use crate::ecs::slice_photos::Photo;
use crate::ecs::systems::AnimationStep;
use crate::ecs::workspace::spawn_snap_strip_guard;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, MissionControlActive, NativeFullscreenMarker,
    Position, RepositionMarker, ResizeMarker, ResizeMotion, Scrolling, Unmanaged, ease,
};
use crate::manager::{Display, Window, WindowManager};
use crate::platform::WinID;

/// One window under a resize's cover: where its picture starts and where it
/// heads, in global points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reframe {
    entity: Entity,
    id: WinID,
    from: IRect,
    to: IRect,
}

impl Reframe {
    /// The frame the picture shows `share` of the way along.
    fn at(&self, share: f64) -> IRect {
        #[allow(clippy::cast_possible_truncation)]
        let mix = |from: i32, to: i32| {
            (f64::from(from) + (f64::from(to) - f64::from(from)) * share).round() as i32
        };
        IRect {
            min: IVec2::new(
                mix(self.from.min.x, self.to.min.x),
                mix(self.from.min.y, self.to.min.y),
            ),
            max: IVec2::new(
                mix(self.from.max.x, self.to.max.x),
                mix(self.from.max.y, self.to.max.y),
            ),
        }
    }
}

/// Where a resize's cover is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    /// The pictures move.
    Moving,
    /// The pictures have landed: when the cover comes down at the latest,
    /// and for how many frames in a row the windows have been in place.
    Landing { deadline: Duration, settled: u8 },
}

/// A resize under the cover, on its strip.
#[derive(Component, Clone, Debug)]
pub(crate) struct CoverMorph {
    display: CGDirectDisplayID,
    /// The display's bounds and the strip's viewport when the resize began.
    bounds: IRect,
    viewport: IRect,
    /// The part of the display the cover hides: the visible area, with its
    /// edge padding.
    area: IRect,
    frames: Vec<Reframe>,
    /// Every window whose frame met `area` at any start or turn of the
    /// morph: the window server may still show it there.
    hid: Vec<Entity>,
    /// The size each window's picture on the cover shows it at: where it
    /// rested, until a fresh picture replaces it.
    pictured: Vec<(Entity, IVec2)>,
    /// Each window's active fresh-picture ask for its current target size,
    /// paired with that ask's generation. Cleared for a window whenever a
    /// turn changes its target size.
    refresh_ask: Vec<(Entity, IVec2, u32)>,
    /// Fresh pictures accepted for this turn whose presented frame is still
    /// wider than the new picture (for instance after a turn back from a
    /// wider frame). Unveiled as soon as the presented width fits the new
    /// picture so the fill behind the old picture is never dropped early.
    pending_unveil: Vec<(Entity, WinID, IVec2, Photo)>,
    /// Where the morph entered the curve, as a share of its time, how long it
    /// has run in animation time, and how long the whole curve lasts.
    entry: f64,
    elapsed: f64,
    duration: f64,
    /// How long the morph has run by the engine's count, for when Core
    /// Animation cannot tell how far it shows.
    counted: f64,
    stage: Stage,
    /// The snap guard that holds the real windows to their places.
    guard: Option<Entity>,
}

impl CoverMorph {
    /// Adds each window whose frame, before or after, meets the cover's
    /// area to the ones it hid.
    fn note_hidden(&mut self) {
        for frame in &self.frames {
            let meets = |rect: IRect| !rect.intersect(self.area).is_empty();
            if (meets(frame.from) || meets(frame.to)) && !self.hid.contains(&frame.entity) {
                self.hid.push(frame.entity);
            }
        }
    }

    pub(super) fn display(&self) -> CGDirectDisplayID {
        self.display
    }

    pub(super) fn guard(&self) -> Option<Entity> {
        self.guard
    }

    /// Each window whose frame heads wider than its picture on the cover,
    /// with its frame, that has no fresh-picture ask for this target size yet.
    fn outgrown(&self) -> impl Iterator<Item = &Reframe> {
        self.frames.iter().filter(|frame| {
            self.pictured(frame.entity)
                .is_some_and(|size| frame.to.width() > size.x)
                && !self
                    .refresh_ask
                    .iter()
                    .any(|&(entity, size, _)| entity == frame.entity && size == frame.to.size())
        })
    }

    /// The size window `entity`'s picture on the cover shows it at.
    fn pictured(&self, entity: Entity) -> Option<IVec2> {
        self.pictured
            .iter()
            .find(|(pictured, _)| *pictured == entity)
            .map(|(_, size)| *size)
    }

    /// How long the morph lasts.
    fn length(&self) -> f64 {
        self.duration * (1.0 - self.entry)
    }

    /// The share of the way the pictures have gone `at` seconds in.
    fn share_at(&self, at: f64) -> f64 {
        if self.duration <= 0.0 {
            return 1.0;
        }
        ease::share(self.entry, self.entry + (at / self.duration).max(0.0))
    }

    /// How far into the morph the pictures show `share` of the way along,
    /// in seconds: the share only grows, so each has one time.
    fn phase_of(&self, share: f64) -> f64 {
        let (mut low, mut high) = (0.0, self.length());
        for _ in 0..40 {
            let mid = f64::midpoint(low, high);
            if self.share_at(mid) < share {
                low = mid;
            } else {
                high = mid;
            }
        }
        high
    }

    /// The share at each [`PATH_STEP`] of the morph, ending on 1.
    fn shares(&self) -> Vec<f64> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (self.length() / PATH_STEP).ceil().max(1.0) as usize;
        let mut shares = (0..steps)
            .map(|step| {
                #[allow(clippy::cast_precision_loss)]
                let at = step as f64 * PATH_STEP;
                self.share_at(at)
            })
            .collect::<Vec<_>>();
        shares.push(1.0);
        shares
    }

    /// The share of the way the pictures show now, and how fast it grows,
    /// per second.
    fn progress(&self) -> (f64, f64) {
        if self.stage != Stage::Moving || self.elapsed >= self.length() {
            return (1.0, 0.0);
        }
        let dt = 1e-3;
        let share = self.share_at(self.elapsed);
        (share, (share - self.share_at(self.elapsed - dt)) / dt)
    }

    fn sent(&self) -> Vec<(WinID, IRect, IRect)> {
        self.frames
            .iter()
            .map(|frame| (frame.id, frame.from, frame.to))
            .collect()
    }
}

/// A strip whose windows rested at the end of the last pass: its origin, and
/// each tiled window's frame from that origin.
#[derive(Debug)]
struct Resting {
    origin: IVec2,
    frames: Vec<(Entity, IRect)>,
}

/// The strips that rested at the end of the last pass, so a resize can tell
/// where its windows showed before it.
#[derive(Resource, Debug, Default)]
pub(crate) struct RestingStrips(HashMap<Entity, Resting>);

type RestStrips<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static LayoutStrip, &'static Position),
    (
        With<LayoutStrip>,
        Without<Window>,
        Without<RepositionMarker>,
        Without<Scrolling>,
        Without<CoverSlide>,
        Without<CoverSwipe>,
        Without<CoverMorph>,
    ),
>;

/// Remembers each strip whose windows rest, after the pass wrote them.
pub(super) fn remember_resting(
    strips: RestStrips,
    windows: CoverWindows,
    config: Res<Config>,
    mut resting: ResMut<RestingStrips>,
) {
    if !cover_enabled(&config) {
        resting.0.clear();
        return;
    }
    resting.0 = strips
        .iter()
        .filter_map(|(entity, strip, position)| {
            let row = strip_windows(strip, &windows);
            row.iter().all(|window| window.at_rest).then(|| {
                let frames = row
                    .iter()
                    .map(|window| (window.entity, window.frame))
                    .collect();
                (
                    entity,
                    Resting {
                        origin: position.0,
                        frames,
                    },
                )
            })
        })
        .collect();
}

type MorphStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LayoutStrip,
        &'static mut Position,
        Option<Ref<'static, RepositionMarker>>,
        Option<&'static mut CoverMorph>,
        &'static ChildOf,
        Has<ActiveWorkspaceMarker>,
        Has<Scrolling>,
        Has<NativeFullscreenMarker>,
        Has<CoverSlide>,
        Has<CoverSwipe>,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

/// Where each window of `row` heads, in global points, with the strip's
/// origin at `origin`.
fn headings(row: &[StripWindow], origin: IVec2) -> Vec<(Entity, WinID, IRect)> {
    row.iter()
        .map(|window| {
            let to = IRect::from_corners(window.heading.min + origin, window.heading.max + origin);
            (window.entity, window.id, to)
        })
        .collect()
}

/// Gives the real windows of a resized strip their new frames at once,
/// under the cover: the strip takes its target, each resizing window its
/// new size, and a fresh snap guard keeps the layout from easing them
/// there: the one before may have timed out.
fn snap(
    strip: Entity,
    position: &mut Mut<Position>,
    target: IVec2,
    row: &[StripWindow],
    guard: Option<Entity>,
    commands: &mut Commands,
) -> Entity {
    end_guard(guard, commands);
    position.0 = target;
    // Every window of the strip takes its place again, from the new origin.
    position.set_changed();
    // The pictures ease, so the real windows take no ease of their own.
    commands
        .entity(strip)
        .remove::<(RepositionMarker, ResizeMotion)>();
    for window in row {
        let Ok(mut entity_commands) = commands.get_entity(window.entity) else {
            continue;
        };
        // The layout places every window again from the new origin, so a
        // move asked for before, such as the resize's own, is spent.
        entity_commands.try_remove::<(RepositionMarker, ResizeMotion)>();
        if let Some(size) = window.resize_to() {
            entity_commands
                .try_insert(Bounds(size))
                .try_remove::<ResizeMarker>();
        }
    }
    spawn_snap_strip_guard(strip, commands)
}

/// Starts a cover for each eligible resize, turns a covered resize that
/// gets new frames, and ends one the cover can no longer show.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn cover_resizes(
    mut strips: MorphStrips,
    shown_strips: ShownStrips,
    windows: CoverWindows,
    floating: Query<(&Window, &Unmanaged)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    mut photos: ResMut<CoverPhotos>,
    resting: Res<RestingStrips>,
    drag: Option<Res<DragState>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    time: Res<Time>,
    step: Res<AnimationStep>,
    mission_control: Res<MissionControlActive>,
    mut started: ResMut<StartedCovers>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
    started.0.clear();
    let Some(mut screen) = screen else {
        return;
    };
    let dragging = drag
        .as_ref()
        .is_some_and(|drag| drag.held_window().is_some());
    for (
        entity,
        strip,
        mut position,
        marker,
        morph,
        child,
        active,
        scrolling,
        fullscreen,
        sliding,
        swiping,
    ) in &mut strips
    {
        let row = strip_windows(strip, &windows);
        if morph.is_none() && row.iter().all(|window| window.resize.is_none()) {
            continue;
        }
        let Ok((display, dock)) = displays.get(child.parent()) else {
            continue;
        };
        let target = marker.as_ref().map_or(position.0, |marker| marker.0);
        let viewport = display.actual_display_bounds(dock, &config);
        let shown_here = active || shows(&window_manager, display.id(), &shown_strips, entity);

        if let Some(mut morph) = morph {
            if morph.stage == Stage::Moving {
                morph.counted += step.secs();
                // Core Animation runs on while the engine stalls, so the
                // morph's clock is where the screen shows it.
                morph.elapsed = match screen.0.morph_share(morph.display) {
                    Some(share) if morph.counted < morph.length() + SHOWN_GRACE => {
                        if share >= 1.0 {
                            morph.length()
                        } else {
                            morph.phase_of(share)
                        }
                    }
                    _ => morph.counted,
                };
            }
            let same_windows = row.len() == morph.frames.len()
                && row
                    .iter()
                    .zip(&morph.frames)
                    .all(|(window, frame)| window.entity == frame.entity);
            let aborted = !cover_enabled(&config)
                || !shown_here
                || display.id() != morph.display
                || display.bounds() != morph.bounds
                || viewport != morph.viewport
                || scrolling
                || dragging
                || mission_control.0
                || !same_windows
                || row.iter().any(|window| {
                    window.tabbed || window.turns_height() || window.asks_refused_width()
                });
            // A change that moves a window up or down, such as a stack or a
            // vertical strip move, is no resize: the real windows take it.
            let heading = headings(&row, target);
            let aborted = aborted
                || heading
                    .iter()
                    .zip(&morph.frames)
                    .any(|((.., to), frame)| !same_rows(*to, frame.to));
            if aborted {
                // The real windows are where the layout put them already, or
                // ease on from there.
                debug!("cover over resized strip {entity} ended early");
                for &(_, _, generation) in &morph.refresh_ask {
                    photos
                        .morph_asked
                        .retain(|asked| asked.generation != generation);
                    photos
                        .morph_replies
                        .retain(|reply| reply.generation != generation);
                }
                end_guard(morph.guard.take(), &mut commands);
                if !started.0.contains(&morph.display) {
                    screen.0.hide(morph.display);
                }
                commands.entity(entity).remove::<CoverMorph>();
                continue;
            }
            let turned = heading
                .iter()
                .zip(&morph.frames)
                .any(|((.., to), frame)| *to != frame.to);
            if turned {
                // New frames while covered, such as another resize or a
                // focus move: the pictures turn there from where they show,
                // at the speed they have, and the real windows go at once.
                let (share, rate) = morph.progress();
                let shown = morph
                    .frames
                    .iter()
                    .map(|frame| frame.at(share))
                    .collect::<Vec<_>>();
                let duration = ease::duration(&config);
                let entry = turn_entry(&morph.frames, &shown, &heading, rate, duration);
                // Drop any pending fresh-picture ask (and any reply that
                // arrived in this frame's Events pass) whose window changed
                // its target size on this turn.
                morph
                    .refresh_ask
                    .retain(|&(win_entity, ask_size, generation)| {
                        let unchanged = heading
                            .iter()
                            .find(|&&(e, _, _)| e == win_entity)
                            .is_some_and(|&(_, _, to)| to.size() == ask_size);
                        if !unchanged {
                            photos
                                .morph_asked
                                .retain(|asked| asked.generation != generation);
                            photos
                                .morph_replies
                                .retain(|reply| reply.generation != generation);
                        }
                        unchanged
                    });
                morph.pending_unveil.retain(|&(win_entity, _, size, _)| {
                    heading
                        .iter()
                        .find(|&&(e, _, _)| e == win_entity)
                        .is_some_and(|&(_, _, to)| to.size() == size)
                });
                // Drop any in-flight background or morph capture for windows
                // whose size is changing on this turn, so a capture taken at
                // the old size cannot land after `snap` resizes the window.
                for (frame, &(.., to)) in morph.frames.iter().zip(&heading) {
                    if frame.to.size() != to.size() {
                        photos.supersede(CoverSubject::Window(frame.id));
                    }
                }
                morph.frames = heading
                    .iter()
                    .zip(shown)
                    .map(|(&(entity, id, to), from)| Reframe {
                        entity,
                        id,
                        from,
                        to,
                    })
                    .collect();
                morph.note_hidden();
                morph.entry = entry;
                morph.elapsed = 0.0;
                morph.counted = 0.0;
                morph.duration = duration;
                morph.stage = if morph.frames.iter().all(|frame| frame.from == frame.to) {
                    Stage::Landing {
                        deadline: time.elapsed() + LANDING_TIMEOUT,
                        settled: 0,
                    }
                } else {
                    Stage::Moving
                };
                let guard = morph.guard.take();
                morph.guard = Some(snap(
                    entity,
                    &mut position,
                    target,
                    &row,
                    guard,
                    &mut commands,
                ));
                screen
                    .0
                    .morph(morph.display, &morph.sent(), &morph.shares(), PATH_STEP);
                debug!("cover over resized strip {entity} turned");
                continue;
            }
            if morph.stage == Stage::Moving && morph.elapsed >= morph.length() {
                debug!("cover over resized strip {entity} landed");
                morph.stage = Stage::Landing {
                    deadline: time.elapsed() + LANDING_TIMEOUT,
                    settled: 0,
                };
            }
            continue;
        }

        let refused = if !cover_enabled(&config) {
            Some("switched off")
        } else if !shown_here {
            Some("strip not shown")
        } else if sliding || swiping || scrolling {
            Some("strip moves")
        } else if fullscreen {
            Some("fullscreen")
        } else if dragging {
            Some("drag")
        } else if mission_control.0 {
            Some("Mission Control")
        } else if row.iter().any(StripWindow::turns_height) {
            Some("height changes")
        } else if row.iter().any(StripWindow::asks_refused_width) {
            Some("a width the window refused before")
        } else {
            None
        };
        if let Some(reason) = refused {
            debug!("cover skipped over resized strip {entity}: {reason}");
            continue;
        }
        let Some(rest) = resting.0.get(&entity) else {
            debug!("cover skipped over resized strip {entity}: it did not rest");
            continue;
        };
        let same_windows = rest.frames.len() == row.len()
            && rest
                .frames
                .iter()
                .zip(&row)
                .all(|((rested, _), window)| *rested == window.entity);
        if !same_windows {
            debug!("cover skipped over resized strip {entity}: windows changed");
            continue;
        }
        let heading = headings(&row, target);
        let frames = heading
            .iter()
            .zip(&rest.frames)
            .map(|(&(entity, id, to), (_, frame))| Reframe {
                entity,
                id,
                from: IRect::from_corners(frame.min + rest.origin, frame.max + rest.origin),
                to,
            })
            .collect::<Vec<_>>();
        if frames.iter().all(|frame| frame.from == frame.to) {
            continue;
        }
        if frames.iter().any(|frame| !same_rows(frame.from, frame.to)) {
            debug!("cover skipped over resized strip {entity}: a window moves up or down");
            continue;
        }
        // The pictures show where the windows rested.
        let before = row
            .iter()
            .zip(&rest.frames)
            .map(|(window, (_, frame))| StripWindow {
                id: window.id,
                entity: window.entity,
                frame: *frame,
                heading: window.heading,
                size: window.size,
                resize: window.resize,
                limits: window.limits,
                at_rest: true,
                tabbed: window.tabbed,
                focused: window.focused,
            })
            .collect();
        let scene = match scene_of(
            before,
            display,
            display.visible_bounds(dock),
            rest.origin,
            &floating,
            &photos,
        ) {
            Ok(scene) => scene,
            Err(reason) => {
                debug!("cover skipped over resized strip {entity}: {reason}");
                continue;
            }
        };
        let shown = scene.pictures.len();
        let area = scene.viewport;
        screen.0.show(&scene);
        started.0.push(display.id());
        // Supersede any in-flight background capture for windows whose size
        // changes here, so an old-size ask cannot capture the snapped window.
        for frame in &frames {
            if frame.to.size() != frame.from.size() {
                photos.supersede(CoverSubject::Window(frame.id));
            }
        }
        let duration = ease::duration(&config);
        let pictured = frames
            .iter()
            .map(|frame| (frame.entity, frame.from.size()))
            .collect();
        let mut morph = CoverMorph {
            display: display.id(),
            bounds: display.bounds(),
            viewport,
            area,
            frames,
            hid: Vec::new(),
            pictured,
            refresh_ask: Vec::new(),
            pending_unveil: Vec::new(),
            entry: 0.0,
            elapsed: 0.0,
            duration,
            counted: 0.0,
            stage: Stage::Moving,
            guard: None,
        };
        morph.note_hidden();
        morph.guard = Some(snap(
            entity,
            &mut position,
            target,
            &row,
            None,
            &mut commands,
        ));
        screen
            .0
            .morph(display.id(), &morph.sent(), &morph.shares(), PATH_STEP);
        debug!("cover over resized strip {entity}: {shown} pictures");
        commands.entity(entity).insert(morph);
    }
}

/// Whether two frames of a window span the same rows: a resize under the
/// cover moves windows only sideways.
fn same_rows(a: IRect, b: IRect) -> bool {
    a.min.y == b.min.y && a.max.y == b.max.y
}

/// Where a turn enters the curve: the edge that has furthest to go keeps
/// the speed it had, so the turn does not jolt.
fn turn_entry(
    before: &[Reframe],
    shown: &[IRect],
    heading: &[(Entity, WinID, IRect)],
    rate: f64,
    duration: f64,
) -> f64 {
    let edges = before
        .iter()
        .zip(shown)
        .zip(heading)
        .flat_map(|((frame, shown), (.., to))| {
            [
                (frame.from.min.x, frame.to.min.x, shown.min.x, to.min.x),
                (frame.from.max.x, frame.to.max.x, shown.max.x, to.max.x),
            ]
        });
    let Some((from, old_to, now, to)) = edges.max_by_key(|(_, _, now, to)| (to - now).abs()) else {
        return 0.0;
    };
    let speed = f64::from(old_to - from) * rate;
    ease::entry_for_speed(speed, f64::from(to - now), duration)
}

/// Whether the window server shows each window of `row` that the cover of
/// `morph` hides, before or after the resize, at the frame the layout
/// committed it to. A write call can return before its app shows the new
/// frame, and an app that is late can have its old frame read back as the
/// window's own, so the check goes by the layout's frame, and a window the
/// window server does not list is not there yet. Windows the cover never
/// hides cannot show an overlap as it comes down, and are left out to keep
/// the read short.
fn shows_committed(
    row: &[StripWindow],
    frames: &Query<(&Window, &Position, &Bounds)>,
    morph: &CoverMorph,
    shown: &ShownFrames,
) -> bool {
    let hidden = |rect: IRect| !rect.intersect(morph.area).is_empty();
    let committed = row
        .iter()
        .filter_map(|window| {
            let (found, position, bounds) = frames.get(window.entity).ok()?;
            let cell = IRect::from_corners(position.0, position.0 + bounds.0);
            let frame = found.margins().shown(cell);
            (hidden(frame) || morph.hid.contains(&window.entity)).then_some((found.id(), frame))
        })
        .collect::<Vec<_>>();
    let ids = committed.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let read = shown.watch_morph(&ids);
    committed.iter().all(|(id, want)| {
        read.iter()
            .find(|(shown, _)| shown == id)
            .is_some_and(|(_, frame)| close(frame.min, want.min) && close(frame.max, want.max))
    })
}

/// Takes each resize's cover down at once when its pictures have landed
/// and its windows rest at their new frames, or when it has waited
/// [`LANDING_TIMEOUT`]. It never fades: a fade over the real windows shows
/// the old picture and the new layout as one blurred double image.
pub(super) fn lift_morphs(
    mut strips: Query<(Entity, &LayoutStrip, &mut CoverMorph)>,
    windows: CoverWindows,
    frames: Query<(&Window, &Position, &Bounds)>,
    shown_frames: Option<Res<ShownFrames>>,
    time: Res<Time>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let now = time.elapsed();
    for (entity, strip, mut morph) in &mut strips {
        let Stage::Landing { deadline, settled } = morph.stage else {
            continue;
        };
        let row = strip_windows(strip, &windows);
        let in_place = row.iter().all(|window| window.at_rest)
            && shown_frames
                .as_ref()
                .is_none_or(|shown| shows_committed(&row, &frames, &morph, shown));
        let settled = if in_place { settled + 1 } else { 0 };
        if settled < LANDED_FRAMES && now < deadline {
            morph.stage = Stage::Landing { deadline, settled };
            continue;
        }
        let late = if settled >= LANDED_FRAMES {
            ""
        } else {
            " at its time limit"
        };
        debug!("cover over resized strip {entity} lifted{late}");
        screen.0.hide(morph.display);
        end_guard(morph.guard, &mut commands);
        commands.entity(entity).remove::<CoverMorph>();
    }
}

/// Asks for a fresh picture of each window a resize's cover shows widening
/// past its picture, once the window server shows the window at its new
/// frame, so the frame opens onto the window's new layout rather than onto
/// the fill. It asks once a window and size, and the picture arrives
/// through [`keep_cover_photos`](super::keep_cover_photos), which hands it
/// to [`unveil`].
#[allow(clippy::too_many_arguments)]
pub(super) fn refresh_morph_pictures(
    mut morphs: Query<&mut CoverMorph>,
    frames: Query<(&Window, &Position, &Bounds)>,
    shown_frames: Option<Res<ShownFrames>>,
    mut photos: ResMut<CoverPhotos>,
    config: Res<Config>,
    time: Res<Time>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut watching_morph: Local<bool>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let now = time.elapsed();
    if !photos.morph_asked.is_empty() {
        photos
            .morph_asked
            .retain(|asked| now.saturating_sub(asked.at) < ASK_TIMEOUT);
    }
    if morphs.is_empty() {
        if *watching_morph {
            if let Some(shown) = shown_frames.as_ref() {
                shown.stop_morph();
            }
            *watching_morph = false;
        }
        return;
    }
    *watching_morph = true;
    if !cover_enabled(&config) || photos.access != Access::Allowed {
        return;
    }
    for mut morph in &mut morphs {
        let ready = morph
            .outgrown()
            .filter_map(|frame| {
                let (window, position, bounds) = frames.get(frame.entity).ok()?;
                let cell = IRect::from_corners(position.0, position.0 + bounds.0);
                // The layout's frame, and no write on its way to the app.
                let committed = window.margins().shown(cell);
                (!window.write_pending()
                    && close(committed.min, frame.to.min)
                    && close(committed.max, frame.to.max))
                .then_some((frame.entity, frame.id, frame.to))
            })
            .collect::<Vec<_>>();
        if ready.is_empty() {
            continue;
        }
        // Sample off the main thread via the background shown-frame sampler.
        let ids = ready.iter().map(|(_, id, _)| *id).collect::<Vec<_>>();
        let read = shown_frames.as_ref().map(|shown| shown.watch_morph(&ids));
        for (entity, id, to) in ready {
            let shows = read.as_ref().is_none_or(|read| {
                read.iter().any(|(shown, frame)| {
                    *shown == id && close(frame.min, to.min) && close(frame.max, to.max)
                })
            });
            if !shows {
                continue;
            }
            photos.generation = photos.generation.wrapping_add(1);
            let generation = photos.generation;
            let subject = CoverSubject::Window(id);
            photos.supersede(subject);
            screen
                .0
                .photograph_window(id, to.size(), morph.display, generation);
            photos.morph_asked.push(Asked {
                subject,
                generation,
                at: now,
                size: to.size(),
                excluding: Vec::new(),
            });
            morph.refresh_ask.retain(|&(e, ..)| e != entity);
            morph.refresh_ask.push((entity, to.size(), generation));
            debug!(
                "fresh cover picture of window {id} asked at {}x{}",
                to.width(),
                to.height()
            );
        }
    }
}

/// Validates and applies morph capture replies staged in this frame's `Events`
/// pass, after [`cover_resizes`] has resolved any turn or abort for this
/// frame. Replies superseded by a turn or abort have already been discarded;
/// valid replies update the cache (only when the window still exists and heads
/// to that size if no live morph owns it) and unveil on any live morph once
/// its presented frame fits the new picture.
pub(super) fn apply_morph_replies(
    mut morphs: Query<&mut CoverMorph>,
    windows: Query<(&Window, &Bounds, Option<&ResizeMarker>)>,
    mut photos: ResMut<CoverPhotos>,
    mut screen: Option<NonSendMut<CoverScreenHandle>>,
) {
    let replies = std::mem::take(&mut photos.morph_replies);
    for reply in replies {
        let mut owned_by_morph = false;
        let mut accepted_for_morph = false;
        for mut morph in &mut morphs {
            let Some(frame) = morph
                .frames
                .iter()
                .find(|frame| frame.id == reply.id)
                .copied()
            else {
                continue;
            };
            owned_by_morph = true;
            let matches_ask = morph
                .refresh_ask
                .iter()
                .any(|&(entity, ask_size, ask_gen)| {
                    entity == frame.entity
                        && ask_gen == reply.generation
                        && close(ask_size, reply.size)
                });
            if !matches_ask || !close(frame.to.size(), reply.size) {
                continue;
            }
            accepted_for_morph = true;
            if let Ok(photo) = &reply.result
                && morph
                    .pictured(frame.entity)
                    .is_some_and(|pictured| frame.to.width() > pictured.x)
            {
                let fits = frame.to.size();
                morph.pending_unveil.retain(|(e, ..)| *e != frame.entity);
                morph
                    .pending_unveil
                    .push((frame.entity, reply.id, fits, photo.clone()));
            }
        }
        if owned_by_morph {
            if !accepted_for_morph {
                continue;
            }
        } else {
            // Without a live morph (e.g. after a normal lift), validate
            // against the window's current resolved target size so a late
            // reply cannot overwrite a usable cache entry after a subsequent
            // uncovered resize.
            let still_matches = windows
                .iter()
                .find(|(window, ..)| window.id() == reply.id)
                .is_some_and(|(window, bounds, resize)| {
                    let cell = resize.map_or(bounds.0, |marker| marker.0);
                    let shown_target = window
                        .margins()
                        .shown(IRect::from_corners(IVec2::ZERO, cell))
                        .size();
                    close(shown_target, reply.size)
                });
            if !still_matches {
                continue;
            }
        }
        apply_cover_reply(
            &mut photos,
            screen.as_deref_mut(),
            CoverSubject::Window(reply.id),
            reply.size,
            Vec::new(),
            &reply.result,
            reply.taken,
        );
    }

    if let Some(mut screen) = screen {
        unveil_ready_pictures(&mut morphs, screen.0.as_mut());
    }
}

/// Unveils each accepted fresh picture whose presented frame now fits it
/// (never replacing early while a turn back from a wider frame is still
/// wider than the new picture).
fn unveil_ready_pictures(morphs: &mut Query<&mut CoverMorph>, screen: &mut dyn super::CoverScreen) {
    for mut morph in morphs.iter_mut() {
        if morph.pending_unveil.is_empty() {
            continue;
        }
        let share = morph.progress().0;
        let pending = std::mem::take(&mut morph.pending_unveil);
        for (entity, id, fits, photo) in pending {
            let Some(frame) = morph
                .frames
                .iter()
                .find(|frame| frame.entity == entity)
                .copied()
            else {
                continue;
            };
            let shown_width = frame.at(share).width();
            if shown_width > fits.x {
                morph.pending_unveil.push((entity, id, fits, photo));
                continue;
            }
            if morph
                .pictured(entity)
                .is_some_and(|pictured| fits.x > pictured.x)
            {
                if screen.replace_picture(morph.display, id, fits, &photo) {
                    if let Some((_, pictured)) = morph
                        .pictured
                        .iter_mut()
                        .find(|(pictured_entity, _)| *pictured_entity == entity)
                    {
                        *pictured = fits;
                    }
                    debug!("cover over a resize shows the fresh picture of window {id}");
                } else {
                    morph.pending_unveil.push((entity, id, fits, photo));
                }
            }
        }
    }
}
