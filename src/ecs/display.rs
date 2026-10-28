use bevy::app::{App, Plugin, PreUpdate};
use bevy::ecs::change_detection::DetectChangesMut;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, NonSend, Query, Res, SystemParam};
use bevy::ecs::world::World;
use bevy::math::IRect;
use bevy::platform::collections::HashSet;
use bevy::time::{Time, Timer, TimerMode};
use objc2_app_kit::NSScreen;
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;
use tracing::{Level, debug, error, instrument, warn};

use crate::config::Config;
use crate::ecs::layout::{Before, Column, LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, DockPosition, EnsureVisibleMarker, FocusedMarker,
    LayoutPosition, ManualStripOffset, NativeFullscreenMarker, Position, ReadDisplayProperties,
    RefreshWindowSizeOnFocus, RepositionMarker, SelectedVirtualMarker, SendMessageTrigger,
    SpawnCommandsExt, Timeout,
};
use crate::events::Event;
use crate::manager::{Display, Size, Window, WindowManager, irect_from};
use crate::platform::{PlatformCallbacks, WorkspaceId};
use crate::util::{read_screen_property, round_px};
use bevy::ecs::entity::EntityHashSet;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::ResMut;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// A new column after the last one.
    Append,
    /// A new column at gap `index`, where gap 0 is before the first column.
    Column(usize),
    /// Into column `column`, `slot` items from the top, as `Into(column, slot)`.
    Into(usize, usize),
}

pub(crate) struct DisplayMove {
    /// The strip the window joined. `None` when the display shows no strip,
    /// which only an [`Placement::Append`] move allows.
    pub strip: Option<Entity>,
    pub display_bounds: IRect,
    pub left_behind: Option<Entity>,
    /// The moved window and the native tabs grouped with it.
    pub members: Vec<Entity>,
    /// What a maximized window that joined no strip returns to. The strip a
    /// native report later adds it to keeps it maximized.
    pub maximized: Option<Before>,
}

type MovableStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        &'static Position,
        &'static ChildOf,
        Has<SelectedVirtualMarker>,
    ),
>;

type DisplaysWithDocks<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Display,
        Option<&'static DockPosition>,
        Has<ActiveDisplayMarker>,
    ),
>;

#[derive(SystemParam)]
pub(crate) struct DisplayMoves<'w, 's> {
    pub strips: MovableStrips<'w, 's>,
    pub displays: DisplaysWithDocks<'w, 's>,
    window_manager: Res<'w, WindowManager>,
    config: Res<'w, Config>,
}

impl DisplayMoves<'_, '_> {
    pub(crate) fn active_display(&self) -> Option<&Display> {
        self.displays
            .iter()
            .find_map(|(_, display, _, active)| active.then_some(display))
    }

    pub(crate) fn other_displays(&self) -> impl Iterator<Item = &Display> {
        self.displays
            .iter()
            .filter_map(|(_, display, _, active)| (!active).then_some(display))
    }

    /// The strip shown on display `id`: the selected strip of the space macOS
    /// shows there.
    pub(crate) fn shown_strip(&self, id: CGDirectDisplayID) -> Option<Entity> {
        let space = self.window_manager.active_display_space(id).ok()?;
        self.strips
            .iter()
            .find(|(_, strip, _, _, selected)| *selected && strip.id() == space)
            .map(|(entity, _, _, _, _)| entity)
    }

    pub(crate) fn viewport(&self, display_entity: Entity) -> Option<IRect> {
        let (_, display, dock, _) = self.displays.get(display_entity).ok()?;
        Some(display.actual_display_bounds(dock, &self.config))
    }

