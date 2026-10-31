use bevy::app::{PostUpdate, PreUpdate};
use bevy::ecs::entity::{Entity, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{on_message, resource_changed};
use bevy::ecs::system::{Commands, ParamSet, Query, Res, ResMut, Single, SystemParam};
use bevy::math::IRect;
use bevy::time::Time;
use tracing::{Level, instrument};
use tracing::{debug, error, info, warn};

mod query;

use crate::config::Config;
use crate::config::snippet::{RuleSubject, SnippetDialect, window_rule_snippet};
use crate::ecs::display::{DisplayMoves, FloatingLayer};
use crate::ecs::drag::NativeResizeStart;
use crate::ecs::focus::{FocusHistory, PageHapticRequest};
use crate::ecs::layout::{
    Column, LayoutStrip, MIN_WINDOW_HEIGHT, RestoreTo, StackItem, anchored_origin,
    clamp_origin_to_viewport, effective_stack_limit, hold_minimum_heights, strip_signature,
};
use crate::ecs::maximize::{FloatingMaximized, restore_tiled};
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, WindowCtx, Windows};
use crate::ecs::scroll::{PageFocusRequest, PageGestureState, PageInFlight};
use crate::ecs::tile_overrides::TileOverrides;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, DockPosition, FocusedMarker, ManualStripOffset,
    MinimumHeight, NativeFullscreenMarker, PendingWidthSettlement, Position, RaiseWindow,
    RepositionMarker, SelectedVirtualMarker, SpawnCommandsExt, StepPartners, Timeout, Unmanaged,
    VerifyWindowSize, WidthAnchor, WidthRun, WindowProperties,
};
use crate::events::{Event, InputEvent};
use crate::manager::{Application, Display, Origin, Size, Window, WindowManager, origin_from};
use crate::platform::WorkspaceId;
use crate::util::round_px;

pub use crate::types::commands::{
    Command, Direction, MouseMove, MoveFocus, Operation, Reach, ResizeDirection, Side,
};

/// Every strip alongside the two flags that say where it sits: whether it is the
/// one currently on screen, and whether it is its display's selected virtual
/// workspace.
type StripsWithVisibility<'w, 's> = Query<
    'w,
    's,
    (
        &'static ChildOf,
        &'static LayoutStrip,
        Entity,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
>;

#[derive(SystemParam)]
struct PagingFocusParams<'w, 's> {
    page_requests: MessageReader<'w, 's, PageFocusRequest>,
    page_state: ResMut<'w, PageGestureState>,
    config: Res<'w, Config>,
    strip_positions:
        Query<'w, 's, (&'static Position, Option<&'static RepositionMarker>), With<LayoutStrip>>,
    haptics: MessageWriter<'w, PageHapticRequest>,
}

pub fn register_commands(app: &mut bevy::app::App) {
    app.init_resource::<crate::menubar::MenuBarVisibility>()
        .add_systems(
            PreUpdate,
            crate::menubar::menu_bar_command_handler
                .in_set(crate::ecs::PreUpdateSet::Dispatch)
                .run_if(on_message::<Event>),
        )
        .add_systems(
            PostUpdate,
            crate::menubar::save_menu_bar_preference
                .in_set(crate::ecs::PostUpdateSet::Bookkeeping)
                .run_if(resource_changed::<crate::menubar::MenuBarVisibility>),
        );
    // Registered here (not with the Lua systems) so it's exercised by the mock
    // harness without a running interpreter.
    #[cfg(feature = "lua")]
    app.add_systems(
        PreUpdate,
        crate::ecs::layout_ops::apply_layout_ops.in_set(crate::ecs::PreUpdateSet::Dispatch),
    );

    query::register_query_commands(app);
    // Empty store so the mock harness and saveless runs still have one to
    // answer from; the real app overwrites it from disk.
    app.init_resource::<crate::ecs::script_state::ScriptStateStore>();
    app.init_resource::<crate::ecs::tile_overrides::TileOverrides>();
    app.add_systems(
        PreUpdate,
        crate::ecs::script_state::script_state_handler.in_set(crate::ecs::PreUpdateSet::Dispatch),
    );
    app.add_systems(
        PreUpdate,
        (
            command_quit_handler,
            command_restart_handler,
            print_internal_state_handler,
            mouse_to_next_display,
            resize_window,
            resize_window_vertical,
            command_center_window,
            to_next_display,
            equalize_column,
            balance_strip,
            manage_window,
            stack_windows_handler,
            command_move_focus,
            command_focus_unmanaged,
            command_focus_managed,
            command_raise_floating,
            command_toggle_floating_layer,
            command_swap_focus,
            snap_window,
        )
            .in_set(crate::ecs::PreUpdateSet::Dispatch),
    );
    // A separate registration because the tuple above is already at Bevy's
    // 20-system limit.
    //
    // A default dialect so the mock harness has one; the real app overwrites it
    // once it knows whether a Lua script took over the configuration.
    app.init_resource::<SnippetDialect>();
    app.init_resource::<HeldEdge>();
    app.add_systems(
        PreUpdate,
        (
            copy_window_rule,
            toggle_tabbed_display_handler,
            focus_display,
            end_width_runs.before(resize_window),
        )
            .in_set(crate::ecs::PreUpdateSet::Dispatch),
    );
    app.init_resource::<WidthBatchInterrupted>();
    app.add_observer(end_width_runs_on_focus);
    app.add_observer(cancel_width_settlement);
}

pub fn filter_window_operations<'a, F: Fn(&Operation) -> bool>(
    messages: &'a mut MessageReader<Event>,
    filter: F,
) -> impl Iterator<Item = &'a Operation> {
    messages.read().filter_map(move |event| {
        if let Event::Command {
            command: Command::Window(op),
        } = event
            && filter(op)
        {
            Some(op)
        } else {
            None
        }
    })
}

/// Retrieves a window `Entity` in a specified direction relative to a `current_window_id` within a `LayoutStrip`.
///
/// # Arguments
///
/// * `direction` - The direction (e.g., `West`, `East`, `First`, `Last`, `North`, `South`).
/// * `current_window_id` - The `Entity` of the current window.
/// * `strip` - A reference to the `LayoutStrip` to search within.
///
/// # Returns
///
/// `Some(Entity)` with the found window's entity, otherwise `None`.
#[instrument(level = Level::DEBUG, ret)]
pub(crate) fn get_window_in_direction(
    direction: &Direction,
    entity: Entity,
    strip: &LayoutStrip,
) -> Option<Entity> {
    let index = strip.index_of(entity).ok()?;

    match direction {
        Direction::West => strip.left_neighbour(entity),
        Direction::East => strip.right_neighbour(entity),

        Direction::First => strip.first().ok().and_then(|column| column.top()),

        Direction::Last => strip.last().ok().and_then(|column| column.top()),

        Direction::Nth(index) => strip.get(*index).ok().and_then(|column| column.top()),

        Direction::North => match strip.get(index).ok()? {
            Column::Single(_) | Column::Tabs(_) | Column::Fullscren(_) | Column::Maximized(_) => {
                None
            }
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, item)| item.contains(entity))
                .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                .and_then(StackItem::top),
        },

        Direction::South => match strip.get(index).ok()? {
            Column::Single(_) | Column::Tabs(_) | Column::Fullscren(_) | Column::Maximized(_) => {
                None
            }
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, item)| item.contains(entity))
                .and_then(|(index, _)| {
                    (index < stack.len() - 1)
                        .then(|| stack.get(index + 1))
                        .flatten()
                })
                .and_then(StackItem::top),
        },
    }
}

/// 45° direction cone, closest by squared Euclidean distance.
/// `First` / `Last` are strip-only and return `None`.
fn pick_nearest_in_direction(
    direction: &Direction,
    focused_center: bevy::math::IVec2,
    candidates: impl IntoIterator<Item = (Entity, bevy::math::IVec2)>,
) -> Option<Entity> {
    candidates
        .into_iter()
        .filter_map(|(entity, center)| {
            let dx = center.x - focused_center.x;
            let dy = center.y - focused_center.y;
            let in_direction = match direction {
                Direction::East => dx > 0 && dy.abs() <= dx.abs(),
                Direction::West => dx < 0 && dy.abs() <= dx.abs(),
                Direction::North => dy < 0 && dx.abs() <= dy.abs(),
                Direction::South => dy > 0 && dx.abs() <= dy.abs(),
                Direction::First | Direction::Last | Direction::Nth(_) => return None,
            };
            in_direction.then_some((entity, dx * dx + dy * dy))
        })
        .min_by_key(|(_, dist_sq)| *dist_sq)
        .map(|(entity, _)| entity)
}

/// The display that lies in `direction` from the display bounded by `from`, in
/// the physical arrangement: the nearest one wholly on that side, preferring
/// one that overlaps `from` along the other axis. Only the four compass
/// directions name a display.
pub(crate) fn display_in_direction<T>(
    direction: &Direction,
    from: IRect,
    displays: impl IntoIterator<Item = (T, IRect)>,
) -> Option<T> {
    let overlaps = |a: (i32, i32), b: (i32, i32)| a.0 < b.1 && b.0 < a.1;
    displays
        .into_iter()
        .filter_map(|(display, bounds)| {
            let horizontal = overlaps((bounds.min.x, bounds.max.x), (from.min.x, from.max.x));
            let vertical = overlaps((bounds.min.y, bounds.max.y), (from.min.y, from.max.y));
            let (beyond, overlap) = match direction {
                Direction::East => (bounds.min.x >= from.max.x, vertical),
                Direction::West => (bounds.max.x <= from.min.x, vertical),
                Direction::North => (bounds.max.y <= from.min.y, horizontal),
                Direction::South => (bounds.min.y >= from.max.y, horizontal),
                Direction::First | Direction::Last | Direction::Nth(_) => return None,
            };
            let offset = (bounds.center() - from.center()).as_i64vec2();
            beyond.then_some((display, !overlap, offset.length_squared()))
        })
        .min_by_key(|(_, apart, distance)| (*apart, *distance))
        .map(|(display, _, _)| display)
}

