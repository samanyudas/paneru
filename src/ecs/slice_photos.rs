//! Panels that show photos of the slices on shared display edges, and the
//! photos themselves: one panel per showing slice, a photo when it first
//! shows, and one every [`REFRESH`] while someone can see it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bevy::app::{App, PostUpdate, Update};
use bevy::ecs::change_detection::{DetectChanges as _, DetectChangesMut as _};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::{any_with_component, resource_changed};
use bevy::ecs::schedule::{IntoScheduleConfigs as _, SystemCondition as _};
use bevy::ecs::system::{
    Commands, In, IntoSystem as _, Local, NonSend, NonSendMut, Query, Res, ResMut, SystemParam,
};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use bevy::time::common_conditions::on_timer;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use tracing::{debug, warn};

use crate::config::Config;
use crate::ecs::layout::{LayoutStrip, lies_mostly_elsewhere};
use crate::ecs::pacing::{self, SlideHolds};
use crate::ecs::shared_edge::{
    EdgeSlice, Hold, PhotoWait, Slide, showing_edge_slices, showing_may_change,
};
use crate::ecs::systems::{self, AnimationStep, ease_out_factor, ease_step};
use crate::ecs::{
    Position, PostUpdateSet, PresentedOrigin, RepositionMarker, Scrolling, SelectedVirtualMarker,
    UpdateSet,
};
use crate::events::Event;
use crate::manager::window_writer::LANDING_TIMEOUT;
use crate::manager::{Display, Window, WindowManager};
use crate::platform::WinID;
use crate::types::commands::Side;

/// How often a shown slice is photographed again.
pub(crate) const REFRESH: Duration = Duration::from_secs(2);

/// How long a window heading for a tuck waits for its photo before it tucks
/// without one. Most photos arrive within 150 ms, but the first photo after
/// the engine starts took a second on Sam's Mac, while the engine laid out
/// every window. A window that tucks without a photo leaves the desktop
/// showing on the edge, because some apps draw nothing once tucked.
const PHOTO_WAIT: Duration = Duration::from_millis(1500);

/// How long a window leaving the screen waits for a new photo before its
/// kept photo shows instead. A kept photo can show what the window looked
/// like before it came on screen, and a photo taken once the window tucks
/// comes back blank, so a late photo leaves the old one up (#451). On Sam's
/// Mac a new photo of a test window took 120 ms, and one of a Helium window
/// that needed a lookup took 256 to 276 ms.
const KEPT_WAIT: Duration = Duration::from_millis(750);

pub(crate) fn register(app: &mut App) {
    app.init_resource::<SliceAccess>()
        .init_resource::<Slices>()
        // Between the swipe's input and layout, which tucks a window that the
        // swipe's first step carries past the edge.
        .add_systems(
            Update,
            renew_kept_on_swipe
                .after(UpdateSet::Input)
                .before(UpdateSet::Layout),
        )
        .add_systems(
            PostUpdate,
            (slide_photos, slide_photos_out)
                .in_set(PostUpdateSet::Animate)
                .after(pacing::pace_rows)
                .before(systems::commit_window_frame)
                .run_if(any_with_component::<Slide>),
        )
        // The panels take their places before the frame's window writes, so
        // a window ahead of a sliding photo never shows its move first.
        .add_systems(
            PostUpdate,
            (
                update_access.run_if(resource_changed::<Config>.or_eager(on_timer(REFRESH))),
                receive_photos,
                show_kept_when_late,
                showing_edge_slices
                    .pipe(present_slices)
                    .run_if(showing_may_change.or_eager(resource_changed::<SliceAccess>)),
                refresh_slices.run_if(on_timer(REFRESH)),
            )
                .chain()
                .in_set(PostUpdateSet::Animate)
                .after(slide_photos)
                .after(slide_photos_out)
                .before(systems::commit_window_frame),
        );
}

/// Identifies a photo's pixels: two photos with the same digest look the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digest(pub u64);

#[derive(Clone, Debug)]
pub(crate) struct Photo {
    pub digest: Digest,
    pub image: CFRetained<CGImage>,
}

/// Why a photo did not arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhotoError {
    /// The window is no longer there to photograph.
    WindowGone,
    /// macOS refused to photograph windows for the engine.
    Refused,
    Failed,
    /// The photo shows almost nothing, as the window of an app that stops
    /// drawing off screen does.
    Blank,
}

/// Tells the capture of one photo whether the engine still wants it. The
/// engine calls the photo off when it stops waiting for it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ticket(Arc<AtomicBool>);

