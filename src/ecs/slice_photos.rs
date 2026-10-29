//! Panels that show photos of the slices on shared display edges, and the
//! photos themselves: one panel per showing slice, a photo when it first
//! shows, and one every [`REFRESH`] while someone can see it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bevy::app::{App, PostUpdate};
use bevy::ecs::change_detection::{DetectChanges as _, DetectChangesMut as _};
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::Has;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::{any_with_component, resource_changed};
use bevy::ecs::schedule::{IntoScheduleConfigs as _, SystemCondition as _};
use bevy::ecs::system::{Commands, In, IntoSystem as _, NonSend, NonSendMut, Query, Res, ResMut};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use bevy::time::common_conditions::on_timer;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use tracing::{debug, warn};

use crate::config::Config;
use crate::ecs::layout::lies_mostly_elsewhere;
use crate::ecs::shared_edge::{EdgeSlice, Slide, showing_edge_slices, showing_may_change};
use crate::ecs::systems::{self, ease_out_factor, ease_step};
use crate::ecs::{Position, PostUpdateSet, PresentedOrigin, RepositionMarker};
use crate::events::Event;
use crate::manager::window_writer::LANDING_TIMEOUT;
use crate::manager::{Display, Window};
use crate::platform::WinID;

/// How often a shown slice is photographed again.
pub(crate) const REFRESH: Duration = Duration::from_secs(2);

/// How long a window heading for a tuck waits for its photo before it tucks
/// without one. Most photos arrive within 150 ms.
const PHOTO_WAIT: Duration = Duration::from_millis(250);