fn visible_floating_entities(
    windows: &Windows,
    window_manager: &WindowManager,
    workspace_id: WorkspaceId,
    display_bounds: IRect,
) -> Vec<Entity> {
    let workspace_window_ids: std::collections::HashSet<_> = window_manager
        .windows_in_workspace(workspace_id)
        .ok()
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default();

    windows
        .iter()
        .filter_map(|(_, entity)| {
            let (window, _, Some(Unmanaged::Floating)) = windows.get_managed(entity)? else {
                return None;
            };
            if !workspace_window_ids.contains(&window.id()) {
                return None;
            }
            let frame = windows.frame(entity)?;
            (!display_bounds.intersect(frame).is_empty()).then_some(entity)
        })
        .collect()
}

fn nearest_float_in_direction(
    direction: &Direction,
    focused_entity: Entity,
    windows: &Windows,
    window_manager: &WindowManager,
    workspace_id: WorkspaceId,
    display_bounds: IRect,
) -> Option<Entity> {
    let focused_center = windows.frame(focused_entity)?.center();

    let candidates =
        visible_floating_entities(windows, window_manager, workspace_id, display_bounds)
            .into_iter()
            .filter(|entity| *entity != focused_entity)
            .filter_map(|entity| windows.frame(entity).map(|frame| (entity, frame.center())));

    pick_nearest_in_direction(direction, focused_center, candidates)
}

/// Handles the "focus" command, moving focus to a window in a specified direction.
///
/// # Arguments
///
/// * `direction` - The `Direction` to move focus (e.g., `Direction::East`).
/// * `current_window` - The `Entity` of the currently focused `Window`.
/// * `strip` - A reference to the active `LayoutStrip`.
/// * `windows` - A query for all `Window` components.
///
/// # Returns
///
/// `Some(Entity)` with the entity of the newly focused window, otherwise `None`.
pub(crate) fn focus_with_verification(
    entity: Entity,
    focused_entity: Option<Entity>,
    paging: Option<&mut PageGestureState>,
    commands: &mut Commands,
) {
    if focused_entity != Some(entity)
        && let Ok(mut entity_commands) = commands.get_entity(entity)
    {
        if let Some(paging) = paging {
            paging.await_focus(entity);
        }
        entity_commands.try_insert((
            crate::ecs::VerifyFocus::default(),
            Timeout::for_component::<crate::ecs::VerifyFocus>(
                crate::ecs::focus::VERIFY_FOCUS_TIMEOUT,
            ),
        ));
    }
    commands.focus_entity(entity, true);
}

/// A first, last, or numbered column jump lands on the window last focused in
/// that column, not on the column top.
fn active_focus_candidate(
    direction: &Direction,
    focused_entity: Entity,
    active_strip: &LayoutStrip,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    focus_history: &FocusHistory,
) -> Option<Entity> {
    let column_jump = match direction {
        Direction::First => active_strip.first().ok(),
        Direction::Last => active_strip.last().ok(),
        Direction::Nth(index) => active_strip.get(*index).ok(),
        Direction::North | Direction::South | Direction::West | Direction::East => None,
    }
    .map(|column| focus_history.last_focused_in(&column));
    if let Some(target) = column_jump {
        target
    } else if active_strip.contains(focused_entity) {
        get_window_in_direction(direction, focused_entity, active_strip).or_else(|| {
            // At the right edge going East, enter the fullscreen workspaces.
            (matches!(direction, Direction::East)
                && active_strip.right_neighbour(focused_entity).is_none())
            .then(|| {
                workspaces
                    .iter()
                    .find(|(strip, _, fullscreen)| {
                        fullscreen.is_some() && strip.id() != active_strip.id()
                    })
                    .and_then(|(strip, _, _)| strip.get(0).ok().and_then(|col| col.top()))
            })
            .flatten()
        })
    } else {
        match direction {
            Direction::East => active_strip.first().ok().and_then(|col| col.top()),
            Direction::West => active_strip.last().ok().and_then(|col| col.top()),
            _ => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn command_move_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    workspaces: Query<(&LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    focus_history: Res<FocusHistory>,
    mut paging: PagingFocusParams,
    mut commands: Commands,
) {
    let Some((direction, is_page)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Focus(_)))
            .find_map(|op| match op {
                Operation::Focus(direction) => Some((direction.clone(), false)),
                _ => None,
            })
            .or_else(|| {
                let generation = paging.page_state.generation;
                let dispatched =
                    matches!(paging.page_state.in_flight, Some(PageInFlight::Dispatched));
                paging.page_requests.read().find_map(|req| {
                    (dispatched && req.generation == generation)
                        .then(|| (req.direction.clone(), true))
                })
            })
    else {
        return;
    };
    let direction = &direction;
    if is_page {
        paging.page_state.in_flight = None;
    }

    let active_strip = active_display.active_strip();

    // On a fullscreen space, swap to the last column in the workspace.
    if let Some(fullscreen) = active_display.fullscreen()
        && matches!(direction, Direction::West)
    {
        focus_from_fullscreen(
            fullscreen,
            &workspaces,
            &windows,
            is_page,
            &mut paging.page_state,
            &mut commands,
        );
        return;
    }

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };

    if let Some((_, _, Some(Unmanaged::Floating))) = windows.get_managed(focused_entity)
        && !matches!(direction, Direction::Nth(_))
    {
        if let Some(entity) = nearest_float_in_direction(
            direction,
            focused_entity,
            &windows,
            &window_manager,
            active_strip.id(),
            active_display.bounds(),
        ) {
            focus_with_verification(
                entity,
                Some(focused_entity),
                is_page.then_some(&mut *paging.page_state),
                &mut commands,
            );
        }
        return;
    }

    if is_page
        && reveal_clipped_page(
            direction,
            focused_entity,
            &windows,
            &active_display,
            &mut paging,
            &mut commands,
        )
    {
        return;
    }

    let candidate = active_focus_candidate(
        direction,
        focused_entity,
        active_strip,
        &workspaces,
        &focus_history,
    );

    if let Some(entity) = candidate {
        focus_with_verification(
            entity,
            Some(focused_entity),
            is_page.then_some(&mut *paging.page_state),
            &mut commands,
        );
        // Explicitly reshuffle so the target window is brought into view.
        // This avoids a race where focus-follows-mouse leaves skip_reshuffle
        // set, causing the WindowFocused handler to skip the reshuffle.
        commands.reshuffle_around(entity);
    }
}

fn focus_from_fullscreen(
    fullscreen: &NativeFullscreenMarker,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    windows: &Windows,
    is_page: bool,
    page_state: &mut PageGestureState,
    commands: &mut Commands,
) {
    let mut strip = workspaces
        .iter()
        .find_map(|(strip, entity, _)| (entity == fullscreen.layout_strip).then_some(strip));
    if strip.is_none() {
        strip = workspaces
            .iter()
            .find_map(|(strip, _, _)| (strip.id() == fullscreen.workspace_id).then_some(strip));
    }

    if let Some(entity) = strip.and_then(|strip| strip.last().ok().and_then(|col| col.top())) {
        debug!("fullscreen: swap raising {entity}");
        focus_with_verification(
            entity,
            windows.focused().map(|(_, e)| e),
            is_page.then_some(page_state),
            commands,
        );
    }
}

fn command_focus_unmanaged(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FocusUnmanaged))
        .next()
        .is_none()
    {
        return;
    }

    let display_bounds = active_display.bounds();
    let workspace_id = active_display.active_strip().id();
    let visible_floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds);
    let is_visible_float = |entity: Entity| -> bool { visible_floats.contains(&entity) };

    let target = focus_history
        .last_floating(workspace_id)
        .filter(|entity| is_visible_float(*entity))
        .or_else(|| visible_floats.into_iter().next());

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
    }
}

fn command_focus_managed(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FocusManaged))
        .next()
        .is_none()
    {
        return;
    }

    let active_strip = active_display.active_strip();
    let workspace_id = active_strip.id();

    let target = focus_history
        .last_managed(workspace_id)
        .filter(|entity| active_strip.contains(*entity))
        .or_else(|| active_strip.all_columns().into_iter().next());

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
        commands.reshuffle_around(entity);
    }
}

fn command_raise_floating(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::RaiseFloating))
        .next()
        .is_none()
    {
        return;
    }

    let display_bounds = active_display.bounds();
    let workspace_id = active_display.active_strip().id();
    let visible_floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds);
    let is_visible_float = |entity: Entity| -> bool { visible_floats.contains(&entity) };

    let target = focus_history
        .last_floating(workspace_id)
        .filter(|entity| is_visible_float(*entity))
        .or_else(|| visible_floats.first().copied());

    for (_, entity) in windows.iter() {
        if is_visible_float(entity) && Some(entity) != target {
            commands.trigger(RaiseWindow {
                entity,
                with_strip: false,
            });
        }
    }

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
    }
}