impl Ticket {
    pub fn called_off(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// The engine's hold on a photo on its way. Dropping it calls the photo off.
#[derive(Debug)]
struct Wanted(Ticket);

impl Drop for Wanted {
    fn drop(&mut self) {
        self.0.0.store(true, Ordering::Relaxed);
    }
}

/// How a photo finds the window it photographs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// Looks the window up among the windows macOS can photograph, which
    /// costs more than the photo itself.
    Fresh,
    /// Reuses the window's last lookup, and looks up only when it has none.
    Reuse,
}

/// Shows the slice panels and photographs the windows behind them.
/// [`SliceScreenOS`](crate::slice_screen::SliceScreenOS) does it on screen,
/// and tests record the calls.
pub(crate) trait SliceScreen {
    /// Whether macOS lets the engine photograph other apps' windows. Never
    /// asks the user.
    fn may_photograph(&self) -> bool;
    /// Gives each of `slices` a panel at its place on screen, showing the
    /// slice's part of the window's photo, and closes every other panel.
    /// Each panel shows above the ones before it. A panel stays out of sight
    /// until its first paint. Forgets the lookups of windows that get no
    /// slice and are not `remembered`.
    fn place(&mut self, slices: &[(WinID, EdgeSlice)], remembered: &[WinID]);
    /// Starts a photo of the whole of `window`, which shows a frame of
    /// `slice.size` and gives the panel `slice`, unless `ticket` is called off
    /// before the capture. The result arrives as [`Event::SlicePhoto`]
    /// carrying `generation`. `Lookup::Reuse` reuses the window's last
    /// lookup that `place` still remembers.
    fn photograph(
        &mut self,
        window: WinID,
        slice: &EdgeSlice,
        generation: u32,
        ticket: Ticket,
        lookup: Lookup,
    );
    /// Shows `photo` in the panel of `window`.
    fn paint(&mut self, window: WinID, photo: &Photo);
    /// Whether anyone can see the panel of `window` now.
    fn seen(&self, window: WinID) -> bool;
}

/// The slice screen the engine draws with, as a non-send resource.
pub(crate) struct SliceScreenHandle(pub Box<dyn SliceScreen>);

/// Whether shared edges show slices.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SliceAccess {
    /// The switch is off, or the engine may not photograph windows.
    #[default]
    Off,
    On,
    /// macOS refused a photo while `may_photograph` answered `preflight`.
    /// Shared edges stay empty until the config or that answer changes.
    Refused {
        preflight: bool,
    },
}

/// The photo of one slice with a panel.
#[derive(Debug)]
enum SlicePhoto {
    /// A photo is on its way. `last` is the photo the panel shows now. A
    /// window leaving the screen shows its kept photo once `kept_wait` ends
    /// if the new one has not arrived.
    Asked {
        generation: u32,
        last: Option<Digest>,
        kept_wait: Option<PhotoWait>,
        _wanted: Wanted,
    },
    /// The panel shows the photo with this digest.
    Shown { digest: Digest },
    /// A photo failed. The slice keeps no panel and gets no photo until it
    /// stops showing.
    Ended,
}

impl SlicePhoto {
    fn shown(&self) -> Option<Digest> {
        match *self {
            SlicePhoto::Asked { last, .. } => last,
            SlicePhoto::Shown { digest } => Some(digest),
            SlicePhoto::Ended => None,
        }
    }
}

/// A new photo on its way for a window's kept photo, of the window at the
/// size it has now.
#[derive(Debug)]
struct Renewal {
    generation: u32,
    slice: EdgeSlice,
    _wanted: Wanted,
}

/// The showing slices, back to front, each with the slice its photo is of.
#[derive(Resource, Default)]
struct Slices {
    showing: Vec<(WinID, EdgeSlice, SlicePhoto)>,
    /// The latest photo of each window, with the size of the frame it shows,
    /// kept after its slice stops showing. A slice that shows again for a
    /// window still past the edge shows it at once, rather than waiting for a
    /// new photo that can take 700 ms. A window leaving the screen gets a new
    /// photo instead, because it may have changed while on screen, and macOS
    /// photographs a tucked window blank, so no later refresh could repair it.
    kept: BTreeMap<WinID, (EdgeSlice, Photo)>,
    /// New photos on their way of windows in view. A swipe tucks a window
    /// the moment it crosses the edge, with no time for a photo, so its kept
    /// photo is renewed, or its first one taken, while it is in view: when a
    /// swipe starts, and every [`REFRESH`].
    renewing: BTreeMap<WinID, Renewal>,
    /// The generation of the latest photo asked for, so a photo of an
    /// earlier ask is told apart.
    generation: u32,
}

impl Slices {
    fn placed(&self) -> Vec<(WinID, EdgeSlice)> {
        self.showing
            .iter()
            .filter(|(_, _, photo)| !matches!(photo, SlicePhoto::Ended))
            .map(|(id, slice, _)| (*id, *slice))
            .collect()
    }

    /// The windows with a kept photo, whose lookups the screen remembers so
    /// their next photo can skip the lookup.
    fn remembered(&self) -> Vec<WinID> {
        self.kept.keys().copied().collect()
    }

    fn photo(&self, window: WinID) -> Option<&SlicePhoto> {
        self.showing
            .iter()
            .find(|(id, _, _)| *id == window)
            .map(|(_, _, photo)| photo)
    }

