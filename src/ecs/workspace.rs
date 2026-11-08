use bevy::app::{App, Plugin, PostUpdate, PreUpdate, Update};
use bevy::ecs::change_detection::{DetectChanges as _, Ref};
use bevy::ecs::component::Component;
use bevy::ecs::entity::{Entity, EntityHashMap};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, ParamSet, Populated, Query, Res, ResMut, Single};
use bevy::ecs::world::World;
use bevy::time::Time;
use bevy::time::common_conditions::on_timer;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{Level, debug, error, instrument, warn};

use super::ActiveDisplayMarker;
use crate::commands::{Operation, filter_window_operations};
use crate::config::Config;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER, origin_exposing};
use crate::ecs::native_membership::{NativeMembership, NativeScope};
use crate::ecs::params::Windows;
use crate::ecs::{
    ActiveWorkspaceMarker, DockPosition, FocusedMarker, ManualStripOffset, NativeFullscreenMarker,
    PointerFollowsFocus, Position, RaiseWindow, RepositionMarker, Scrolling, SelectedVirtualMarker,
    SendMessageTrigger, SpawnCommandsExt, Timeout, Unmanaged,
};
use crate::events::{DestroySource, Event};
use crate::manager::{Display, Origin, Window, WindowManager};
use crate::platform::WorkspaceId;
use crate::util::round_px;

pub struct WorkspaceEventsPlugin;

/// Suppresses the popup for a strip activated by startup session restore.
/// Removed after that activation so later user switches still flash.
#[derive(Component)]
pub(crate) struct RestoredActivation;

/// The saved origin and any in-flight `RepositionMarker`, as
/// [`show_active_workspace`] needs them to restore a strip.
type RestorableStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Position,
        &'static LayoutStrip,
        &'static ChildOf,
        Option<&'static PreviousStripPosition>,
        Option<&'static RepositionMarker>,
    ),
    Without<Window>,
>;

/// `p0`: strips added this tick. `p1`: every strip, to merge those sharing a
/// native Space. Must be a `ParamSet` since `p1` aliases `p0` mutably.
type MergeStrips<'w, 's> = ParamSet<
    'w,
    's,
    (
        Query<'static, 'static, &'static LayoutStrip, Added<LayoutStrip>>,
        Query<
            'static,
            'static,
            (
                Entity,
                &'static mut LayoutStrip,
                Has<ActiveWorkspaceMarker>,
                Has<SelectedVirtualMarker>,
            ),
        >,
    ),
>;

/// Workspace identity, display ownership, and popup-specific activation state.
type PopupStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LayoutStrip,
        &'static ChildOf,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
        Has<RestoredActivation>,
    ),
>;

impl Plugin for WorkspaceEventsPlugin {
    fn build(&self, app: &mut App) {
        const DISPLAY_CHANGE_CHECK_FREQ: Duration = Duration::from_secs(1);

        let reap_workspaces = |config: Option<Res<Config>>| {
            config.is_some_and(|config| config.reap_empty_workspaces())
        };
        let workspace_or_display_activated =
            |workspace_activated: Query<(), Added<ActiveWorkspaceMarker>>,
             display_activated: Query<(), Added<ActiveDisplayMarker>>| {
                !workspace_activated.is_empty() || !display_activated.is_empty()
            };

        app.add_systems(
            PreUpdate,
            refuse_virtual_workspace_commands.in_set(super::PreUpdateSet::Dispatch),
        );
        app.add_systems(
            PreUpdate,
            find_orphaned_workspaces
                .in_set(super::PreUpdateSet::Dispatch)
                .after(crate::ecs::display::reconcile_displays)
                .run_if(on_timer(DISPLAY_CHANGE_CHECK_FREQ)),
        );
        app.add_systems(
            Update,
            (
                merge_strips_per_space,
                reap_empty_virtual_workspaces.run_if(reap_workspaces),
                workspace_change_handler,
                workspace_created_handler,
            )
                .in_set(super::UpdateSet::Events),
        );
        app.add_systems(
            Update,
            show_active_workspace.in_set(super::UpdateSet::Workspace),
        );
        app.add_systems(
            PostUpdate,
            (
                workspace_destroyed_handler.in_set(super::PostUpdateSet::Bookkeeping),
                flash_workspace_on_activation
                    .in_set(super::PostUpdateSet::Ui)
                    .run_if(workspace_or_display_activated)
                    .before(crate::ecs::systems::update_flash_messages),
            ),
        );
        app.add_observer(cleanup_active_workspace_marker)
            .add_observer(cleanup_selected_space_marker);
    }
}