pub(crate) fn register(app: &mut App) {
    app.init_resource::<SliceAccess>()
        .init_resource::<Slices>()
        .add_systems(
            PostUpdate,
            (slide_photos, slide_photos_out)
                .in_set(PostUpdateSet::Animate)
                .after(systems::animate_entities)
                .before(systems::commit_window_frame)
                .run_if(any_with_component::<Slide>),
        )
        .add_systems(
            PostUpdate,
            (
                update_access.run_if(resource_changed::<Config>.or_eager(on_timer(REFRESH))),
                receive_photos,
                showing_edge_slices
                    .pipe(present_slices)
                    .run_if(showing_may_change.or_eager(resource_changed::<SliceAccess>)),
                refresh_slices.run_if(on_timer(REFRESH)),
            )
                .chain()
                .in_set(PostUpdateSet::Ui),
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
    /// slice's part of the window's photo, and closes every other panel. A
    /// panel stays out of sight until its first paint.
    fn place(&mut self, slices: &[(WinID, EdgeSlice)]);
    /// Starts a photo of the whole of `window`, which shows a frame of
    /// `slice.size` and gives the panel `slice`, unless `ticket` is called off
    /// before the capture. The result arrives as [`Event::SlicePhoto`]
    /// carrying `generation`. `place` forgets the lookups of windows it gets
    /// no slice for.
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
    /// A photo is on its way. `last` is the photo the panel shows now.
    Asked {
        generation: u32,
        last: Option<Digest>,
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

/// The showing slices, by window, each with the slice its photo is of.
#[derive(Resource, Default)]
struct Slices {
    showing: BTreeMap<WinID, (EdgeSlice, SlicePhoto)>,
    /// The generation of the latest photo asked for, so a photo of an
    /// earlier ask is told apart.
    generation: u32,
}

impl Slices {
    fn placed(&self) -> Vec<(WinID, EdgeSlice)> {
        self.showing
            .iter()
            .filter(|(_, (_, photo))| !matches!(photo, SlicePhoto::Ended))
            .map(|(id, (slice, _))| (*id, *slice))
            .collect()
    }
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
/// a slice that only moved keeps it. A slice whose painted panel no one can
/// see waits for the next refresh that sees it. A panel never painted is out
/// of sight, so its first photo never waits.
fn present_slices(
    In(showing): In<Vec<(WinID, EdgeSlice)>>,
    access: Res<SliceAccess>,
    mut slices: ResMut<Slices>,
    screen: Option<NonSendMut<SliceScreenHandle>>,
) {
    let Some(mut screen) = screen else {
        return;
    };
    let showing = if *access == SliceAccess::On {
        showing
    } else {
        Vec::new()
    };
    let Slices {
        showing: shown,
        generation,
    } = &mut *slices;
    let mut before = std::mem::take(shown);
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
                last => ask(
                    screen.0.as_mut(),
                    generation,
                    id,
                    &slice,
                    last,
                    Lookup::Fresh,
                ),
            },
        };
        shown.insert(id, (slice, photo));
    }
    screen.0.place(&slices.placed());
}

/// Photographs each shown slice again with its last lookup, unless no one
/// can see its panel.
fn refresh_slices(mut slices: ResMut<Slices>, screen: Option<NonSendMut<SliceScreenHandle>>) {
    let Some(mut screen) = screen else {
        return;
    };
    let Slices {
        showing,
        generation,
    } = &mut *slices;
    for (id, (slice, photo)) in showing.iter_mut() {
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

/// Paints each photo that differs from the one its panel shows. A failed
/// photo closes its panel, and a refused one closes them all.
fn receive_photos(
    mut messages: MessageReader<Event>,
    mut slices: ResMut<Slices>,
    mut access: ResMut<SliceAccess>,
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
        let asked = slices
            .showing
            .get_mut(window_id)
            .and_then(|(_, photo)| match *photo {
                SlicePhoto::Asked {
                    generation: asked,
                    last,
                    ..
                } if asked == *generation => Some((photo, last)),
                _ => None,
            });
        let Some((photo, last)) = asked else {
            debug!("edge slice photo dropped as stale: window {window_id} generation {generation}");
            continue;
        };
        *photo = match result {
            Ok(taken) if Some(taken.digest) == last => {
                debug!("edge slice unchanged: window {window_id}");
                SlicePhoto::Shown {
                    digest: taken.digest,
                }
            }
            Ok(taken) => {
                screen.0.paint(*window_id, taken);
                debug!("edge slice repainted: window {window_id}");
                SlicePhoto::Shown {
                    digest: taken.digest,
                }
            }
            Err(PhotoError::Refused) => {
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
            Err(error) => {
                debug!("edge slice photo failed: window {window_id}: {error:?}, closing its panel");
                ended = true;
                SlicePhoto::Ended
            }
        };
    }
    if ended {
        screen.0.place(&slices.placed());
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
/// and the photo closes when macOS has taken the move, or after
/// [`LANDING_TIMEOUT`] if the app does not answer. A window whose photo does
/// not show moves at once.
fn slide_photos(
    mut sliding: Sliding,
    displays: Query<&Display>,
    slices: Res<Slices>,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let t = ease_out_factor(config.animation_speed(), time.delta_secs_f64());
    for (entity, window, mut slide, mut position, presented, tucked) in &mut sliding {
        let id = window.id();
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        match *slide {
            Slide::Moving { at, from, slice } => {
                let shows = slices
                    .showing
                    .get(&id)
                    .is_some_and(|(_, photo)| photo.shown().is_some());
                let (next, arrived) = ease_step(at, presented.0, t);
                let moved = displays
                    .get(slice.display)
                    .ok()
                    .and_then(|own| slice.moved(next - from, own.bounds()))
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
                    *slide = Slide::Moving {
                        at: next,
                        from,
                        slice,
                    };
                    debug!(
                        "edge slice photo sliding: window {id} at ({}, {})",
                        next.x, next.y
                    );
                }
            }
            Slide::Landing { .. } if !tucked => {
                entity_commands.try_remove::<Slide>();
            }
            Slide::Landing { until } if !window.write_pending() || time.elapsed() >= until => {
                entity_commands.try_remove::<EdgeSlice>();
                debug!("edge slice photo handed over: window {id}");
            }
            Slide::Landing { .. } | Slide::Leaving { .. } | Slide::Tucking { .. } => {}
        }
    }
}

/// Moves each window heading for a tuck with the row while most of it stays
/// on its display, until its photo shows in its place. Then the window tucks
/// under the photo, and once macOS has taken that, the photo slides on to
/// the slice. A window whose photo does not show within [`PHOTO_WAIT`]
/// tucks at once.
fn slide_photos_out(
    mut sliding: Sliding,
    displays: Query<&Display>,
    slices: Res<Slices>,
    access: Res<SliceAccess>,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let t = ease_out_factor(config.animation_speed(), time.delta_secs_f64());
    for (entity, window, mut slide, mut position, presented, _) in &mut sliding {
        let id = window.id();
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        match *slide {
            Slide::Leaving {
                at,
                to,
                slice,
                until,
            } => {
                let now = time.elapsed();
                let photo = slices.showing.get(&id).map(|(_, photo)| photo);
                if photo.is_some_and(|photo| photo.shown().is_some()) {
                    position.0 = presented.0;
                    *slide = Slide::Tucking {
                        at,
                        to,
                        slice,
                        until: now + LANDING_TIMEOUT,
                    };
                    debug!("edge slice photo covers: window {id} tucks under it");
                    continue;
                }
                // After its first wait, a slice with no photo on its way does
                // not show, so it never gets one.
                let waits = *access == SliceAccess::On
                    && until.is_none_or(|until| {
                        now < until
                            && photo.is_some_and(|photo| !matches!(photo, SlicePhoto::Ended))
                    });
                let own = displays.get(slice.display).map(Display::bounds);
                let (true, Ok(own)) = (waits, own) else {
                    debug!("edge slice photo not shown: window {id} tucks at once");
                    position.0 = presented.0;
                    entity_commands.try_insert(slice).try_remove::<Slide>();
                    continue;
                };
                let all = displays.iter().map(Display::bounds).collect::<Vec<_>>();
                let (at, moved) = step_in_view(&slice, at, to, t, own, &all);
                if position.0 != at {
                    position.0 = at;
                }
                entity_commands.try_insert(moved);
                *slide = Slide::Leaving {
                    at,
                    to,
                    slice,
                    until: Some(until.unwrap_or(now + PHOTO_WAIT)),
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
                slice,
                until,
            } => {
                if position.0 != presented.0 {
                    position.0 = presented.0;
                }
                let (next, arrived) = ease_step(at, to, t);
                let moved = displays
                    .get(slice.display)
                    .ok()
                    .and_then(|own| slice.moved(next - to, own.bounds()))
                    .filter(|_| !arrived);
                if let Some(moved) = moved {
                    entity_commands.try_insert(moved);
                    *slide = Slide::Tucking {
                        at: next,
                        to,
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
    }
}

/// One step of the row's ease for a window at `at` heading for `to`, where
/// its frame shows `slice` on the display with bounds `own`, among the
/// displays with bounds `all`. Stays at `at` when the step would put most
/// of the window on another display. Returns where the window is, with its
/// slice there.
fn step_in_view(
    slice: &EdgeSlice,
    at: IVec2,
    to: IVec2,
    t: f32,
    own: IRect,
    all: &[IRect],
) -> (IVec2, EdgeSlice) {
    let others = all
        .iter()
        .copied()
        .filter(|other| *other != own)
        .collect::<Vec<_>>();
    let shows_at = |origin: IVec2| {
        slice
            .moved(origin - to, own)
            .filter(|moved| !lies_mostly_elsewhere(moved.shown(), own, &others))
    };
    let (next, _) = ease_step(at, to, t);
    match shows_at(next) {
        Some(moved) => (next, moved),
        None => (at, shows_at(at).unwrap_or(*slice)),
    }
}
