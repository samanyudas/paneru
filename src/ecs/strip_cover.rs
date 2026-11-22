//! The picture cover for strip moves (miri#509).
//!
//! Apps take the positions the engine writes on their own time, so the
//! windows of one strip move out of step and the motion looks choppy. With
//! `slide_window_pictures` on, a keyboard strip move instead slides pictures
//! of the strip's windows on a cover over the display, along the same curve
//! the windows would follow, while the real windows stay put. Once the
//! pictures land, the real windows move to their places in one step under
//! the cover, and the cover comes off when they are there, or after
//! [`LANDING_TIMEOUT`].
//!
//! The engine keeps a picture of every window of each display's active strip
//! and one of each display's desktop without those windows, refreshed one at
//! a time in the background, so a move never waits for a capture. A move
//! that lacks a picture it would show, or that the cover would get wrong,
//! moves the real windows as before.

use std::collections::HashMap;
use std::time::Duration;

use bevy::app::{App, PostUpdate, Update};
use bevy::ecs::change_detection::DetectChanges;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use bevy::ecs::world::Ref;
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::debug;

use crate::config::Config;
use crate::ecs::drag::DragState;
use crate::ecs::layout::{Column, LayoutStrip, ensure_visible_in_strip, position_layout_strips};
use crate::ecs::slice_photos::{Photo, PhotoError};
use crate::ecs::systems::AnimationStep;
use crate::ecs::workspace::spawn_snap_strip_guard;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition,
    NativeFullscreenMarker, Position, PostUpdateSet, RepositionMarker, ResizeMarker, Scrolling,
    Unmanaged, UpdateSet, ease, systems,
};
use crate::events::Event;
use crate::manager::{Display, Window};
use crate::platform::WinID;

/// How long the cover stays up after the pictures land, at most, for windows
/// whose apps are slow to take their places.
const LANDING_TIMEOUT: Duration = Duration::from_millis(350);

/// How many frames in a row the windows must have taken their writes before
/// the cover comes off: the window server shows a moved window a frame later.
const LANDED_FRAMES: u8 = 2;

/// How often the focused window's picture is refreshed, so typing shows.
const FOCUSED_REFRESH: Duration = Duration::from_secs(1);

/// How often every other window's picture is refreshed.
const WINDOW_REFRESH: Duration = Duration::from_secs(6);

/// How often each display's desktop picture is refreshed.
const DESKTOP_REFRESH: Duration = Duration::from_secs(20);

/// How long a capture may take before the engine asks for the next one.
const ASK_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a window whose picture came out blank waits before another try.
const BLANK_RETRY: Duration = Duration::from_secs(6);

/// How often the engine asks macOS whether it may still take pictures.
const ACCESS_CHECK: Duration = Duration::from_secs(2);

/// How far past the ends of a move the cover shows pictures, as a share of
/// the viewport, for a slide that swings a little past its target.
const SWING_MARGIN: i32 = 8;

/// The step between the points of a slide's path, in seconds: twice a
/// 60 Hz frame, so a 120 Hz display still gets a point per frame.
const PATH_STEP: f64 = 1.0 / 120.0;

/// What a picture shows: one window, or one display's desktop without the
/// windows of its active strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CoverSubject {
    Window(WinID),
    Desktop(CGDirectDisplayID),
}

/// What a cover shows when it goes up.
pub(crate) struct CoverScene<'a> {
    pub display: CGDirectDisplayID,
    /// The whole display, in global points, where the desktop picture goes.
    pub display_bounds: IRect,
    /// The part of the display the cover covers: where the strip shows.
    pub viewport: IRect,
    pub desktop: &'a Photo,
    /// Where the strip's origin is, in global points.
    pub strip: IVec2,
    /// Each window's picture and the frame it shows, from the strip's origin.
    pub pictures: Vec<(WinID, IRect, &'a Photo)>,
}