    /// Whether a photo of `window` that shows `size` can show now: its panel
    /// shows one, or it has no panel and one is kept.
    fn has_photo(&self, window: WinID, size: IVec2) -> bool {
        match self.photo(window) {
            Some(photo) => photo.shown().is_some(),
            None => self
                .kept
                .get(&window)
                .is_some_and(|(kept, _)| kept.size == size),
        }
    }
}

/// The kept photo of `window`, if it shows a frame of `size`.
fn kept_photo(
    kept: &BTreeMap<WinID, (EdgeSlice, Photo)>,
    window: WinID,
    size: IVec2,
) -> Option<&Photo> {
    kept.get(&window)
        .filter(|(kept, _)| kept.size == size)
        .map(|(_, photo)| photo)
}

fn ask(
    screen: &mut dyn SliceScreen,
    generation: &mut u32,
    window: WinID,
    slice: &EdgeSlice,
    last: Option<Digest>,
    lookup: Lookup,
) -> SlicePhoto {
    *generation = generation.wrapping_add(1);
    let ticket = Ticket::default();
    screen.photograph(window, slice, *generation, ticket.clone(), lookup);
    debug!("edge slice photo asked: window {window} generation {generation}");
    SlicePhoto::Asked {
        generation: *generation,
        last,
        kept_wait: None,
        _wanted: Wanted(ticket),
    }
}

fn update_access(
    config: Res<Config>,
    mut access: ResMut<SliceAccess>,
    screen: Option<NonSend<SliceScreenHandle>>,
) {
    let next = if config.shared_edge_slices() {
        let may = screen.is_some_and(|screen| screen.0.may_photograph());
        match *access {
            SliceAccess::Refused { preflight } if !config.is_changed() && may == preflight => {
                return;
            }
            _ if may => SliceAccess::On,
            _ => SliceAccess::Off,
        }
    } else {
        SliceAccess::Off
    };
    access.set_if_neq(next);
}

/// Gives each showing slice a panel, photographs the slices that are new or
/// whose window changed size after looking their windows up, and closes the
/// panels of slices that stopped showing. A photo shows the whole window, so
/// a slice that only moved keeps it, and a new slice shows its window's kept
/// photo of the same size at once. A window leaving the screen may have
/// changed while on screen, so it waits for a new photo first, for at most
/// [`KEPT_WAIT`]. A slice whose painted panel no one can
/// see waits for the next refresh that sees it. A panel never painted is out
/// of sight, so its first photo never waits.
fn present_slices(
    In(showing): In<Vec<(WinID, EdgeSlice)>>,
    access: Res<SliceAccess>,
    mut slices: ResMut<Slices>,
    windows: Query<(&Window, Option<&Slide>)>,
    screen: Option<NonSendMut<SliceScreenHandle>>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let leaving = |id: WinID| {
        windows.iter().any(|(window, slide)| {
            window.id() == id && matches!(slide, Some(Slide::Leaving { .. }))
        })
    };
    let Slices {
        showing: shown,
        kept,
        renewing,
        generation,
    } = &mut *slices;
    let showing = if *access == SliceAccess::On {
        kept.retain(|id, _| windows.iter().any(|(window, _)| window.id() == *id));
        showing
    } else {
        kept.clear();
        Vec::new()
    };
    // A first photo is on its way for a window with none kept yet.
    if *access == SliceAccess::On {
        renewing.retain(|id, _| windows.iter().any(|(window, _)| window.id() == *id));
    } else {
        renewing.clear();
    }
    let mut before = std::mem::take(shown)
        .into_iter()
        .map(|(id, slice, photo)| (id, (slice, photo)))
        .collect::<BTreeMap<_, _>>();
    let mut from_kept = Vec::new();
    for (id, slice) in showing {
        let photo = match before.remove(&id) {
            Some((was, photo)) if was.size == slice.size || matches!(photo, SlicePhoto::Ended) => {
                photo
            }
            was => match was.and_then(|(_, photo)| photo.shown()) {
                Some(digest) if !screen.0.seen(id) => {
                    debug!("edge slice photo deferred, panel unseen: window {id}");
                    SlicePhoto::Shown { digest }
                }
                last => match kept_photo(kept, id, slice.size) {
                    Some(_) if leaving(id) => {
                        let mut asked = ask(
                            screen.0.as_mut(),
                            generation,
                            id,
                            &slice,
                            None,
                            Lookup::Reuse,
                        );
                        if let SlicePhoto::Asked { kept_wait, .. } = &mut asked {
                            *kept_wait = Some(PhotoWait::Asked);
                        }
                        asked
                    }
                    Some(photo) => {
                        from_kept.push(id);
                        SlicePhoto::Shown {
                            digest: photo.digest,
                        }
                    }
                    None => ask(
                        screen.0.as_mut(),
                        generation,
                        id,
                        &slice,
                        last,
                        Lookup::Fresh,
                    ),
                },
            },
        };
        shown.push((id, slice, photo));
    }
    screen.0.place(&slices.placed(), &slices.remembered());
    for id in from_kept {
        if let Some((_, photo)) = slices.kept.get(&id) {
            screen.0.paint(id, photo);
            debug!("edge slice photo kept: window {id}");
        }
    }
}

