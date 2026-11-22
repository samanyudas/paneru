//! The picture cover for strip moves (miri#509).
//!
//! Apps take the positions the engine writes on their own time, so the
//! windows of one strip move out of step and the motion looks choppy. With
//! `slide_window_pictures` on, a keyboard strip move instead slides pictures
//! of the strip's windows on a cover over the display, along the same curve
//! the windows would follow, while the real windows stay put. Once the
//! pictures land, the real windows move to their places in one step under
//! the cover, and the cover comes off when they are there, or after
//! [`LANDING_TIMEOUT`]. A swipe that starts with every picture of its strip
//! ready goes under the cover too: the pictures follow the fingers and the
//! settle, and the real windows land once it settles. A press on a picture
//! is held and reaches its window once the window is in place.
//!
//! The engine keeps a picture of every window of each display's active strip
//! and one of each display's desktop without those windows, refreshed one at
//! a time in the background, so a move never waits for a capture. A move
//! that lacks a picture it would show, or that the cover would get wrong,
//! moves the real windows as before.

use std::collections::HashMap;
use std::time::Duration;

use bevy::app::{App, PostUpdate, Update};
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut as _};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use bevy::ecs::world::Ref;
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use bevy::time::common_conditions::on_timer;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::debug;

use crate::config::Config;
use crate::ecs::display::shown_strip;
use crate::ecs::drag::DragState;
use crate::ecs::layout::{Column, LayoutStrip, ensure_visible_in_strip, position_layout_strips};
use crate::ecs::slice_photos::{Photo, PhotoError};
use crate::ecs::systems::AnimationStep;
use crate::ecs::workspace::spawn_snap_strip_guard;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition,
    NativeFullscreenMarker, Position, PostUpdateSet, RepositionMarker, ResizeMarker, ScrollInput,
    Scrolling, SelectedVirtualMarker, SpawnCommandsExt as _, Unmanaged, UpdateSet, ease, systems,
};
use crate::events::Event;
use crate::manager::{Display, Window, WindowManager};
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

/// How often the engine looks for a picture to refresh.
const REFRESH_TICK: Duration = Duration::from_millis(100);

/// How long past its length a slide waits for Core Animation to show it
/// landed before the windows land anyway.
const SHOWN_GRACE: f64 = 0.5;

/// The longest slide the cover plays. A slower `animation_speed` moves the
/// real windows as before.
const LONGEST_SLIDE: f64 = 1.0;

/// How often the engine asks macOS whether it may still take pictures.
const ACCESS_CHECK: Duration = Duration::from_secs(2);

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
    /// Tells the event tap that the strip on the cover of `display` shows
    /// at strip x `x`, so a press on a picture reaches its window.
    fn track(&mut self, display: CGDirectDisplayID, x: f64);
    /// Shows the strip on the cover of `display` at strip x `x` at once, as
    /// a swipe moves it, and tells the event tap.
    fn place(&mut self, display: CGDirectDisplayID, x: f64);
    /// Where the strip on the cover of `display` shows now, as Core
    /// Animation draws it. `None` when it cannot tell.
    fn showing(&self, display: CGDirectDisplayID) -> Option<f64>;
    /// The displays whose cover is up.
    fn shown(&self) -> Vec<CGDirectDisplayID>;
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
    /// The snap guard the landing spawned, which ends when the cover lifts.
    guard: Option<Entity>,
    /// The display's bounds and the strip's viewport when the slide began.
    /// A display or a Dock that changed since has placed the strip anew.
    bounds: IRect,
    viewport: IRect,
    /// How long the slide has run by the engine's count, for when Core
    /// Animation cannot tell where it shows: the landing waits at most
    /// [`SHOWN_GRACE`] past the slide's length for the screen.
    counted: f64,
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

    /// How far into the slide the strip shows at strip x `x`, in seconds:
    /// the slide moves one way only, so each place has one time.
    fn phase_of(&self, x: f64) -> f64 {
        let to = f64::from(self.target.x);
        if (to - self.from).abs() < f64::EPSILON {
            return self.length();
        }
        let share = ((x - self.from) / (to - self.from)).clamp(0.0, 1.0);
        let (mut low, mut high) = (0.0, self.length());
        for _ in 0..40 {
            let mid = f64::midpoint(low, high);
            if (self.x_at(mid) - self.from) / (to - self.from) < share {
                low = mid;
            } else {
                high = mid;
            }
        }
        high
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

/// A swipe under the cover: the pictures follow the strip, and its real
/// windows wait where they are until the swipe lands. The landing is a
/// [`CoverSlide`] that has landed.
#[derive(bevy::ecs::component::Component, Clone, Copy, Debug)]
pub(crate) struct CoverSwipe {
    display: CGDirectDisplayID,
    /// The strip x the cover shows.
    shown: i32,
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
            (cover_strip_moves, cover_swipes, mark_moved)
                .chain()
                .in_set(UpdateSet::Layout)
                .after(ensure_visible_in_strip)
                .before(position_layout_strips),
        )
        .add_systems(
            PostUpdate,
            (
                lift_covers,
                drop_orphan_covers,
                refresh_cover_photos.run_if(on_timer(REFRESH_TICK)),
            )
                .chain()
                .in_set(PostUpdateSet::Animate)
                .after(systems::commit_window_frame),
        );
}

