//! Panels that show photos of the slices on shared display edges, and the
//! photos themselves: one panel per showing slice, a photo when it first
//! shows, and one every [`REFRESH`] while someone can see it.

use std::collections::BTreeMap;
use std::time::Duration;

use bevy::app::{App, PostUpdate};
use bevy::ecs::change_detection::{DetectChanges as _, DetectChangesMut as _};
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::resource_changed;
use bevy::ecs::schedule::{IntoScheduleConfigs as _, SystemCondition as _};
use bevy::ecs::system::{In, IntoSystem as _, NonSend, NonSendMut, Res, ResMut};
use bevy::time::common_conditions::on_timer;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use tracing::{debug, warn};

use crate::config::Config;
use crate::ecs::PostUpdateSet;
use crate::ecs::shared_edge::{EdgeSlice, showing_edge_slices, showing_may_change};
use crate::events::Event;
use crate::platform::WinID;

/// How often a shown slice is photographed again.
pub(crate) const REFRESH: Duration = Duration::from_secs(2);

pub(crate) fn register(app: &mut App) {
    app.init_resource::<SliceAccess>()
        .init_resource::<Slices>()
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

/// Shows the slice panels and photographs the windows behind them.
/// [`SliceScreenOS`](crate::slice_screen::SliceScreenOS) does it on screen,
/// and tests record the calls.
pub(crate) trait SliceScreen {
    /// Whether macOS lets the engine photograph other apps' windows. Never
    /// asks the user.
    fn may_photograph(&self) -> bool;
    /// Gives each of `slices` a panel at its place on screen and closes every
    /// other panel. A panel stays out of sight until its first paint.
    fn place(&mut self, slices: &[(WinID, EdgeSlice)]);
    /// Starts a photo of the slice of `window`. The result arrives as
    /// [`Event::SlicePhoto`] carrying `generation`.
    fn photograph(&mut self, window: WinID, slice: &EdgeSlice, generation: u32);
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlicePhoto {
    /// A photo is on its way. `last` is the photo the panel shows now.
    Asked {
        generation: u32,
        last: Option<Digest>,
    },
    /// The panel shows the photo with this digest.
    Shown { digest: Digest },
    /// A photo failed. The slice keeps no panel and gets no photo until it
    /// stops showing.
    Ended,
}

impl SlicePhoto {
    fn shown(self) -> Option<Digest> {
        match self {
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
            .filter(|(_, (_, photo))| *photo != SlicePhoto::Ended)
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
) -> SlicePhoto {
    *generation = generation.wrapping_add(1);
    screen.photograph(window, slice, *generation);
    debug!("edge slice photo asked: window {window} generation {generation}");
    SlicePhoto::Asked {
        generation: *generation,
        last,
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
/// moved, and closes the panels of slices that stopped showing.
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
            Some((was, photo)) if was == slice || photo == SlicePhoto::Ended => photo,
            was => {
                let last = was.and_then(|(_, photo)| photo.shown());
                ask(screen.0.as_mut(), generation, id, &slice, last)
            }
        };
        shown.insert(id, (slice, photo));
    }
    screen.0.place(&slices.placed());
}

/// Photographs each shown slice again, unless no one can see its panel.
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
        *photo = ask(screen.0.as_mut(), generation, *id, slice, Some(digest));
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