/// Photographs each shown slice again with its last lookup, unless no one
/// can see its panel, and renews the kept photos of the windows in view.
fn refresh_slices(
    mut slices: ResMut<Slices>,
    in_view: InView,
    next: NextToCross,
    access: Res<SliceAccess>,
    screen: Option<NonSendMut<SliceScreenHandle>>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    if *access == SliceAccess::On {
        let next = next.find(&in_view);
        renew_kept(screen.0.as_mut(), &mut slices, &in_view, &next);
    }
    let Slices {
        showing,
        generation,
        ..
    } = &mut *slices;
    for (id, slice, photo) in showing.iter_mut() {
        let SlicePhoto::Shown { digest } = *photo else {
            continue;
        };
        if !screen.0.seen(*id) {
            debug!("edge slice photo skipped, panel unseen: window {id}");
            continue;
        }
        *photo = ask(
            screen.0.as_mut(),
            generation,
            *id,
            slice,
            Some(digest),
            Lookup::Reuse,
        );
    }
}

/// Paints each photo that differs from the one its panel shows, and keeps it.
/// A blank photo leaves the panel with the photo it shows, or paints the
/// window's kept photo if the panel shows none. A blank photo of a window
/// that still waits in view for its first photo asks for another, as an app
/// that is still starting may not have drawn yet. A failed photo, or another
/// blank one for a panel that shows none, closes its panel, and a refused one
/// closes them all.
#[allow(clippy::too_many_lines)]
fn receive_photos(
    mut messages: MessageReader<Event>,
    mut slices: ResMut<Slices>,
    mut access: ResMut<SliceAccess>,
    sliding: Query<(&Window, &Slide)>,
    screen: Option<NonSendMut<SliceScreenHandle>>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let mut ended = false;
    for event in messages.read() {
        let Event::SlicePhoto {
            window_id,
            generation,
            result,
        } = event
        else {
            continue;
        };
        if receive_renewal(
            *window_id,
            *generation,
            result,
            &mut slices,
            screen.0.as_mut(),
        ) {
            continue;
        }
        let Slices {
            showing,
            kept,
            renewing,
            generation: latest,
        } = &mut *slices;
        let asked = showing
            .iter_mut()
            .find(|(id, _, _)| id == window_id)
            .and_then(|(_, slice, photo)| match *photo {
                SlicePhoto::Asked {
                    generation: asked,
                    last,
                    ..
                } if asked == *generation => Some((*slice, photo, last)),
                _ => None,
            });
        let Some((slice, photo, last)) = asked else {
            debug!("edge slice photo dropped as stale: window {window_id} generation {generation}");
            continue;
        };
        if result
            .as_ref()
            .is_err_and(|error| *error != PhotoError::Blank)
        {
            kept.remove(window_id);
            renewing.remove(window_id);
        }
        *photo = match (result, last) {
            (Ok(taken), _) => {
                if Some(taken.digest) == last {
                    debug!("edge slice unchanged: window {window_id}");
                } else {
                    screen.0.paint(*window_id, taken);
                    debug!("edge slice repainted: window {window_id}");
                }
                kept.insert(*window_id, (slice, taken.clone()));
                renewing.remove(window_id);
                SlicePhoto::Shown {
                    digest: taken.digest,
                }
            }
            (Err(PhotoError::Blank), Some(digest)) => {
                debug!("edge slice photo blank: window {window_id} keeps the photo it shows");
                SlicePhoto::Shown { digest }
            }
            (Err(PhotoError::Blank), None)
                if let Some(kept) = kept_photo(kept, *window_id, slice.size) =>
            {
                screen.0.paint(*window_id, kept);
                debug!("edge slice photo blank: window {window_id} shows its kept photo");
                SlicePhoto::Shown {
                    digest: kept.digest,
                }
            }
            (Err(PhotoError::Blank), None)
                if sliding.iter().any(|(window, slide)| {
                    window.id() == *window_id && matches!(slide, Slide::Leaving { .. })
                }) =>
            {
                debug!("edge slice photo blank: window {window_id} still shows, asking again");
                ask(
                    screen.0.as_mut(),
                    latest,
                    *window_id,
                    &slice,
                    None,
                    Lookup::Reuse,
                )
            }
            (Err(PhotoError::Refused), _) => {
                if *access == SliceAccess::On {
                    warn!(
                        "macOS refused a photo of window {window_id}: shared edges show nothing until the config or the Screen Recording permission changes"
                    );
                }
                access.set_if_neq(SliceAccess::Refused {
                    preflight: screen.0.may_photograph(),
                });
                SlicePhoto::Ended
            }
            (Err(error), _) => {
                debug!("edge slice photo failed: window {window_id}: {error:?}, closing its panel");
                ended = true;
                SlicePhoto::Ended
            }
        };
    }
    if ended {
        screen.0.place(&slices.placed(), &slices.remembered());
    }
}