/// Whether the cover may show at all: the switch is on and windows
/// animate, no slower than [`LONGEST_SLIDE`].
fn cover_enabled(config: &Config) -> bool {
    let duration = ease::duration(config);
    config.slide_window_pictures()
        && config.options().animation_speed.is_some()
        && duration.is_finite()
        && duration <= LONGEST_SLIDE
        && ease::animates(duration)
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
    focused: bool,
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
            let (window, layout, bounds, unmanaged, moving, resizing, focused) =
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
                focused,
            })
        })
        .collect()
}

/// Every strip, and whether it is the selected strip of its Space, to find
/// the strip each display shows.
type ShownStrips<'w, 's> =
    Query<'w, 's, (Entity, &'static LayoutStrip, Has<SelectedVirtualMarker>)>;

/// Whether `entity` is the strip that `display` shows. A swipe over a
/// display drives its shown strip, which need not be the active one.
fn shows(
    window_manager: &WindowManager,
    display: CGDirectDisplayID,
    strips: &ShownStrips,
    entity: Entity,
) -> bool {
    shown_strip(window_manager, display, strips.iter()) == Some(entity)
}

/// What a cover over `strip`, at strip origin `at`, would show, or why it
/// cannot: it needs a picture of every window of the strip, so a move that
/// turns, or a swipe, shows each window wherever it goes. The focused window
/// comes last, over any window it overlaps.
fn cover_scene<'a>(
    strip: &LayoutStrip,
    display: &Display,
    viewport: IRect,
    at: IVec2,
    windows: &CoverWindows,
    floating: &Query<(&Window, &Unmanaged)>,
    photos: &'a CoverPhotos,
) -> Result<CoverScene<'a>, &'static str> {
    let mut row = strip_windows(strip, windows);
    if row.is_empty() {
        return Err("no windows");
    }
    if row.iter().any(|window| !window.at_rest) {
        return Err("a window moves");
    }
    if row.iter().any(|window| window.tabbed) {
        return Err("tabbed windows");
    }
    let floats_over = floating.iter().any(|(window, unmanaged)| {
        matches!(unmanaged, Unmanaged::Floating)
            && !window.shown_frame().intersect(viewport).is_empty()
    });
    if floats_over {
        return Err("a floating window over the strip");
    }
    let excluding = row.iter().map(|window| window.id).collect::<Vec<_>>();
    let desktop = photos
        .desktop(display.id(), &excluding)
        .ok_or("no desktop picture")?;
    row.sort_by_key(|window| window.focused);
    let pictures = row
        .iter()
        .map(|window| {
            photos
                .window(window.id, window.frame.size())
                .map(|photo| (window.id, window.frame, photo))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or("a window has no picture")?;
    Ok(CoverScene {
        display: display.id(),
        display_bounds: display.bounds(),
        viewport,
        desktop,
        strip: at,
        pictures,
    })
}

/// Ends a snap guard that a landing spawned, once the cover lifts, so a
/// later layout change animates again.
fn end_guard(guard: Option<Entity>, commands: &mut Commands) {
    if let Some(mut entity_commands) = guard.and_then(|guard| commands.get_entity(guard).ok()) {
        entity_commands.try_despawn();
    }
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
    shown_strips: ShownStrips,
    windows: CoverWindows,
    floating: Query<(&Window, &Unmanaged)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    photos: Res<CoverPhotos>,
    drag: Option<Res<DragState>>,
    window_manager: Res<WindowManager>,
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
        let target = marker.as_ref().map(|marker| marker.0);
        let fresh = marker.as_ref().is_some_and(DetectChanges::is_changed);
        if slide.is_none() && !fresh {
            continue;
        }
        let Ok((display, dock)) = displays.get(child.parent()) else {
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        let shown_here = active || shows(&window_manager, display.id(), &shown_strips, entity);

        if let Some(mut slide) = slide {
            // Where the pictures show, as Core Animation draws them, which
            // runs on while the engine stalls.
            let showing = if slide.landing.is_none() {
                slide.counted += step.secs();
                let showing = match screen.0.showing(slide.display) {
                    // Core Animation runs on while the engine stalls, and the
                    // engine's count can run ahead of it, so the slide's clock
                    // is where the screen shows it: a turn and the landing
                    // follow the screen.
                    Some(showing) if slide.counted < slide.length() + SHOWN_GRACE => {
                        slide.elapsed = slide.phase_of(showing);
                        showing
                    }
                    _ => {
                        slide.elapsed = slide.counted;
                        slide.x_at(slide.elapsed)
                    }
                };
                screen.0.track(slide.display, showing);
                showing
            } else {
                f64::from(position.0.x)
            };
            // Anything but a new target for the strip ends the cover: the
            // windows go where the slide headed, at once. A strip no longer
            // shown keeps where the workspace switch put it.
            let aborted = !cover_enabled(&config)
                || !shown_here
                || scrolling
                || dragging
                || (strip.is_changed() && !strip.is_added());
            if aborted {
                debug!("cover over strip {entity} ended early");
                // A strip moved to another display keeps where that display
                // put it.
                let same_place = display.id() == slide.display
                    && display.bounds() == slide.bounds
                    && viewport == slide.viewport;
                if slide.landing.is_none() && shown_here {
                    if same_place {
                        position.0 = slide.target;
                        spawn_snap_strip_guard(entity, &mut commands);
                    } else if let Some(focused) = strip_windows(&strip, &windows)
                        .iter()
                        .find(|window| window.focused)
                    {
                        // The display or the Dock changed under the slide:
                        // its target is stale, so the layout shows the
                        // focused window again where it now fits.
                        commands.ensure_visible(focused.entity);
                    }
                }
                end_guard(slide.guard.take(), &mut commands);
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
                // A slide shown landed has no speed left to carry on.
                let speed = if slide.landing.is_none() && slide.elapsed < slide.length() {
                    slide.speed_at(slide.elapsed)
                } else {
                    0.0
                };
                position.0.y = target.y;
                let guard = slide.guard;
                *slide = start_slide(
                    slide.display,
                    (slide.bounds, slide.viewport),
                    showing,
                    speed,
                    target,
                    &config,
                );
                slide.guard = guard;
                screen.0.slide(slide.display, &slide.path(), PATH_STEP);
                debug!("cover over strip {entity} turned to {}", target.x);
                commands.entity(entity).remove::<RepositionMarker>();
                continue;
            }
            if slide.landing.is_none() && slide.elapsed >= slide.length() {
                // The pictures have landed: the real windows follow in one
                // step under the cover.
                position.0 = slide.target;
                end_guard(slide.guard.take(), &mut commands);
                slide.guard = Some(spawn_snap_strip_guard(entity, &mut commands));
                screen.0.track(slide.display, f64::from(slide.target.x));
                slide.landing = Some((time.elapsed() + LANDING_TIMEOUT, 0));
                debug!("cover over strip {entity} landed at {}", slide.target.x);
            }
            continue;
        }

        let Some(target) = target else {
            continue;
        };
        let from = position.0.x;
        if target.x == from {
            // A target where the strip rests moves nothing to cover.
            continue;
        }
        let refused = if !cover_enabled(&config) {
            Some("switched off")
        } else if !shown_here {
            Some("strip not shown")
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
        let scene = match cover_scene(
            &strip, display, viewport, position.0, &windows, &floating, &photos,
        ) {
            Ok(scene) => scene,
            Err(reason) => {
                debug!("cover skipped over strip {entity}: {reason}");
                continue;
            }
        };
        let pictures = scene.pictures.len();
        screen.0.show(&scene);
        let slide = start_slide(
            display.id(),
            (display.bounds(), viewport),
            f64::from(from),
            0.0,
            target,
            &config,
        );
        screen.0.slide(display.id(), &slide.path(), PATH_STEP);
        screen.0.track(display.id(), f64::from(from));
        debug!(
            "cover over strip {entity}: {pictures} pictures, {from} to {}",
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
    (bounds, viewport): (IRect, IRect),
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
        guard: None,
        bounds,
        viewport,
        counted: 0.0,
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
            let late = if settled >= LANDED_FRAMES {
                ""
            } else {
                " at its time limit"
            };
            debug!("cover over strip {entity} lifted{late}");
            screen.0.hide(slide.display);
            end_guard(slide.guard, &mut commands);
            commands.entity(entity).remove::<CoverSlide>();
        } else {
            slide.landing = Some((deadline, settled));
        }
    }
}

/// Takes down any cover that no strip move or swipe holds up any more, as
/// when its strip or display went away mid-move. A move whose strip left
/// the cover's display ends there too.
fn drop_orphan_covers(
    slides: Query<(Entity, &CoverSlide, Option<&ChildOf>)>,
    swipes: Query<(Entity, &CoverSwipe, Option<&ChildOf>)>,
    displays: Query<&Display>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let on = |child: Option<&ChildOf>, display: CGDirectDisplayID| {
        child
            .and_then(|child| displays.get(child.parent()).ok())
            .is_some_and(|parent| parent.id() == display)
    };
    let mut held = Vec::new();
    for (entity, slide, child) in &slides {
        if on(child, slide.display) {
            held.push(slide.display);
        } else {
            end_guard(slide.guard, &mut commands);
            commands.entity(entity).remove::<CoverSlide>();
        }
    }
    for (entity, swipe, child) in &swipes {
        if on(child, swipe.display) {
            held.push(swipe.display);
        } else {
            commands.entity(entity).remove::<CoverSwipe>().insert(Moved);
        }
    }
    for id in screen.0.shown() {
        if !held.contains(&id) {
            debug!("cover on display {id} had no move, taken down");
            screen.0.hide(id);
        }
    }
}

type SwipedStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Ref<'static, LayoutStrip>,
        Ref<'static, Position>,
        Option<Ref<'static, Scrolling>>,
        Option<&'static mut CoverSwipe>,
        Has<CoverSlide>,
        &'static ChildOf,
        Has<ActiveWorkspaceMarker>,
        Has<NativeFullscreenMarker>,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

/// Puts a trackpad swipe under the cover when it starts with every picture
/// of its strip ready, moves the pictures with the strip, and lands the
/// real windows in one step once the swipe settles. Modifier-scroll stays
/// uncovered: its ticks over the cover would not reach the engine.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn cover_swipes(
    mut strips: SwipedStrips,
    shown_strips: ShownStrips,
    windows: CoverWindows,
    floating: Query<(&Window, &Unmanaged)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    photos: Res<CoverPhotos>,
    drag: Option<Res<DragState>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    time: Res<Time>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let dragging = drag
        .as_ref()
        .is_some_and(|drag| drag.held_window().is_some());
    for (entity, strip, position, scrolling, swipe, sliding, child, active, fullscreen) in
        &mut strips
    {
        let starts = scrolling.as_ref().is_some_and(|scrolling| {
            scrolling.is_added() && scrolling.input == ScrollInput::Gesture
        });
        if swipe.is_none() && !starts {
            continue;
        }
        let Ok((display, dock)) = displays.get(child.parent()) else {
            continue;
        };
        let shown_here = active || shows(&window_manager, display.id(), &shown_strips, entity);
        if let Some(mut swipe) = swipe {
            let aborted = !cover_enabled(&config)
                || !shown_here
                || dragging
                || (strip.is_changed() && !strip.is_added());
            if aborted || scrolling.is_none() {
                // The swipe has landed, or the cover cannot stay: the real
                // windows take their places in one step.
                let guard = spawn_snap_strip_guard(entity, &mut commands);
                let mut entity_commands = commands.entity(entity);
                entity_commands.remove::<CoverSwipe>().insert(Moved);
                if aborted {
                    debug!("cover over swiped strip {entity} ended early");
                    screen.0.hide(swipe.display);
                } else {
                    screen.0.place(swipe.display, f64::from(position.0.x));
                    entity_commands.insert(CoverSlide {
                        display: swipe.display,
                        from: f64::from(position.0.x),
                        target: position.0,
                        entry: 0.0,
                        elapsed: 0.0,
                        duration: 0.0,
                        landing: Some((time.elapsed() + LANDING_TIMEOUT, 0)),
                        guard: Some(guard),
                        bounds: display.bounds(),
                        viewport: display.actual_display_bounds(dock, &config),
                        counted: 0.0,
                    });
                    let x = position.0.x;
                    debug!("cover over swiped strip {entity} landed at {x}");
                }
                continue;
            }
            if swipe.shown != position.0.x {
                swipe.shown = position.0.x;
                screen.0.place(swipe.display, f64::from(position.0.x));
            }
            continue;
        }

        if sliding || !cover_enabled(&config) || !shown_here || fullscreen || dragging {
            continue;
        }
        let viewport = display.actual_display_bounds(dock, &config);
        let scene = match cover_scene(
            &strip, display, viewport, position.0, &windows, &floating, &photos,
        ) {
            Ok(scene) => scene,
            Err(reason) => {
                debug!("cover skipped over swiped strip {entity}: {reason}");
                continue;
            }
        };
        let pictures = scene.pictures.len();
        screen.0.show(&scene);
        screen.0.track(display.id(), f64::from(position.0.x));
        debug!("cover over swiped strip {entity}: {pictures} pictures");
        commands.entity(entity).insert(CoverSwipe {
            display: display.id(),
            shown: position.0.x,
        });
    }
}

/// Marks a strip whose windows must take their places again, once a swipe
/// under the cover lands. [`mark_moved`] turns it into a change of the
/// strip's position, which repositions its windows.
#[derive(bevy::ecs::component::Component)]
struct Moved;

/// Repositions the windows of each strip marked [`Moved`].
fn mark_moved(mut strips: Query<(Entity, &mut Position), With<Moved>>, mut commands: Commands) {
    for (entity, mut position) in &mut strips {
        position.set_changed();
        commands.entity(entity).remove::<Moved>();
    }
}

/// Every strip under a cover, by a move or a swipe.
type AnyCover<'w, 's> = Query<'w, 's, (), Or<(With<CoverSlide>, With<CoverSwipe>)>>;

/// Asks for the next picture the cover needs, one at a time: a missing or
/// outgrown one first, then the desktop, then the focused window, then the
/// oldest.
#[allow(clippy::too_many_arguments)]
fn refresh_cover_photos(
    shown_strips: ShownStrips,
    covers: AnyCover,
    windows: CoverWindows,
    all_windows: Query<&Window>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
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

    // The strip each display shows, so every display's next move is ready.
    let rows = displays
        .iter()
        .filter_map(|display| {
            let entity = shown_strip(&window_manager, display.id(), shown_strips.iter())?;
            let (_, strip, _) = shown_strips.get(entity).ok()?;
            Some((display, strip))
        })
        .collect::<Vec<_>>();
    let Some(want) = next_picture(&rows, &windows, &photos, now) else {
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
    rows: &[(&Display, &LayoutStrip)],
    windows: &CoverWindows,
    photos: &CoverPhotos,
    now: Duration,
) -> Option<Want> {
    let mut wanted = Vec::new();
    for (display, strip) in rows {
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
