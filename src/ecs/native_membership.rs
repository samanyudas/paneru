//! macOS reports space membership late, in batches, and sometimes stale. This
//! module replays those reports and owns every strip change inferred from
//! them.
//!
//! A report that a window is in space S moves the window only when the window
//! is not on a display other than the one that shows S. One rule decides
//! which display a window is on:
//!
//! 1. A window at an origin the engine wrote is on the display of the strip
//!    that holds it, wherever that frame is and whatever size the app kept.
//!    That is the origin the engine placed it at, or, while an animation
//!    moves the window there, the last origin the animation wrote. Any other
//!    origin, even while the engine is moving the window, is a native move
//!    and falls to rule 3. The engine
//!    parks off-screen columns as slivers that hang past the display's edge,
//!    onto a neighboring display or onto no display at all, and an animation
//!    passes through such frames. A window whose write the app has yet to
//!    apply counts as at the frame the engine wrote.
//! 2. A window of a parked virtual workspace is the exception. The engine
//!    tucks the whole strip into a corner, so its frame is no evidence.
//! 3. Any other frame is on the display that holds its center. A center on
//!    no display is no evidence.
//!
//! A report with no evidence against it moves the window. A stale report for
//! another display, such as after a refused drop returned the window,
//! therefore changes nothing, however long macOS keeps it, while macOS's
//! own moves change the frame and pass.
//!
//! Two trades follow from the rule:
//!
//! - A failed AX frame read is no evidence, so a stale report wins for a
//!   window whose app stops responding.
//! - If macOS moves a window into another display's space without changing
//!   its frame, the report is refused. A Mission Control drag of a parked
//!   sliver into the neighboring display's space is such a move.
//!
//! The engine's own placements win while they are in progress: a held drag
//! group, and an explicit placement until the engine has finished moving its
//! windows. A space whose report names such a window waits and is read again
//! once the hold ends.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bevy::ecs::entity::{Entity, EntityHashSet};
use bevy::prelude::*;
use objc2_core_foundation::CFRetained;
use tracing::{debug, warn};

use super::display::migrate_windows_to_space;
use super::drag::DragState;
use super::layout::{Before, LayoutStrip};
use super::workspace::PreviousStripPosition;
use super::{
    ActiveWorkspaceMarker, AwaitingHandle, Bounds, Initializing, NativeFullscreenMarker, Position,
    PresentedOrigin, RepositionMarker, ResizeMarker, SelectedVirtualMarker, SpawnWindowTrigger,
    Unmanaged, VerifyWindowPosition, VerifyWindowSize,
};
use crate::config::Config;
use crate::manager::{Application, Display, Window, WindowManager};
use crate::platform::{WinID, WorkspaceId};
use crate::util::AXUIWrapper;

const QUERY_ATTEMPTS: u8 = 4;
/// Rounding allowed between a window's AX frame and the frame the engine
/// placed it at.
const FRAME_TOLERANCE: i32 = 2;
/// How long a window ID stays ignored after it closed or could not be
/// resolved. After that, a native list that still reports it is checked
/// again.
const IGNORE_FOR: Duration = Duration::from_secs(30);

pub(crate) enum NativeScope {
    ActivatedSpace(WorkspaceId),
    DisplayTopology,
}

#[derive(Default)]
struct DirtySpace {
    /// The read follows a display topology change, so it moves windows
    /// grouped by their source strip, as display reconciliation always has.
    topology: bool,
    retry: QueryRetry,
}

#[derive(Default)]
enum QueryRetry {
    #[default]
    Ready,
    Backoff {
        failures: u8,
        at: Duration,
    },
    /// The report names a window the engine is placing. The space is read
    /// again when that placement ends.
    Held,
    /// The read kept failing. The space is read again on a fresh signal.
    Suspended,
}

impl QueryRetry {
    fn due(&self, now: Duration) -> bool {
        match self {
            Self::Ready => true,
            Self::Backoff { at, .. } => now >= *at,
            Self::Held | Self::Suspended => false,
        }
    }

