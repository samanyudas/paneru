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
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::time::Duration;

use bevy::app::{App, PostUpdate, Update};
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut as _};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, NonSend, NonSendMut, Query, Res, ResMut};
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
use crate::ecs::pacing::ShownFrames;
use crate::ecs::scroll;
use crate::ecs::slice_photos::{Photo, PhotoError};
use crate::ecs::systems::AnimationStep;
use crate::ecs::workspace::spawn_snap_strip_guard;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition,
    MissionControlActive, NativeFullscreenMarker, Position, PostUpdateSet, RepositionMarker,
    ResizeMarker, ScrollInput, Scrolling, SelectedVirtualMarker, SpawnCommandsExt as _, Unmanaged,
    UpdateSet, WidthLimits, ease, systems,
};
use crate::events::Event;
use crate::manager::{Display, Window, WindowManager};
use crate::platform::WinID;
use crate::util::round_px;

mod resize;

pub(crate) use resize::CoverMorph;

/// How long the cover stays up after the pictures land, at most, for windows
/// whose apps are slow to take their places.
const LANDING_TIMEOUT: Duration = Duration::from_millis(350);

/// How many frames in a row the windows must have taken their writes before
/// the cover comes off: the window server shows a moved window a frame later.
const LANDED_FRAMES: u8 = 2;

/// How often the focused window's picture is refreshed, so typing shows.
const FOCUSED_REFRESH: Duration = Duration::from_secs(1);

/// How long a window's picture at the size it had before is kept. A window
/// that returns to that size, as a maximized window that is restored does,
/// shows it while it gets there. An older one shows too little of what the
/// window holds now.
const PREVIOUS_KEEP: Duration = Duration::from_mins(10);

/// How long that picture is kept for a window that shrinks back to its size,
/// as a restored window does. It lies over the window's larger picture, so
/// content that changed since without a new title, as a page that scrolled,
/// shows a seam between the two.
const PREVIOUS_SHRINK_KEEP: Duration = Duration::from_mins(1);

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
    /// The part of the display the cover covers: its visible area, where
    /// the strip and its edge padding show.
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
    /// Moves each window's frame on the cover of `display` from one frame
    /// to another, in global points, along `shares`: the share of the way
    /// it has gone every `step` seconds from now. The picture in a frame
    /// never stretches: it stays at its own size from the frame's top left
    /// corner, a frame narrower than it crops it, and a wider one fills the
    /// strip it opens with the picture's edge colour. Each rests at its last
    /// frame, and the event tap takes a press there. Replaces any morph
    /// before it.
    fn morph(
        &mut self,
        display: CGDirectDisplayID,
        frames: &[(WinID, IRect, IRect)],
        shares: &[f64],
        step: f64,
    );
    /// Puts `photo`, a picture of `window` at `size` points, into the
    /// window's frame on the cover of `display` at once, at its own size,
    /// in place of the picture there. Returns `false` if the live layer's
    /// presented width is still wider than `size` (leaving the old picture
    /// and fill untouched so the caller keeps the swap pending).
    fn replace_picture(
        &mut self,
        display: CGDirectDisplayID,
        window: WinID,
        size: IVec2,
        photo: &Photo,
    ) -> bool;
    /// Makes the frame of `window` on the cover of `display` open onto the
    /// window itself in the next [`CoverScreen::morph`]: the cover shows
    /// nothing inside that frame, so the window below it shows through as
    /// the frame grows. For a window that already has its final frame.
    fn open(&mut self, display: CGDirectDisplayID, window: WinID);
    /// Makes the next [`CoverScreen::morph`] of the cover of `display` show
    /// `photo`, a picture of `window` at the `size` its frame heads to, from
    /// the morph's start and in the same transaction: in place of the
    /// picture there when the frame grows to that size, and over that
    /// picture's top left corner when the frame shrinks to it. The window
    /// then shows as it will be laid out while its frame gets there. The
    /// morph leaves the picture out when the frame heads to another size.
    /// Returns `false`, and keeps nothing, when the cover has no frame of
    /// `window`.
    fn head_picture(
        &mut self,
        display: CGDirectDisplayID,
        window: WinID,
        size: IVec2,
        photo: &Photo,
    ) -> bool;
    /// How far along its morph the cover of `display` shows its pictures
    /// now, as Core Animation draws them, from 0 to 1. `None` when it
    /// cannot tell.
    fn morph_share(&self, display: CGDirectDisplayID) -> Option<f64>;
    /// Where the strip on the cover of `display` shows now, as Core
    /// Animation draws it. `None` when it cannot tell.
    fn showing(&self, display: CGDirectDisplayID) -> Option<f64>;
    /// The displays whose cover is up.
    fn shown(&self) -> Vec<CGDirectDisplayID>;
    /// Takes the cover of `display` down.
    fn hide(&mut self, display: CGDirectDisplayID);
    /// Lets go of what the screen keeps to find `window`, or every window,
    /// so a stale lookup never sticks.
    fn forget(&mut self, window: Option<WinID>);
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

/// A morph capture reply waiting for the frame's resize layout to resolve
/// before touching the cache or unveiling on a morph cover.
#[derive(Debug)]
struct MorphReply {
    id: WinID,
    size: IVec2,
    generation: u32,
    result: Result<Photo, PhotoError>,
    taken: Duration,
}