/// The windows that show themselves rather than a slice, and are not on
/// their way to or from one.
type InView<'w, 's> = Query<'w, 's, &'static Window, (Without<EdgeSlice>, Without<Slide>)>;

/// Asks for a new photo of each window in view with a kept photo, unless the
/// window shows a slice. A new ask calls off one still on its way. A window
/// in `next`, which a swipe would carry past a shared edge first, gets its
/// first photo if it has none, as after the engine starts with it tucked:
/// past the edge it photographs blank.
fn renew_kept(
    screen: &mut dyn SliceScreen,
    slices: &mut Slices,
    in_view: &InView,
    next: &[(WinID, EdgeSlice)],
) {
    let Slices {
        showing,
        kept,
        renewing,
        generation,
    } = slices;
    for window in in_view {
        let id = window.id();
        if showing.iter().any(|(showing, _, _)| *showing == id) {
            continue;
        }
        // A window resized in view, as by maximizing, would otherwise keep
        // a photo whose size no later slice of it matches.
        let slice = match kept.get(&id) {
            Some((kept, _)) => EdgeSlice {
                size: window.shown_frame().size(),
                ..*kept
            },
            None => match next.iter().find(|(next, _)| *next == id) {
                Some((_, slice)) => *slice,
                None => continue,
            },
        };
        *generation = generation.wrapping_add(1);
        let ticket = Ticket::default();
        screen.photograph(id, &slice, *generation, ticket.clone(), Lookup::Reuse);
        debug!("edge slice kept photo renewing: window {id} generation {generation}");
        renewing.insert(
            id,
            Renewal {
                generation: *generation,
                slice,
                _wanted: Wanted(ticket),
            },
        );
    }
}