    fn failed(&mut self, now: Duration, space: WorkspaceId) {
        let failures = match self {
            Self::Backoff { failures, .. } => *failures + 1,
            _ => 1,
        };
        if failures >= QUERY_ATTEMPTS {
            warn!(
                space,
                "Native membership query suspended until a fresh signal"
            );
            *self = Self::Suspended;
        } else {
            *self = Self::Backoff {
                failures,
                at: now + Duration::from_millis(100 << (failures - 1)),
            };
        }
    }
}

struct HeldDragGroup {
    window: Entity,
    members: Vec<Entity>,
}

/// An explicit placement the engine is still moving into place.
struct Placing {
    members: Vec<Entity>,
    /// The layout frames seen on the last pass. A placement is done once its
    /// windows stop moving and these stay the same for a pass.
    targets: Vec<(Entity, IRect)>,
    maximized: Option<Before>,
}

/// A group a placement left in no strip, because the destination display
/// showed none. A later native report adds it as one item, maximized again
/// when it was.
struct Stripless {
    members: Vec<Entity>,
    maximized: Option<Before>,
}

struct IgnoredWindow {
    /// What the window had when it closed. `None` for a window no app could
    /// resolve, or a closed window without an element or an app.
    closed: Option<ClosedWindow>,
    until: Duration,
}

/// Native and AX lists can replay a closed window's element for a moment
/// after the close. Only its own app can list a different element for it,
/// which is a live window again.
struct ClosedWindow {
    app: Entity,
    element: CFRetained<AXUIWrapper>,
}

#[derive(Default, Resource)]
pub(crate) struct NativeMembership {
    dirty: HashMap<WorkspaceId, DirtySpace>,
    /// Every live space is to be read.
    all_spaces: bool,
    /// The next read of every space follows a display topology change.
    topology: bool,
    held: Option<HeldDragGroup>,
    placing: Vec<Placing>,
    stripless: Vec<Stripless>,
    ignored: HashMap<WinID, IgnoredWindow>,
}

impl NativeMembership {
    pub(crate) fn request(&mut self, scope: NativeScope) {
        match scope {
            NativeScope::ActivatedSpace(space) => {
                self.dirty.entry(space).or_default().retry = QueryRetry::Ready;
            }
            NativeScope::DisplayTopology => {
                self.all_spaces = true;
                self.topology = true;
            }
        }
    }

    /// Holds `members`, which the engine just placed explicitly, against
    /// native reports until the engine has moved them into place.
    /// `maximized` is what they return to when they are a maximized window
    /// that joined no strip yet.
    pub(crate) fn placed(&mut self, members: Vec<Entity>, maximized: Option<Before>) {
        debug!(?members, "Explicit placement holds native membership");
        self.placing.retain(|placing| {
            !placing
                .members
                .iter()
                .any(|member| members.contains(member))
        });
        self.placing.push(Placing {
            members,
            targets: Vec::new(),
            maximized,
        });
    }

    pub(crate) fn needs_frame(&self, now: Duration) -> bool {
        !self.placing.is_empty()
            || self.all_spaces
            || self.dirty.values().any(|dirty| dirty.retry.due(now))
    }

    fn owned(&self, member: Entity) -> bool {
        self.held
            .as_ref()
            .is_some_and(|held| held.members.contains(&member))
            || self
                .placing
                .iter()
                .any(|placing| placing.members.contains(&member))
    }

    /// Reads every live space again when some report waited for a placement
    /// to end. Reading only the spaces that waited would miss a window that
    /// macOS moved on to a third space during the hold. A hold that no report
    /// waited for reads nothing.
    fn release(&mut self) {
        if self
            .dirty
            .values()
            .any(|dirty| matches!(dirty.retry, QueryRetry::Held))
        {
            self.all_spaces = true;
        }
    }