/// Shows the covers and takes their pictures.
/// [`CoverScreenOS`](crate::cover_screen::CoverScreenOS) does it on screen,
/// and tests record the calls.
pub(crate) trait CoverScreen {
    /// Whether macOS lets the engine take pictures of other apps' windows.
    /// Never asks the user.
    fn may_photograph(&self) -> bool;
    /// Starts a picture of the whole of `window`, which shows `size` points,
    /// at the scale of `display`. It arrives as [`Event::CoverPhoto`].
    fn photograph_window(
        &mut self,
        window: WinID,
        size: IVec2,
        display: CGDirectDisplayID,
        generation: u32,
    );
    /// Starts a picture of `display` without the windows `excluding` and
    /// without the engine's own windows. It arrives as [`Event::CoverPhoto`].
    fn photograph_desktop(
        &mut self,
        display: CGDirectDisplayID,
        excluding: &[WinID],
        generation: u32,
    );
    /// Puts up the cover of `scene.display`, or replaces what it shows.
    fn show(&mut self, scene: &CoverScene);
    /// Moves the strip on the cover of `display` along `path`, one strip x
    /// in global points every `step` seconds from now. It rests at the last.
    fn slide(&mut self, display: CGDirectDisplayID, path: &[f64], step: f64);
    /// Takes the cover of `display` down.
    fn hide(&mut self, display: CGDirectDisplayID);
}

/// The cover screen the engine draws with, as a non-send resource.
pub(crate) struct CoverScreenHandle(pub Box<dyn CoverScreen>);

/// A picture the engine keeps.
#[derive(Debug)]
struct Kept {
    photo: Photo,
    /// The size the window showed, or the display's size.
    size: IVec2,
    taken: Duration,
    /// For a desktop, the windows it leaves out.
    excluding: Vec<WinID>,
}

/// A picture on its way.
#[derive(Debug)]
struct Asked {
    subject: CoverSubject,
    generation: u32,
    at: Duration,
    size: IVec2,
    excluding: Vec<WinID>,
}

/// Whether the engine may take pictures for the cover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Access {
    /// Unknown until the first check.
    #[default]
    Unchecked,
    Allowed,
    /// The switch is off, or macOS does not allow pictures.
    Off,
    /// macOS refused a picture while the check said yes. The cover stays off
    /// until the check's answer changes.
    Refused,
}

/// The pictures the engine keeps for the cover.
#[derive(Resource, Debug, Default)]
pub(crate) struct CoverPhotos {
    windows: HashMap<WinID, Kept>,
    desktops: HashMap<CGDirectDisplayID, Kept>,
    /// Windows whose last picture came out blank, and when to try again.
    blank_until: HashMap<WinID, Duration>,
    asked: Option<Asked>,
    generation: u32,
    access: Access,
    checked_at: Option<Duration>,
}

impl CoverPhotos {
    fn window(&self, id: WinID, size: IVec2) -> Option<&Photo> {
        self.windows
            .get(&id)
            .filter(|kept| close(kept.size, size))
            .map(|kept| &kept.photo)
    }

    fn desktop(&self, display: CGDirectDisplayID, excluding: &[WinID]) -> Option<&Photo> {
        self.desktops
            .get(&display)
            .filter(|kept| excluding.iter().all(|id| kept.excluding.contains(id)))
            .map(|kept| &kept.photo)
    }

    fn clear(&mut self) {
        self.windows.clear();
        self.desktops.clear();
        self.blank_until.clear();
        self.asked = None;
    }
}

/// Whether two sizes match to within the rounding of a capture.
fn close(a: IVec2, b: IVec2) -> bool {
    (a - b).abs().max_element() <= 2
}

/// A strip move under the cover.
#[derive(bevy::ecs::component::Component, Clone, Copy, Debug)]
pub(crate) struct CoverSlide {
    display: CGDirectDisplayID,
    /// Where the strip origin starts, in global points, and where it heads.
    from: f64,
    target: IVec2,
    /// Where the slide entered the curve, as a share of its time.
    entry: f64,
    /// How long the slide has run, in animation time, which leaves out the
    /// engine's idle sleep, and how long the whole curve lasts. A slide that
    /// entered the curve partway lasts only the rest of it.
    elapsed: f64,
    duration: f64,
    /// Once the pictures land: when the cover comes off at the latest, and
    /// for how many frames in a row the windows have been in place.
    landing: Option<(Duration, u8)>,
}

impl CoverSlide {
    /// Where the strip shows `at` seconds into the slide.
    fn x_at(&self, at: f64) -> f64 {
        let to = f64::from(self.target.x);
        if self.duration <= 0.0 {
            return to;
        }
        let s = (at / self.duration).max(0.0);
        self.from + (to - self.from) * ease::share(self.entry, self.entry + s)
    }

    /// How fast the strip moves `at` seconds into the slide, in points per
    /// second.
    fn speed_at(&self, at: f64) -> f64 {
        let dt = 1e-3;
        (self.x_at(at) - self.x_at(at - dt)) / dt
    }