/// Focus-and-raise are deliberately coupled here: macOS AX raise can't lift a
/// window above another app's frontmost window, so the target's app must be
/// made frontmost. Other windows in the new top tier are raised within their
/// own apps' stacks as a best-effort.
fn command_toggle_floating_layer(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    mut floating_layers: Query<&mut FloatingLayer>,
    focus_history: Res<FocusHistory>,
    window_manager: Res<WindowManager>,
    windows: Windows,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::ToggleFloatingLayer)
    })
    .next()
    .is_none()
    {
        return;
    }

    let display_bounds = active_display.bounds();
    let active_strip = active_display.active_strip();
    let workspace_id = active_strip.id();

    let floating_front = floating_layers
        .iter_mut()
        .find_map(|mut layer| {
            if layer.workspace_id == workspace_id {
                layer.flip();
                Some(layer.front)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            let layer = FloatingLayer::new(workspace_id);
            commands.spawn((layer, ChildOf(active_display.entity())));
            false
        });

    let visible_floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds);
    let visible_float = |entity: Entity| -> bool {
        visible_floats.contains(&entity) && !active_strip.contains(entity)
    };

    let target = if floating_front {
        focus_history
            .last_floating(workspace_id)
            .filter(|entity| visible_float(*entity))
            .or_else(|| visible_floats.iter().copied().find(|e| visible_float(*e)))
    } else {
        focus_history
            .last_managed(workspace_id)
            .filter(|entity| active_strip.contains(*entity))
            .or_else(|| active_strip.all_columns().into_iter().next())
    };

    if floating_front {
        windows
            .iter()
            .filter_map(|(_, e)| visible_float(e).then_some(e))
            .for_each(|entity| {
                commands.trigger(RaiseWindow {
                    entity,
                    with_strip: false,
                });
            });
    } else if let Some(entity) = target {
        commands.trigger(RaiseWindow {
            entity,
            with_strip: true,
        });
    }

    debug!("floating layer -> front: {floating_front}");
}

#[instrument(level = Level::DEBUG, skip_all)]
fn command_swap_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    let Some(Operation::Swap(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Swap(_))).next()
    else {
        return;
    };

    let active_strip = active_display.active_strip();
    let mut handler = || {
        let (_, current) = windows.focused()?;
        swap_in_strip(active_strip, current, direction)?;
        Some(current)
    };

    // Keep the focused window on-screen, but don't anchor it: if its new
    // layout slot is already visible with the strip where it is, the strip
    // stays put and per-window animation slides the window into the slot.
    // Only when the slot would fall off the edge does the strip scroll —
    // and only by the shortfall.
    if let Some(window) = handler() {
        commands.ensure_visible(window);
    } else {
        debug!(
            "swap {direction:?}: handler returned None (focused={:?}, strip_len={})",
            windows.focused().map(|(_, e)| e),
            active_strip.len()
        );
    }
}

/// Moves `current` in `direction` within `strip`, as `window_swap_*` does.
/// Toward another column, the whole column moves past every column between
/// the two. Within a stack, the window trades places with the item in
/// `direction`. Returns `None`, changing nothing, when nothing lies in
/// `direction`.
fn swap_in_strip(strip: &mut LayoutStrip, current: Entity, direction: &Direction) -> Option<()> {
    let index = strip.index_of(current).ok()?;
    let other_window = get_window_in_direction(direction, current, strip)?;
    let new_index = strip.index_of(other_window).ok()?;
    debug!(
        "swap {direction:?}: current={current} idx={index}, other={other_window} idx={new_index}, strip_len={}",
        strip.len()
    );

    if index == new_index
        && let Some(Column::Stack(stack)) = strip.get_column_mut(index)
    {
        let pos_a = stack.iter().position(|i| i.contains(current))?;
        let pos_b = stack.iter().position(|i| i.contains(other_window))?;
        stack.swap(pos_a, pos_b);
    } else if index < new_index {
        (index..new_index).for_each(|idx| strip.swap(idx, idx + 1));
    } else {
        (new_index..index)
            .rev()
            .for_each(|idx| strip.swap(idx, idx + 1));
    }
    Some(())
}

/// What an edge command does from the window `focused` in `strip`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeStep {
    /// Reach the strip's end column, at this index.
    Column(usize),
    /// Cross to the display beyond the strip's end.
    Display,
}

/// Where an edge command toward `side` goes: to the strip's end column on
/// that side, unless `focused` is already in it or the strip has no column.
/// Then it goes to the display on that side.
fn edge_step(strip: &LayoutStrip, focused: Option<Entity>, side: Side) -> EdgeStep {
    let Some(last) = strip.len().checked_sub(1) else {
        return EdgeStep::Display;
    };
    let edge = match side {
        Side::West => 0,
        Side::East => last,
    };
    match focused.and_then(|entity| strip.index_of(entity).ok()) {
        Some(index) if index == edge => EdgeStep::Display,
        _ => EdgeStep::Column(edge),
    }
}

/// Whether the fresh press of a held edge key crossed to another display.
/// The key's repeats then do nothing, so a hold crosses at most once and stops
/// where it arrived.
#[derive(bevy::ecs::resource::Resource, Default)]
struct HeldEdge {
    crossed: bool,
}

impl HeldEdge {
    /// Whether an edge command with `reach` may take `step`. A fresh press
    /// starts a new hold.
    fn allows(&mut self, reach: Reach, step: EdgeStep) -> bool {
        match reach {
            Reach::Display => {
                self.crossed = step == EdgeStep::Display;
                true
            }
            Reach::Strip => !self.crossed,
        }
    }
}

/// The window display `bounds` shows as active in `strip`: the window that
/// last had focus there when it is visible, otherwise the most visible window.
/// A native tab behind another counts as hidden. Returns the window with its
/// visible part.
fn active_window_on_display(
    strip: &LayoutStrip,
    bounds: IRect,
    focus_history: &FocusHistory,
    frame_of: impl Fn(Entity) -> Option<IRect>,
) -> Option<(Entity, IRect)> {
    let visible = |entity: Entity| {
        if strip
            .tab_group(entity)
            .is_some_and(|tabs| tabs.first() != Some(&entity))
        {
            return None;
        }
        frame_of(entity)
            .map(|frame| bounds.intersect(frame))
            .filter(|visible| !visible.is_empty())
    };
    focus_history
        .last_managed(strip.id())
        .filter(|entity| strip.contains(*entity))
        .and_then(|entity| Some((entity, visible(entity)?)))
        .or_else(|| {
            strip
                .all_windows()
                .into_iter()
                .filter_map(|entity| Some((entity, visible(entity)?)))
                .max_by_key(|(_, visible)| visible.width())
        })
}

/// Records that the user placed this strip's offset by hand, so
/// `reshuffle_layout_strip` keeps it instead of re-deriving one from the
/// focused window's frame. The mark is dropped again as soon as the strip's
/// shape changes, the user swipes, or a window has to be scrolled into view.
fn mark_manual_offset(
    strip_entity: Entity,
    strip: &LayoutStrip,
    windows: &Windows,
    commands: &mut Commands,
) {
    if let Ok(mut entity_commands) = commands.get_entity(strip_entity) {
        entity_commands.try_insert(ManualStripOffset {
            signature: strip_signature(strip, windows),
        });
    }
}

/// Centers the focused window on the active display.
fn command_center_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Center))
        .next()
        .is_none()
    {
        return;
    }

    if let Some((_, entity)) = windows.focused()
        && let Some(size) = windows.size(entity)
        && let Some(mut origin) = windows.origin(entity)
    {
        let center = active_display.bounds().center().x;
        origin.x = center - size.x / 2;

        if active_display.active_strip().contains(entity)
            && let Some(layout_position) = windows.layout_position(entity)
        {
            // Directly reposition the strip (bypasses hidden_ratio check), and
            // record the placement as deliberate so a later reshuffle — a
            // repeated focus event, a return from another display — does not
            // re-derive the offset and undo the centering.
            let strip_position = origin - layout_position.0;
            let strip_entity = active_display.active_strip_entity();
            commands.reposition_entity(strip_entity, strip_position);
            mark_manual_offset(
                strip_entity,
                active_display.active_strip(),
                &windows,
                &mut commands,
            );
        } else {
            commands.reposition_entity(entity, origin);
        }

        if config.mouse_follows_focus() {
            window_manager.warp_mouse(active_display.bounds().center());
        }
    }
}

#[derive(bevy::ecs::resource::Resource, Default)]
struct WidthBatchInterrupted(bool);

fn interrupts_width_run(event: &Event) -> bool {
    let Event::Command { command } = event else {
        return false;
    };
    match command {
        Command::Window(
            Operation::Resize(_) | Operation::ResizeStep(_) | Operation::SetWidth(_),
        )
        | Command::PrintState
        | Command::Lua(_) => false,
        Command::Layout(ops) => ops
            .iter()
            .any(|op| !matches!(op, crate::types::windowset::LayoutOp::SetWidth { .. })),
        _ => true,
    }
}

/// Ends every width run when an interrupting command arrives, or
/// when the user scrolls, swipes, drags, or resizes with the mouse.
fn end_width_runs(
    mut messages: MessageReader<Event>,
    mut inputs: MessageReader<InputEvent>,
    runs: Query<Entity, With<WidthRun>>,
    mut interrupted: ResMut<WidthBatchInterrupted>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let mut command = false;
    for event in messages.read() {
        command |= interrupts_width_run(event);
    }
    let mouse_resize = config.mouse_resize_modifier();
    let mut scrolled = false;
    for InputEvent(event) in inputs.read() {
        scrolled |= match event {
            Event::Swipe { .. } | Event::Scroll { .. } | Event::MouseDragged { .. } => true,
            Event::MouseMoved { modifiers, .. } => {
                mouse_resize.is_some_and(|modifier| modifier.matches(*modifiers))
            }
            _ => false,
        };
    }
    interrupted.0 = command || scrolled;
    if !command && !scrolled {
        return;
    }
    for entity in runs {
        if let Ok(mut cmds) = commands.get_entity(entity) {
            cmds.try_remove::<WidthRun>();
        }
    }
}