/// The pictures the engine keeps for the cover.
#[derive(Resource, Debug, Default)]
pub(crate) struct CoverPhotos {
    windows: HashMap<WinID, Kept>,
    /// Each window's last picture at the size it had before its current one.
    /// A maximize and its restore go between two sizes, so the picture of
    /// the size a window heads to is here, and the cover shows the window as
    /// it will be laid out, rather than its old layout cropped or padded.
    previous: HashMap<WinID, Kept>,
    /// When each window's title last changed. A picture taken before shows
    /// another tab or document, so it is retaken first.
    retitled: HashMap<WinID, Duration>,
    desktops: HashMap<CGDirectDisplayID, Kept>,
    /// Windows whose last picture came out blank, and when to try again.
    blank_until: HashMap<WinID, Duration>,
    /// The background refresh's one picture on its way.
    asked: Option<Asked>,
    /// The fresh pictures a resize's cover asked for, apart from the
    /// background refresh so neither drops the other's answer.
    morph_asked: Vec<Asked>,
    /// Morph replies received in this frame's `Events` pass, validated and
    /// applied once `cover_resizes` resolves the frame's turns and aborts.
    morph_replies: Vec<MorphReply>,
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

    /// A kept picture of window `id` at `size`, the size it heads to, if it
    /// is recent enough: younger than [`PREVIOUS_KEEP`] when it `replaces`
    /// the window's picture on the cover, being no smaller, and than
    /// [`PREVIOUS_SHRINK_KEEP`] when it lies over that larger picture. The
    /// picture of the size it had before its current one comes first, then
    /// its current one, which a turn back to where a resize began finds
    /// before a fresh picture of the new size replaces it, unless the
    /// window's title changed since.
    fn heading(&self, id: WinID, size: IVec2, replaces: bool, now: Duration) -> Option<&Photo> {
        let keep = if replaces {
            PREVIOUS_KEEP
        } else {
            PREVIOUS_SHRINK_KEEP
        };
        let usable = |kept: &&Kept| close(kept.size, size) && now.saturating_sub(kept.taken) < keep;
        let current = || {
            self.windows.get(&id).filter(usable).filter(|kept| {
                self.retitled
                    .get(&id)
                    .is_none_or(|retitled| kept.taken >= *retitled)
            })
        };
        self.previous
            .get(&id)
            .filter(usable)
            .or_else(current)
            .map(|kept| &kept.photo)
    }

    /// Keeps `kept` as the picture of window `id`. A picture of another
    /// size that it replaces stays as the window's previous one, unless it
    /// was taken before the window's title last changed: it shows another
    /// tab or document.
    fn keep_window(&mut self, id: WinID, kept: Kept) {
        let retitled = self.retitled.get(&id).copied();
        if retitled.is_some_and(|retitled| kept.taken >= retitled) {
            self.retitled.remove(&id);
        }
        if let Some(old) = self.windows.insert(id, kept)
            && retitled.is_none_or(|retitled| old.taken >= retitled)
            && self
                .windows
                .get(&id)
                .is_some_and(|new| !close(old.size, new.size))
        {
            self.previous.insert(id, old);
        }
    }

    /// Forgets every picture of window `id`.
    fn forget_window(&mut self, id: WinID) {
        self.windows.remove(&id);
        self.previous.remove(&id);
        self.retitled.remove(&id);
    }

    /// Notes that window `id` changed its title at `now`: its previous
    /// picture shows what it held before, and its current one is retaken.
    fn retitle(&mut self, id: WinID, now: Duration) {
        self.previous.remove(&id);
        if self.windows.contains_key(&id) {
            self.retitled.insert(id, now);
        }
    }

    fn desktop(&self, display: CGDirectDisplayID, excluding: &[WinID]) -> Option<&Photo> {
        self.desktops
            .get(&display)
            .filter(|kept| excluding.iter().all(|id| kept.excluding.contains(id)))
            .map(|kept| &kept.photo)
    }

    /// Invalidates any pending background or morph ask (and queued morph
    /// reply) for `subject` before issuing a replacement, so an obsolete
    /// request neither blocks the background queue nor lands out of order.
    fn supersede(&mut self, subject: CoverSubject) {
        if self
            .asked
            .as_ref()
            .is_some_and(|asked| asked.subject == subject)
        {
            self.asked = None;
        }
        self.morph_asked.retain(|asked| asked.subject != subject);
        if let CoverSubject::Window(id) = subject {
            self.morph_replies.retain(|reply| reply.id != id);
        }
    }

    fn clear(&mut self) {
        self.windows.clear();
        self.previous.clear();
        self.retitled.clear();
        self.desktops.clear();
        self.blank_until.clear();
        self.asked = None;
        self.morph_asked.clear();
        self.morph_replies.clear();
    }

    fn is_clear(&self) -> bool {
        self.windows.is_empty()
            && self.previous.is_empty()
            && self.retitled.is_empty()
            && self.desktops.is_empty()
            && self.blank_until.is_empty()
            && self.asked.is_none()
            && self.morph_asked.is_empty()
            && self.morph_replies.is_empty()
    }
}

/// Whether two sizes match to within the rounding of a capture.
fn close(a: IVec2, b: IVec2) -> bool {
    (a - b).abs().max_element() <= 2
}

/// A strip move under the cover.
#[derive(bevy::ecs::component::Component, Clone, Debug)]
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
    /// The part of the display the cover hides: the visible area, with its
    /// edge padding.
    area: IRect,
    /// Every window whose frame met `area` where it showed or where it
    /// lands, at any start, turn, or landing of the slide: the window server
    /// may still show it there.
    hid: Vec<Entity>,
    /// Where the pictures showed when a swipe caught them mid-way: the
    /// swipe's scroll state was moved there before the scroll systems held
    /// it to the strip's ends and columns, so the cover hands over to it
    /// as it is.
    caught: Option<f64>,
    /// The strip x the real windows are laid out at: where the slide began,
    /// until they land at its target. A swipe or a move that the cover
    /// gives way to puts the strip back here, so no window takes a place
    /// the pictures never showed.
    rest: i32,
    /// Where the pictures show in this frame, as of when: a frame samples
    /// them once, however many systems look.
    sampled: Option<(Duration, f64)>,
    /// How long the slide has run by the engine's count, for when Core
    /// Animation cannot tell where it shows: the landing waits at most
    /// [`SHOWN_GRACE`] past the slide's length for the screen.
    counted: f64,
    /// The strip's layout when the slide began, as [`layout_print`] gives it.
    layout: u64,
}