    /// Moves `entity`, with the native tabs grouped with it, from its strip
    /// to the strip shown on display `target`, at `placement`.
    ///
    /// The window is sized for the destination: the width of the column it
    /// joins, or for a new column the width it had relative to its old
    /// display, at most the destination's width, and the height of the
    /// destination. `size` is its size now,
    /// and `frame_of` gives the frame of any window, for a column's width.
    /// The window it leaves behind refits its strip.
    ///
    /// Returns `None`, changing nothing, when `entity` is in no strip, there
    /// is no display `target`, the target strip is its own, or `placement`
    /// names a column the target strip cannot fill.
    pub(crate) fn move_to_display(
        &mut self,
        entity: Entity,
        target: CGDirectDisplayID,
        placement: Placement,
        size: Size,
        frame_of: impl Fn(Entity) -> Option<IRect>,
        commands: &mut Commands,
    ) -> Option<DisplayMove> {
        let (source, source_display) = self
            .strips
            .iter()
            .find(|(_, strip, _, _, _)| strip.contains(entity))
            .map(|(strip_entity, _, _, child, _)| (strip_entity, child.parent()))?;
        let (target_display_entity, display_bounds) = self
            .displays
            .iter()
            .find(|(_, display, _, _)| display.id() == target)
            .map(|(display_entity, display, _, _)| (display_entity, display.bounds()))?;
        let strip = self.shown_strip(target);
        if strip == Some(source) {
            return None;
        }
        let source_viewport = self.viewport(source_display)?;
        let target_viewport = self.viewport(target_display_entity)?;

        let joined_width = match placement {
            Placement::Into(column, _) => {
                let target_strip = self.strips.get(strip?).ok()?.1;
                match target_strip.get(column).ok()? {
                    Column::Fullscren(_) => return None,
                    column => column.width(&frame_of),
                }
            }
            Placement::Append | Placement::Column(_) => None,
        };
        if strip.is_none() && placement != Placement::Append {
            return None;
        }
        let width = joined_width
            .unwrap_or_else(|| new_column_width(size.x, source_viewport, target_viewport));
        let (_, mut source_strip, _, _, _) = self.strips.get_mut(source).ok()?;
        let members = source_strip
            .tab_group(entity)
            .unwrap_or_else(|| vec![entity]);
        let left_behind = source_strip
            .left_neighbour(entity)
            .or_else(|| source_strip.right_neighbour(entity))
            .filter(|neighbour| !members.contains(neighbour));
        // A maximized window stays maximized in a column of its own. One
        // dropped into a column joins it as an ordinary window.
        let mut maximized = source_strip
            .maximized(entity)
            .filter(|_| !matches!(placement, Placement::Into(..)))
            .cloned();
        for member in &members {
            size_for_arrival(
                *member,
                Size::new(width, target_viewport.height()),
                commands,
            );
            source_strip.remove(*member);
        }
        if let Some(neighbour) = left_behind {
            commands.refit_after_removal(neighbour);
        }

        if let Some((_, mut target_strip, _, _, _)) =
            strip.and_then(|strip| self.strips.get_mut(strip).ok())
        {
            match (placement, members.as_slice()) {
                (Placement::Append, [_]) => target_strip.append(entity),
                (Placement::Append, group) => target_strip.append_tab_group(group),
                (Placement::Column(index), group) => target_strip.insert_tab_group_at(index, group),
                (Placement::Into(column, slot), group) => {
                    _ = target_strip.join_column(group, column, slot);
                }
            }
            if let Some(before) = maximized.take() {
                target_strip.adopt_maximized(entity, before);
            }
        }
        Some(DisplayMove {
            strip,
            display_bounds,
            left_behind,
            members,
            maximized,
        })
    }