fn cancel_width_settlement(trigger: On<Remove, WidthRun>, mut commands: Commands) {
    if let Ok(mut entity) = commands.get_entity(trigger.entity) {
        entity.try_remove::<PendingWidthSettlement>();
    }
}

/// Ends every width run when focus moves to a window.
fn end_width_runs_on_focus(
    _trigger: On<Add, FocusedMarker>,
    runs: Query<Entity, With<WidthRun>>,
    mut commands: Commands,
) {
    for entity in runs {
        if let Ok(mut cmds) = commands.get_entity(entity) {
            cmds.try_remove::<WidthRun>();
        }
    }
}

/// The anchor for a width change: the current run's anchor, otherwise a fresh
/// one from where the column stands.
fn width_anchor(
    run: Option<WidthAnchor>,
    frame: IRect,
    first_at_left: bool,
    last_at_right: bool,
    auto_center: bool,
) -> WidthAnchor {
    if let Some(anchor) = run {
        return anchor;
    }
    if !auto_center && first_at_left {
        WidthAnchor::Left
    } else if !auto_center && last_at_right {
        WidthAnchor::Right
    } else {
        WidthAnchor::Center(frame.center().x)
    }
}

type WidthWork<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static NativeResizeStart>,
        Has<VerifyWindowSize>,
        Has<PendingWidthSettlement>,
    ),
>;

/// Resizes the focused window to the next or previous preset column width, or
/// by one `window_resize_step` of the display width.
///
/// # Arguments
///
/// * `active_display` - A mutable reference to the `Display` resource.
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The `Config` resource.
#[allow(clippy::too_many_lines)]
fn resize_window(
    mut messages: MessageReader<Event>,
    mut ctx: ParamSet<(WindowCtx, Query<&mut Window, Without<FloatingMaximized>>)>,
    engine_work: WidthWork,
    runs: Query<&WidthRun>,
    interrupted: Res<WidthBatchInterrupted>,
    time: Res<Time>,
    mut active_display: ActiveDisplayMut,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(
            op,
            Operation::Resize(_) | Operation::ResizeStep(_) | Operation::SetWidth(_)
        )
    })
    .next() else {
        return;
    };

    let Some(entity) = ctx.p0().windows.focused().map(|(_, entity)| entity) else {
        return;
    };
    // A maximized window is restored first, and the change starts from the
    // width it had before.
    let restored = if active_display.strip().maximized(entity).is_some() {
        let WindowCtx {
            config,
            mut commands,
            ..
        } = ctx.p0();
        let viewport = active_display.actual_bounds(&config);
        let limit = effective_stack_limit(&config, viewport.height());
        restore_tiled(
            entity,
            active_display.active_strip(),
            RestoreTo::Home { limit },
            viewport,
            &mut commands,
        )
    } else {
        None
    };
    let (native_resize, verifying, settling) = engine_work.get(entity).unwrap_or_default();
    // A write still on its way to the app counts as pending engine work: its
    // frame is the one the engine asked for, and reading the app's frame
    // would wait on the app.
    let writing = ctx
        .p1()
        .get(entity)
        .is_ok_and(|window| window.write_pending());
    let pending = ctx.p0().windows.is_resizing(entity) || verifying || settling || writing;
    let held = native_resize.and_then(|start| start.held_size(time.elapsed()));
    let native = (restored.is_none()
        && !matches!(operation, Operation::SetWidth(_))
        && !pending
        && held.is_none())
    .then(|| ctx.p1().get_mut(entity).ok()?.update_frame().ok())
    .flatten();
    let WindowCtx {
        windows,
        config,
        mut commands,
    } = ctx.p0();
    let Some(frame) = windows.frame(entity) else {
        return;
    };
    let frame = restored.map_or(frame, |restored| {
        IRect::from_corners(frame.min, frame.min + restored.size)
    });
    let viewport = active_display.actual_bounds(&config);
    // Pending resizes count, so a quick second press moves on to the next preset.
    let width = if restored.is_some() {
        frame.width()
    } else if pending {
        windows.moving_frame(entity).unwrap_or(frame).width()
    } else {
        held.or(native.map(|native| native.size()))
            .map_or(frame.width(), |size| size.x)
    };
    let current_ratio = f64::from(width) / f64::from(viewport.width());
    let widths = config.preset_column_widths();
    let fallback = *widths.first().unwrap_or(&0.5);
    let cycle = config.window_resize_cycle();
    let next_ratio = match operation {
        Operation::SetWidth(ratio) if ratio.is_finite() && *ratio > 0.0 => *ratio,
        Operation::Resize(ResizeDirection::Grow) => widths
            .iter()
            .copied()
            .find(|&r| r > current_ratio + 0.05)
            .unwrap_or_else(|| {
                if cycle {
                    fallback
                } else {
                    *widths.last().unwrap_or(&fallback)
                }
            }),
        Operation::Resize(ResizeDirection::Shrink) => widths
            .iter()
            .rev()
            .copied()
            .find(|&r| r < current_ratio - 0.05)
            .unwrap_or_else(|| {
                if cycle {
                    *widths.last().unwrap_or(&fallback)
                } else {
                    fallback
                }
            }),
        Operation::ResizeStep(direction) => {
            step_ratio(current_ratio, *direction, config.window_resize_step(), 1.0)
        }
        _ => return,
    };

    let new_width = round_px(next_ratio * f64::from(viewport.width()));
    let size = Size::new(new_width, frame.height());

    let strip = active_display.strip();
    let strip_entity = active_display.active_strip_entity();
    if size.x > 0
        && size.y > 0
        && let Some(column) = strip
            .index_of(entity)
            .ok()
            .and_then(|idx| strip.get(idx).ok())
    {
        for member in column.window_iter() {
            if let Ok(mut cmds) = commands.get_entity(member) {
                cmds.try_remove::<NativeResizeStart>();
            }
        }
    }
    let anchor = width_anchor(
        restored
            .map(|restored| restored.anchor)
            .or_else(|| runs.get(entity).ok().map(|run| run.anchor)),
        frame,
        strip.index_of(entity).ok() == Some(0) && frame.min.x == viewport.min.x,
        strip
            .last()
            .is_ok_and(|column| column.window_iter().any(|window| window == entity))
            && frame.max.x == viewport.max.x,
        config.auto_center(),
    );
    let origin = anchored_origin(anchor, frame, size, viewport);
    commands.reposition_entity(entity, origin);
    if !interrupted.0
        && let Ok(mut cmds) = commands.get_entity(entity)
    {
        cmds.try_insert((
            WidthRun { anchor },
            PendingWidthSettlement::new(new_width, strip_entity, viewport, time.elapsed()),
        ));
    }

    // Resize all windows in the column so stacked siblings share the new width.
    if let Some(Column::Stack(stack)) = strip
        .index_of(entity)
        .ok()
        .and_then(|idx| strip.get(idx).ok())
    {
        for sibling in stack.iter().flat_map(StackItem::window_iter) {
            if sibling != entity
                && let Some(frame) = windows.moving_frame(sibling)
            {
                commands.resize_entity(sibling, frame.size().with_x(new_width));
            }
        }
    }

    commands.resize_entity(entity, size);
    commands.reshuffle_around(entity);
}

/// One step from `ratio` in `direction`, kept between one step and `maximum`.
/// A ratio already outside that range never moves further out of it.
fn step_ratio(ratio: f64, direction: ResizeDirection, step: f64, maximum: f64) -> f64 {
    match direction {
        ResizeDirection::Grow => (ratio + step).min(maximum.max(ratio)),
        ResizeDirection::Shrink => (ratio - step).max(step.min(ratio)),
    }
}