    /// `member` and the windows that move with it: its native tabs, or the
    /// rest of its stripless group.
    fn group(&self, world: &mut World, member: Entity) -> Vec<Entity> {
        world
            .query::<&LayoutStrip>()
            .iter(world)
            .find_map(|strip| strip.tab_group(member))
            .or_else(|| {
                self.stripless
                    .iter()
                    .find(|group| group.members.contains(&member))
                    .map(|group| group.members.clone())
            })
            .unwrap_or_else(|| vec![member])
    }
}

pub(super) fn register(app: &mut App) {
    app.init_resource::<NativeMembership>()
        .add_observer(forget_removed_window)
        .add_observer(accept_created_window)
        .add_systems(
            Update,
            (capture_activations, reconcile.run_if(membership_work))
                .chain()
                .after(super::drag::drag_watchdog)
                .after(super::drag::drag_events)
                .in_set(super::UpdateSet::Focus)
                .run_if(not_initializing),
        )
        .add_systems(
            PostUpdate,
            observe_placements
                .after(super::systems::verify_window_position)
                .after(super::triggers::window_resize_verifier)
                .after(super::triggers::settle_width_requests)
                .in_set(super::PostUpdateSet::Animate)
                .run_if(not_initializing)
                .run_if(|native: Res<NativeMembership>| !native.placing.is_empty()),
        );
}

fn forget_removed_window(
    removed: On<Remove, Window>,
    windows: Query<(&Window, Option<&ChildOf>)>,
    time: Res<Time>,
    mut native: ResMut<NativeMembership>,
) {
    if let Ok((window, app)) = windows.get(removed.entity) {
        let closed = app
            .zip(window.element())
            .map(|(app, element)| ClosedWindow {
                app: app.parent(),
                element,
            });
        native.ignored.insert(
            window.id(),
            IgnoredWindow {
                closed,
                until: time.elapsed() + IGNORE_FOR,
            },
        );
    }
}

fn accept_created_window(
    created: On<Insert, Window>,
    windows: Query<&Window>,
    mut native: ResMut<NativeMembership>,
) {
    if let Ok(window) = windows.get(created.entity) {
        native.ignored.remove(&window.id());
    }
}

fn not_initializing(initializing: Option<Res<Initializing>>) -> bool {
    initializing.is_none()
}

fn held_window(drag: &DragState) -> Option<Entity> {
    drag.holds_window().then(|| drag.window()).flatten()
}

fn membership_work(native: Res<NativeMembership>, drag: Res<DragState>, time: Res<Time>) -> bool {
    native.all_spaces
        || native
            .dirty
            .values()
            .any(|dirty| dirty.retry.due(time.elapsed()))
        || native.held.as_ref().map(|held| held.window) != held_window(&drag)
}

fn capture_activations(
    activated: Populated<
        &LayoutStrip,
        (
            Added<ActiveWorkspaceMarker>,
            Without<NativeFullscreenMarker>,
        ),
    >,
    mut native: ResMut<NativeMembership>,
) {
    for strip in &activated {
        native.request(NativeScope::ActivatedSpace(strip.id()));
    }
}

/// The windows the engine is moving or waiting to move.
type Moving = Or<(
    With<RepositionMarker>,
    With<ResizeMarker>,
    With<VerifyWindowPosition>,
    With<VerifyWindowSize>,
    With<AwaitingHandle>,
)>;

fn live_spaces(world: &mut World) -> Vec<(WorkspaceId, Entity)> {
    let mut seen = HashSet::new();
    world
        .query::<(&LayoutStrip, &ChildOf, Has<NativeFullscreenMarker>)>()
        .iter(world)
        .filter_map(|(strip, child, fullscreen)| {
            (!fullscreen && seen.insert(strip.id())).then_some((strip.id(), child.parent()))
        })
        .collect()
}