/// Finds the windows in view that a swipe would carry past a shared edge
/// first.
#[derive(SystemParam)]
struct NextToCross<'w, 's> {
    strips: Query<'w, 's, (&'static LayoutStrip, &'static ChildOf), With<SelectedVirtualMarker>>,
    displays: Query<'w, 's, (Entity, &'static Display)>,
    window_manager: Res<'w, WindowManager>,
}

impl NextToCross<'_, '_> {
    /// On the strip each display shows, the windows of the last column in
    /// view when the display shares its east edge with another display, and
    /// of the first one when it shares its west edge. Each comes with the
    /// slice a first photo of it is for. Until the window crosses, only the
    /// slice's size and display matter: a photo shows the whole window.
    fn find(&self, in_view: &InView) -> Vec<(WinID, EdgeSlice)> {
        let mut found = Vec::new();
        for (strip, child_of) in &self.strips {
            let Ok((display, own)) = self.displays.get(child_of.parent()) else {
                continue;
            };
            if !self
                .window_manager
                .active_display_space(own.id())
                .is_ok_and(|space| space == strip.id())
            {
                continue;
            }
            let bounds = own.bounds();
            let shares = |touches: fn(IRect, IRect) -> bool| {
                self.displays.iter().any(|(other, other_display)| {
                    let other_bounds = other_display.bounds();
                    other != display
                        && touches(bounds, other_bounds)
                        && other_bounds.min.y < bounds.max.y
                        && other_bounds.max.y > bounds.min.y
                })
            };
            let east = shares(|own, other| other.min.x == own.max.x);
            let west = shares(|own, other| other.max.x == own.min.x);
            if !east && !west {
                continue;
            }
            let columns = strip
                .columns()
                .filter_map(|column| {
                    let windows = column
                        .window_iter()
                        .map(|entity| in_view.get(entity).ok())
                        .collect::<Option<Vec<_>>>()?;
                    windows
                        .iter()
                        .any(|window| !window.shown_frame().intersect(bounds).is_empty())
                        .then_some(windows)
                })
                .collect::<Vec<_>>();
            let ends = [
                west.then(|| columns.first()).flatten(),
                east.then(|| columns.last()).flatten(),
            ];
            for window in ends.into_iter().flatten().flatten() {
                if found.iter().any(|(id, _)| *id == window.id()) {
                    continue;
                }
                if let Some(slice) = EdgeSlice::new(window.shown_frame(), bounds, display) {
                    found.push((window.id(), slice));
                }
            }
        }
        found
    }
}

/// Renews the kept photos of the windows in view as a swipe or scroll
/// starts, before it carries any of them past the edge.
fn renew_kept_on_swipe(
    scrolls: Query<&Scrolling>,
    mut was_swiping: Local<bool>,
    mut slices: ResMut<Slices>,
    in_view: InView,
    next: NextToCross,
    access: Res<SliceAccess>,
    screen: Option<NonSendMut<SliceScreenHandle>>,
) {
    let swiping = scrolls.iter().any(|scrolling| scrolling.is_user_swiping);
    if swiping
        && !*was_swiping
        && *access == SliceAccess::On
        && let Some(mut screen) = screen
    {
        let next = next.find(&in_view);
        renew_kept(screen.0.as_mut(), &mut slices, &in_view, &next);
    }
    *was_swiping = swiping;
}

/// Takes the answer to a renewal of `window`'s kept photo, and returns
/// whether it was one. A new photo replaces the kept one, and repaints the
/// window's slice if the slice shows the kept photo, as when the window
/// crossed the edge before the photo arrived. A blank or failed one
/// leaves the kept photo as it is.
fn receive_renewal(
    window: WinID,
    generation: u32,
    result: &Result<Photo, PhotoError>,
    slices: &mut Slices,
    screen: &mut dyn SliceScreen,
) -> bool {
    if slices
        .renewing
        .get(&window)
        .is_none_or(|renewal| renewal.generation != generation)
    {
        return false;
    }
    let Some(renewal) = slices.renewing.remove(&window) else {
        return true;
    };
    let Ok(taken) = result else {
        debug!("edge slice kept photo not renewed: window {window}");
        return true;
    };
    let old = slices
        .kept
        .insert(window, (renewal.slice, taken.clone()))
        .map(|(_, photo)| photo.digest);
    debug!("edge slice kept photo renewed: window {window}");
    // A slice still showing the old photo, or a swiped one still waiting for
    // its first, or closed because that photo came back blank, as a tucked
    // window's photo does. A window leaving the screen waits for its own
    // newer photo instead.
    let replaces = |photo: &SlicePhoto| match *photo {
        SlicePhoto::Shown { digest } => Some(digest) == old && digest != taken.digest,
        SlicePhoto::Asked {
            last: Some(last), ..
        } => Some(last) == old && last != taken.digest,
        SlicePhoto::Asked {
            last: None,
            kept_wait,
            ..
        } => kept_wait.is_none(),
        SlicePhoto::Ended => true,
    };
    let Some((_, _, photo)) = slices.showing.iter_mut().find(|(id, slice, photo)| {
        *id == window && slice.size == renewal.slice.size && replaces(photo)
    }) else {
        return true;
    };
    let reopened = matches!(photo, SlicePhoto::Ended);
    match photo {
        SlicePhoto::Asked { last, .. } => *last = Some(taken.digest),
        _ => {
            *photo = SlicePhoto::Shown {
                digest: taken.digest,
            };
        }
    }
    if reopened {
        screen.place(&slices.placed(), &slices.remembered());
    }
    screen.paint(window, taken);
    debug!("edge slice repainted with the renewed photo: window {window}");
    true
}

/// Paints the kept photo of each window leaving the screen whose new photo
/// is later than [`KEPT_WAIT`], so it can tuck, and keeps the new photo on
/// its way to replace it. The wait counts from the frame after the ask, as
/// [`PhotoWait`] explains.
fn show_kept_when_late(
    mut slices: ResMut<Slices>,
    time: Res<Time>,
    screen: Option<NonSendMut<SliceScreenHandle>>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let Slices { showing, kept, .. } = &mut *slices;
    for (id, slice, photo) in showing.iter_mut() {
        let SlicePhoto::Asked {
            last: last @ None,
            kept_wait: kept_wait @ Some(_),
            ..
        } = photo
        else {
            continue;
        };
        let now = time.elapsed();
        match *kept_wait {
            Some(PhotoWait::Until(until)) if now >= until => *kept_wait = None,
            Some(PhotoWait::Until(_)) => continue,
            _ => {
                *kept_wait = Some(PhotoWait::Until(now + KEPT_WAIT));
                continue;
            }
        }
        if let Some(photo) = kept_photo(kept, *id, slice.size) {
            screen.0.paint(*id, photo);
            *last = Some(photo.digest);
            debug!("edge slice photo late: window {id} shows its kept photo");
        }
    }
}

/// The windows whose photo slides: where each one's photo is, its `Position`,
/// where layout puts it, and whether it still has a slice.
type Sliding<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Window,
        &'static mut Slide,
        &'static mut Position,
        &'static PresentedOrigin,
        Has<EdgeSlice>,
    ),
>;