    /// How long the slide lasts.
    fn length(&self) -> f64 {
        self.duration * (1.0 - self.entry)
    }

    /// The strip x at each [`PATH_STEP`] of the slide, ending on its target.
    fn path(&self) -> Vec<f64> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (self.length() / PATH_STEP).ceil().max(1.0) as usize;
        let mut path = (0..steps)
            .map(|step| {
                #[allow(clippy::cast_precision_loss)]
                let at = step as f64 * PATH_STEP;
                self.x_at(at)
            })
            .collect::<Vec<_>>();
        path.push(f64::from(self.target.x));
        path
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<CoverPhotos>()
        .add_systems(
            Update,
            keep_cover_photos
                .in_set(UpdateSet::Events)
                .run_if(on_message::<Event>),
        )
        .add_systems(
            Update,
            cover_strip_moves
                .in_set(UpdateSet::Layout)
                .after(ensure_visible_in_strip)
                .before(position_layout_strips),
        )
        .add_systems(
            PostUpdate,
            (lift_covers, refresh_cover_photos)
                .chain()
                .in_set(PostUpdateSet::Animate)
                .after(systems::commit_window_frame),
        );
}

/// Whether the cover may show at all: the switch is on and windows animate.
fn cover_enabled(config: &Config) -> bool {
    config.slide_window_pictures() && config.options().animation_speed.is_some()
}

/// Keeps each picture that arrives for the ask it answers.
fn keep_cover_photos(
    mut messages: MessageReader<Event>,
    mut photos: ResMut<CoverPhotos>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    time: Res<Time>,
) {
    for event in messages.read() {
        let Event::CoverPhoto {
            subject,
            generation,
            result,
        } = event
        else {
            continue;
        };
        let Some(asked) = photos
            .asked
            .take_if(|asked| asked.generation == *generation && asked.subject == *subject)
        else {
            continue;
        };
        match result {
            Ok(photo) => {
                let kept = Kept {
                    photo: photo.clone(),
                    size: asked.size,
                    taken: time.elapsed(),
                    excluding: asked.excluding,
                };
                match *subject {
                    CoverSubject::Window(id) => {
                        photos.blank_until.remove(&id);
                        photos.windows.insert(id, kept);
                    }
                    CoverSubject::Desktop(display) => {
                        photos.desktops.insert(display, kept);
                    }
                }
            }
            Err(PhotoError::Blank) => {
                if let CoverSubject::Window(id) = *subject {
                    debug!("cover picture of window {id} is blank");
                    photos.blank_until.insert(id, time.elapsed() + BLANK_RETRY);
                }
            }
            Err(PhotoError::WindowGone) => {
                if let CoverSubject::Window(id) = *subject {
                    photos.windows.remove(&id);
                }
            }
            Err(PhotoError::Refused) => {
                debug!("cover pictures refused by macOS");
                if screen
                    .as_ref()
                    .is_some_and(|screen| screen.0.may_photograph())
                {
                    photos.access = Access::Refused;
                }
                photos.clear();
            }
            Err(PhotoError::Failed) => {}
        }
    }
}

/// A tiled window of a strip, with the frame it shows from the strip's
/// origin.
struct StripWindow {
    id: WinID,
    entity: Entity,
    frame: IRect,
    at_rest: bool,
    tabbed: bool,
}

type CoverWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static Window,
        &'static LayoutPosition,
        &'static Bounds,
        Option<&'static Unmanaged>,
        Has<RepositionMarker>,
        Has<ResizeMarker>,
        Has<FocusedMarker>,
    ),
>;

/// The tiled windows of `strip`.
fn strip_windows(strip: &LayoutStrip, windows: &CoverWindows) -> Vec<StripWindow> {
    let tabbed = strip
        .columns()
        .flat_map(Column::window_iter)
        .filter(|entity| strip.tab_group(*entity).is_some())
        .collect::<Vec<_>>();
    strip
        .all_windows()
        .into_iter()
        .filter_map(|entity| {
            let (window, layout, bounds, unmanaged, moving, resizing, _) =
                windows.get(entity).ok()?;
            if unmanaged.is_some() {
                return None;
            }
            let cell = IRect::from_corners(layout.0, layout.0 + bounds.0);
            Some(StripWindow {
                id: window.id(),
                entity,
                frame: window.margins().shown(cell),
                at_rest: !moving && !resizing && !window.write_pending(),
                tabbed: tabbed.contains(&entity),
            })
        })
        .collect()
}