fn reconcile(world: &mut World) {
    world.resource_scope(|world, mut native: Mut<NativeMembership>| {
        let held = held_window(world.resource::<DragState>());
        if native.held.as_ref().map(|held| held.window) != held {
            if native.held.is_some() {
                native.release();
            }
            native.held = held.map(|window| HeldDragGroup {
                window,
                members: native.group(world, window),
            });
        }
        for group in &mut native.stripless {
            group
                .members
                .retain(|member| world.get::<Window>(*member).is_some());
        }
        native.stripless.retain(|group| {
            !group.members.is_empty()
                && group.members.iter().all(|member| stripless(world, *member))
        });
        let spaces = live_spaces(world);
        let live = |space: &WorkspaceId| spaces.iter().any(|(live, _)| live == space);
        if native.dirty.keys().any(|space| !live(space)) {
            native.all_spaces = true;
            native.topology = true;
        }
        if std::mem::take(&mut native.all_spaces) {
            let topology = std::mem::take(&mut native.topology);
            for (space, _) in &spaces {
                let dirty = native.dirty.entry(*space).or_default();
                dirty.topology |= topology;
                dirty.retry = QueryRetry::Ready;
            }
        }
        native.dirty.retain(|space, _| live(space));
        if world
            .get_resource::<super::display::DisplayReconcileState>()
            .is_some_and(super::display::DisplayReconcileState::is_settling)
        {
            return;
        }
        let now = world.resource::<Time>().elapsed();
        native.ignored.retain(|_, ignored| ignored.until > now);
        for (space, display) in spaces {
            let Some(mut dirty) = native.dirty.remove(&space) else {
                continue;
            };
            if !dirty.retry.due(now) {
                native.dirty.insert(space, dirty);
                continue;
            }
            match world
                .resource::<WindowManager>()
                .windows_in_workspace(space)
            {
                Ok(ids) => {
                    let unresolved = discover_unresolved(world, &ids, &mut native.ignored, now);
                    let held =
                        reconcile_space(world, &native, space, display, &ids, dirty.topology);
                    if unresolved {
                        dirty.retry.failed(now, space);
                    } else if held {
                        dirty.retry = QueryRetry::Held;
                    } else {
                        continue;
                    }
                }
                Err(error) => {
                    debug!(space, %error, "Native membership query failed");
                    dirty.retry.failed(now, space);
                }
            }
            native.dirty.insert(space, dirty);
        }
    });
}

/// Asks the apps for the listed windows the engine does not know, and spawns
/// the ones they resolve. A closed window can come back only through its own
/// app, so when every unknown window closed, only their apps are asked.
/// Returns whether any spawned, so the space is read again once they exist.
fn discover_unresolved(
    world: &mut World,
    ids: &[WinID],
    ignored: &mut HashMap<WinID, IgnoredWindow>,
    now: Duration,
) -> bool {
    let known: HashSet<_> = world
        .query::<&Window>()
        .iter(world)
        .map(|window| window.id())
        .collect();
    let unresolved: HashSet<_> = ids
        .iter()
        .copied()
        .filter(|id| {
            !known.contains(id)
                && ignored
                    .get(id)
                    .is_none_or(|ignored| ignored.closed.is_some())
        })
        .collect();
    if unresolved.is_empty() {
        return false;
    }
    let closed_apps: Option<HashSet<Entity>> = unresolved
        .iter()
        .map(|id| ignored.get(id)?.closed.as_ref().map(|closed| closed.app))
        .collect();
    let config = world.resource::<Config>().clone();
    let listed: Vec<_> = world
        .query::<(Entity, &Application)>()
        .iter(world)
        .filter(|(app, _)| closed_apps.as_ref().is_none_or(|apps| apps.contains(app)))
        .flat_map(|(_, app)| app.window_list(&config))
        .filter(|window| unresolved.contains(&window.id()))
        .collect();
    for id in &unresolved {
        if !listed.iter().any(|window| window.id() == *id) {
            ignored.insert(
                *id,
                IgnoredWindow {
                    closed: None,
                    until: now + IGNORE_FOR,
                },
            );
        }
    }
    let fresh: Vec<_> = listed
        .into_iter()
        .filter(|window| {
            ignored
                .get(&window.id())
                .and_then(|ignored| ignored.closed.as_ref())
                .is_none_or(|closed| !window.is_current_element(&closed.element))
        })
        .collect();
    if fresh.is_empty() {
        return false;
    }
    for window in &fresh {
        ignored.remove(&window.id());
    }
    world.trigger(SpawnWindowTrigger(fresh));
    true
}