impl CoverSlide {
    /// Where the strip heads while its pictures slide. Once they land, the
    /// strip is there already.
    pub(crate) fn heading(&self) -> Option<IVec2> {
        self.landing.is_none().then_some(self.target)
    }

    /// Whether the real windows land under the cover.
    fn lands(&self) -> bool {
        self.landing.is_some()
    }

    /// The windows the slide hid, as a test sees them.
    #[cfg(test)]
    pub(crate) fn hid(&self) -> &[Entity] {
        &self.hid
    }

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
#[derive(bevy::ecs::component::Component, Clone, Debug)]
pub(crate) struct CoverSwipe {
    display: CGDirectDisplayID,
    /// The strip x the cover shows.
    shown: i32,
    /// The display's bounds and the strip's viewport when the swipe began.
    bounds: IRect,
    viewport: IRect,
    /// The part of the display the cover hides, as [`CoverSlide::area`].
    area: IRect,
    /// Every window the cover hid since the slide or swipe that put it up,
    /// as [`CoverSlide::hid`]: the window server may still show one where
    /// the engine no longer has it, as a window tucked at an edge.
    hid: Vec<Entity>,
    /// The strip x the real windows wait at until the swipe lands, as
    /// [`CoverSlide::rest`].
    rest: i32,
    /// The strip's layout when the swipe began, as [`layout_print`] gives it.
    layout: u64,
}

impl CoverSwipe {
    /// The windows the swipe's cover hid, as a test sees them.
    #[cfg(test)]
    pub(crate) fn hid(&self) -> &[Entity] {
        &self.hid
    }
}

/// Displays on which a cover started during this frame's `UpdateSet::Layout`
/// pass, shared across `cover_resizes`, `cover_strip_moves`, and
/// `cover_swipes` so an aborting strip on the same display never hides a
/// cover another strip just put up.
#[derive(Resource, Debug, Default)]
pub(super) struct StartedCovers(pub(super) Vec<CGDirectDisplayID>);

pub(crate) fn register(app: &mut App) {
    app.init_resource::<CoverPhotos>()
        .init_resource::<StripLayouts>()
        .init_resource::<StartedCovers>()
        .init_resource::<resize::RestingStrips>()
        .add_systems(
            Update,
            keep_cover_photos
                .in_set(UpdateSet::Events)
                .run_if(on_message::<Event>),
        )
        .add_systems(
            Update,
            catch_slides
                .in_set(UpdateSet::Input)
                .after(scroll::swipe_gesture)
                .before(scroll::apply_inertia),
        )
        .add_systems(
            Update,
            (
                resize::cover_resizes,
                resize::start_waiting_morphs,
                resize::apply_morph_replies,
                cover_strip_moves,
                cover_swipes,
                mark_moved,
                remember_layouts,
            )
                .chain()
                .in_set(UpdateSet::Layout)
                .after(ensure_visible_in_strip)
                .before(position_layout_strips),
        )
        .add_systems(
            PostUpdate,
            (
                lift_covers,
                resize::lift_morphs,
                resize::refresh_morph_pictures,
                drop_orphan_covers,
                resize::remember_resting,
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

/// Keeps each background picture that arrives for the ask it answers, and
/// stages morph replies for [`resize::apply_morph_replies`] so they are
/// validated against the frame's resolved turns and aborts before touching
/// the cache.
fn keep_cover_photos(
    mut messages: MessageReader<Event>,
    mut photos: ResMut<CoverPhotos>,
    mut screen: Option<NonSendMut<CoverScreenHandle>>,
    config: Res<Config>,
    time: Res<Time>,
) {
    if !cover_enabled(&config) || photos.access != Access::Allowed {
        // A picture that arrives after the switch or the permission went
        // is dropped, never kept.
        messages.clear();
        return;
    }
    for event in messages.read() {
        if let Event::WindowTitleChanged { window_id } = event {
            photos.retitle(*window_id, time.elapsed());
            continue;
        }
        let Event::CoverPhoto {
            subject,
            generation,
            result,
        } = event
        else {
            continue;
        };
        let answers = |asked: &Asked| asked.generation == *generation && asked.subject == *subject;
        if let Some(asked) = photos.asked.take_if(|asked| answers(asked)) {
            apply_cover_reply(
                &mut photos,
                screen.as_deref_mut(),
                *subject,
                asked.size,
                asked.excluding,
                result,
                time.elapsed(),
            );
            continue;
        }
        let Some(index) = photos.morph_asked.iter().position(answers) else {
            continue;
        };
        let asked = photos.morph_asked.swap_remove(index);
        if matches!(result, Err(PhotoError::Refused)) {
            apply_cover_reply(
                &mut photos,
                screen.as_deref_mut(),
                *subject,
                asked.size,
                asked.excluding,
                result,
                time.elapsed(),
            );
            continue;
        }
        if let CoverSubject::Window(id) = *subject {
            photos.morph_replies.push(MorphReply {
                id,
                size: asked.size,
                generation: *generation,
                result: result.clone(),
                taken: time.elapsed(),
            });
        }
    }
}

/// Applies one accepted capture reply to `photos` and `screen`.
fn apply_cover_reply(
    photos: &mut CoverPhotos,
    screen: Option<&mut CoverScreenHandle>,
    subject: CoverSubject,
    size: IVec2,
    excluding: Vec<WinID>,
    result: &Result<Photo, PhotoError>,
    now: Duration,
) {
    match result {
        Ok(photo) => {
            let kept = Kept {
                photo: photo.clone(),
                size,
                taken: now,
                excluding,
            };
            match subject {
                CoverSubject::Window(id) => {
                    photos.blank_until.remove(&id);
                    photos.keep_window(id, kept);
                }
                CoverSubject::Desktop(display) => {
                    photos.desktops.insert(display, kept);
                }
            }
        }
        Err(PhotoError::Blank) => {
            if let CoverSubject::Window(id) = subject {
                debug!("cover picture of window {id} is blank");
                photos.blank_until.insert(id, now + BLANK_RETRY);
            }
        }
        Err(error @ (PhotoError::WindowGone | PhotoError::Failed)) => {
            // A failed lookup is looked up afresh next time.
            if let CoverSubject::Window(id) = subject {
                if *error == PhotoError::WindowGone {
                    photos.forget_window(id);
                }
                if let Some(screen) = screen {
                    screen.0.forget(Some(id));
                }
            }
        }
        Err(PhotoError::Refused) => {
            debug!("cover pictures refused by macOS");
            photos.access = if screen
                .as_ref()
                .is_some_and(|screen| screen.0.may_photograph())
            {
                Access::Refused
            } else {
                Access::Off
            };
            photos.clear();
            if let Some(screen) = screen {
                screen.0.forget(None);
            }
        }
    }
}

/// A tiled window of a strip, with the frame it shows from the strip's
/// origin.
struct StripWindow {
    id: WinID,
    entity: Entity,
    frame: IRect,
    /// The frame it heads to: a window being resized shows its new width.
    heading: IRect,
    /// Its cell's size, and the size a resize on its way asks for.
    size: IVec2,
    resize: Option<IVec2>,
    /// The widths the window was found to accept, if it refused one.
    limits: Option<WidthLimits>,
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
        Option<&'static ResizeMarker>,
        Has<FocusedMarker>,
        Option<&'static WidthLimits>,
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
            let (window, layout, bounds, unmanaged, moving, resize, focused, limits) =
                windows.get(entity).ok()?;
            if unmanaged.is_some() {
                return None;
            }
            let cell = IRect::from_corners(layout.0, layout.0 + bounds.0);
            // A width change reserves its final slot in the layout at once,
            // as `position_layout_windows` places it.
            let resize_to = resize.map(|resize| bounds.0.with_x(resize.0.x));
            let heading = IRect::from_corners(layout.0, layout.0 + resize_to.unwrap_or(bounds.0));
            Some(StripWindow {
                id: window.id(),
                entity,
                frame: window.margins().shown(cell),
                heading: window.margins().shown(heading),
                size: bounds.0,
                resize: resize.map(|resize| resize.0),
                limits: limits.copied(),
                at_rest: !moving && resize.is_none() && !window.write_pending(),
                tabbed: tabbed.contains(&entity),
                focused,
            })
        })
        .collect()
}

impl StripWindow {
    /// The size the window's cell heads to: a resize on its way sets its
    /// width, and the layout eases its height on its own.
    fn resize_to(&self) -> Option<IVec2> {
        self.resize.map(|resize| self.size.with_x(resize.x))
    }

    /// Whether a resize on its way changes the window's height.
    fn turns_height(&self) -> bool {
        self.resize.is_some_and(|resize| resize.y != self.size.y)
    }

    /// Whether a resize on its way asks for a width the window already
    /// refused: the layout clamps it a pass later and moves the strip again.
    fn asks_refused_width(&self) -> bool {
        let (Some(resize), Some(limits)) = (self.resize, self.limits) else {
            return false;
        };
        limits.min.is_some_and(|min| resize.x < min) || limits.max.is_some_and(|max| resize.x > max)
    }
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

/// What a cover over `area` of `strip`'s display, the display's visible
/// area with its edge padding, at strip origin `at`, would show, or why it
/// cannot: it needs a picture of every window of the strip, so a move that
/// turns, or a swipe, shows each window wherever it goes. The focused window
/// comes last, over any window it overlaps.
fn cover_scene<'a>(
    strip: &LayoutStrip,
    display: &Display,
    area: IRect,
    at: IVec2,
    windows: &CoverWindows,
    floating: &Query<(&Window, &Unmanaged)>,
    photos: &'a CoverPhotos,
) -> Result<CoverScene<'a>, &'static str> {
    let row = strip_windows(strip, windows);
    if row.is_empty() {
        return Err("no windows");
    }
    if row.iter().any(|window| !window.at_rest) {
        return Err("a window moves");
    }
    scene_of(row, display, area, at, floating, photos)
}

/// What a cover over `area` would show of the windows in `row`, each at its
/// `frame` from the strip origin `at`.
fn scene_of<'a>(
    mut row: Vec<StripWindow>,
    display: &Display,
    area: IRect,
    at: IVec2,
    floating: &Query<(&Window, &Unmanaged)>,
    photos: &'a CoverPhotos,
) -> Result<CoverScene<'a>, &'static str> {
    if row.iter().any(|window| window.tabbed) {
        return Err("tabbed windows");
    }
    let floats_over = floating.iter().any(|(window, unmanaged)| {
        matches!(unmanaged, Unmanaged::Floating) && !window.shown_frame().intersect(area).is_empty()
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
        display_bounds: display.physical_bounds(),
        viewport: area,
        desktop,
        strip: at,
        pictures,
    })
}

/// A digest of `strip`'s layout: its tiled windows and the frames they show
/// from the strip's origin. Two prints differ when a window joins, leaves,
/// moves, or resizes in the layout, not when a system only touches the
/// strip.
fn layout_print(strip: &LayoutStrip, windows: &CoverWindows) -> u64 {
    let mut hasher = DefaultHasher::new();
    for window in strip_windows(strip, windows) {
        window.entity.to_bits().hash(&mut hasher);
        // Where it heads too: a resize on its way changes the layout before
        // the window's own size does.
        (window.frame.min, window.frame.max).hash(&mut hasher);
        (window.heading.min, window.heading.max).hash(&mut hasher);
    }
    hasher.finish()
}

/// Adds to `hid` each tiled window of `strip` whose frame meets `area`
/// where the window shows now, or where it lands with the strip origin at
/// `at`: a cover over `area` hides it there, and the window server may
/// still show it there as the cover comes down.
fn note_hidden(
    hid: &mut Vec<Entity>,
    strip: &LayoutStrip,
    windows: &CoverWindows,
    area: IRect,
    at: IVec2,
) {
    let meets = |rect: IRect| !rect.intersect(area).is_empty();
    for window in strip_windows(strip, windows) {
        let lands = IRect::from_corners(window.frame.min + at, window.frame.max + at);
        let shows = windows
            .get(window.entity)
            .is_ok_and(|(found, ..)| meets(found.shown_frame()));
        if (shows || meets(lands)) && !hid.contains(&window.entity) {
            hid.push(window.entity);
        }
    }
}

/// Each strip's [`layout_print`] at the end of the last pass, so a move can
/// tell whether its strip's layout changed in this pass.
#[derive(Resource, Debug, Default)]
pub(crate) struct StripLayouts(HashMap<Entity, u64>);

/// Remembers each strip's layout for the next pass.
fn remember_layouts(
    strips: Query<(Entity, &LayoutStrip)>,
    windows: CoverWindows,
    mut layouts: ResMut<StripLayouts>,
) {
    layouts.0 = strips
        .iter()
        .map(|(entity, strip)| (entity, layout_print(strip, &windows)))
        .collect();
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
        Option<&'static mut Scrolling>,
        Has<NativeFullscreenMarker>,
        Has<CoverMorph>,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

/// Starts a cover for each eligible strip move, steers a cover whose strip
/// gets a new target, and lands the real windows once the pictures do. A
/// trackpad swipe that starts while the pictures slide or land takes them
/// over where they show, on the same cover.
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
    mission_control: Res<MissionControlActive>,
    layouts: Res<StripLayouts>,
    mut started: ResMut<StartedCovers>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut commands: Commands,
) {
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
        slide,
        child,
        active,
        mut scrolling,
        fullscreen,
        morphing,
    ) in &mut strips
    {
        let target = marker.as_ref().map(|marker| marker.0);
        let fresh = marker.as_ref().is_some_and(DetectChanges::is_changed);
        // A strip under a resize's cover moves with that cover.
        if morphing || slide.is_none() && !fresh {
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
                let showing = sample(&mut slide, screen.0.as_ref(), step.secs(), time.elapsed());
                screen.0.track(slide.display, showing);
                showing
            } else {
                // Landed, the pictures rest at the slide's target, where a
                // swipe that starts with this frame may have moved `Position`.
                f64::from(slide.target.x)
            };
            // Anything but a new target for the strip ends the cover: the
            // windows go where the slide headed, at once. A strip no longer
            // shown keeps where the workspace switch put it.
            // A display or a Dock that changes under the slide ends it too,
            // even when the strip's layout stays the same.
            let same_place = display.id() == slide.display
                && display.bounds() == slide.bounds
                && viewport == slide.viewport;
            let mut fresh = fresh;
            if fresh && target == Some(slide.target) {
                // The same target set again, as a Snap can, leaves the slide
                // as it is, and a swipe that starts with it takes it over.
                commands.entity(entity).remove::<RepositionMarker>();
                fresh = false;
            }
            let turns_height = target.is_some_and(|target| fresh && target.y != position.0.y);
            // A new target for the strip, which the slide turns to.
            let turn = target.filter(|_| fresh && !turns_height);
            let keeps = cover_enabled(&config)
                && shown_here
                && same_place
                && !dragging
                && !mission_control.0
                && layout_print(&strip, &windows) == slide.layout;
            // Only a swipe with no strip move of its own this frame takes the
            // pictures over: a move goes to its turn or ends the cover.
            if keeps
                && !fresh
                && let Some(scroll) = scrolling
                    .as_mut()
                    .filter(|scroll| scroll.input == ScrollInput::Gesture)
            {
                // A swipe that starts while the pictures slide or land takes
                // them over where they show, on the same cover. Taking the
                // cover down there showed the real windows at the slide's
                // target, then jumped them to where the fingers had moved
                // the strip.
                end_guard(slide.guard.take(), &mut commands);
                if slide.landing.is_none() && slide.caught.is_none() {
                    // The swipe began from the real windows' place, but the
                    // pictures show the strip elsewhere: the fingers move it
                    // on from there, and it rests where the slide headed.
                    // [`catch_slides`] does this before the scroll systems
                    // run, unless the swipe started without it.
                    rebase(scroll, showing, slide.target.x);
                    position.0.x = round_px(scroll.position);
                }
                screen.0.place(slide.display, f64::from(position.0.x));
                debug!(
                    "cover over strip {entity} carries on under a swipe from {}",
                    position.0.x
                );
                commands
                    .entity(entity)
                    .remove::<(CoverSlide, RepositionMarker)>()
                    .insert(CoverSwipe {
                        display: slide.display,
                        shown: position.0.x,
                        bounds: slide.bounds,
                        viewport: slide.viewport,
                        area: slide.area,
                        hid: std::mem::take(&mut slide.hid),
                        rest: slide.rest,
                        layout: slide.layout,
                    });
                continue;
            }
            let ended = if !cover_enabled(&config) {
                Some("switched off")
            } else if !shown_here {
                Some("strip not shown")
            } else if !same_place {
                Some("display or viewport changed")
            } else if scrolling.is_some() && turn.is_none() && !turns_height {
                Some("scroll")
            } else if dragging {
                Some("drag")
            } else if mission_control.0 {
                Some("Mission Control")
            } else if turns_height {
                Some("vertical move")
            } else if layout_print(&strip, &windows) != slide.layout {
                Some("layout changed")
            } else {
                None
            };
            if let Some(reason) = ended {
                debug!("cover over strip {entity} ended early: {reason}");
                // A strip moved to another display keeps where that display
                // put it.
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
                if !started.0.contains(&slide.display) {
                    screen.0.hide(slide.display);
                }
                commands.entity(entity).remove::<CoverSlide>();
                continue;
            }
            if let Some(target) = turn {
                // A new target while the pictures slide: the slide turns
                // there from where it shows, at the speed it has. After
                // they land, the strip already rests at the old target.
                // A slide shown landed has no speed left to carry on. A
                // swipe that starts with it gives way to it, as a strip
                // move during a covered swipe does.
                let speed = if slide.landing.is_none() && slide.elapsed < slide.length() {
                    slide.speed_at(slide.elapsed)
                } else {
                    0.0
                };
                let (guard, layout) = (slide.guard, slide.layout);
                let mut hid = std::mem::take(&mut slide.hid);
                note_hidden(&mut hid, &strip, &windows, slide.area, target);
                let rest = slide.rest;
                *slide = start_slide(
                    slide.display,
                    (slide.bounds, slide.viewport, slide.area),
                    showing,
                    speed,
                    target,
                    &config,
                );
                slide.guard = guard;
                slide.layout = layout;
                slide.hid = hid;
                slide.rest = rest;
                screen.0.slide(slide.display, &slide.path(), PATH_STEP);
                debug!("cover over strip {entity} turned to {}", target.x);
                let mut entity_commands = commands.entity(entity);
                entity_commands.remove::<RepositionMarker>();
                if scrolling.is_some() {
                    // The swipe that started with the move gives way, and
                    // the real windows stay where they are laid out, not
                    // where its first frame moved the strip.
                    entity_commands.remove::<Scrolling>();
                    position.0.x = rest;
                }
                continue;
            }
            if slide.landing.is_none() && slide.elapsed >= slide.length() {
                // The pictures have landed: the real windows follow in one
                // step under the cover.
                let (area, target) = (slide.area, slide.target);
                note_hidden(&mut slide.hid, &strip, &windows, area, target);
                position.0 = slide.target;
                slide.rest = slide.target.x;
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
        } else if scrolling.is_some() {
            Some("swipe")
        } else if fullscreen {
            Some("fullscreen")
        } else if dragging {
            Some("drag")
        } else if target.y != position.0.y {
            Some("vertical move")
        } else if layouts.0.get(&entity) != Some(&layout_print(&strip, &windows)) {
            // A layout that changed in this pass shows its windows where
            // they were, not where the pictures would put them.
            Some("layout changed")
        } else {
            None
        };
        if let Some(reason) = refused {
            debug!("cover skipped over strip {entity}: {reason}");
            continue;
        }
        let scene = match cover_scene(
            &strip,
            display,
            display.visible_bounds(dock),
            position.0,
            &windows,
            &floating,
            &photos,
        ) {
            Ok(scene) => scene,
            Err(reason) => {
                debug!("cover skipped over strip {entity}: {reason}");
                continue;
            }
        };
        let (pictures, area) = (scene.pictures.len(), scene.viewport);
        screen.0.show(&scene);
        started.0.push(display.id());
        let mut slide = start_slide(
            display.id(),
            (display.bounds(), viewport, area),
            f64::from(from),
            0.0,
            target,
            &config,
        );
        slide.layout = layout_print(&strip, &windows);
        note_hidden(&mut slide.hid, &strip, &windows, area, target);
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

/// Where the pictures of `slide` show now, as Core Animation draws them,
/// which runs on while the engine stalls, advancing the slide's clock by
/// this frame's animation `step` at `now`. The first look in a frame
/// samples them, and later looks in the same frame get the same answer.
fn sample(slide: &mut CoverSlide, screen: &dyn CoverScreen, step: f64, now: Duration) -> f64 {
    if let Some((at, showing)) = slide.sampled
        && at == now
    {
        return showing;
    }
    slide.counted += step;
    let showing = match screen.showing(slide.display) {
        // Core Animation runs on while the engine stalls, and the engine's
        // count can run ahead of it, so the slide's clock is where the
        // screen shows it: a turn and the landing follow the screen.
        Some(showing) if slide.counted < slide.length() + SHOWN_GRACE => {
            // Within half a point of the target, the slide shows landed,
            // whatever rounding is left in the sample.
            slide.elapsed = if (showing - f64::from(slide.target.x)).abs() < 0.5 {
                slide.length()
            } else {
                slide.phase_of(showing)
            };
            showing
        }
        _ => {
            slide.elapsed = slide.counted;
            slide.x_at(slide.elapsed)
        }
    };
    slide.sampled = Some((now, showing));
    showing
}

/// Moves the scroll state of a swipe that catches a slide's pictures
/// mid-way to where they show: the swipe began from the real windows'
/// place, but the fingers move the strip on from `showing`, and it rests at
/// strip x `target`, where the slide headed.
fn rebase(scroll: &mut Scrolling, showing: f64, target: i32) {
    let travel = scroll.position - scroll.origin;
    scroll.origin = showing;
    scroll.position = showing + travel;
    scroll.anchor = f64::from(target);
    // The pictures may show the strip stretched past an end or a column:
    // the scroll systems turn `showing` into the finger travel that shows
    // it there, rather than stretch it again.
    scroll.caught = Some(showing);
}

/// Moves each trackpad swipe that starts while a slide's pictures move or
/// rest landed to where they show, before the scroll systems hold the strip
/// to its ends and columns and pick its landing column from where it rests.
/// [`cover_strip_moves`] then hands the cover over to the swipe as it is.
fn catch_slides(
    mut strips: Query<(&mut CoverSlide, &mut Scrolling), Added<Scrolling>>,
    time: Res<Time>,
    step: Res<AnimationStep>,
    screen: Option<NonSend<CoverScreenHandle>>,
) {
    let Some(screen) = screen else {
        return;
    };
    for (mut slide, mut scroll) in &mut strips {
        if scroll.input != ScrollInput::Gesture {
            continue;
        }
        // Landed pictures rest at the slide's target, which may lie past an
        // end, as a centered strip does, while the real windows catch up.
        let showing = if slide.landing.is_none() {
            sample(&mut slide, screen.0.as_ref(), step.secs(), time.elapsed())
        } else {
            f64::from(slide.target.x)
        };
        rebase(&mut scroll, showing, slide.target.x);
        slide.caught = Some(showing);
    }
}

/// A slide from strip x `from`, moving at `speed` points per second, to
/// `target`, under a cover that hides `area`.
fn start_slide(
    display: CGDirectDisplayID,
    (bounds, viewport, area): (IRect, IRect, IRect),
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
        area,
        hid: Vec::new(),
        caught: None,
        rest: round_px(from),
        sampled: None,
        counted: 0.0,
        layout: 0,
    }
}

/// Takes each cover down once the real windows under it have taken their
/// places, and the window server shows each window the cover hid at the
/// frame the layout committed it to, or once it has waited
/// [`LANDING_TIMEOUT`]. A write can return before its app shows the new
/// frame, so the cover came down over a window still where it was.
fn lift_covers(
    mut strips: Query<(Entity, &LayoutStrip, &mut CoverSlide)>,
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
    for (entity, strip, mut slide) in &mut strips {
        let Some((deadline, settled)) = slide.landing else {
            continue;
        };
        let row = strip_windows(strip, &windows);
        let in_place = row.iter().all(|window| window.at_rest)
            && shown_frames.as_ref().is_none_or(|shown| {
                resize::shows_committed(&row, &frames, slide.area, &slide.hid, shown)
            });
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
    morphs: Query<(Entity, &CoverMorph, Option<&ChildOf>)>,
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
    for (entity, morph, child) in &morphs {
        if on(child, morph.display()) {
            held.push(morph.display());
        } else {
            end_guard(morph.guard(), &mut commands);
            commands.entity(entity).remove::<CoverMorph>();
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
        &'static mut Position,
        Option<Ref<'static, Scrolling>>,
        Option<&'static mut CoverSwipe>,
        Option<Ref<'static, RepositionMarker>>,
        Has<CoverSlide>,
        Has<CoverMorph>,
        &'static ChildOf,
        Has<ActiveWorkspaceMarker>,
        Has<NativeFullscreenMarker>,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

/// Puts a trackpad swipe under the cover when it starts with every picture
/// of its strip ready, moves the pictures with the strip, and lands the
/// real windows in one step once the swipe settles. Modifier-scroll stays
/// uncovered: the wheel scrolls in steps that real windows follow well.
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
    mission_control: Res<MissionControlActive>,
    layouts: Res<StripLayouts>,
    screen: Option<NonSendMut<CoverScreenHandle>>,
    mut started: ResMut<StartedCovers>,
    mut commands: Commands,
) {
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
        scrolling,
        swipe,
        marker,
        sliding,
        morphing,
        child,
        active,
        fullscreen,
    ) in &mut strips
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
            // A strip move that arrives during the swipe, such as a key or a
            // press on a picture, ends the cover, as does a display or Dock
            // that changes under it, or Mission Control. A move to where the
            // strip already is moves nothing: the swipe goes on, or lands as
            // it would without it.
            let marked = marker.as_ref().is_some_and(DetectChanges::is_changed);
            let moved = marked && marker.as_ref().is_some_and(|marker| marker.0 != position.0);
            if marked && !moved {
                commands.entity(entity).remove::<RepositionMarker>();
            }
            let same_place = display.bounds() == swipe.bounds
                && display.actual_display_bounds(dock, &config) == swipe.viewport;
            let ended = if !cover_enabled(&config) {
                Some("switched off")
            } else if !shown_here {
                Some("strip not shown")
            } else if !same_place {
                Some("display or viewport changed")
            } else if dragging {
                Some("drag")
            } else if mission_control.0 {
                Some("Mission Control")
            } else if layout_print(&strip, &windows) != swipe.layout {
                Some("layout changed")
            } else if moved {
                Some("strip move")
            } else {
                None
            };
            // A strip move that arrives during the swipe, such as the one
            // that brings the newly focused column into view, carries the
            // pictures on to where it heads. Taking the cover down there
            // showed the real windows where the swipe began, then the
            // desktop between them as each app moved its window.
            if ended == Some("strip move")
                && let Some(marker) = marker.as_ref()
                && marker.0.y == position.0.y
            {
                let from = f64::from(position.0.x);
                let viewport = display.actual_display_bounds(dock, &config);
                // The pictures carry on at the speed the strip had, so the
                // slide does not start from rest.
                let speed = scrolling.as_ref().map_or(0.0, |scrolling| {
                    scroll::strip_speed(scrolling, f64::from(viewport.width()), &config)
                });
                let mut slide = start_slide(
                    swipe.display,
                    (display.bounds(), viewport, swipe.area),
                    from,
                    speed,
                    marker.0,
                    &config,
                );
                slide.layout = swipe.layout;
                slide.hid = std::mem::take(&mut swipe.hid);
                note_hidden(&mut slide.hid, &strip, &windows, swipe.area, marker.0);
                // The real windows wait where the swipe found them until the
                // slide lands, not where the fingers last took the strip.
                slide.rest = swipe.rest;
                position.0.x = swipe.rest;
                screen.0.slide(swipe.display, &slide.path(), PATH_STEP);
                screen.0.track(swipe.display, from);
                debug!(
                    "cover over swiped strip {entity} slides on from {from} to {}",
                    marker.0.x
                );
                commands
                    .entity(entity)
                    .remove::<(CoverSwipe, RepositionMarker, Scrolling)>()
                    .insert(slide);
                continue;
            }
            let aborted = ended.is_some();
            if aborted || scrolling.is_none() {
                // The swipe has landed, or the cover cannot stay: the real
                // windows take their places in one step.
                let guard = spawn_snap_strip_guard(entity, &mut commands);
                let mut entity_commands = commands.entity(entity);
                entity_commands.remove::<CoverSwipe>().insert(Moved);
                if let Some(reason) = ended {
                    debug!("cover over swiped strip {entity} ended early: {reason}");
                    if !started.0.contains(&swipe.display) {
                        screen.0.hide(swipe.display);
                    }
                } else {
                    screen.0.place(swipe.display, f64::from(position.0.x));
                    let mut hid = std::mem::take(&mut swipe.hid);
                    note_hidden(&mut hid, &strip, &windows, swipe.area, position.0);
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
                        area: swipe.area,
                        hid,
                        caught: None,
                        rest: position.0.x,
                        sampled: None,
                        counted: 0.0,
                        layout: swipe.layout,
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

        // A strip move on its way takes the strip: a swipe that starts with
        // it gets no cover, which would hold the move back until it lands.
        let moving = marker.as_ref().is_some_and(|marker| marker.0 != position.0);
        if sliding
            || morphing
            || moving
            || !cover_enabled(&config)
            || !shown_here
            || fullscreen
            || dragging
            || mission_control.0
            || layouts.0.get(&entity) != Some(&layout_print(&strip, &windows))
        {
            continue;
        }
        let scene = match cover_scene(
            &strip,
            display,
            display.visible_bounds(dock),
            position.0,
            &windows,
            &floating,
            &photos,
        ) {
            Ok(scene) => scene,
            Err(reason) => {
                debug!("cover skipped over swiped strip {entity}: {reason}");
                continue;
            }
        };
        let (pictures, area) = (scene.pictures.len(), scene.viewport);
        screen.0.show(&scene);
        started.0.push(display.id());
        screen.0.track(display.id(), f64::from(position.0.x));
        debug!("cover over swiped strip {entity}: {pictures} pictures");
        let mut hid = Vec::new();
        note_hidden(&mut hid, &strip, &windows, area, position.0);
        commands.entity(entity).insert(CoverSwipe {
            display: display.id(),
            shown: position.0.x,
            bounds: display.bounds(),
            viewport: display.actual_display_bounds(dock, &config),
            area,
            hid,
            // Where the strip was when the fingers moved it first.
            rest: scrolling
                .as_ref()
                .map_or(position.0.x, |scrolling| round_px(scrolling.origin)),
            layout: layout_print(&strip, &windows),
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

/// A strip under a cover, by a move or a swipe.
pub(crate) type UnderCover = Or<(With<CoverSlide>, With<CoverSwipe>, With<CoverMorph>)>;

/// Every strip under a cover.
type AnyCover<'w, 's> = Query<'w, 's, (), UnderCover>;

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
    if config.is_changed() {
        photos.checked_at = None;
    }
    check_access(&mut photos, &config, screen.0.as_ref(), now);
    if photos.access != Access::Allowed || !cover_enabled(&config) {
        if !photos.is_clear() {
            photos.clear();
            screen.0.forget(None);
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
    let gone = photos
        .windows
        .keys()
        .filter(|id| !alive.contains(id))
        .copied()
        .collect::<Vec<_>>();
    for id in gone {
        photos.forget_window(id);
        screen.0.forget(Some(id));
    }

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
    photos.supersede(want.subject);
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
                size: display.physical_bounds().size(),
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
                .is_ok_and(|(.., focused, _)| focused);
            let size = window.frame.size();
            let kept = photos.windows.get(&window.id);
            let age = kept.map_or(Duration::MAX, |kept| now.saturating_sub(kept.taken));
            let retitled = kept.is_some_and(|kept| {
                photos
                    .retitled
                    .get(&window.id)
                    .is_some_and(|retitled| kept.taken < *retitled)
            });
            let priority = match kept {
                None => 0,
                Some(kept) if !close(kept.size, size) => 0,
                // Its title changed since, so it shows another tab or
                // document.
                Some(_) if retitled => 1,
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