    /// Moves the whole column holding `entity` from its strip to the strip
    /// shown on display `target`, as a new column at gap `index`, or after
    /// the last column for `None`. Every stack item moves in its order, with
    /// its native tabs, and a tabbed or maximized column stays so.
    ///
    /// The column keeps the width it had relative to its old display, at
    /// most the destination's width, as [`Self::move_to_display`] sizes a
    /// new column. A stack's windows split the destination height evenly, as
    /// after any change to a stack's membership. A stack taller than the
    /// destination's stack limit arrives whole, and the layout overlaps it
    /// as it does any stack that does not fit its column.
    ///
    /// A column of one window or native tab group moves as
    /// [`Self::move_to_display`] moves it, which also handles a display that
    /// shows no strip. A stack needs a strip to stay a stack.
    ///
    /// Returns `None`, changing nothing, when `entity` is in no strip, there
    /// is no display `target`, the target strip is its own, or the column is
    /// a stack and the display shows no strip.
    pub(crate) fn move_column_to_display(
        &mut self,
        entity: Entity,
        target: CGDirectDisplayID,
        index: Option<usize>,
        size: Size,
        frame_of: impl Fn(Entity) -> Option<IRect>,
        commands: &mut Commands,
    ) -> Option<DisplayMove> {
        let (source, source_display, column_index, stacked) =
            self.strips
                .iter()
                .find_map(|(strip_entity, strip, _, child, _)| {
                    let column_index = strip.index_of(entity).ok()?;
                    let stacked = matches!(strip.get(column_index).ok()?, Column::Stack(_));
                    Some((strip_entity, child.parent(), column_index, stacked))
                })?;
        let strip = self.shown_strip(target);
        if !stacked {
            let placement = match (strip, index) {
                (Some(_), Some(index)) => Placement::Column(index),
                _ => Placement::Append,
            };
            return self.move_to_display(entity, target, placement, size, frame_of, commands);
        }
        let strip = strip.filter(|strip| *strip != source)?;
        let (target_display_entity, display_bounds) = self
            .displays
            .iter()
            .find(|(_, display, _, _)| display.id() == target)
            .map(|(display_entity, display, _, _)| (display_entity, display.bounds()))?;
        let source_viewport = self.viewport(source_display)?;
        let target_viewport = self.viewport(target_display_entity)?;
        let width = new_column_width(size.x, source_viewport, target_viewport);

        let [
            (_, mut source_strip, _, _, _),
            (_, mut target_strip, _, _, _),
        ] = self.strips.get_many_mut([source, strip]).ok()?;
        let left_behind = source_strip
            .left_neighbour(entity)
            .or_else(|| source_strip.right_neighbour(entity));
        let column = source_strip.take_column(column_index)?;
        let members = column.windows();
        for member in &members {
            size_for_arrival(
                *member,
                Size::new(width, target_viewport.height()),
                commands,
            );
        }
        if let Some(neighbour) = left_behind {
            commands.refit_after_removal(neighbour);
        }
        let index = index.unwrap_or(target_strip.len());
        target_strip.insert_column(index, column);
        Some(DisplayMove {
            strip: Some(strip),
            display_bounds,
            left_behind,
            members,
            maximized: None,
        })
    }
}

/// The width of a column that moves from `source` to `target`, both strip
/// viewports: the share `width` had of the old viewport, at most the new
/// viewport's width.
fn new_column_width(width: i32, source: IRect, target: IRect) -> i32 {
    let ratio = (source.width() > 0).then(|| f64::from(width) / f64::from(source.width()));
    // A window wider than its old viewport, such as one macOS filled,
    // fills the new viewport instead of overflowing it.
    ratio
        .map_or(width, |ratio| round_px(ratio * f64::from(target.width())))
        .min(target.width())
}

/// Sizes `member`, which is moving to another display, for the layout there.
pub(crate) fn size_for_arrival(member: Entity, bounds: Size, commands: &mut Commands) {
    // The destination layout sizes the window from `Bounds`. A resize
    // animation toward the full height would outlive that layout: a
    // window joining a stack keeps only its share, and the stale
    // height would mislead every reader of the moving frame.
    if let Ok(mut entity_commands) = commands.get_entity(member) {
        entity_commands
            .try_remove::<crate::ecs::ResizeMarker>()
            .try_insert((
                crate::ecs::Bounds(bounds),
                crate::ecs::VerifyWindowSize::default(),
            ));
    }
}