fn stripless(world: &mut World, member: Entity) -> bool {
    world.get::<Window>(member).is_some()
        && !world
            .query::<&LayoutStrip>()
            .iter(world)
            .any(|strip| strip.contains(member))
}

/// Whether a window of `members` is on a display other than `display`.
fn on_another_display(world: &mut World, members: &[Entity], display: Entity) -> bool {
    members
        .iter()
        .any(|member| window_display(world, *member).is_some_and(|shown| shown != display))
}

/// The display `member` is on, by the rule in the module docs. `None` is no
/// evidence either way.
fn window_display(world: &mut World, member: Entity) -> Option<Entity> {
    let frame = {
        let mut window = world.get_mut::<Window>(member)?;
        // The engine's own write is still on its way. The OS frame is the one
        // the window is leaving, and reading it would wait on the app, so the
        // frame the engine wrote is the evidence.
        if window.write_pending() {
            window.frame()
        } else {
            window.update_frame().ok()?
        }
    };
    let near = |a: IVec2, b: IVec2| (a - b).abs().max_element() <= FRAME_TOLERANCE;
    // `PresentedOrigin` is where the engine is taking the window. While a
    // `RepositionMarker` animates it, `Position` is the last frame the engine
    // wrote. Otherwise `Position` can hold a native move the engine adopted.
    // Only the origin counts: an app that refuses to make a window shorter
    // than its minimum allows keeps the origin the engine wrote.
    let animated = world.get::<RepositionMarker>(member).is_some();
    let written = [
        world.get::<PresentedOrigin>(member).map(|origin| origin.0),
        world
            .get::<Position>(member)
            .filter(|_| animated)
            .map(|position| position.0),
    ]
    .into_iter()
    .flatten()
    .any(|origin| near(frame.min, origin));
    let strip = world
        .query::<(&LayoutStrip, &ChildOf, Has<PreviousStripPosition>)>()
        .iter(world)
        .find(|(strip, _, _)| strip.contains(member))
        .map(|(_, child, parked)| (child.parent(), parked));
    if written && let Some((display, parked)) = strip {
        return (!parked).then_some(display);
    }
    world
        .query::<(Entity, &Display)>()
        .iter(world)
        .find(|(_, display)| display.bounds().contains(frame.center()))
        .map(|(shown, _)| shown)
}

/// Moves the managed windows `ids` lists for `space` into its strips. Returns
/// whether a window the engine is placing kept the space from being read in
/// full.
fn reconcile_space(
    world: &mut World,
    native: &NativeMembership,
    space: WorkspaceId,
    display: Entity,
    ids: &[WinID],
    topology: bool,
) -> bool {
    let managed: HashMap<_, _> = world
        .query::<(Entity, &Window, Has<Unmanaged>)>()
        .iter(world)
        .filter_map(|(entity, window, unmanaged)| (!unmanaged).then_some((window.id(), entity)))
        .collect();
    let mut held = false;
    let mut admitted = Vec::new();
    let mut seen = EntityHashSet::default();
    for candidate in ids.iter().filter_map(|id| managed.get(id).copied()) {
        if !seen.insert(candidate) {
            continue;
        }
        let members = native.group(world, candidate);
        seen.extend(members.iter().copied());
        let already_here = world
            .query::<(&LayoutStrip, Has<NativeFullscreenMarker>)>()
            .iter(world)
            .any(|(strip, fullscreen)| {
                (strip.id() == space || fullscreen) && strip.contains(candidate)
            });
        if already_here {
            continue;
        }
        if members.iter().any(|member| native.owned(*member)) {
            held = true;
            continue;
        }
        if on_another_display(world, &members, display) {
            debug!(
                ?members,
                space, "Ignoring a stale native report for another display"
            );
            continue;
        }
        admitted.push(members);
    }
    if !admitted.is_empty() {
        apply_native_move(world, space, display, &admitted, topology);
        for members in &admitted {
            let maximized = native
                .stripless
                .iter()
                .find(|group| group.members == *members)
                .and_then(|group| group.maximized.clone());
            if let Some(before) = maximized
                && let Some(mut strip) = world
                    .query::<&mut LayoutStrip>()
                    .iter_mut(world)
                    .find(|strip| strip.contains(members[0]))
            {
                strip.adopt_maximized(members[0], before);
            }
        }
    }
    held
}

