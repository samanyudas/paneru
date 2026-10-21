//! Native notifications invalidate membership observations. This module owns
//! their replay and every strip mutation inferred from those observations.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bevy::ecs::entity::{Entity, EntityHashMap, EntityHashSet};
use bevy::prelude::*;
use tracing::{debug, warn};

use super::display::compact_workspace_strips;
use super::drag::DragState;
use super::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use super::workspace::PreviousStripPosition;
use super::{
    ActiveWorkspaceMarker, AwaitingHandle, Bounds, FocusedMarker, Initializing,
    NativeFullscreenMarker, Position, PresentedOrigin, RepositionMarker, ResizeMarker,
    SelectedVirtualMarker, SpawnWindowTrigger, Unmanaged, VerifyWindowPosition, VerifyWindowSize,
};
use crate::config::Config;
use crate::manager::{Application, Display, Window, WindowManager};
use crate::platform::{WinID, WorkspaceId};

const QUERY_ATTEMPTS: u8 = 4;
const FRAME_TOLERANCE: i32 = 2;

pub(crate) enum NativeScope {
    ActivatedSpace(WorkspaceId),
    DisplayTopology,
    TargetedPress(Entity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementDestination {
    Strip(Entity),
    // Append is allowed before the destination display has any strip.
    EmptyDisplay(Entity),
}

pub(crate) struct AppliedPlacement {
    pub members: Vec<Entity>,
    pub destination: PlacementDestination,
    pub previous_space: WorkspaceId,
}

struct PendingPlacement {
    result: AppliedPlacement,
    issued_after: u64,
    interaction: u64,
    last_targets: Vec<(Entity, IRect)>,
    failures: u8,
}

struct PlacementReceipt {
    members: Vec<Entity>,
    destination: PlacementDestination,
    interaction: u64,
    frames: Vec<(Entity, IRect)>,
    native: NativeSnapshot,
    reflow: bool,
}

type NativeSnapshot = Vec<(WorkspaceId, Vec<Entity>)>;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ScanKind {
    #[default]
    Activation,
    Topology,
}

#[derive(Default)]
struct DirtySpace {
    kind: ScanKind,
    blocked: EntityHashSet,
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
    Suspended,
}

impl QueryRetry {
    fn due(&self, now: Duration) -> bool {
        match self {
            Self::Ready => true,
            Self::Backoff { at, .. } => now >= *at,
            Self::Suspended => false,
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

#[derive(Default, Resource)]
pub(crate) struct NativeMembership {
    dirty: HashMap<WorkspaceId, DirtySpace>,
    all_spaces: Option<ScanKind>,
    pending: Vec<PendingPlacement>,
    receipts: Vec<PlacementReceipt>,
    interactions: EntityHashMap<u64>,
    next_interaction: u64,
    committed_pass: u64,
    // This source snapshot survives group edits until the gesture releases it.
    held: Option<(Entity, Entity, Vec<Entity>)>,
    ignored_windows: HashSet<WinID>,
}

impl NativeMembership {
    pub(crate) fn request(&mut self, scope: NativeScope) {
        match scope {
            NativeScope::ActivatedSpace(space) => {
                let dirty = self.dirty.entry(space).or_default();
                dirty.retry = QueryRetry::Ready;
                dirty.blocked.clear();
            }
            NativeScope::DisplayTopology => {
                self.all_spaces = Some(ScanKind::Topology);
            }
            NativeScope::TargetedPress(entity) => {
                self.next_interaction += 1;
                self.interactions.insert(entity, self.next_interaction);
                self.receipts
                    .retain(|receipt| !receipt.members.contains(&entity));
                self.all_spaces.get_or_insert(ScanKind::Activation);
            }
        }
    }

    pub(crate) fn placed(&mut self, result: AppliedPlacement) {
        self.receipts
            .retain(|receipt| !overlaps(&receipt.members, &result.members));
        self.pending
            .retain(|pending| !overlaps(&pending.result.members, &result.members));
        self.request(NativeScope::ActivatedSpace(result.previous_space));
        let interaction = self.interaction(&result.members);
        debug!(members = ?result.members, destination = ?result.destination,
            pass = self.committed_pass, interaction, "Explicit placement awaiting native verification");
        self.pending.push(PendingPlacement {
            result,
            issued_after: self.committed_pass,
            interaction,
            last_targets: Vec::new(),
            failures: 0,
        });
        self.all_spaces.get_or_insert(ScanKind::Activation);
    }

    fn interaction(&self, members: &[Entity]) -> u64 {
        members
            .iter()
            .filter_map(|member| self.interactions.get(member))
            .copied()
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn needs_frame(&self, now: Duration) -> bool {
        !self.pending.is_empty()
            || self.all_spaces.is_some()
            || self
                .dirty
                .values()
                .any(|dirty| dirty.blocked.is_empty() && dirty.retry.due(now))
    }

    fn owned(&self, member: Entity) -> bool {
        self.held
            .as_ref()
            .is_some_and(|(_, _, members)| members.contains(&member))
            || self
                .pending
                .iter()
                .any(|pending| pending.result.members.contains(&member))
    }
}

fn overlaps(a: &[Entity], b: &[Entity]) -> bool {
    a.iter().any(|member| b.contains(member))
}

pub(super) fn register(app: &mut App) {
    app.init_resource::<NativeMembership>()
        .add_systems(
            Update,
            (
                capture_activations,
                capture_observed_frames,
                reconcile.run_if(membership_work),
            )
                .chain()
                .after(super::drag::drag_watchdog)
                .after(super::drag::drag_events)
                .in_set(super::UpdateSet::Focus)
                .run_if(not_initializing),
        )
        .add_systems(
            PostUpdate,
            capture_layout_changes
                .before(super::systems::animate_entities)
                .in_set(super::PostUpdateSet::Animate),
        )
        .add_systems(
            PostUpdate,
            observe_completion
                .after(super::systems::verify_window_position)
                .after(super::triggers::window_resize_verifier)
                .after(super::triggers::settle_width_requests)
                .in_set(super::PostUpdateSet::Animate)
                .run_if(not_initializing)
                .run_if(|native: Res<NativeMembership>| {
                    !native.pending.is_empty() || !native.receipts.is_empty()
                }),
        );
}

fn membership_work(native: Res<NativeMembership>, drag: Res<DragState>, time: Res<Time>) -> bool {
    native.all_spaces.is_some()
        || !native.pending.is_empty()
        || !native.dirty.is_empty()
            && native
                .dirty
                .values()
                .any(|dirty| dirty.retry.due(time.elapsed()))
        || native.held.as_ref().map(|(member, _, _)| *member)
            != drag.holds_window().then(|| drag.window()).flatten()
}

fn capture_layout_changes(
    geometry: Query<(&PresentedOrigin, Option<&ResizeMarker>)>,
    mut native: ResMut<NativeMembership>,
) {
    for receipt in &mut native.receipts {
        if matches!(receipt.destination, PlacementDestination::EmptyDisplay(_)) {
            continue;
        }
        if receipt.frames.iter().any(|(member, frame)| {
            geometry.get(*member).is_ok_and(|(origin, resize)| {
                origin.0 != frame.min || resize.is_some_and(|resize| resize.0 != frame.size())
            })
        }) {
            receipt.reflow = true;
        }
    }
}

fn movement_pending(world: &World, member: Entity) -> bool {
    world.get::<RepositionMarker>(member).is_some()
        || world.get::<ResizeMarker>(member).is_some()
        || world.get::<VerifyWindowPosition>(member).is_some()
        || world.get::<VerifyWindowSize>(member).is_some()
        || world.get::<AwaitingHandle>(member).is_some()
}

fn not_initializing(initializing: Option<Res<Initializing>>) -> bool {
    initializing.is_none()
}

type MovingWindows<'w, 's> = Query<
    'w,
    's,
    (),
    Or<(
        With<RepositionMarker>,
        With<ResizeMarker>,
        With<VerifyWindowPosition>,
        With<VerifyWindowSize>,
        With<AwaitingHandle>,
    )>,
>;

fn capture_observed_frames(
    mut events: MessageReader<crate::events::Event>,
    windows: Query<&Window>,
    moving: MovingWindows,
    mut native: ResMut<NativeMembership>,
) {
    let mut changed = false;
    for event in events.read() {
        let (crate::events::Event::WindowMoved { window_id }
        | crate::events::Event::WindowResized { window_id }) = event
        else {
            continue;
        };
        native.receipts.retain(|receipt| {
            let unexplained = !receipt.reflow
                && receipt.frames.iter().any(|(member, expected)| {
                    moving.get(*member).is_err()
                        && windows.get(*member).is_ok_and(|window| {
                            window.id() == *window_id
                                && !frames_match(
                                    &[(*member, window.frame())],
                                    &[(*member, *expected)],
                                )
                        })
                });
            changed |= unexplained;
            !unexplained
        });
    }
    if changed {
        native.all_spaces.get_or_insert(ScanKind::Activation);
    }
}

fn capture_activations(
    activated: Query<
        &LayoutStrip,
        (
            Added<ActiveWorkspaceMarker>,
            Without<NativeFullscreenMarker>,
        ),
    >,
    changed_groups: Query<(), Changed<LayoutStrip>>,
    mut recovered: RemovedComponents<AwaitingHandle>,
    mut removed: RemovedComponents<Window>,
    mut native: ResMut<NativeMembership>,
) {
    for strip in &activated {
        native.request(NativeScope::ActivatedSpace(strip.id()));
    }
    if !changed_groups.is_empty() && (!native.receipts.is_empty() || !native.pending.is_empty()) {
        native.all_spaces.get_or_insert(ScanKind::Activation);
    }
    if recovered.read().count() > 0 || removed.read().count() > 0 {
        native.all_spaces.get_or_insert(ScanKind::Activation);
    }
}

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

fn group(world: &mut World, member: Entity) -> Vec<Entity> {
    world
        .query::<&LayoutStrip>()
        .iter(world)
        .find_map(|strip| strip.tab_group(member))
        .unwrap_or_else(|| vec![member])
}

fn reconcile(world: &mut World) {
    world.resource_scope(|world, mut native: Mut<NativeMembership>| {
        let held = world.resource::<DragState>();
        let held = held.holds_window().then(|| held.window()).flatten();
        if native.held.as_ref().map(|(member, _, _)| *member) != held {
            native.all_spaces.get_or_insert(ScanKind::Activation);
            native.held = held.and_then(|member| {
                world
                    .query::<(Entity, &LayoutStrip)>()
                    .iter(world)
                    .find(|(_, strip)| strip.contains(member))
                    .map(|(strip_entity, strip)| {
                        (
                            member,
                            strip_entity,
                            strip.tab_group(member).unwrap_or_else(|| vec![member]),
                        )
                    })
            });
        }
        native
            .interactions
            .retain(|entity, _| world.get::<Window>(*entity).is_some());
        let spaces = live_spaces(world);
        if native
            .dirty
            .keys()
            .any(|space| !spaces.iter().any(|(live, _)| live == space))
        {
            native.all_spaces = Some(ScanKind::Topology);
        }
        if let Some(kind) = native.all_spaces.take() {
            for (space, _) in &spaces {
                let dirty = native.dirty.entry(*space).or_default();
                if kind == ScanKind::Topology {
                    dirty.kind = kind;
                }
                dirty.retry = QueryRetry::Ready;
                dirty.blocked.clear();
            }
        }
        native
            .dirty
            .retain(|space, _| spaces.iter().any(|(live, _)| live == space));
        if world
            .get_resource::<super::display::DisplayReconcileState>()
            .is_some_and(super::display::DisplayReconcileState::is_settling)
        {
            return;
        }
        let now = world.resource::<Time>().elapsed();
        for (space, display) in spaces {
            let Some(mut dirty) = native.dirty.remove(&space) else {
                continue;
            };
            dirty.blocked.retain(|member| {
                (native.owned(*member) || movement_pending(world, *member))
                    && world.get::<Window>(*member).is_some()
            });
            if !dirty.blocked.is_empty() || !dirty.retry.due(now) {
                native.dirty.insert(space, dirty);
                continue;
            }
            let result = world
                .resource::<WindowManager>()
                .windows_in_workspace(space);
            match result {
                Ok(ids) => {
                    let unresolved = discover_unresolved(world, &ids, &mut native.ignored_windows);
                    if let Ok(blocked) = reconcile_space(
                        world,
                        &mut native,
                        space,
                        display,
                        &ids,
                        dirty.kind == ScanKind::Topology,
                    ) {
                        if blocked.is_empty() && !unresolved {
                            continue;
                        }
                        dirty.blocked = blocked;
                        if unresolved {
                            dirty.retry.failed(now, space);
                        } else {
                            dirty.retry = QueryRetry::Ready;
                        }
                    } else {
                        dirty.retry.failed(now, space);
                    }
                    native.dirty.insert(space, dirty);
                }
                Err(error) => {
                    debug!(space, %error, "Native membership query failed");
                    dirty.retry.failed(now, space);
                    native.dirty.insert(space, dirty);
                }
            }
        }
    });
}

fn discover_unresolved(world: &mut World, ids: &[WinID], ignored: &mut HashSet<WinID>) -> bool {
    let known: HashSet<_> = world
        .query::<&Window>()
        .iter(world)
        .map(|window| window.id())
        .collect();
    let unresolved: HashSet<_> = ids
        .iter()
        .copied()
        .filter(|id| !known.contains(id) && !ignored.contains(id))
        .collect();
    if unresolved.is_empty() {
        return false;
    }
    let config = world.resource::<Config>().clone();
    let retry: Vec<_> = world
        .query::<&Application>()
        .iter(world)
        .flat_map(|app| app.window_list(&config))
        .filter(|window| unresolved.contains(&window.id()))
        .collect();
    if retry.is_empty() {
        ignored.extend(unresolved);
        false
    } else {
        world.trigger(SpawnWindowTrigger(retry));
        true
    }
}

fn native_snapshot(world: &mut World, members: &[Entity]) -> Result<NativeSnapshot, ()> {
    let ids: HashMap<_, _> = members
        .iter()
        .filter_map(|member| {
            world
                .get::<Window>(*member)
                .map(|window| (window.id(), *member))
        })
        .collect();
    let mut result = Vec::new();
    for (space, _) in live_spaces(world) {
        let windows = world
            .resource::<WindowManager>()
            .windows_in_workspace(space)
            .map_err(|_| ())?;
        let mut present: Vec<_> = windows
            .iter()
            .filter_map(|id| ids.get(id).copied())
            .collect();
        present.sort_unstable();
        if !present.is_empty() {
            result.push((space, present));
        }
    }
    result.sort_unstable_by_key(|(space, _)| *space);
    Ok(result)
}

fn read_frames(world: &mut World, members: &[Entity]) -> Result<Vec<(Entity, IRect)>, ()> {
    members
        .iter()
        .map(|member| {
            let frame = world
                .get_mut::<Window>(*member)
                .ok_or(())?
                .update_frame()
                .map_err(|_| ())?;
            Ok((*member, frame))
        })
        .collect()
}

fn frames_match(actual: &[(Entity, IRect)], expected: &[(Entity, IRect)]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|((a, frame), (b, target))| {
                a == b
                    && (frame.min - target.min).abs().max_element() <= FRAME_TOLERANCE
                    && (frame.max - target.max).abs().max_element() <= FRAME_TOLERANCE
            })
}

fn same_group(world: &mut World, members: &[Entity], strip: Entity) -> bool {
    let Some(member) = members.first() else {
        return false;
    };
    world.get::<LayoutStrip>(strip).is_some_and(|layout| {
        if !layout.contains(*member) {
            return false;
        }
        let current = layout.tab_group(*member).unwrap_or_else(|| vec![*member]);
        current.len() == members.len() && current.iter().all(|member| members.contains(member))
    })
}

fn same_placement(
    world: &mut World,
    members: &[Entity],
    destination: PlacementDestination,
) -> bool {
    match destination {
        PlacementDestination::Strip(strip) => same_group(world, members, strip),
        PlacementDestination::EmptyDisplay(display) => {
            world.get::<Display>(display).is_some()
                && !world
                    .query::<&LayoutStrip>()
                    .iter(world)
                    .any(|strip| members.iter().any(|member| strip.contains(*member)))
        }
    }
}

#[derive(PartialEq, Eq)]
enum ReceiptEvidence {
    Unchanged,
    Changed,
    Placing,
}

fn receipt_supersedes(
    world: &mut World,
    native: &mut NativeMembership,
    members: &[Entity],
) -> Result<ReceiptEvidence, ()> {
    let Some(index) = native
        .receipts
        .iter()
        .position(|receipt| overlaps(&receipt.members, members))
    else {
        return Ok(ReceiptEvidence::Changed);
    };
    let receipt = &native.receipts[index];
    if !same_placement(world, &receipt.members, receipt.destination)
        || native.interaction(&receipt.members) != receipt.interaction
    {
        native.receipts.remove(index);
        return Ok(ReceiptEvidence::Changed);
    }
    if receipt.reflow
        && receipt
            .members
            .iter()
            .any(|member| movement_pending(world, *member))
    {
        return Ok(ReceiptEvidence::Placing);
    }
    let current = native_snapshot(world, &receipt.members)?;
    let frames = read_frames(world, &receipt.members)?;
    if current != receipt.native {
        native.receipts.remove(index);
        return Ok(ReceiptEvidence::Changed);
    }
    if receipt.reflow {
        let targets: Vec<_> = receipt
            .members
            .iter()
            .filter_map(|member| {
                let origin = world.get::<PresentedOrigin>(*member)?;
                let bounds = world.get::<Bounds>(*member)?;
                Some((*member, IRect::from_corners(origin.0, origin.0 + bounds.0)))
            })
            .collect();
        if frames_match(&frames, &targets) {
            let receipt = &mut native.receipts[index];
            receipt.frames = frames;
            receipt.reflow = false;
            return Ok(ReceiptEvidence::Unchanged);
        }
    }
    if !frames_match(&frames, &receipt.frames) {
        native.receipts.remove(index);
        return Ok(ReceiptEvidence::Changed);
    }
    Ok(ReceiptEvidence::Unchanged)
}

fn reconcile_space(
    world: &mut World,
    native: &mut NativeMembership,
    space: WorkspaceId,
    display: Entity,
    ids: &[WinID],
    topology: bool,
) -> Result<EntityHashSet, ()> {
    let managed: HashMap<_, _> = world
        .query::<(Entity, &Window, Has<Unmanaged>)>()
        .iter(world)
        .filter_map(|(entity, window, unmanaged)| (!unmanaged).then_some((window.id(), entity)))
        .collect();
    let candidates: Vec<_> = ids
        .iter()
        .filter_map(|id| managed.get(id).copied())
        .collect();
    let mut query_failed = false;
    let mut admitted = Vec::new();
    let mut blocked = EntityHashSet::default();
    let mut seen = EntityHashSet::default();
    for candidate in candidates {
        if !seen.insert(candidate) {
            continue;
        }
        let members = if world
            .query::<&LayoutStrip>()
            .iter(world)
            .any(|strip| strip.contains(candidate))
        {
            group(world, candidate)
        } else {
            native
                .pending
                .iter()
                .find(|request| request.result.members.contains(&candidate))
                .map(|request| request.result.members.clone())
                .or_else(|| {
                    native
                        .receipts
                        .iter()
                        .find(|receipt| receipt.members.contains(&candidate))
                        .map(|receipt| receipt.members.clone())
                })
                .unwrap_or_else(|| vec![candidate])
        };
        seen.extend(members.iter().copied());
        let already_here = world
            .query::<(&LayoutStrip, Has<NativeFullscreenMarker>)>()
            .iter(world)
            .any(|(strip, fullscreen)| {
                (strip.id() == space || fullscreen) && strip.contains(candidate)
            });
        if already_here {
            native.receipts.retain(|receipt| {
                !overlaps(&receipt.members, &members)
                    || !receipt.members.iter().all(|member| {
                        world
                            .get::<Window>(*member)
                            .is_some_and(|window| ids.contains(&window.id()))
                    })
            });
            continue;
        }
        if members.iter().any(|member| native.owned(*member)) {
            blocked.extend(members);
            continue;
        }
        let Ok(evidence) = receipt_supersedes(world, native, &members) else {
            query_failed = true;
            continue;
        };
        match evidence {
            ReceiptEvidence::Unchanged => continue,
            ReceiptEvidence::Placing => {
                blocked.extend(members);
                continue;
            }
            ReceiptEvidence::Changed => {}
        }
        admitted.push(members);
    }
    native.receipts.retain(|receipt| {
        let expected = receipt
            .native
            .iter()
            .find(|(observed, _)| *observed == space)
            .map(|(_, members)| members.as_slice())
            .unwrap_or_default();
        let actual: Vec<_> = receipt
            .members
            .iter()
            .copied()
            .filter(|member| {
                world
                    .get::<Window>(*member)
                    .is_some_and(|window| ids.contains(&window.id()))
            })
            .collect();
        actual.len() == expected.len() && actual.iter().all(|member| expected.contains(member))
    });
    // Admission and extraction share this exclusive operation. Neither queued
    // commands nor native-tab expansion can add an owned member after admission.
    if !admitted.is_empty() {
        apply_native_move(world, space, display, &admitted, topology);
    }
    if query_failed { Err(()) } else { Ok(blocked) }
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
        apply_batch(world, space, display, &admitted);
        // An Append can reach an empty display before its strip exists.
        // Its remembered native group still needs admission as one item.
        for members in groups {
            if !world
                .query::<&LayoutStrip>()
                .iter(world)
                .any(|strip| strip.contains(members[0]))
            {
                append_native_group(world, space, members);
            }
        }
    } else {
        for members in groups {
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

fn apply_batch(world: &mut World, space: WorkspaceId, display: Entity, admitted: &EntityHashSet) {
    let Some(bounds) = world.get::<Display>(display).map(Display::bounds) else {
        return;
    };
    let mut sources: Vec<_> = world
        .query::<(Entity, &LayoutStrip, Has<NativeFullscreenMarker>)>()
        .iter(world)
        .filter_map(|(entity, strip, fullscreen)| {
            if fullscreen || strip.id() == space {
                return None;
            }
            let members: EntityHashSet = strip
                .all_windows()
                .into_iter()
                .filter(|member| admitted.contains(member))
                .collect();
            (!members.is_empty()).then_some((entity, strip.id(), strip.virtual_index, members))
        })
        .collect();
    sources.sort_by_key(|(_, id, index, _)| (*id, *index));
    if !sources.is_empty() {
        compact_workspace_strips(world, space);
    }
    let mut affected = HashSet::new();
    for (source, source_space, _, members) in sources {
        let mut extracted = LayoutStrip::new(space, 0);
        if let Some(mut strip) = world.get_mut::<LayoutStrip>(source) {
            strip.extract_windows_into(&members, &mut extracted);
        }
        if extracted.len() > 0 {
            place_extracted_strip(world, display, bounds, space, extracted);
        }
        affected.insert(source_space);
    }
    for source in affected {
        compact_workspace_strips(world, source);
    }
}

fn observe_completion(world: &mut World) {
    world.resource_scope(|world, mut native: Mut<NativeMembership>| {
        native.committed_pass += 1;
        let pending = std::mem::take(&mut native.pending);
        for mut request in pending {
            let members = &request.result.members;
            if members.iter().any(|member| world.get::<Window>(*member).is_none() || world.get::<Unmanaged>(*member).is_some()) {
                native.all_spaces.get_or_insert(ScanKind::Activation);
                continue;
            }
            let valid = same_placement(world, members, request.result.destination);
            if !valid {
                native.all_spaces.get_or_insert(ScanKind::Activation);
                continue;
            }
            let targets: Vec<_> = members.iter().filter_map(|member| {
                let position = world.get::<Position>(*member)?;
                let bounds = world.get::<Bounds>(*member)?;
                Some((*member, IRect::from_corners(position.0, position.0 + bounds.0)))
            }).collect();
            let busy = members.iter().any(|member| {
                world.get::<RepositionMarker>(*member).is_some() || world.get::<ResizeMarker>(*member).is_some()
                    || world.get::<VerifyWindowPosition>(*member).is_some() || world.get::<VerifyWindowSize>(*member).is_some()
                    || world.get::<AwaitingHandle>(*member).is_some()
            });
            if native.committed_pass <= request.issued_after || busy || targets != request.last_targets {
                request.last_targets = targets;
                native.pending.push(request);
                continue;
            }
            let verified = read_frames(world, members).and_then(|frames| {
                if !frames_match(&frames, &targets) { return Err(()); }
                native_snapshot(world, members).map(|snapshot| (frames, snapshot))
            });
            if let Ok((frames, snapshot)) = verified {
                debug!(?members, pass = native.committed_pass, "Explicit placement verified by native frame readback");
                if native.interaction(members) == request.interaction {
                    native.receipts.push(PlacementReceipt {
                        members: members.clone(), destination: request.result.destination,
                        interaction: request.interaction, frames, native: snapshot, reflow: false,
                    });
                }
                native.all_spaces.get_or_insert(ScanKind::Activation);
            } else {
                request.failures += 1;
                if request.failures >= QUERY_ATTEMPTS {
                    warn!(?members, "Explicit placement failed native verification; reconciling current membership");
                    native.all_spaces.get_or_insert(ScanKind::Activation);
                } else { native.pending.push(request); }
            }
        }
        native.receipts.retain(|receipt| {
            same_placement(world, &receipt.members, receipt.destination)
                && receipt.members.iter().all(|member| world.get::<Window>(*member).is_some() && world.get::<Unmanaged>(*member).is_none())
        });
    });
}
fn place_extracted_strip(
    world: &mut World,
    display_entity: Entity,
    display_bounds: IRect,
    target_space_id: WorkspaceId,
    mut extracted: LayoutStrip,
) {
    let retains_focus = extracted
        .all_windows()
        .into_iter()
        .any(|entity| world.get::<FocusedMarker>(entity).is_some());
    let empty_row_0 = world
        .query::<(Entity, &LayoutStrip, Has<NativeFullscreenMarker>)>()
        .iter(world)
        .find_map(|(entity, strip, fullscreen)| {
            (!fullscreen
                && strip.id() == target_space_id
                && strip.virtual_index == 0
                && strip.len() == 0)
                .then_some(entity)
        });

    if let Some(dst_entity) = empty_row_0 {
        if let Some(mut dst_strip) = world.get_mut::<LayoutStrip>(dst_entity) {
            dst_strip.append_strip(&mut extracted);
        }
        let any_selected = world
            .query::<(&LayoutStrip, Has<SelectedVirtualMarker>)>()
            .iter(world)
            .any(|(strip, selected)| strip.id() == target_space_id && selected);
        if !any_selected {
            world
                .entity_mut(dst_entity)
                .remove::<PreviousStripPosition>()
                .insert((Position(display_bounds.min), SelectedVirtualMarker));
        }
        if retains_focus {
            world.entity_mut(dst_entity).insert(ActiveWorkspaceMarker);
        }
        return;
    }

    let next_virtual_index = world
        .query::<(&LayoutStrip, Has<NativeFullscreenMarker>)>()
        .iter(world)
        .filter(|(strip, fullscreen)| !fullscreen && strip.id() == target_space_id)
        .map(|(strip, _)| strip.virtual_index)
        .max()
        .map_or(0, |max_idx| max_idx + 1);

    extracted.virtual_index = next_virtual_index;
    let focus = extracted.first().ok().and_then(|col| col.top());
    let strip_entity = if next_virtual_index == 0 {
        world
            .spawn((
                Position(display_bounds.min),
                extracted,
                ChildOf(display_entity),
                SelectedVirtualMarker,
            ))
            .id()
    } else {
        world
            .spawn((
                Position(display_bounds.max - PARKED_STRIP_SLIVER),
                PreviousStripPosition {
                    origin: display_bounds.min,
                    focus,
                },
                extracted,
                ChildOf(display_entity),
            ))
            .id()
    };
    if retains_focus {
        world.entity_mut(strip_entity).insert(ActiveWorkspaceMarker);
    }
}