#[derive(Component, Debug)]
pub(crate) struct PreviousStripPosition {
    pub origin: Origin,
    pub focus: Option<Entity>,
}

/// Guard spawned alongside the re-focus of a restored strip's remembered
/// window. The OS's focus acknowledgment arrives several ticks after the
/// strip was already placed at its saved origin, so without this guard
/// `autocenter_window_on_focus` and `window_focused_trigger`'s reshuffle would
/// slide the strip away again (a visible wiggle on every switch); both skip
/// the named entity while this marker exists. It is despawned once focus
/// moves elsewhere, or expires via `timeout_ticker`.
#[derive(Component, Debug)]
pub(crate) struct RestoreFocusMarker {
    pub entity: Entity,
}

/// Long enough for the OS focus acknowledgments to arrive, short enough that
/// a user re-clicking the restored window soon after the switch gets the
/// normal expose-reshuffle back.
const RESTORE_FOCUS_GUARD_TIMEOUT: Duration = Duration::from_secs(2);

fn spawn_restore_focus_guard(entity: Entity, commands: &mut Commands) {
    let timeout = Timeout::new(RESTORE_FOCUS_GUARD_TIMEOUT, None, commands);
    commands.spawn((timeout, RestoreFocusMarker { entity }));
}

/// Guard spawned when `show_active_workspace` writes a restored strip's
/// `Position` directly (`virtual_workspace_animations = false`). That write
/// only fixes the strip's own aggregate offset instantly; individual member
/// windows still get re-evaluated by `position_layout_windows` on a
/// following tick (via `position_layout_strips` reacting to the strip's now
/// -`Changed<Position>`), using whatever `Position` they were last left at —
/// which, for a lower stack member parked while its strip was hidden, can
/// land close enough to its restored target that neither the "offscreen
/// distance" nor the "parked at the corner" heuristic recognizes it as a
/// restore-driven move. Left to the ordinary heuristic, that move animates,
/// riding along even though the strip itself just snapped. While a live
/// guard names a strip, `position_layout_windows` snaps every member
/// window directly, bypassing that heuristic entirely. Expires via
/// `timeout_ticker` like `RestoreFocusMarker`.
#[derive(Component, Debug)]
pub(crate) struct SnapStripMarker {
    pub strip: Entity,
}

/// Long enough for the settle described above to actually finish (it can
/// take more than one tick), short enough that an ordinary layout change
/// shortly after a switch still animates normally.
const SNAP_STRIP_GUARD_TIMEOUT: Duration = Duration::from_millis(500);

fn spawn_snap_strip_guard(strip: Entity, commands: &mut Commands) {
    let timeout = Timeout::new(SNAP_STRIP_GUARD_TIMEOUT, None, commands);
    commands.spawn((timeout, SnapStripMarker { strip }));
}