/// Whether a window `frame` from the strip origin shows in `viewport` with
/// the strip anywhere between `lo` and `hi`.
fn shows_between(frame: IRect, lo: i32, hi: i32, viewport: IRect) -> bool {
    frame.max.x + hi > viewport.min.x && frame.min.x + lo < viewport.max.x
}

type CoveredStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Ref<'static, LayoutStrip>,
        &'static mut Position,
        Option<Ref<'static, RepositionMarker>>,
        Option<&'static mut CoverSlide>,
        &'static ChildOf,
        Has<ActiveWorkspaceMarker>,
        Has<Scrolling>,
        Has<NativeFullscreenMarker>,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

/// Starts a cover for each eligible strip move, steers a cover whose strip
/// gets a new target, and lands the real windows once the pictures do.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn cover_strip_moves(
    mut strips: CoveredStrips,
    windows: CoverWindows,
    floating: Query<(&Window, &Unmanaged)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    photos: Res<CoverPhotos>,
    drag: Option<Res<DragState>>,
    config: Res<Config>,
    time: Res<Time>,
    step: Res<AnimationStep>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let dragging = drag
        .as_ref()
        .is_some_and(|drag| drag.held_window().is_some());
    for (entity, strip, mut position, marker, slide, child, active, scrolling, fullscreen) in
        &mut strips
    {
        let Ok((display, dock)) = displays.get(child.parent()) else {
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        let target = marker.as_ref().map(|marker| marker.0);
        let fresh = marker.as_ref().is_some_and(DetectChanges::is_changed);

        if let Some(mut slide) = slide {
            if slide.landing.is_none() {
                slide.elapsed += step.secs();
            }
            // Anything but a new target for the strip ends the cover: the
            // windows go where the slide headed, at once.
            let aborted = !cover_enabled(&config)
                || !active
                || scrolling
                || dragging
                || (strip.is_changed() && !strip.is_added());
            if aborted {
                debug!("cover over strip {entity} ended early");
                if slide.landing.is_none() {
                    position.0 = slide.target;
                    spawn_snap_strip_guard(entity, &mut commands);
                }
                screen.0.hide(slide.display);
                commands.entity(entity).remove::<CoverSlide>();
                continue;
            }
            if target.is_some_and(|target| fresh && target == slide.target) {
                // The same target set again leaves the slide as it is.
                commands.entity(entity).remove::<RepositionMarker>();
                continue;
            }
            if let Some(target) = target.filter(|_| fresh) {
                // A new target while the pictures slide: the slide turns
                // there from where it shows, at the speed it has. After
                // they land, the strip already rests at the old target.
                let (from, speed) = if slide.landing.is_none() {
                    (slide.x_at(slide.elapsed), slide.speed_at(slide.elapsed))
                } else {
                    (f64::from(position.0.x), 0.0)
                };
                position.0.y = target.y;
                *slide = start_slide(slide.display, from, speed, target, &config);
                screen.0.slide(slide.display, &slide.path(), PATH_STEP);
                debug!("cover over strip {entity} turned to {}", target.x);
                commands.entity(entity).remove::<RepositionMarker>();
                continue;
            }
            if slide.landing.is_none() && slide.elapsed >= slide.length() {
                // The pictures have landed: the real windows follow in one
                // step under the cover.
                position.0 = slide.target;
                spawn_snap_strip_guard(entity, &mut commands);
                slide.landing = Some((time.elapsed() + LANDING_TIMEOUT, 0));
                debug!("cover over strip {entity} landed at {}", slide.target.x);
            }
            continue;
        }

        let Some(target) = target.filter(|_| fresh) else {
            continue;
        };
        let refused = if !cover_enabled(&config) {
            Some("switched off")
        } else if !active {
            Some("strip not active")
        } else if scrolling {
            Some("swipe")
        } else if fullscreen {
            Some("fullscreen")
        } else if dragging {
            Some("drag")
        } else if target.y != position.0.y {
            Some("vertical move")
        } else if strip.is_changed() && !strip.is_added() {
            Some("layout changed")
        } else {
            None
        };
        if let Some(reason) = refused {
            debug!("cover skipped over strip {entity}: {reason}");
            continue;
        }
        let from = position.0.x;
        let swing = viewport.width() / SWING_MARGIN;
        let (lo, hi) = (from.min(target.x) - swing, from.max(target.x) + swing);
        let row = strip_windows(&strip, &windows);
        let shown = row
            .iter()
            .filter(|window| shows_between(window.frame, lo, hi, viewport))
            .collect::<Vec<_>>();
        if let Some(window) = shown.iter().find(|window| !window.at_rest || window.tabbed) {
            let (id, tabbed) = (window.id, window.tabbed);
            debug!("cover skipped over strip {entity}: window {id} moving or tabbed ({tabbed})");
            continue;
        }
        if shown.is_empty() {
            continue;
        }
        // The cover would hide a floating window over the strip.
        let floats_over = floating.iter().find(|(window, unmanaged)| {
            matches!(unmanaged, Unmanaged::Floating)
                && !window.shown_frame().intersect(viewport).is_empty()
        });
        if let Some((window, _)) = floats_over {
            let id = window.id();
            debug!("cover skipped over strip {entity}: floating window {id}");
            continue;
        }
        let excluding = row.iter().map(|window| window.id).collect::<Vec<_>>();
        let Some(desktop) = photos.desktop(display.id(), &excluding) else {
            let id = display.id();
            debug!("cover skipped: no desktop picture of display {id}");
            continue;
        };
        let pictures = shown
            .iter()
            .map(|window| {
                photos
                    .window(window.id, window.frame.size())
                    .map(|photo| (window.id, window.frame, photo))
            })
            .collect::<Option<Vec<_>>>();
        let Some(pictures) = pictures else {
            debug!("cover skipped: a window of strip {entity} has no picture");
            continue;
        };
        screen.0.show(&CoverScene {
            display: display.id(),
            display_bounds: display.bounds(),
            viewport,
            desktop,
            strip: position.0,
            pictures,
        });
        let slide = start_slide(display.id(), f64::from(from), 0.0, target, &config);
        screen.0.slide(display.id(), &slide.path(), PATH_STEP);
        debug!(
            "cover over strip {entity}: {} pictures, {} to {}",
            shown.len(),
            from,
            target.x
        );
        commands
            .entity(entity)
            .insert(slide)
            .remove::<RepositionMarker>();
    }
}