/// Cycles the focused window's height through `preset_stack_heights`, or
/// changes it by one `window_resize_step` of the display height. See
/// [`stepped_heights`] for which windows absorb the difference.
///
/// Only stacks are eligible: `binpack_heights` stretches a column's lone member
/// to the full strip height, so a height written anywhere else is overwritten on
/// the next layout pass.
#[allow(clippy::too_many_lines)]
fn resize_window_vertical(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    minimum_heights: Query<&MinimumHeight>,
    step_partners: Query<&StepPartners>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some((direction, stepped)) = filter_window_operations(&mut messages, |op| {
        matches!(
            op,
            Operation::ResizeVertical(_) | Operation::ResizeVerticalStep(_)
        )
    })
    .find_map(|op| match *op {
        Operation::ResizeVertical(direction) => Some((direction, false)),
        Operation::ResizeVerticalStep(direction) => Some((direction, true)),
        _ => None,
    }) else {
        return;
    };

    let Some((_, entity)) = windows.focused() else {
        return;
    };

    let strip = active_display.active_strip();
    let Some(Column::Stack(items)) = strip
        .index_of(entity)
        .ok()
        .and_then(|index| strip.get(index).ok())
    else {
        return;
    };
    let Some(position) = items.iter().position(|item| item.contains(entity)) else {
        return;
    };
    // Pending resizes count: holding the key down must keep advancing through
    // the presets instead of re-reading a mid-animation height.
    let Some(heights) = items
        .iter()
        .map(|item| {
            item.top()
                .and_then(|entity| windows.moving_frame(entity))
                .map(|frame| frame.height())
        })
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };

    // A height an app is known to refuse is never asked for: the layout
    // would hold the window at its minimum while the resize kept pulling it.
    let floors = items
        .iter()
        .map(|item| {
            item.top()
                .and_then(|entity| minimum_heights.get(entity).ok())
                .map_or(MIN_WINDOW_HEIGHT, |minimum| {
                    minimum.0.max(MIN_WINDOW_HEIGHT)
                })
        })
        .collect::<Vec<_>>();
    if items.len() < 2 || heights.iter().sum::<i32>() < floors.iter().sum::<i32>() {
        return;
    }
    let room = (0..items.len())
        .filter(|index| *index != position)
        .map(|index| (heights[index] - floors[index]).max(0))
        .sum::<i32>();

    let viewport = active_display.actual_bounds(&config);
    let current_ratio = f64::from(heights[position]) / f64::from(viewport.height());
    let presets = config.preset_stack_heights();
    let fallback = *presets.first().unwrap_or(&0.5);
    let cycle = config.window_resize_cycle();
    // The presets are assumed sorted ascending, and the `0.05` dead-band keeps
    // float noise from re-selecting the preset the window is already at.
    let next_ratio = match direction {
        _ if stepped => step_ratio(current_ratio, direction, config.window_resize_step(), 1.0),
        ResizeDirection::Grow => presets
            .iter()
            .copied()
            .find(|&r| r > current_ratio + 0.05)
            .unwrap_or_else(|| {
                if cycle {
                    fallback
                } else {
                    *presets.last().unwrap_or(&fallback)
                }
            }),
        ResizeDirection::Shrink => presets
            .iter()
            .rev()
            .copied()
            .find(|&r| r < current_ratio - 0.05)
            .unwrap_or_else(|| {
                if cycle {
                    *presets.last().unwrap_or(&fallback)
                } else {
                    fallback
                }
            }),
    };
    let new_height = round_px(next_ratio * f64::from(viewport.height()))
        .clamp(floors[position], heights[position] + room);

    // Leaving the column's total untouched is what makes the new height
    // survive `binpack_heights`, which hands the column's last item the
    // remainder.
    let new_heights = stepped_heights(&heights, &floors, position, new_height);
    // A step the window has not confirmed yet, such as while the key is held,
    // still counts: the window may refuse both at once.
    let mut partners = step_partners
        .get(entity)
        .map(|partners| partners.0.clone())
        .unwrap_or_default();
    for (index, item) in items.iter().enumerate() {
        if index != position && new_heights[index] == heights[index] {
            continue;
        }
        if index != position
            && let Some(top) = item.top()
        {
            let moved = new_heights[index] - heights[index];
            add_step_partner(&mut partners, top, moved);
            let mut back = step_partners
                .get(top)
                .map(|partners| partners.0.clone())
                .unwrap_or_default();
            add_step_partner(&mut back, entity, -moved);
            if let Ok(mut entity_commands) = commands.get_entity(top) {
                entity_commands.try_insert(StepPartners(back));
            }
        }
        for entity in item.window_iter() {
            if let Some(size) = windows
                .size(entity)
                .map(|size| size.with_y(new_heights[index]))
            {
                if size.x > 0
                    && size.y > 0
                    && let Ok(mut cmds) = commands.get_entity(entity)
                {
                    cmds.try_remove::<NativeResizeStart>();
                }
                commands.resize_entity(entity, size);
            }
        }
    }
    if !partners.is_empty()
        && let Ok(mut entity_commands) = commands.get_entity(entity)
    {
        entity_commands.try_insert(StepPartners(partners));
    }
}

fn add_step_partner(partners: &mut Vec<(Entity, i32)>, partner: Entity, moved: i32) {
    match partners.iter_mut().find(|(entry, _)| *entry == partner) {
        Some((_, total)) => *total += moved,
        None => partners.push((partner, moved)),
    }
}

/// A stacked column's `heights` once the window at `position` takes
/// `height`. A shrink goes to the nearest window, the one below first. A grow
/// takes from the nearest windows first, each down to its floor in `floors`,
/// so a neighbour at its minimum does not stop it.
fn stepped_heights(heights: &[i32], floors: &[i32], position: usize, height: i32) -> Vec<i32> {
    let nearest = (1..heights.len())
        .flat_map(|distance| [Some(position + distance), position.checked_sub(distance)])
        .flatten()
        .filter(|index| *index < heights.len());
    let mut stepped = heights.to_vec();
    stepped[position] = height;
    let mut change = height - heights[position];
    for index in nearest {
        if change == 0 {
            break;
        }
        let given = if change < 0 {
            change
        } else {
            change.min((heights[index] - floors[index]).max(0))
        };
        stepped[index] -= given;
        change -= given;
    }
    stepped
}

/// Toggles the managed state of the focused window.
/// If the window is currently unmanaged, it becomes managed. If managed, it becomes unmanaged (floating).
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `commands` - Bevy commands to modify entities.
#[allow(clippy::too_many_arguments)]
fn manage_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    apps: Query<&Application>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>, &ChildOf)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    config: Res<Config>,
    mut overrides: ResMut<TileOverrides>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Manage))
        .next()
        .is_none()
    {
        return;
    }

    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    // The engine's focus lags behind macOS while a newly focused window is
    // not managed yet. Toggling then would float a window the user is not
    // looking at, so only the window macOS has focused is toggled.
    let app = windows
        .find_parent(window.id())
        .and_then(|(_, _, parent)| apps.get(parent).ok());
    let focused_by_macos = app.is_some_and(|app| {
        app.is_frontmost() && app.focused_window_id().is_ok_and(|id| id == window.id())
    });
    if !focused_by_macos {
        debug!(
            "not toggling window {}: macOS has focused another window",
            window.id()
        );
        return;
    }
    debug!(
        "window: {} {entity} unmanaged: {}.",
        window.id(),
        unmanaged.is_some()
    );
    let was_unmanaged = unmanaged.is_some();
    // Tiling a window that a floating rule matches overrides the rule for that
    // window alone, until it floats again or closes.
    match app {
        Some(app)
            if matches!(unmanaged, Some(Unmanaged::Floating))
                && WindowProperties::new(app, window, &config).floating() =>
        {
            overrides.remember(window, app);
        }
        _ => overrides.forget(window.id()),
    }
    // A maximized window is restored first, so it floats at the size it had
    // before.
    if !was_unmanaged
        && let Some((mut strip, _, child_of)) = workspaces
            .iter_mut()
            .find(|(strip, _, _)| strip.maximized(entity).is_some())
        && let Ok((display, dock)) = displays.get(child_of.parent())
    {
        let viewport = display.actual_display_bounds(dock, &config);
        restore_tiled(
            entity,
            &mut strip,
            RestoreTo::Float,
            viewport,
            &mut commands,
        );
    }
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        if was_unmanaged {
            entity_commands.try_remove::<Unmanaged>();
        } else {
            entity_commands.try_insert(Unmanaged::Floating);
        }
    }

    // Going floating -> managed only flips the component. Nothing else in
    // the pipeline reinserts the window into a strip, so if it had been
    // stripped of membership (spawn-floating path in window_unmanaged_trigger
    // strip.removes; orphan rescue in find_orphaned_workspaces despawns the
    // strip) the toggle is invisible — the window stays where it floated
    // and the user thinks the keybind is broken. Append to the active
    // strip and reshuffle so the layout pipeline tiles it.
    if was_unmanaged
        && !workspaces
            .iter()
            .any(|(strip, _, _)| strip.contains(entity))
        && let Some(mut strip) = workspaces
            .iter_mut()
            .find_map(|(strip, active, _)| active.then_some(strip))
    {
        strip.append(entity);
        commands.reshuffle_around(entity);
    }
}

/// Copies a `[windows]` configuration rule for the focused window to the
/// clipboard, so the user can paste a working starting point instead of
/// guessing the app's bundle id and transcribing the title into a regex.
fn copy_window_rule(
    mut messages: MessageReader<Event>,
    windows: Windows,
    apps: Query<&Application>,
    dialect: Res<SnippetDialect>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::CopyRule))
        .next()
        .is_none()
    {
        return;
    }

    let Some((window, app)) = windows
        .focused()
        .and_then(|(window, _)| windows.find_parent(window.id()))
        .and_then(|(window, _, app_entity)| Some((window, apps.get(app_entity).ok()?)))
    else {
        debug!("no focused window to copy a rule for");
        return;
    };

    let bundle_id = app.bundle_id().unwrap_or_default();
    let title = window.title().unwrap_or_default();
    let role = window.role().unwrap_or_default();
    let subrole = window.subrole().unwrap_or_default();
    let snippet = window_rule_snippet(
        *dialect,
        &RuleSubject {
            app_name: app.name(),
            bundle_id: &bundle_id,
            title: &title,
            role: &role,
            subrole: &subrole,
        },
    );

    if crate::pasteboard::copy_to_clipboard(&snippet) {
        info!("copied window rule for {}", app.name());
        commands.flash_message("Window rule copied".to_owned(), 1.5);
    } else {
        error!("unable to copy the window rule to the clipboard");
    }
}

/// Whether `operation` moves the focused window `entity` to a display, with
/// the focus rule and the direction of that display, `None` for the next one.
/// An edge move away from its strip's end takes the column to that end
/// instead, and returns `None`, as for anything else that crosses no display.
fn display_move(
    operation: Operation,
    entity: Entity,
    moves: &mut DisplayMoves,
    held: &mut HeldEdge,
    commands: &mut Commands,
) -> Option<(MoveFocus, Option<Direction>)> {
    match operation {
        Operation::ToNextDisplay(move_focus) => Some((move_focus, None)),
        Operation::MoveToDisplay(direction) => Some((MoveFocus::Follow, Some(direction))),
        Operation::MoveEdge(side, reach) => {
            let (_, mut strip, _, _, _) = moves
                .strips
                .iter_mut()
                .find(|(_, strip, _, _, _)| strip.contains(entity))?;
            let step = edge_step(&strip, Some(entity), side);
            if !held.allows(reach, step) {
                return None;
            }
            if let EdgeStep::Column(_) = step {
                if swap_in_strip(&mut strip, entity, &side.end()).is_some() {
                    commands.ensure_visible(entity);
                }
                return None;
            }
            (reach == Reach::Display).then_some((MoveFocus::Follow, Some(side.direction())))
        }
        _ => None,
    }
}