fn fullscreen_window_in_strip(
    workspace_id: WorkspaceId,
    strip: &LayoutStrip,
    windows: &Windows,
    window_manager: &WindowManager,
) -> Option<Entity> {
    window_manager
        .windows_in_workspace(workspace_id)
        .ok()
        .and_then(|window_ids| {
            window_ids.into_iter().find_map(|window_id| {
                windows
                    .find_managed(window_id)
                    .map(|(_, entity)| entity)
                    .filter(|entity| strip.contains(*entity))
            })
        })
        .or_else(|| {
            windows.managed_iter().find_map(|(window, entity, _)| {
                (strip.contains(entity) && window.is_full_screen()).then_some(entity)
            })
        })
        .or_else(|| {
            windows
                .focused()
                .map(|(_, entity)| entity)
                .filter(|entity| strip.contains(*entity))
        })
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn workspace_change_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut workspaces: Query<(
        &mut LayoutStrip,
        Entity,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    )>,
    active_display: Single<(&Display, Entity), With<ActiveDisplayMarker>>,
    window_manager: Res<WindowManager>,
    mut native: ResMut<NativeMembership>,
    mut commands: Commands,
) {
    if !messages
        .read()
        .any(|event| matches!(event, Event::SpaceChanged))
    {
        return;
    }
    let (active_display, display_entity) = *active_display;

    let Ok(workspace_id) = window_manager.active_display_space(active_display.id()) else {
        error!("Unable to get active workspace id!");
        return;
    };

    native.request(NativeScope::ActivatedSpace(workspace_id));

    let mut remove_from = None;
    let mut insert_into = None;
    for (strip, entity, active, selected) in &workspaces {
        if active && strip.id() == workspace_id {
            debug!("Workspace id {} already active", strip.id());
            return;
        }
        if active && strip.id() != workspace_id {
            debug!("Workspace id {} no longer active", strip.id());
            remove_from = Some(entity);
        }
        if !active && strip.id() == workspace_id && selected {
            debug!("Workspace id {} is active", strip.id());
            insert_into = Some(entity);
        }
    }

    if insert_into.is_none() {
        // Fallback: find any strip for this workspace, preferably the one with virtual_index 0.
        insert_into = workspaces
            .iter()
            .filter(|(strip, _, _, _)| strip.id() == workspace_id)
            .min_by_key(|(strip, _, _, _)| strip.virtual_index)
            .map(|(_, entity, _, _)| entity);
    }

    if insert_into.is_none()
        && let Some(old_space) = remove_from
        && window_manager.is_fullscreen_space(active_display.id())
        && let Ok((mut old_strip, old_strip_entity, _, _)) = workspaces.get_mut(old_space)
        && let Some(fullscreen_window) =
            fullscreen_window_in_strip(workspace_id, &old_strip, &windows, &window_manager)
        && let Ok(original_index) = old_strip.index_of(fullscreen_window)
    {
        debug!("workspace_change: space={workspace_id} fullscreen");

        let fullscreen_marker = NativeFullscreenMarker {
            layout_strip: old_strip_entity,
            workspace_id: old_strip.id(),
            index: original_index,
        };
        old_strip.remove(fullscreen_window);

        let fullscreen_strip = LayoutStrip::fullscreen(workspace_id, fullscreen_window);
        let entity = commands
            .spawn((
                Position(active_display.bounds().min),
                fullscreen_marker,
                fullscreen_strip,
                ChildOf(display_entity),
            ))
            .id();
        insert_into = Some(entity);
    }

    if let Some(into) = insert_into
        && let Ok(mut entity_commands) = commands.get_entity(into)
    {
        entity_commands.try_insert(ActiveWorkspaceMarker);
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn workspace_destroyed_handler(
    mut messages: MessageReader<Event>,
    mut workspaces: Populated<(&mut LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    mut focus_history: ResMut<FocusHistory>,
    mut native: ResMut<NativeMembership>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::SpaceDestroyed { space_id } = event else {
            continue;
        };
        focus_history.forget_workspace(*space_id);
        for (strip, _, fullscreen) in &workspaces {
            if fullscreen.is_none() && strip.id() == *space_id {
                native.space_destroyed(strip, time.elapsed());
            }
        }

        let Some((entity, fullscreen)) =
            workspaces.iter().find_map(|(strip, entity, fullscreen)| {
                let window = strip.first().ok().and_then(|col| col.top());
                (strip.id() == *space_id).then_some((entity, window.zip(fullscreen.cloned())))
            })
        else {
            continue;
        };

        if let Some((
            window,
            NativeFullscreenMarker {
                layout_strip,
                workspace_id,
                index,
            },
        )) = fullscreen
        {
            let mut strip = workspaces
                .iter_mut()
                .find_map(|(strip, entity, _)| (entity == layout_strip).then_some(strip));
            if strip.is_none() {
                strip = workspaces
                    .iter_mut()
                    .find_map(|(strip, _, _)| (strip.id() == workspace_id).then_some(strip));
            }

            debug!(
                "previously fullscreened window {entity} inserted at {}",
                index
            );
            if let Some(mut strip) = strip {
                strip.insert_at(index, window);
                commands.reshuffle_around(window);
            }
        }

        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            debug!("Workspace destroyed {space_id} {entity}");
            entity_commands.try_despawn();
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn workspace_created_handler(
    mut messages: MessageReader<Event>,
    active_display: Single<(&Display, Entity), With<ActiveDisplayMarker>>,
    workspaces: Query<&LayoutStrip>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::SpaceCreated { space_id } = event else {
            continue;
        };

        if workspaces.into_iter().any(|strip| strip.id() == *space_id) {
            warn!("Workspace {space_id} already exists!");
            continue;
        }
        debug!("Workspace create {space_id}");
        let (active_display, display_entity) = *active_display;
        let strip = LayoutStrip::new(*space_id, 0);
        let origin = active_display.bounds().min;
        commands.spawn_layout_strip(strip, origin, display_entity, false);
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
fn find_orphaned_workspaces(
    orphans: Populated<(&LayoutStrip, Entity, &Timeout, Option<&ChildOf>), With<Timeout>>,
    displays: Populated<(&Display, Entity)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let present = window_manager.present_displays();
    let mut rehomed = false;

    for (orphan, orphan_entity, timeout, child) in orphans {
        // Row 0 is the row every space is created with, so it is re-parented
        // like a populated strip: despawning it leaves the space numbered from
        // "2" with no row that any switch or reap path recreates.
        if orphan.len() == 0 && orphan.virtual_index > 0 {
            if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
                cmd.try_despawn();
            }
            debug!("despawning empty orphan workspace {}", orphan.id());
            continue;
        }
        if child.is_some() {
            // Was reparented, remove timer.
            if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
                cmd.try_remove::<Timeout>();
            }
            debug!(
                "layout strip {} was re-parented, removing timeout.",
                orphan.id()
            );
            continue;
        }

        if timeout.timer.is_finished() {
            // Rescue windows from orphaned strips before despawning by floating them.
            debug!("Rescue windows from timed out orphan {}.", orphan.id());
            for lost_window in orphan.all_windows() {
                if let Ok(mut cmd) = commands.get_entity(lost_window) {
                    cmd.try_insert(Unmanaged::Floating);
                }
            }
            continue;
        }

        // Find which display now owns this space ID.
        let target = present.iter().find_map(|(present_display, spaces)| {
            if spaces.iter().any(|&id| id == orphan.id()) {
                displays
                    .iter()
                    .find(|(d, _)| d.id() == present_display.id())
            } else {
                None
            }
        });
        let Some((target_display, target_entity)) = target else {
            continue; // No display owns this space yet; wait for next tick.
        };

        debug!(
            "Re-parenting orphaned strip {} to display {}",
            orphan.id(),
            target_display.id(),
        );

        if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
            cmd.try_remove::<Timeout>().insert(ChildOf(target_entity));
            commands.queue(move |world: &mut World| {
                crate::ecs::display::refresh_reparented_strip(world, orphan_entity);
            });
            rehomed = true;
        }
    }
    if rehomed {
        commands.trigger(SendMessageTrigger(Event::DisplayChanged));
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn cleanup_unordered_windows(
    windows: Query<&Window>,
    workspaces: Query<&LayoutStrip>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let windows = workspaces
        .iter()
        .flat_map(|strip| {
            strip
                .all_windows()
                .into_iter()
                .filter(|entity| !strip.tabbed(*entity))
        })
        .filter_map(|entity| windows.get(entity).ok());

    for window in windows {
        let window_id = window.id();
        if matches!(window_manager.window_ordered_in(window_id), Ok(false)) && !window.is_alive() {
            debug!("Window {window_id} is unordered; removing it.");
            commands.trigger(SendMessageTrigger(Event::WindowDestroyed {
                window_id,
                source: DestroySource::Accessibility { element: None },
            }));
        }
    }
}

/// Removes previuos `ActiveWorkspaceMarker`'s when a new one is inserted.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_active_workspace_marker(
    trigger: On<Add, ActiveWorkspaceMarker>,
    workspaces: Query<(Entity, Has<ActiveWorkspaceMarker>), With<LayoutStrip>>,
    mut commands: Commands,
) {
    workspaces.iter().for_each(|(entity, marker)| {
        if entity == trigger.entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            // Mark the currently selected VW with selected marker. This also removes the previously
            // selected markers from other VW's on the same workspace.
            entity_commands.try_insert(SelectedVirtualMarker);
        } else if marker && let Ok(mut entity_commands) = commands.get_entity(entity) {
            // Remove the active marker from any other workspace.
            entity_commands.try_remove::<ActiveWorkspaceMarker>();
        }
    });
}

fn flash_workspace_on_activation(
    active_display: Single<Entity, With<ActiveDisplayMarker>>,
    displays: Query<(Entity, &Display)>,
    strips: PopupStrips,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut previous_by_display: Local<EntityHashMap<Entity>>,
    mut commands: Commands,
) {
    previous_by_display.retain(|display, _| displays.contains(*display));
    for (display_entity, display) in &displays {
        if previous_by_display.contains_key(&display_entity) {
            continue;
        }
        let Ok(space_id) = window_manager.active_display_space(display.id()) else {
            continue;
        };
        let selected = strips
            .iter()
            .filter(|(_, strip, parent, _, _, _)| {
                parent.parent() == display_entity && strip.id() == space_id
            })
            .min_by_key(|(_, strip, _, _, selected, _)| (!*selected, strip.virtual_index));
        if let Some((strip_entity, _, _, _, _, _)) = selected {
            previous_by_display.insert(display_entity, strip_entity);
        }
    }

    let Some((entity, strip, display, _, _, restored)) =
        strips.iter().find(|(_, _, _, active, _, _)| *active)
    else {
        return;
    };
    if display.parent() != *active_display {
        // Focus can activate the destination strip before the OS notifies us
        // which display is active. Re-run when ActiveDisplayMarker moves so the
        // message is never rendered at the old display's coordinates.
        return;
    }

    // Compare only with the strip last active on this display. A focus move
    // between displays is not a workspace switch on either display.
    let switched = previous_by_display
        .insert(display.parent(), entity)
        .is_some_and(|old| old != entity);
    if restored {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<RestoredActivation>();
        }
        return;
    }
    if switched && config.workspace_popup_status() {
        commands.flash_message(format!("{}", strip.virtual_index + 1), 1.0);
    }
}

/// Removes previuos `SelectedVirtualMarker`'s when a new one is inserted.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_selected_space_marker(
    trigger: On<Add, SelectedVirtualMarker>,
    workspaces: Query<(Entity, &LayoutStrip, Has<SelectedVirtualMarker>)>,
    mut commands: Commands,
) {
    let Ok(workspace_id) = workspaces
        .get(trigger.entity)
        .map(|(_, strip, _)| strip.id())
    else {
        return;
    };

    // Remove the marker from other strips on the same workspace.
    workspaces.iter().for_each(|(entity, strip, marker)| {
        if marker
            && entity != trigger.entity
            && strip.id() == workspace_id
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<SelectedVirtualMarker>();
        }
    });
}