const ORPHANED_SPACES_TIMEOUT_SEC: u64 = 30;
const DISPLAY_SETTLE_DELAY: Duration = Duration::from_millis(350);
const DISPLAY_VERIFY_DELAY: Duration = Duration::from_secs(1);
const DISPLAY_RETRY_DELAY: Duration = Duration::from_secs(1);
const DISPLAY_RETRIES: u8 = 3;

#[derive(Default, Resource)]
pub(crate) struct DisplayReconcileState {
    pending: Option<Timer>,
    retries_left: u8,
    verify_again: bool,
}

impl DisplayReconcileState {
    pub(crate) fn is_settling(&self) -> bool {
        self.pending.is_some() && self.verify_again
    }
}

pub struct DisplayEventsPlugin;

impl Plugin for DisplayEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DisplayReconcileState>()
            .add_systems(
                PreUpdate,
                (display_change_handler, reconcile_displays)
                    .chain()
                    .in_set(super::PreUpdateSet::Dispatch),
            )
            .add_observer(read_display_properties_trigger)
            .add_observer(cleanup_active_display_marker);
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_active_display_marker(
    trigger: On<Add, ActiveDisplayMarker>,
    displays: Query<(Entity, Has<ActiveDisplayMarker>), With<Display>>,
    mut commands: Commands,
) {
    for (entity, active) in displays {
        if active
            && entity != trigger.entity
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {entity} lost active marker.");
            cmd.try_remove::<ActiveDisplayMarker>();
        }
    }
}

/// Handles display change events.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn display_change_handler(
    mut messages: MessageReader<Event>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Query<Entity, With<Window>>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if !messages
        .read()
        .any(|event| matches!(event, Event::DisplayChanged))
    {
        return;
    }

    for window in windows {
        if let Ok(mut cmd) = commands.get_entity(window) {
            cmd.try_insert(RefreshWindowSizeOnFocus);
        }
    }

    let Ok(active_id) = window_manager.active_display_id() else {
        error!("Unable to get active display id!");
        return;
    };

    for (display, entity, focused) in displays {
        let display_id = display.id();
        if !focused
            && display_id == active_id
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {display_id} is active");
            cmd.try_insert(ActiveDisplayMarker);
        }
    }
    commands.trigger(SendMessageTrigger(Event::SpaceChanged));
}