fn apply_native_move(
    world: &mut World,
    space: WorkspaceId,
    display: Entity,
    groups: &[Vec<Entity>],
    topology: bool,
) {
    let admitted: EntityHashSet = groups.iter().flatten().copied().collect();
    let batch = topology
        || world
            .query::<(&LayoutStrip, Option<&ChildOf>, Has<NativeFullscreenMarker>)>()
            .iter(world)
            .any(|(strip, child, fullscreen)| {
                if fullscreen || strip.id() == space {
                    return false;
                }
                let matches: Vec<_> = strip
                    .all_windows()
                    .into_iter()
                    .filter(|entity| admitted.contains(entity))
                    .collect();
                let Some(first) = matches.first() else {
                    return false;
                };
                let first_group = strip.tab_group(*first).unwrap_or_else(|| vec![*first]);
                child.is_none() || matches.iter().any(|entity| !first_group.contains(entity))
            });
    if batch {
        migrate_windows_to_space(world, display, space, &admitted);
    }
    for members in groups {
        let placed = world
            .query::<&LayoutStrip>()
            .iter(world)
            .any(|strip| strip.contains(members[0]));
        if !batch || !placed {
            append_native_group(world, space, members);
        }
    }
}

fn append_native_group(world: &mut World, space: WorkspaceId, members: &[Entity]) {
    let target = world
        .query::<(
            Entity,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        )>()
        .iter(world)
        .filter(|(_, strip, _, _)| strip.id() == space)
        .min_by_key(|(_, strip, active, selected)| (!active, !selected, strip.virtual_index))
        .map(|(entity, _, _, _)| entity);
    let Some(target) = target else {
        return;
    };
    for (entity, mut strip) in world.query::<(Entity, &mut LayoutStrip)>().iter_mut(world) {
        if entity == target {
            strip.append_tab_group(members);
        } else {
            for member in members {
                strip.remove(*member);
            }
        }
    }
}

/// Ends each explicit placement once its windows are gone, or have stopped
/// moving with the same layout frames for a pass, and reads again the spaces
/// that waited for it.
fn observe_placements(world: &mut World) {
    world.resource_scope(|world, mut native: Mut<NativeMembership>| {
        let mut moving = world.query_filtered::<(), Moving>();
        let mut ended = false;
        for mut placing in std::mem::take(&mut native.placing) {
            placing.members.retain(|member| {
                world.get::<Window>(*member).is_some() && world.get::<Unmanaged>(*member).is_none()
            });
            let gone = placing.members.is_empty();
            let targets = layout_frames(world, &placing.members);
            let settled = targets == placing.targets
                && !placing
                    .members
                    .iter()
                    .any(|member| moving.get(world, *member).is_ok());
            if !gone && !settled {
                placing.targets = targets;
                native.placing.push(placing);
                continue;
            }
            debug!(members = ?placing.members, "Explicit placement released");
            ended = true;
            if !gone
                && placing
                    .members
                    .iter()
                    .all(|member| stripless(world, *member))
            {
                native.stripless.push(Stripless {
                    members: placing.members,
                    maximized: placing.maximized,
                });
            }
        }
        if ended {
            native.release();
        }
    });
}

fn layout_frames(world: &World, members: &[Entity]) -> Vec<(Entity, IRect)> {
    members
        .iter()
        .filter_map(|member| {
            let position = world.get::<Position>(*member)?;
            let bounds = world.get::<Bounds>(*member)?;
            Some((
                *member,
                IRect::from_corners(position.0, position.0 + bounds.0),
            ))
        })
        .collect()
}