/// Each native Space has one strip, so the virtual workspace commands do
/// nothing. They still parse, so a configuration that binds them loads.
fn refuse_virtual_workspace_commands(mut messages: MessageReader<Event>) {
    let virtual_workspace = |operation: &Operation| {
        matches!(
            operation,
            Operation::Virtual(_)
                | Operation::VirtualNumber(_)
                | Operation::VirtualAdd
                | Operation::FocusOrVirtual(_)
                | Operation::VirtualMove(..)
                | Operation::VirtualMoveNumber(..)
        )
    };
    for operation in filter_window_operations(&mut messages, virtual_workspace) {
        warn!("Virtual workspaces are disabled, so {operation:?} does nothing");
    }
}

/// The column of `strip` whose window sits nearest the display's horizontal
/// centre. Stands in for the focus a strip never had when it is parked, so
/// that returning to it lands on whatever the user was looking at.
fn column_closest_to_center(
    strip: &LayoutStrip,
    display: &Display,
    windows: &Windows,
) -> Option<Entity> {
    let display_center = display.bounds().center().x;
    strip
        .all_columns()
        .into_iter()
        .filter_map(|candidate| {
            let center = windows.moving_frame(candidate)?.center().x;
            Some((candidate, (center - display_center).abs()))
        })
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| candidate)
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(crate) fn show_active_workspace(
    activated: Single<Entity, Added<ActiveWorkspaceMarker>>,
    windows: Windows,
    mut workspaces: RestorableStrips,
    displays: Query<(&Display, Option<&DockPosition>)>,
    focus_markers: Query<Ref<FocusedMarker>>,
    scrollings: Query<&Scrolling>,
    pointer_follows: Query<(), With<PointerFollowsFocus>>,
    config: Res<Config>,
    mut commands: Commands,
) {
    // A command that switched to this strip moves the pointer to the focus it
    // restores. `clear_pointer_follows_focus` drops the tag after this frame.
    let from_command = pointer_follows.contains(*activated);

    let Some(workspace_id) = workspaces
        .iter()
        .find_map(|(entity, _, strip, _, _, _)| (entity == *activated).then_some(strip.id()))
    else {
        return;
    };

    // Hide other strips on the current workspace
    let current_focus = windows.focused();
    let current_workspace = workspaces
        .iter_mut()
        .filter(|(entity, _, strip, _, _, _)| strip.id() == workspace_id && *entity != *activated);

    for (entity, mut position, strip, child, previous, moving) in current_workspace {
        let Ok((active_display, _)) = displays.get(child.parent()) else {
            continue;
        };
        if previous.is_none() {
            let mut focus =
                current_focus.and_then(|(_, entity)| strip.contains(entity).then_some(entity));
            if focus.is_none() {
                focus = column_closest_to_center(strip, active_display, &windows);
                debug!("No previous focus, taking centered one {focus:?}.");
            }

            // An unfinished settle is where the strip was headed.
            let settle = scrollings.get(entity).ok().and_then(|scroll| scroll.settle);
            let origin = match (settle, &moving) {
                (Some(target), _) => Origin::new(round_px(target), position.0.y),
                (None, Some(RepositionMarker(destination))) => *destination,
                (None, None) => position.0,
            };
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_insert(PreviousStripPosition { origin, focus });
            }
        }

        let bounds = active_display.bounds();

        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<Scrolling>()
                .try_remove::<RepositionMarker>()
                .try_remove::<ManualStripOffset>();
        }

        if config.virtual_workspace_animations() {
            commands.reposition_entity(entity, bounds.max - PARKED_STRIP_SLIVER);
        } else {
            position.0 = bounds.max - PARKED_STRIP_SLIVER;
        }
    }

    let Ok((_, mut position, strip, child, previous_position, _)) = workspaces.get_mut(*activated)
    else {
        return;
    };
    debug!("showing virtual workspace {} ({})", strip.id(), *activated);

    // If no previous strip position exists, then the workspace was not hidden.
    if let Some(PreviousStripPosition { origin, focus }) = previous_position {
        if let Ok(mut entity_commands) = commands.get_entity(*activated) {
            entity_commands.try_remove::<PreviousStripPosition>();
        }

        // A window of this strip already holding focus is what activated it in
        // the first place - a Cmd-Tab straight into one of its windows. The
        // remembered focus is a fallback for a plain workspace switch, so it
        // must not pull focus away from that window.
        let focused = windows
            .focused()
            .map(|(_, entity)| entity)
            .filter(|entity| strip.contains(*entity));
        let keeps_focus = focused.is_some();

        // Only a focus that arrived with this activation gets the strip moved
        // for it. A window that held focus all along - switching back from an
        // empty workspace, say - keeps the offset the user left behind, even
        // if they had scrolled it out of sight.
        let arriving_focus = focused.filter(|entity| {
            focus_markers
                .get(*entity)
                .is_ok_and(|marker| marker.is_added())
        });

        // The saved origin describes where the strip sat when it was parked,
        // which says nothing about the window that just took focus: it can
        // easily land mostly off-screen. Scroll the strip the minimum amount
        // needed to expose it, the same correction `ensure_visible_in_strip`
        // makes - it cannot do it here, because it skips a strip on the frame
        // its `ActiveWorkspaceMarker` is added.
        let origin = arriving_focus
            .and_then(|focus_entity| {
                let layout = windows.layout_position(focus_entity)?.0;
                let size = windows.size(focus_entity)?;
                let (display, dock) = displays.get(child.parent()).ok()?;
                let viewport = display.actual_display_bounds(dock, &config);
                Some(origin_exposing(layout, size, *origin, viewport))
            })
            .unwrap_or(*origin);

        if config.virtual_workspace_animations() {
            commands.reposition_entity(*activated, origin);
        } else {
            position.0 = origin;
            // The strip's own offset is fixed instantly, but member windows
            // (a lower stack member especially) can still need a correction
            // on a following tick that the ordinary offscreen/parking
            // heuristic in `position_layout_windows` won't recognize as
            // restore-driven — see `SnapStripMarker`.
            spawn_snap_strip_guard(*activated, &mut commands);
        }

        if keeps_focus {
            // The layout of a strip that has just been unparked can still
            // settle over the next frames (widths recomputed, windows the
            // workspace picked up while it was hidden). Re-check the arriving
            // window then: `ensure_visible_in_strip` only scrolls if the slot
            // it ends up in really falls off an edge. That correction fires a
            // tick after this one (its own `is_added` guard skips the
            // activation tick), so it must snap rather than animate when
            // `virtual_workspace_animations` is off — otherwise the strip
            // (and everything in it) visibly slides on top of the restore
            // this function just did instantly.
            if let Some(focus_entity) = arriving_focus {
                commands.ensure_visible_snap(focus_entity, !config.virtual_workspace_animations());
            }
            return;
        }

        // Focus on the previous window
        if let Some(focus) = *focus
            && strip.contains(focus)
        {
            spawn_restore_focus_guard(focus, &mut commands);

            if from_command {
                commands.focus_entity_for_command(focus, false);
            } else {
                commands.focus_entity(focus, false);
            }
            commands.trigger(RaiseWindow {
                entity: focus,
                with_strip: true,
            });
        }
    }
}