/// The gap a column arriving in `strip`, shown on display `bounds`, takes:
/// right after the active column, the one `focus_display` would focus. An
/// empty strip takes it as its first column. `None` puts it after the last
/// column, when no column is active.
fn arrival_index(
    strip: &LayoutStrip,
    bounds: IRect,
    focus_history: &FocusHistory,
    frame_of: impl Fn(Entity) -> Option<IRect>,
) -> Option<usize> {
    if strip.len() == 0 {
        return Some(0);
    }
    let (active, _) = active_window_on_display(strip, bounds, focus_history, frame_of)?;
    strip.index_of(active).ok().map(|index| index + 1)
}

/// Moves the focused window's whole column to another display: the next one,
/// or the one in a compass direction. An edge move first takes the column to
/// its strip's end on that side, and crosses only from there. The column
/// lands right after the destination's active column, the window
/// `focus_display` would focus there.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn to_next_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut moves: DisplayMoves,
    focus_history: Res<FocusHistory>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut native: ResMut<crate::ecs::native_membership::NativeMembership>,
    mut held: ResMut<HeldEdge>,
    mut commands: Commands,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(
            op,
            Operation::ToNextDisplay(_) | Operation::MoveToDisplay(_) | Operation::MoveEdge(..)
        )
    })
    .next()
    .cloned() else {
        return;
    };

    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    if unmanaged.is_some() {
        return;
    }

    let Some((move_focus, direction)) =
        display_move(operation, entity, &mut moves, &mut held, &mut commands)
    else {
        return;
    };

    let Some(source_bounds) = moves.active_display().map(Display::bounds) else {
        return;
    };
    let target = match &direction {
        None => moves.other_displays().next(),
        Some(direction) => display_in_direction(
            direction,
            source_bounds,
            moves
                .other_displays()
                .map(|display| (display, display.bounds())),
        ),
    };
    let Some((target_id, target_bounds)) = target.map(|display| (display.id(), display.bounds()))
    else {
        debug!("no display to move window to ({direction:?}).");
        return;
    };
    let Some(size) = windows.size(entity) else {
        return;
    };
    // The destination's active column is chosen before anything moves.
    let shown = moves
        .shown_strip(target_id)
        .and_then(|strip| moves.strips.get(strip).ok())
        .map(|(_, strip, _, _, _)| strip);
    if shown.is_none()
        && moves
            .strips
            .iter()
            .find_map(|(_, strip, _, _, _)| strip.get(strip.index_of(entity).ok()?).ok())
            .is_some_and(|column| matches!(column, Column::Stack(_)))
    {
        commands.flash_message(
            "Stack not moved: the other display shows no window strip".to_owned(),
            2.0,
        );
        return;
    }
    let index = shown.and_then(|strip| {
        arrival_index(strip, target_bounds, &focus_history, |entity| {
            windows.frame(entity)
        })
    });
    let window_id = window.id();
    let Some(arrival) = moves.move_column_to_display(
        entity,
        target_id,
        index,
        size,
        |entity| windows.frame(entity),
        &mut commands,
    ) else {
        debug!("window {window_id} ({entity}) can not move to display {target_id}.");
        return;
    };
    debug!("moved the column of window (id {window_id}, {entity}) to display {target_id}.");

    let bounds = arrival.display_bounds;
    let origin = bounds.min.with_x(bounds.center().x - size.x / 2);
    if arrival.strip.is_none() {
        for member in &arrival.members {
            commands.reposition_entity(*member, origin);
        }
    } else {
        commands.reposition_entity(entity, origin);
    }
    native.placed(arrival.members, arrival.maximized);
    if matches!(move_focus, MoveFocus::Follow) && config.mouse_follows_focus() {
        window_manager.warp_mouse(bounds.center());
    }
    if matches!(move_focus, MoveFocus::Stay)
        && let Some(neighbour) = arrival.left_behind
    {
        commands.focus_entity(neighbour, false);
    }
    if let Some(strip) = arrival.strip {
        commands.place_after_arrival(entity, strip);
    }
}

/// Focuses the display in a compass direction from the active one: the window
/// that last had focus on it when that window is visible, otherwise its most
/// visible window. A display without one gets its desktop focused instead, so
/// it becomes the active display. The pointer moves only with
/// `mouse_follows_focus`.
///
/// An edge focus first focuses its strip's end column on that side, as
/// `window_focus_first` and `_last` do, and crosses only from there. From a
/// window outside the strip, such as a floating one, it focuses the end column.
#[allow(clippy::too_many_arguments)]
fn focus_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    layout_strips: Query<&LayoutStrip, With<SelectedVirtualMarker>>,
    focus_history: Res<FocusHistory>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut held: ResMut<HeldEdge>,
    mut commands: Commands,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::FocusDisplay(_) | Operation::FocusEdge(..))
    })
    .next()
    .cloned() else {
        return;
    };
    let direction = match operation {
        Operation::FocusDisplay(direction) => direction,
        Operation::FocusEdge(side, reach) => {
            let strip = active_display.active_strip();
            let focused = windows.focused().map(|(_, entity)| entity);
            let step = edge_step(strip, focused, side);
            if !held.allows(reach, step) {
                return;
            }
            if let EdgeStep::Column(index) = step {
                if let Some(entity) = strip
                    .get(index)
                    .ok()
                    .and_then(|column| focus_history.last_focused_in(&column))
                {
                    focus_with_verification(entity, focused, None, &mut commands);
                    commands.reshuffle_around(entity);
                }
                return;
            }
            if reach == Reach::Strip {
                return;
            }
            side.direction()
        }
        _ => return,
    };

    let Some(target) = display_in_direction(
        &direction,
        active_display.bounds(),
        active_display
            .other()
            .map(|display| (display, display.bounds())),
    ) else {
        debug!("no display {direction:?} of the active one.");
        return;
    };
    let target_bounds = target.bounds();

    let strip = window_manager
        .active_display_space(target.id())
        .ok()
        .and_then(|id| layout_strips.iter().find(|strip| strip.id() == id));
    if let Some((entity, frame)) = strip.and_then(|strip| {
        active_window_on_display(strip, target_bounds, &focus_history, |entity| {
            windows.frame(entity)
        })
    }) {
        if config.mouse_follows_focus() {
            window_manager.warp_mouse(frame.center());
        }
        commands.focus_entity(entity, true);
        return;
    }
    if let Err(err) = window_manager.focus_desktop(target_bounds) {
        warn!(
            "can not focus the desktop of display {}: {err}",
            target.id()
        );
    }
    if config.mouse_follows_focus() {
        window_manager.warp_mouse(target_bounds.center());
    }
}

/// Moves the mouse pointer to the next available display.
#[instrument(level = Level::DEBUG, skip_all)]
fn mouse_to_next_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    layout_strips: Query<(&LayoutStrip, Entity, Has<SelectedVirtualMarker>)>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Mouse(MouseMove::ToNextDisplay),
            }
        )
    }) {
        return;
    }

    let Some(cursor_position) = window_manager.cursor_position().map(origin_from) else {
        return;
    };
    let Some(other) = displays
        .into_iter()
        .find(|display| !display.bounds().contains(cursor_position))
    else {
        debug!("no other display to move mouse to.");
        return;
    };
    let Some((other_strip, _)) = window_manager
        .active_display_space(other.id())
        .ok()
        .and_then(|id| {
            layout_strips
                .iter()
                .filter(|(strip, _, _)| strip.id() == id)
                .min_by_key(|(strip, _, selected)| (!*selected, strip.virtual_index))
                .map(|(strip, entity, _)| (strip, entity))
        })
    else {
        return;
    };

    let visible_width = |frame: IRect| other.bounds().intersect(frame).width();
    let Some((frame, entity)) = other_strip
        .all_windows()
        .iter()
        .filter_map(|entity| windows.frame(*entity).zip(Some(*entity)))
        .max_by(|left, right| {
            if visible_width(left.0) < visible_width(right.0) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        })
    else {
        debug!("no suitable windows on the other display to move the mouse.");
        window_manager.warp_mouse(other.bounds().center());
        return;
    };

    let visible_frame = other.bounds().intersect(frame);
    debug!("warping mouse to {visible_frame:?}",);
    window_manager.warp_mouse(visible_frame.center());

    commands.focus_entity(entity, true);
}

/// Distributes heights equally among all windows in the currently focused stack.
fn equalize_column(
    mut messages: MessageReader<Event>,
    current_focus: Single<(&Window, Entity), With<FocusedMarker>>,
    windows: Windows,
    active_display: ActiveDisplay,
    minimum_heights: Query<&MinimumHeight>,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Equalize))
        .next()
        .is_none()
    {
        return;
    }

    let (_, entity) = *current_focus;
    let active_strip = active_display.active_strip();
    let Ok(column) = active_strip
        .index_of(entity)
        .and_then(|index| active_strip.get(index))
    else {
        return;
    };

    if let Column::Stack(stack) = column {
        let total = active_display.actual_bounds(&config).height();
        let Ok(count) = i32::try_from(stack.len()) else {
            return;
        };
        let mut heights = vec![total / count; stack.len()];
        // Windows with a known minimum keep it, and the rest share what is
        // left, so no resize asks for a height the app refuses.
        let minimums = stack
            .iter()
            .map(|item| {
                item.top()
                    .and_then(|entity| minimum_heights.get(entity).ok())
                    .map_or(0, |minimum| minimum.0)
            })
            .collect::<Vec<_>>();
        hold_minimum_heights(&mut heights, &minimums, total);

        for (item, height) in stack.iter().zip(heights) {
            for entity in item.window_iter() {
                if let Some(size) = windows.size(entity).map(|size| size.with_y(height)) {
                    if size.x > 0
                        && size.y > 0
                        && let Ok(mut cmds) = commands.get_entity(entity)
                    {
                        cmds.try_remove::<NativeResizeStart>();
                    }
                    commands.resize_entity(entity, size);
                }
            }
        }
    }
}