/// A slide from strip x `from`, moving at `speed` points per second, to
/// `target`.
fn start_slide(
    display: CGDirectDisplayID,
    from: f64,
    speed: f64,
    target: IVec2,
    config: &Config,
) -> CoverSlide {
    let duration = ease::duration(config);
    let entry = ease::entry_for_speed(speed, f64::from(target.x) - from, duration);
    CoverSlide {
        display,
        from,
        target,
        entry,
        elapsed: 0.0,
        duration,
        landing: None,
    }
}

/// Takes each cover down once the real windows under it have taken their
/// places, or once it has waited [`LANDING_TIMEOUT`].
fn lift_covers(
    mut strips: Query<(Entity, &LayoutStrip, &mut CoverSlide)>,
    windows: CoverWindows,
    time: Res<Time>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
    let Some(mut screen) = screen else {
        return;
    };
    for (entity, strip, mut slide) in &mut strips {
        let Some((deadline, settled)) = slide.landing else {
            continue;
        };
        let in_place = strip_windows(strip, &windows)
            .iter()
            .all(|window| window.at_rest);
        let settled = if in_place { settled + 1 } else { 0 };
        if settled >= LANDED_FRAMES || time.elapsed() >= deadline {
            debug!(
                "cover over strip {entity} lifted{}",
                if settled >= LANDED_FRAMES {
                    ""
                } else {
                    " at its time limit"
                }
            );
            screen.0.hide(slide.display);
            commands.entity(entity).remove::<CoverSlide>();
        } else {
            slide.landing = Some((deadline, settled));
        }
    }
}