/// Moves each sliding photo one step of the row's ease toward where layout
/// puts its window. Once the photo arrives, the window moves there under it,
/// and the photo closes when the window server shows the window there, or
/// after [`LANDING_TIMEOUT`] if it does not. Until then the window holds
/// under the photo, and then it moves on to where layout puts it. A window
/// whose photo does not show moves at once.
#[allow(clippy::too_many_arguments)]
fn slide_photos(
    mut sliding: Sliding,
    displays: Query<&Display>,
    slices: Res<Slices>,
    holds: Res<SlideHolds>,
    time: Res<Time>,
    step: Res<AnimationStep>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let t = ease_out_factor(config.animation_speed(), step.secs());
    for (entity, window, mut slide, mut position, presented, tucked) in &mut sliding {
        let id = window.id();
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        match *slide {
            Slide::Moving { at, from } => {
                let shows = slices.has_photo(id, from.slice.size);
                let (next, arrived) = ease_step(at, presented.0, t);
                let next = holds.limit(entity, at, next);
                let arrived = arrived && next == presented.0;
                let moved = displays
                    .get(from.slice.display)
                    .ok()
                    .and_then(|own| from.photo(next, own.bounds()))
                    .filter(|_| shows);
                let Some(moved) = moved else {
                    debug!("edge slice photo not shown: window {id} moves at once");
                    position.0 = presented.0;
                    entity_commands.try_remove::<(Slide, EdgeSlice, RepositionMarker)>();
                    continue;
                };
                entity_commands.try_insert(moved);
                if arrived {
                    position.0 = presented.0;
                    entity_commands.try_remove::<RepositionMarker>();
                    *slide = Slide::Landing {
                        until: time.elapsed() + LANDING_TIMEOUT,
                    };
                    debug!(
                        "edge slice photo arrived: window {id} at ({}, {})",
                        next.x, next.y
                    );
                } else {
                    *slide = Slide::Moving { at: next, from };
                    debug!(
                        "edge slice photo sliding: window {id} at ({}, {})",
                        next.x, next.y
                    );
                }
            }
            Slide::Landing { .. } if !tucked => {
                entity_commands.try_remove::<Slide>();
            }
            Slide::Landing { until } if holds.landed(entity) || time.elapsed() >= until => {
                entity_commands.try_remove::<EdgeSlice>();
                if position.0 != presented.0 {
                    entity_commands.try_insert(RepositionMarker(presented.0));
                }
                debug!("edge slice photo handed over: window {id}");
            }
            Slide::Landing { .. } | Slide::Leaving { .. } | Slide::Tucking { .. } => {}
        }
    }
}

/// Moves each window heading for a tuck with the row while most of it stays
/// on its display, until its photo shows in its place. Then the window tucks
/// under the photo, and once macOS has taken that, the photo slides on to
/// the slice. A window whose photo does not show within [`PHOTO_WAIT`] of
/// the frame after it was asked for tucks at once. With the switch off, the
/// window tucks once its next step would put most of it on another display.
/// A photo never slides further past the edge than its place in the row
/// beside a nearer window that waits, so no wallpaper shows between them.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn slide_photos_out(
    mut sliding: Sliding,
    displays: Query<&Display>,
    slices: Res<Slices>,
    holds: Res<SlideHolds>,
    access: Res<SliceAccess>,
    time: Res<Time>,
    step: Res<AnimationStep>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let t = ease_out_factor(config.animation_speed(), step.secs());
    let mut moved_out = Vec::<Out>::new();
    for entity in Out::nearest_first(&sliding) {
        let Ok((entity, window, mut slide, mut position, presented, _)) = sliding.get_mut(entity)
        else {
            continue;
        };
        let id = window.id();
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        match *slide {
            Slide::Leaving {
                at,
                to,
                slice,
                wait,
            } => {
                let now = time.elapsed();
                let photo = slices.photo(id);
                if photo.is_some_and(|photo| photo.shown().is_some()) {
                    position.0 = presented.0;
                    *slide = Slide::Tucking {
                        at,
                        to,
                        hold: slice,
                        slice: slice.slice,
                        until: now + LANDING_TIMEOUT,
                    };
                    debug!("edge slice photo covers: window {id} tucks under it");
                    moved_out.extend(Out::of(&slide));
                    continue;
                }
                // After its first step, a slice with no photo on its way does
                // not show, so it never gets one.
                let on_its_way = photo.is_some_and(|photo| !matches!(photo, SlicePhoto::Ended));
                let no_photos = !config.shared_edge_slices();
                let waits = no_photos
                    || *access == SliceAccess::On
                        && match wait {
                            PhotoWait::Unasked => true,
                            PhotoWait::Asked => on_its_way,
                            PhotoWait::Until(until) => now < until && on_its_way,
                        };
                let own = displays.get(slice.slice.display).map(Display::bounds);
                let (true, Ok(own)) = (waits, own) else {
                    let waited = match wait {
                        PhotoWait::Until(until) => Some((now + PHOTO_WAIT).saturating_sub(until)),
                        PhotoWait::Unasked | PhotoWait::Asked => None,
                    };
                    debug!(
                        "edge slice photo not shown: window {id} tucks at once after {waited:?}, photo {photo:?}, access {:?}",
                        *access
                    );
                    position.0 = presented.0;
                    entity_commands
                        .try_insert(slice.slice)
                        .try_remove::<Slide>();
                    continue;
                };
                let all = displays.iter().map(Display::bounds).collect::<Vec<_>>();
                let (at, moved, stopped) = step_in_view(&slice, at, to, t, own, &all, |next| {
                    holds.limit(entity, at, next)
                });
                if no_photos && stopped {
                    debug!("edge slice: window {id} reached the edge with no photo and tucks");
                    position.0 = presented.0;
                    entity_commands
                        .try_insert(slice.slice)
                        .try_remove::<Slide>();
                    continue;
                }
                if position.0 != at {
                    position.0 = at;
                }
                entity_commands.try_insert(moved);
                *slide = Slide::Leaving {
                    at,
                    to,
                    slice,
                    wait: match wait {
                        PhotoWait::Unasked => PhotoWait::Asked,
                        PhotoWait::Asked => PhotoWait::Until(now + PHOTO_WAIT),
                        until @ PhotoWait::Until(_) => until,
                    },
                };
                debug!(
                    "edge slice photo awaited: window {id} shows at ({}, {})",
                    at.x, at.y
                );
            }
            Slide::Tucking { until, .. } if window.write_pending() && time.elapsed() < until => {}
            Slide::Tucking {
                at,
                to,
                hold,
                slice,
                until,
            } => {
                if position.0 != presented.0 {
                    position.0 = presented.0;
                }
                let (next, arrived) = ease_step(at, to, t);
                let next = Out::of(&slide).map_or(next, |out| out.beside(&moved_out, next));
                let next = holds.limit(entity, at, next);
                let arrived = arrived && next == to;
                let moved = displays
                    .get(slice.display)
                    .ok()
                    .and_then(|own| hold.photo(next, own.bounds()))
                    .filter(|moved| !arrived && *moved != slice);
                if let Some(moved) = moved {
                    entity_commands.try_insert(moved);
                    *slide = Slide::Tucking {
                        at: next,
                        to,
                        hold,
                        slice,
                        until,
                    };
                    debug!(
                        "edge slice photo sliding out: window {id} at ({}, {})",
                        next.x, next.y
                    );
                } else {
                    entity_commands.try_insert(slice).try_remove::<Slide>();
                    debug!("edge slice photo tucked: window {id}");
                }
            }
            Slide::Moving { .. } | Slide::Landing { .. } => {}
        }
        moved_out.extend(Out::of(&slide));
    }
}