/// Makes all columns in the active strip the same width as the focused window.
fn balance_strip(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Balance))
        .next()
        .is_none()
    {
        return;
    }

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };
    let strip = active_display.active_strip();
    // A maximized window counts with the width it returns to.
    let focused_width = match strip.maximized(focused_entity) {
        Some(before) => Some(
            before
                .width()
                .of_viewport(active_display.actual_bounds(&config).width()),
        ),
        None => windows.size(focused_entity).map(|s| s.x),
    };
    let Some(focused_width) = focused_width else {
        return;
    };

    for column in strip.columns() {
        if matches!(column, Column::Fullscren(_) | Column::Maximized(_)) {
            continue;
        }

        for entity in column.window_iter() {
            if let Some(size) = windows.size(entity).map(|size| size.with_x(focused_width)) {
                if size.x > 0
                    && size.y > 0
                    && let Ok(mut cmds) = commands.get_entity(entity)
                {
                    cmds.try_remove::<NativeResizeStart>();
                }
                commands.resize_entity(entity, size);
            }
        }
    }

    commands.reshuffle_around(focused_entity);
}

/// Slides the strip so the focused window is fully visible, snapping to the
/// nearest edge: left-aligned when the window overflows left, right-aligned
/// when it overflows right. No resize — the window keeps its current size.
/// Bypasses the lazy-viewport check since the user explicitly asked to reveal.
fn snap_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Snap))
        .next()
        .is_none()
    {
        return;
    }

    let Some((_, entity)) = windows.focused() else {
        return;
    };
    let Some(strip_position) = snap_strip_position(entity, &windows, &active_display, &config)
    else {
        return;
    };
    reposition_strip_to_snap(strip_position, &active_display, &windows, &mut commands);
}

fn snap_strip_position(
    entity: Entity,
    windows: &Windows,
    active_display: &ActiveDisplay,
    config: &Config,
) -> Option<Origin> {
    let layout_position = windows.layout_position(entity)?;
    let mut frame = windows.moving_frame(entity)?;

    let display_bounds = active_display.actual_bounds(config);

    // Clamp the frame into the display and reposition the *strip* (not the
    // window) so the layout stays consistent.
    let size = frame.size();
    frame.min = clamp_origin_to_viewport(frame.min, size, display_bounds);
    Some(frame.min - layout_position.0)
}

/// A page gesture reveals a mouse-focused window that is clipped on the side
/// being swiped toward before advancing focus to the next window.
fn reveal_clipped_page(
    direction: &Direction,
    focused_entity: Entity,
    windows: &Windows,
    active_display: &ActiveDisplay,
    paging: &mut PagingFocusParams,
    commands: &mut Commands,
) -> bool {
    if !active_display.active_strip().contains(focused_entity) {
        return false;
    }
    let Some(frame) = windows.moving_frame(focused_entity) else {
        return false;
    };
    let bounds = active_display.actual_bounds(&paging.config);
    let clipped_on_swipe_side = match direction {
        Direction::West => frame.min.x < bounds.min.x,
        Direction::East => frame.max.x > bounds.max.x,
        _ => false,
    };
    if !clipped_on_swipe_side {
        return false;
    }

    let Some(target) = snap_strip_position(focused_entity, windows, active_display, &paging.config)
    else {
        return false;
    };
    let Ok((position, reposition)) = paging
        .strip_positions
        .get(active_display.active_strip_entity())
    else {
        return false;
    };
    if target == reposition.map_or(position.0, |pending| pending.0) {
        return false;
    }

    reposition_strip_to_snap(target, active_display, windows, commands);
    if paging.config.swipe_gesture_haptics() {
        paging.haptics.write(PageHapticRequest);
    }
    true
}

fn reposition_strip_to_snap(
    strip_position: Origin,
    active_display: &ActiveDisplay,
    windows: &Windows,
    commands: &mut Commands,
) {
    let strip_entity = active_display.active_strip_entity();
    commands.reposition_entity(strip_entity, strip_position);
    mark_manual_offset(
        strip_entity,
        active_display.active_strip(),
        windows,
        commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all)]