/// Full reconciliation of the ECS display set against the OS truth.
///
/// Runs after a brief quiet period following wake or a display notification.
/// macOS may report an empty or incomplete display list during reconfiguration;
/// applying that transient list would orphan every workspace. A successful
/// scan refreshes both display ownership and the positions of its strips.
pub(crate) fn reconcile_displays(
    mut messages: MessageReader<Event>,
    workspaces: Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    mut displays: Query<(&mut Display, Entity)>,
    window_manager: Res<WindowManager>,
    time: Res<Time>,
    mut state: ResMut<DisplayReconcileState>,
    mut commands: Commands,
) {
    let needs_reconcile = messages.read().fold(false, |changed, event| {
        changed
            | matches!(
                event,
                Event::SystemWoke { .. }
                    | Event::DisplayAdded { .. }
                    | Event::DisplayRemoved { .. }
                    | Event::DisplayMoved { .. }
                    | Event::DisplayResized { .. }
                    | Event::DisplayConfigured { .. }
            )
    });
    if needs_reconcile {
        state.pending = Some(Timer::new(DISPLAY_SETTLE_DELAY, TimerMode::Once));
        state.retries_left = DISPLAY_RETRIES;
        state.verify_again = true;
        return;
    }

    let Some(timer) = state.pending.as_mut() else {
        return;
    };
    timer.tick(time.delta());
    if !timer.is_finished() {
        return;
    }
    state.pending = None;

    debug!("Reconciling displays against OS after wake / resize / configure");

    let mut present_displays: HashMap<CGDirectDisplayID, _> = window_manager
        .0
        .present_displays()
        .into_iter()
        .map(|(display, workspaces)| (display.id(), (display, workspaces)))
        .collect();
    if present_displays.is_empty()
        || present_displays
            .values()
            .any(|(_, spaces)| spaces.is_empty())
    {
        if state.retries_left > 0 {
            state.retries_left -= 1;
            warn!("Display list is incomplete; retrying in {DISPLAY_RETRY_DELAY:?}");
            state.pending = Some(Timer::new(DISPLAY_RETRY_DELAY, TimerMode::Once));
            return;
        }
        warn!("Display list is still incomplete after retries; applying the latest scan");
    }

    let previous_bounds: HashMap<Entity, IRect> = displays
        .iter()
        .map(|(display, entity)| (entity, display.bounds()))
        .collect();
    let previous_parents: HashMap<Entity, IRect> = workspaces
        .iter()
        .filter_map(|(_, entity, child)| {
            let bounds = previous_bounds.get(&child?.parent())?;
            Some((entity, *bounds))
        })
        .collect();

    let existing_displays: HashMap<CGDirectDisplayID, _> = displays
        .iter()
        .map(|(display, workspaces)| (display.id(), (display, workspaces)))
        .collect();

    let present_ids = present_displays.keys().copied().collect::<HashSet<_>>();
    let existing_ids = existing_displays.keys().copied().collect::<HashSet<_>>();

    // Displays that vanished while we were away (e.g. unplugged during sleep).
    for display_id in existing_ids.difference(&present_ids) {
        let Some((display, _)) = existing_displays.get(display_id) else {
            error!("Unable to find removed display: {display_id}");
            continue;
        };
        remove_display(display, &workspaces, &displays, &mut commands);
    }

    // Displays that appeared while we were away.
    for display_id in present_ids.difference(&existing_ids) {
        let Some((display, workspace_ids)) = present_displays.remove(display_id) else {
            error!("Unable to find added display: {display_id}");
            continue;
        };
        add_display(display, &workspace_ids, &workspaces, &mut commands);
    }

    // Displays that are still present: refresh their bounds (resolution or
    // menubar may have changed) and re-home any workspaces that drifted.
    for display_id in present_ids.intersection(&existing_ids) {
        let Some((display, workspace_ids)) = present_displays.remove(display_id) else {
            continue;
        };
        move_display(
            display,
            &workspace_ids,
            &mut displays,
            &workspaces,
            &mut commands,
        );
    }

    commands.queue(move |world: &mut World| {
        world
            .resource_mut::<super::native_membership::NativeMembership>()
            .request(super::native_membership::NativeScope::DisplayTopology);
        refresh_display_layout(world, &previous_parents);
    });
    commands.trigger(SendMessageTrigger(Event::DisplayChanged));
    if state.verify_again {
        // A begin-configuration callback can precede the final macOS space
        // assignment even after a quiet period. Verify once more without
        // requiring a second notification from Core Graphics.
        state.verify_again = false;
        state.pending = Some(Timer::new(DISPLAY_VERIFY_DELAY, TimerMode::Once));
    }
}

/// Moves the `admitted` windows, which macOS reports in `space` on `display`,
/// out of their strips on other spaces. Each source strip's windows keep their
/// columns and join the end of the space's strip, so each native space keeps
/// one strip.
pub(crate) fn migrate_windows_to_space(
    world: &mut World,
    display: Entity,
    space: WorkspaceId,
    admitted: &EntityHashSet,
) {
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
    let target = world
        .query::<(
            Entity,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
            Has<NativeFullscreenMarker>,
        )>()
        .iter(world)
        .filter(|(_, strip, _, _, fullscreen)| !fullscreen && strip.id() == target_space_id)
        .min_by_key(|(_, strip, active, selected, _)| (!active, !selected, strip.virtual_index))
        .map(|(entity, ..)| entity);

    let strip_entity = if let Some(dst_entity) = target {
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
        dst_entity
    } else {
        world
            .spawn((
                Position(display_bounds.min),
                extracted,
                ChildOf(display_entity),
                SelectedVirtualMarker,
            ))
            .id()
    };
    if retains_focus {
        world.entity_mut(strip_entity).insert(ActiveWorkspaceMarker);
    }
}