/// Keeps one strip per native Space. A strip added for a Space that already
/// has one merges with it, the active or selected strip first, instead of
/// becoming a hidden row. Fullscreen strips stay on their own.
fn merge_strips_per_space(mut set: MergeStrips, mut commands: Commands) {
    let added: HashSet<WorkspaceId> = set
        .p0()
        .iter()
        .filter(|strip| !strip.is_fullscreen())
        .map(LayoutStrip::id)
        .collect();
    if added.is_empty() {
        return;
    }

    let mut strips = set.p1();
    for workspace_id in added {
        let mut rows = strips
            .iter_mut()
            .filter(|(_, strip, _, _)| strip.id() == workspace_id && !strip.is_fullscreen())
            .collect::<Vec<_>>();
        rows.sort_by_key(|(entity, strip, active, selected)| {
            (!*active, !*selected, strip.virtual_index, *entity)
        });
        let Some(((_, kept, _, _), others)) = rows.split_first_mut() else {
            continue;
        };
        if others.is_empty() && kept.virtual_index == 0 {
            continue;
        }
        kept.virtual_index = 0;
        for (entity, strip, _, _) in others {
            warn!("Merging a second strip of workspace {workspace_id} into its first");
            kept.append_strip(strip);
            if let Ok(mut entity_commands) = commands.get_entity(*entity) {
                entity_commands.try_despawn();
            }
        }
    }
}

fn reap_empty_virtual_workspaces(
    changed: Single<Entity, Added<ActiveWorkspaceMarker>>,
    strips: Populated<(Entity, &LayoutStrip)>,
    mut commands: Commands,
) {
    let changed_entity = *changed;
    let Some(workspace_id) = strips.get(changed_entity).ok().map(|(_, strip)| strip.id()) else {
        return;
    };
    debug!("cleaning up virtual workspaces on space {workspace_id}");
    let mut rows = strips
        .iter()
        .filter(|(_, strip)| strip.id() == workspace_id)
        .collect::<Vec<_>>();
    rows.sort_by_key(|(_, strip)| strip.virtual_index);

    if rows.is_empty() {
        return;
    }

    // Do not reap the strip that just became active. Switching/creating onto an
    // empty virtual workspace is intentional (e.g. South auto-create); only
    // empty rows left behind should be removed.
    for (entity, strip) in rows {
        if entity != changed_entity
            && strip.virtual_index > 0
            && strip.len() == 0
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_despawn();
        }
    }
}