/// A window heading for a tuck on `side` of `display`: the origin `to` of
/// its place in the row, and the origin `at` it or its photo shows at.
#[derive(Clone, Copy, Debug)]
struct Out {
    display: Entity,
    side: Side,
    to: IVec2,
    at: IVec2,
}

impl Out {
    fn of(state: &Slide) -> Option<Self> {
        let (slice, to, at) = match *state {
            Slide::Leaving { at, to, slice, .. } => (slice.slice, to, at),
            Slide::Tucking { at, to, slice, .. } => (slice, to, at),
            Slide::Moving { .. } | Slide::Landing { .. } => return None,
        };
        Some(Self {
            display: slice.display,
            side: slice.side,
            to,
            at,
        })
    }

    /// The windows of `sliding` heading for a tuck, each display's nearest
    /// its edge first, so each one steps after the windows nearer the edge.
    fn nearest_first(sliding: &Sliding) -> Vec<Entity> {
        let mut order = sliding
            .iter()
            .filter_map(|(entity, _, slide, ..)| Some((Self::of(slide)?.outwards(), entity)))
            .collect::<Vec<_>>();
        order.sort_unstable();
        order.into_iter().map(|(_, entity)| entity).collect()
    }

    /// How far past its edge the row places it, so that windows nearer the
    /// edge sort first.
    fn outwards(&self) -> (Entity, i32) {
        let x = match self.side {
            Side::West => -self.to.x,
            Side::East => self.to.x,
        };
        (self.display, x)
    }

    /// `next`, held where it would go further past the edge than its place
    /// beside any of `nearer` that the row puts between it and the display.
    /// Never moves it back toward the display from where it shows now.
    fn beside(&self, nearer: &[Out], next: IVec2) -> IVec2 {
        let places = nearer
            .iter()
            .filter(|other| other.display == self.display && other.side == self.side)
            .filter(|other| other.outwards().1 < self.outwards().1)
            .map(|other| other.at.x + self.to.x - other.to.x);
        let x = match self.side {
            Side::West => places
                .max()
                .map_or(next.x, |held| next.x.max(held.min(self.at.x))),
            Side::East => places
                .min()
                .map_or(next.x, |held| next.x.min(held.max(self.at.x))),
        };
        next.with_x(x)
    }
}

/// One step of the row's ease for a window at `at` heading for `to`, where
/// it shows `slice`, on the display with bounds `own`, among the displays
/// with bounds `all`. Stays at `at` when the step would put most of the
/// window on another display. Returns where the window is, with its slice
/// there, and whether the step stopped because of that.
fn step_in_view(
    slice: &Hold,
    at: IVec2,
    to: IVec2,
    t: f32,
    own: IRect,
    all: &[IRect],
    held: impl Fn(IVec2) -> IVec2,
) -> (IVec2, EdgeSlice, bool) {
    let others = all
        .iter()
        .copied()
        .filter(|other| *other != own)
        .collect::<Vec<_>>();
    let shows_at = |origin: IVec2| {
        slice
            .photo(origin, own)
            .filter(|moved| !lies_mostly_elsewhere(moved.shown(), own, &others))
    };
    let (next, _) = ease_step(at, to, t);
    let next = held(next);
    match shows_at(next) {
        Some(moved) => (next, moved, false),
        None => (at, shows_at(at).unwrap_or(slice.slice), true),
    }
}