/// Removes emptied virtual workspace strips on `workspace_id` and compacts the
/// remaining `virtual_index` sequence from `0` so that a workspace with
/// remaining windows never ends up with an empty first workspace (`virtual_index = 0`).
pub(crate) fn compact_workspace_strips(world: &mut World, workspace_id: WorkspaceId) {
    let mut rows: Vec<(Entity, u32, usize, bool, bool, Option<Entity>)> = world
        .query::<(
            Entity,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
            Option<&ChildOf>,
            Has<NativeFullscreenMarker>,
        )>()
        .iter(world)
        .filter_map(|(entity, strip, active, selected, child, fullscreen)| {
            (!fullscreen && strip.id() == workspace_id).then_some((
                entity,
                strip.virtual_index,
                strip.len(),
                active,
                selected,
                child.map(ChildOf::parent),
            ))
        })
        .collect();

    if rows.is_empty() {
        return;
    }
    rows.sort_by_key(|(_, v_idx, _, _, _, _)| *v_idx);

    let has_populated = rows.iter().any(|(_, _, len, _, _, _)| *len > 0);
    let space_has_display = rows.iter().any(|(_, _, _, _, _, parent)| parent.is_some());

    if has_populated {
        let mut had_active = false;
        let mut had_selected = false;
        let mut populated_rows = Vec::new();

        for (entity, _, len, active, selected, parent) in rows {
            if len == 0 {
                had_active |= active;
                had_selected |= selected;
                world.despawn(entity);
            } else {
                populated_rows.push((entity, active, selected, parent));
            }
        }

        for (idx, (entity, _, _, _)) in populated_rows.iter().enumerate() {
            if let Some(mut strip) = world.get_mut::<LayoutStrip>(*entity) {
                strip.virtual_index = u32::try_from(idx).unwrap_or(0);
            }
        }

        let any_selected = populated_rows.iter().any(|(_, _, selected, _)| *selected);
        if (had_active || had_selected || !any_selected)
            && let Some(&(first_entity, _, _, parent)) = populated_rows.first()
        {
            promote_strip_to_first(world, first_entity, parent, true, had_active);
        }
    } else if !space_has_display {
        for (entity, _, _, _, _, _) in rows {
            world.despawn(entity);
        }
    } else {
        let (first_entity, _, _, _, _, parent) = rows[0];
        let mut had_active = false;
        let mut had_selected = false;
        for &(entity, _, _, active, selected, _) in &rows[1..] {
            had_active |= active;
            had_selected |= selected;
            world.despawn(entity);
        }
        if let Some(mut strip) = world.get_mut::<LayoutStrip>(first_entity) {
            strip.virtual_index = 0;
        }
        promote_strip_to_first(world, first_entity, parent, had_selected, had_active);
    }
}

fn promote_strip_to_first(
    world: &mut World,
    strip_entity: Entity,
    parent: Option<Entity>,
    make_selected: bool,
    make_active: bool,
) {
    if let Some(prev) = world
        .entity_mut(strip_entity)
        .take::<PreviousStripPosition>()
    {
        if let Some(mut pos) = world.get_mut::<Position>(strip_entity) {
            pos.0 = prev.origin;
        }
    } else if let Some(display_entity) = parent
        && let Some(bounds) = world.get::<Display>(display_entity).map(Display::bounds)
        && let Some(mut pos) = world.get_mut::<Position>(strip_entity)
    {
        pos.0 = bounds.min;
    }
    if make_selected {
        world.entity_mut(strip_entity).insert(SelectedVirtualMarker);
    }
    if make_active {
        world.entity_mut(strip_entity).insert(ActiveWorkspaceMarker);
    }
}