/// Asks for the next picture the cover needs, one at a time: a missing or
/// outgrown one first, then the desktop, then the focused window, then the
/// oldest.
#[allow(clippy::too_many_arguments)]
fn refresh_cover_photos(
    strips: Query<(&LayoutStrip, &ChildOf), With<ActiveWorkspaceMarker>>,
    covers: Query<(), With<CoverSlide>>,
    windows: CoverWindows,
    all_windows: Query<&Window>,
    displays: Query<&Display>,
    mut photos: ResMut<CoverPhotos>,
    config: Res<Config>,
    time: Res<Time>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let now = time.elapsed();
    check_access(&mut photos, &config, screen.0.as_ref(), now);
    if photos.access != Access::Allowed || !cover_enabled(&config) {
        if !photos.windows.is_empty() || !photos.desktops.is_empty() {
            photos.clear();
        }
        return;
    }
    if !covers.is_empty() {
        // A capture now would cost the slide frames.
        return;
    }
    if photos
        .asked
        .as_ref()
        .is_some_and(|asked| now.saturating_sub(asked.at) < ASK_TIMEOUT)
    {
        return;
    }
    photos.asked = None;
    let alive = all_windows
        .iter()
        .map(|window| window.id())
        .collect::<Vec<_>>();
    photos.windows.retain(|id, _| alive.contains(id));

    let Some(want) = next_picture(&strips, &windows, &displays, &photos, now) else {
        return;
    };
    photos.generation = photos.generation.wrapping_add(1);
    let generation = photos.generation;
    match want.subject {
        CoverSubject::Window(id) => {
            screen
                .0
                .photograph_window(id, want.size, want.display, generation);
        }
        CoverSubject::Desktop(display) => {
            screen
                .0
                .photograph_desktop(display, &want.excluding, generation);
        }
    }
    photos.asked = Some(Asked {
        subject: want.subject,
        generation,
        at: now,
        size: want.size,
        excluding: want.excluding,
    });
}

/// Asks macOS every [`ACCESS_CHECK`] whether the engine may take pictures.
fn check_access(
    photos: &mut CoverPhotos,
    config: &Config,
    screen: &dyn CoverScreen,
    now: Duration,
) {
    if photos
        .checked_at
        .is_some_and(|checked| now.saturating_sub(checked) < ACCESS_CHECK)
    {
        return;
    }
    photos.checked_at = Some(now);
    let allowed = cover_enabled(config) && screen.may_photograph();
    photos.access = match (photos.access, allowed) {
        (Access::Refused, true) => Access::Refused,
        (_, true) => Access::Allowed,
        (_, false) => Access::Off,
    };
}

/// A picture the cover wants.
struct Want {
    /// Lower comes first.
    priority: u8,
    /// When the kept picture was taken, so the oldest comes first.
    taken: Duration,
    subject: CoverSubject,
    size: IVec2,
    display: CGDirectDisplayID,
    excluding: Vec<WinID>,
}

/// The picture the cover needs most.
fn next_picture(
    strips: &Query<(&LayoutStrip, &ChildOf), With<ActiveWorkspaceMarker>>,
    windows: &CoverWindows,
    displays: &Query<&Display>,
    photos: &CoverPhotos,
    now: Duration,
) -> Option<Want> {
    let mut wanted = Vec::new();
    for (strip, child) in strips {
        let Ok(display) = displays.get(child.parent()) else {
            continue;
        };
        let row = strip_windows(strip, windows);
        let excluding = row.iter().map(|window| window.id).collect::<Vec<_>>();
        match photos.desktops.get(&display.id()) {
            Some(kept)
                if excluding.iter().all(|id| kept.excluding.contains(id))
                    && now.saturating_sub(kept.taken) < DESKTOP_REFRESH => {}
            kept => wanted.push(Want {
                priority: if kept.is_none() { 1 } else { 3 },
                taken: kept.map_or(Duration::ZERO, |kept| kept.taken),
                subject: CoverSubject::Desktop(display.id()),
                size: display.bounds().size(),
                display: display.id(),
                excluding: excluding.clone(),
            }),
        }
        for window in row.iter().filter(|window| window.at_rest && !window.tabbed) {
            if photos
                .blank_until
                .get(&window.id)
                .is_some_and(|until| now < *until)
            {
                continue;
            }
            let focused = windows
                .get(window.entity)
                .is_ok_and(|(.., focused)| focused);
            let size = window.frame.size();
            let kept = photos.windows.get(&window.id);
            let age = kept.map_or(Duration::MAX, |kept| now.saturating_sub(kept.taken));
            let priority = match kept {
                None => 0,
                Some(kept) if !close(kept.size, size) => 0,
                Some(_) if focused && age >= FOCUSED_REFRESH => 2,
                Some(_) if age >= WINDOW_REFRESH => 4,
                Some(_) => continue,
            };
            wanted.push(Want {
                priority,
                taken: kept.map_or(Duration::ZERO, |kept| kept.taken),
                subject: CoverSubject::Window(window.id),
                size,
                display: display.id(),
                excluding: Vec::new(),
            });
        }
    }
    wanted
        .into_iter()
        .min_by_key(|want| (want.priority, want.taken))
}