pub fn stack_windows_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::Stack(_) | Operation::Unstack(_))
    })
    .next()
    .cloned() else {
        return;
    };

    if let Some((_, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
        && unmanaged.is_none()
    {
        let viewport = active_display.actual_bounds(&config);
        let limit = effective_stack_limit(&config, viewport.height());
        let strip = active_display.active_strip();
        // A maximized window, and a maximized column it would join, are
        // restored where they stand first, so the window goes where the
        // layout the user sees says.
        let target = match operation {
            Operation::Stack(Side::West) => strip.left_neighbour(entity),
            Operation::Stack(Side::East) => strip.right_neighbour(entity),
            _ => None,
        };
        for window in std::iter::once(entity).chain(target) {
            restore_tiled(window, strip, RestoreTo::Here, viewport, &mut commands);
        }
        _ = match operation {
            Operation::Stack(side) => strip.stack_within_limit(entity, side, limit),
            Operation::Unstack(side) => strip.unstack(entity, side),
            _ => Ok(()),
        };

        // Stacking/unstacking moves the focused window to a new column slot
        // (into a neighbouring column, or out to its own column).
        // Reshuffle around it so it is brought fully back into view; the
        // edge-clamp in reshuffle_layout_strip keeps the strip pinned so the
        // leftmost window touches the left edge and the rightmost the right.
        commands.reshuffle_around(entity);
    }
}

/// Toggles the focused window's stack between a normal split display and a
/// tabbed display (one window visible at a time, sharing the full column
/// rect — cycle with the existing `Focus` North/South directions). A no-op
/// on anything that isn't a multi-window `Column::Stack`.
#[instrument(level = Level::DEBUG, skip_all)]
pub fn toggle_tabbed_display_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::ToggleTabbedDisplay)
    })
    .next()
    .is_none()
    {
        return;
    }

    if let Some((_, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
        && unmanaged.is_none()
    {
        let strip = active_display.active_strip();
        // `None` means the toggle wasn't applicable (not in a multi-window
        // Stack) — stay silent rather than reporting it the same way as a
        // real "turned tabs off", which previously made repeated presses on
        // a non-stacked window look like a stuck toggle.
        if let Some(now_tabbed) = strip.toggle_tabbed_display(entity) {
            commands.reshuffle_around(entity);
            commands.flash_message(
                if now_tabbed { "Tabs on" } else { "Tabs off" }.to_string(),
                1.0,
            );
        }
    }
}

/// Dispatches a command based on the `CommandTrigger` event.
/// This function is a Bevy system that reacts to `CommandTrigger` events and executes the corresponding window manager command.
///
/// # Arguments
///
/// * `trigger` - The `On<CommandTrigger>` event trigger containing the command to process.
/// * `windows` - A query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` resource.
/// * `window_manager` - The `WindowManager` resource for interacting with the window management logic.
/// * `commands` - Bevy commands to trigger events and modify entities.
/// * `config` - The `Config` resource, containing application settings.
#[instrument(level = Level::DEBUG, skip_all)]
pub fn command_quit_handler(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
) {
    if messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Quit
            }
        )
    }) {
        _ = window_manager.quit();
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
pub fn command_restart_handler(mut messages: MessageReader<Event>) {
    if messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Restart
            }
        )
    }) && let Err(err) = crate::platform::service::Service::request_restart()
    {
        error!("failed to restart service: {err}");
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
fn print_internal_state_handler(
    mut messages: MessageReader<Event>,
    focused: Query<(&Window, Entity), With<FocusedMarker>>,
    windows: Query<(&Window, Entity, &ChildOf, Option<&Unmanaged>)>,
    apps: Query<&Application>,
    workspaces: StripsWithVisibility,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::PrintState,
            }
        )
    }) {
        return;
    }

    let focused = focused.single().ok();
    let print_window = |(window, entity, child, unmanaged): (
        &Window,
        Entity,
        &ChildOf,
        Option<_>,
    )| {
        let bundle_id = apps
            .get(child.parent())
            .ok()
            .and_then(|app| app.bundle_id())
            .unwrap_or_default();
        format!(
            "\tid: {}, {entity}, {}:{}, {}x{}{}{}, bundle: {}, role: {}, subrole: {}, title: '{:.70}'",
            window.id(),
            window.frame().min.x,
            window.frame().min.y,
            window.frame().width(),
            window.frame().height(),
            if focused.is_some_and(|(_, focus)| focus == entity) {
                ", focused"
            } else {
                ""
            },
            unmanaged.map(|m| format!(", {m:?}")).unwrap_or_default(),
            bundle_id,
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
            window.title().unwrap_or_default()
        )
    };

    let mut seen = EntityHashSet::new();

    for (display, display_entity, active) in displays {
        for (_, strip, strip_entity, active_workspace, selected) in workspaces
            .iter()
            .filter(|child| child.0.parent() == display_entity)
        {
            let windows = strip
                .all_windows()
                .iter()
                .filter_map(|entity| windows.get(*entity).ok())
                .inspect(|(_, entity, _, _)| {
                    seen.insert(*entity);
                })
                .map(print_window)
                .collect::<Vec<_>>();

            let display_id = display.id();
            info!(
                "Display {display_id}{}, workspace id {} ({strip_entity}){}{}: {strip}:\n{}",
                if active { ", active" } else { "" },
                strip.id(),
                if active_workspace { ", active" } else { "" },
                if selected { ", selected" } else { "" },
                windows.join("\n")
            );
        }
    }

    let remaining = windows
        .iter()
        .filter(|entity| !seen.contains(&entity.1))
        .map(print_window)
        .collect::<Vec<_>>();
    info!("Remaining:\n{}", remaining.join("\n"));

    if let Some(pool) = bevy::tasks::ComputeTaskPool::try_get() {
        info!("Running with {} threads", pool.thread_num());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn setup_world_with_layout() -> (World, LayoutStrip, Vec<Entity>) {
        let mut world = World::new();
        // e0, e1 are stacked, e2 is single, e3 is single
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]); // This will become a stack
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);
        strip.stack(entities[1], Side::West).unwrap(); // Stack e1 onto e0

        (world, strip, entities)
    }

    #[test]
    fn edge_step_reaches_the_end_column_before_the_display() {
        let (mut world, strip, entities) = setup_world_with_layout();
        let (e0, e1, e2, e3) = (entities[0], entities[1], entities[2], entities[3]);

        // [e0, e1] is the first column, e3 the last.
        assert_eq!(edge_step(&strip, Some(e2), Side::West), EdgeStep::Column(0));
        assert_eq!(edge_step(&strip, Some(e2), Side::East), EdgeStep::Column(2));
        assert_eq!(edge_step(&strip, Some(e1), Side::West), EdgeStep::Display);
        assert_eq!(edge_step(&strip, Some(e0), Side::West), EdgeStep::Display);
        assert_eq!(edge_step(&strip, Some(e3), Side::East), EdgeStep::Display);
        assert_eq!(edge_step(&strip, Some(e1), Side::East), EdgeStep::Column(2));
        // A window outside the strip, or none, reaches the end column first.
        let floating = world.spawn_empty().id();
        assert_eq!(
            edge_step(&strip, Some(floating), Side::West),
            EdgeStep::Column(0)
        );
        assert_eq!(edge_step(&strip, None, Side::East), EdgeStep::Column(2));
        // An empty strip has no column to reach.
        let empty = LayoutStrip::default();
        assert_eq!(edge_step(&empty, Some(e0), Side::West), EdgeStep::Display);
        assert_eq!(edge_step(&empty, None, Side::East), EdgeStep::Display);
    }

    #[test]
    fn swap_to_an_end_moves_the_whole_stack() {
        let (_world, mut strip, entities) = setup_world_with_layout();
        let (e0, e1, e3) = (entities[0], entities[1], entities[3]);

        assert_eq!(swap_in_strip(&mut strip, e1, &Direction::Last), Some(()));
        assert_eq!(strip.index_of(e0).ok(), Some(2));
        assert_eq!(strip.index_of(e1).ok(), Some(2));
        assert_eq!(
            strip.first().ok().and_then(|column| column.top()),
            Some(entities[2])
        );
        assert!(matches!(strip.last(), Ok(Column::Stack(items)) if items.len() == 2));
        assert_eq!(strip.index_of(e3).ok(), Some(1));
    }

    #[test]
    fn test_get_window_in_direction_simple() {
        let (_world, strip, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e2 = entities[2];
        let e3 = entities[3];
        let east = Direction::East;
        let west = Direction::West;

        // From e2, east should be e3, west should be e0 (top of stack)
        assert_eq!(get_window_in_direction(&east, e2, &strip), Some(e3));
        assert_eq!(get_window_in_direction(&west, e2, &strip), Some(e0));

        // From e3, west is e2, east is None
        assert_eq!(get_window_in_direction(&west, e3, &strip), Some(e2));
        assert_eq!(get_window_in_direction(&east, e3, &strip), None);

        // From e0, east is e2, west is None
        assert_eq!(get_window_in_direction(&east, e0, &strip), Some(e2));
        assert_eq!(get_window_in_direction(&west, e0, &strip), None);
    }

    #[test]
    fn test_get_window_in_direction_stacked() {
        let (_world, strip, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e1 = entities[1];
        let north = Direction::North;
        let south = Direction::South;

        // From e0 (top of stack), south should be e1, north is None
        assert_eq!(get_window_in_direction(&south, e0, &strip), Some(e1));
        assert_eq!(get_window_in_direction(&north, e0, &strip), None);

        // From e1 (bottom of stack), north should be e0, south is None
        assert_eq!(get_window_in_direction(&north, e1, &strip), Some(e0));
        assert_eq!(get_window_in_direction(&south, e1, &strip), None);
    }

    #[test]
    fn test_get_window_in_direction_adjacent_stacks() {
        // Layout: [Stack(e0, e1), Stack(e2, e3)]
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);
        strip.stack(entities[1], Side::West).unwrap(); // Stack e1 onto e0: [Stack(e0, e1), e2, e3]
        strip.stack(entities[3], Side::West).unwrap(); // Stack e3 onto e2: [Stack(e0, e1), Stack(e2, e3)]

        let east = Direction::East;
        let west = Direction::West;

        // From e0 (top-left), east should go to e2 (top-right)
        assert_eq!(
            get_window_in_direction(&east, entities[0], &strip),
            Some(entities[2])
        );
        // From e1 (bottom-left), east should go to e3 (bottom-right)
        assert_eq!(
            get_window_in_direction(&east, entities[1], &strip),
            Some(entities[3])
        );
        // From e2 (top-right), west should go to e0 (top-left)
        assert_eq!(
            get_window_in_direction(&west, entities[2], &strip),
            Some(entities[0])
        );
        // From e3 (bottom-right), west should go to e1 (bottom-left)
        assert_eq!(
            get_window_in_direction(&west, entities[3], &strip),
            Some(entities[1])
        );
    }

    #[test]
    fn display_in_direction_follows_a_horizontal_arrangement() {
        let built_in = IRect::new(0, 0, 1512, 982);
        let right = IRect::new(1512, -200, 1512 + 2560, 1240);
        let displays = [(2, right)];

        assert_eq!(
            display_in_direction(&Direction::East, built_in, displays),
            Some(2)
        );
        assert_eq!(
            display_in_direction(&Direction::West, right, [(1, built_in)]),
            Some(1)
        );
        for direction in [Direction::West, Direction::North, Direction::South] {
            assert_eq!(display_in_direction(&direction, built_in, displays), None);
        }
    }

    #[test]
    fn display_in_direction_follows_a_vertical_arrangement() {
        let built_in = IRect::new(0, 0, 1512, 982);
        // A wider display above, offset to the left.
        let above = IRect::new(-500, -1440, 2060, 0);
        let displays = [(2, above)];

        assert_eq!(
            display_in_direction(&Direction::North, built_in, displays),
            Some(2)
        );
        assert_eq!(
            display_in_direction(&Direction::South, above, [(1, built_in)]),
            Some(1)
        );
        for direction in [Direction::South, Direction::West, Direction::East] {
            assert_eq!(display_in_direction(&direction, built_in, displays), None);
        }
    }

    #[test]
    fn display_in_direction_prefers_an_overlapping_display_then_the_nearest() {
        let center = IRect::new(0, 0, 1000, 800);
        let beside = IRect::new(3000, 0, 4000, 800);
        let diagonal = IRect::new(1000, -900, 2000, -100);
        let near = IRect::new(1000, 100, 2000, 900);

        assert_eq!(
            display_in_direction(&Direction::East, center, [(1, diagonal), (2, beside)]),
            Some(2)
        );
        assert_eq!(
            display_in_direction(&Direction::East, center, [(2, beside), (3, near)]),
            Some(3)
        );
        assert_eq!(
            display_in_direction(&Direction::First, center, [(3, near)]),
            None
        );
    }

    #[test]
    fn pick_nearest_in_direction_east_picks_closer() {
        let mut world = World::new();
        let near = world.spawn(()).id();
        let far = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        let candidates = vec![
            (near, bevy::math::IVec2::new(10, 0)),
            (far, bevy::math::IVec2::new(50, 0)),
        ];
        assert_eq!(
            pick_nearest_in_direction(&Direction::East, focused, candidates),
            Some(near),
        );
    }

    #[test]
    fn pick_nearest_in_direction_respects_cone() {
        let mut world = World::new();
        let candidate = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        // y/x ratio > 1 → outside the 45° east cone.
        let candidates = vec![(candidate, bevy::math::IVec2::new(10, 20))];
        assert_eq!(
            pick_nearest_in_direction(&Direction::East, focused, candidates),
            None,
        );
    }

    #[test]
    fn pick_nearest_in_direction_ignores_wrong_side() {
        let mut world = World::new();
        let west_one = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        let candidates = vec![(west_one, bevy::math::IVec2::new(-10, 0))];
        assert_eq!(
            pick_nearest_in_direction(&Direction::East, focused, candidates),
            None,
        );
    }

    #[test]
    fn pick_nearest_in_direction_first_last_return_none() {
        let mut world = World::new();
        let any = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        let candidates = vec![(any, bevy::math::IVec2::new(10, 0))];
        assert_eq!(
            pick_nearest_in_direction(&Direction::First, focused, candidates.clone()),
            None,
        );
        assert_eq!(
            pick_nearest_in_direction(&Direction::Last, focused, candidates),
            None,
        );
    }
}