/// Rebase each strip onto its current display and ask the layout and OS writers
/// to replay window frames. This also covers wakeups where macOS moved windows
/// without changing the reported display geometry.
fn refresh_display_layout(world: &mut World, previous_parents: &HashMap<Entity, IRect>) {
    let strips = world
        .query_filtered::<Entity, With<LayoutStrip>>()
        .iter(world)
        .collect::<Vec<_>>();
    let focused = world
        .query_filtered::<Entity, With<FocusedMarker>>()
        .iter(world)
        .next();

    for strip_entity in strips {
        refresh_strip_layout(
            world,
            strip_entity,
            previous_parents.get(&strip_entity).copied(),
            focused,
        );
    }
}

/// Replays a strip that acquired a display after the initial reconciliation.
/// macOS may publish the space ownership later than the display itself.
pub(crate) fn refresh_reparented_strip(world: &mut World, strip_entity: Entity) {
    let focused = world
        .query_filtered::<Entity, With<FocusedMarker>>()
        .iter(world)
        .next();
    refresh_strip_layout(world, strip_entity, None, focused);
}

fn refresh_strip_layout(
    world: &mut World,
    strip_entity: Entity,
    previous_bounds: Option<IRect>,
    focused: Option<Entity>,
) {
    let Some(display_entity) = world.get::<ChildOf>(strip_entity).map(ChildOf::parent) else {
        return;
    };
    let Some(display_bounds) = world.get::<Display>(display_entity).map(Display::bounds) else {
        return;
    };
    if previous_bounds.is_none() {
        // A rescued orphan has no usable display-relative scroll offset.
        world.entity_mut(strip_entity).remove::<ManualStripOffset>();
    }
    let hidden = world.get::<PreviousStripPosition>(strip_entity).is_some();
    if let Some(mut position) = world.get_mut::<Position>(strip_entity) {
        let origin = if hidden {
            display_bounds.max - PARKED_STRIP_SLIVER
        } else if let Some(previous_bounds) = previous_bounds {
            position.0 + display_bounds.min - previous_bounds.min
        } else {
            display_bounds.min
        };
        if position.0 != origin {
            position.0 = origin;
        }
    }
    if let Some(mut previous) = world.get_mut::<PreviousStripPosition>(strip_entity) {
        previous.origin = previous_bounds.map_or(display_bounds.min, |bounds| {
            previous.origin + display_bounds.min - bounds.min
        });
    }
    if let Some(mut target) = world.get_mut::<RepositionMarker>(strip_entity) {
        target.0 = if hidden {
            display_bounds.max - PARKED_STRIP_SLIVER
        } else {
            previous_bounds.map_or(display_bounds.min, |bounds| {
                target.0 + display_bounds.min - bounds.min
            })
        };
    }

    let Some(mut strip) = world.get_mut::<LayoutStrip>(strip_entity) else {
        return;
    };
    let windows = strip.all_windows();
    strip.set_changed();
    if let Some(focused) = focused.filter(|focused| windows.contains(focused)) {
        world
            .entity_mut(focused)
            .insert(EnsureVisibleMarker { snap: true });
    }
    for window in windows {
        if let Some(mut layout) = world.get_mut::<LayoutPosition>(window) {
            layout.set_changed();
        }
        if let Some(mut position) = world.get_mut::<Position>(window) {
            position.set_changed();
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn add_display(
    display: Display,
    workspace_ids: &[WorkspaceId],
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    let display_id = display.id();
    debug!("Display Added: {display_id}");

    let display_bounds = display.bounds();
    let display_entity = commands.spawn(display).id();
    commands.trigger(ReadDisplayProperties(display_entity));

    reparent_existing_workspaces(
        workspace_ids,
        display_entity,
        &display_bounds,
        existing_strips,
        commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn remove_display(
    display: &Display,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    displays: &Query<(&mut Display, Entity)>,
    commands: &mut Commands,
) {
    let display_id = display.id();
    debug!("Display Removed: {display_id:?}");
    let Some((display, display_entity)) = displays
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find removed display!");
        return;
    };

    for (strip, entity, _) in workspaces
        .into_iter()
        .filter(|(_, _, child)| child.is_some_and(|child| child.parent() == display_entity))
    {
        let display_id = display.id();
        debug!(
            "orphaning strip {} after removal of display {display_id}.",
            strip.id(),
        );
        let timeout = Timeout::new(
            Duration::from_secs(ORPHANED_SPACES_TIMEOUT_SEC),
            Some(format!(
                "Orphaned strip {} ({strip}) could not be re-inserted after {ORPHANED_SPACES_TIMEOUT_SEC}s.",
                strip.id()
            )),
            commands,
        );
        if let Ok(mut commands) = commands.get_entity(entity) {
            commands.try_insert(timeout);
        }
        if let Ok(mut commands) = commands.get_entity(display_entity) {
            commands.detach_child(entity);
        }
    }

    if let Ok(mut commands) = commands.get_entity(display_entity) {
        commands.try_despawn();
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn move_display(
    moved_display: Display,
    workspace_ids: &[WorkspaceId],
    displays: &mut Query<(&mut Display, Entity)>,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    let display_id = moved_display.id();
    debug!("Display Moved: {display_id:?}");
    let Some((mut display, display_entity)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find moved display!");
        return;
    };
    *display = moved_display;
    commands.trigger(ReadDisplayProperties(display_entity));

    reparent_existing_workspaces(
        workspace_ids,
        display_entity,
        &display.bounds(),
        existing_strips,
        commands,
    );
}

fn reparent_existing_workspaces(
    workspace_ids: &[WorkspaceId],
    display_entity: Entity,
    display_bounds: &IRect,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    // Verifies that a moved display has all the workspaces which it owns.
    for &id in workspace_ids {
        let mut found = false;
        for (strip, entity, child) in existing_strips {
            if strip.id() == id {
                found = true;
                if child.is_none_or(|child| child.parent() != display_entity) {
                    // Re-parent this workspace
                    if let Ok(mut cmd) = commands.get_entity(entity) {
                        debug!("reparenting workspace {id} to display {display_entity}");
                        cmd.try_remove::<Timeout>()
                            .try_remove::<ChildOf>()
                            .try_insert(ChildOf(display_entity));
                    }
                }
            }
        }

        if !found {
            // New workspace.
            let origin = display_bounds.min;
            debug!("new workspace {id} on display {display_entity}");
            commands.spawn_layout_strip(LayoutStrip::new(id, 0), origin, display_entity, false);
        }
    }
}

/// Tracks whether floating windows on a workspace sit above or behind tiled
/// ones in the OS z-order. Default is `Front` (floats above tiles).
#[derive(Clone, Component, Copy)]
pub struct FloatingLayer {
    pub workspace_id: WorkspaceId,
    pub front: bool,
}

impl FloatingLayer {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            front: false,
        }
    }

    pub fn flip(&mut self) {
        self.front = !self.front;
    }
}

fn read_display_properties_trigger(
    trigger: On<ReadDisplayProperties>,
    mut displays: Query<(&mut Display, Entity)>,
    platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    config: Option<Res<Config>>,
    mut commands: Commands,
) {
    let Ok((mut display, entity)) = displays.get_mut(trigger.event().0) else {
        return;
    };
    let display_id = display.id();

    // NSScreen::screen needs to run in the main thread, thus we run it in a NonSend trigger.
    let Some(screens) = platform.map(|platform| NSScreen::screens(platform.main_thread_marker))
    else {
        return;
    };

    let notch = read_screen_property(&screens, display_id, |screen| {
        let insets = screen.safeAreaInsets();
        debug!("notch on display {display_id}: {insets:?}");
        round_px(insets.top)
    });
    if let Some(height) = notch {
        display.set_notch_height(height);
    }

    let dock = read_screen_property(&screens, display_id, |screen| {
        let visible_frame = irect_from(screen.visibleFrame());
        display.locate_dock(&visible_frame)
    });
    if let Some(dock) = dock {
        debug!("dock on display {display_id}: {:?}", dock);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(dock);
        }
    }

    if let Some(config) = config {
        let height = config.menubar_height();
        display.set_menubar_height_override(height);
    }
}
