use super::systems::unpadded;
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut, Mut, Ref};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::system::{
    Commands, NonSendMut, ParamSet, Populated, Query, Res, ResMut, Single, SystemParam,
};
use bevy::math::IRect;
use bevy::time::{Time, Timer, TimerMode};
use notify::event::{DataChange, MetadataKind, ModifyKind};
use notify::{EventKind, Watcher};
use std::cmp::Ordering;
use std::time::Duration;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{
    ActiveDisplayMarker, AwaitingHandle, BProcess, FocusedMarker, FreshMarker,
    MissionControlActive, PreviousManagedStrip, RetryFrontSwitch, SpawnWindowTrigger,
    StrayFocusEvent, SystemTheme, Timeout, Unmanaged,
};
use crate::config::Config;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::{Column, ExposeFocusedSurvivorAfterRemoval, LayoutStrip, anchored_origin};
use crate::ecs::params::{ActiveDisplay, GlobalState, WindowCtx, Windows};
use crate::ecs::state::PaneruState;
use crate::ecs::tile_overrides::TileOverrides;
use crate::ecs::workspace::RestoreFocusMarker;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, Initializing, LaunchWindows, LayoutPosition,
    MinimumHeight, PendingWidthSettlement, Position, PreLaunchFrame, RepositionMarker,
    ResizeMarker, RestoreWindowState, Scrolling, SendMessageTrigger, SpawnCommandsExt,
    StepPartners, VerifyWindowPosition, VerifyWindowSize, WidthLimits, WidthRatio, WidthRun,
    WidthSettlementPhase, WindowProperties,
};
use crate::events::{DestroySource, Event};
use crate::manager::{
    Application, Display, Origin, Process, Size, Window, WindowManager, WindowPadding,
    forget_enhanced_ui_state,
};
use crate::platform::WinID;
use crate::util::{round_px, symlink_target};

/// The display currently in front, paired with the Dock's edge — together they
/// give the usable viewport a window has to be fitted into.
type ActiveDisplayViewport<'w, 's> =
    Single<'w, 's, (&'static Display, Option<&'static DockPosition>), With<ActiveDisplayMarker>>;

type AwaitingWindow = (
    Entity,
    &'static mut Window,
    &'static ChildOf,
    &'static mut AwaitingHandle,
    Option<(&'static Position, &'static Bounds)>,
    Has<Unmanaged>,
);

type SizeCheckReady = (
    Without<ResizeMarker>,
    Without<RepositionMarker>,
    Without<AwaitingHandle>,
);

type AddedWindowDefaults<'w, 's> = Populated<
    'w,
    's,
    (
        Entity,
        &'static mut Window,
        &'static mut Position,
        &'static mut Bounds,
        &'static mut WidthRatio,
        &'static ChildOf,
    ),
    Added<Window>,
>;

type WindowDefaultsQueries<'w, 's> = ParamSet<
    'w,
    's,
    (
        AddedWindowDefaults<'static, 'static>,
        Query<'static, 'static, &'static WidthRatio, With<FocusedMarker>>,
    ),
>;

/// Computes the passthrough keybinding set for the given window/app and
/// publishes it to the input thread. Called on focus change and config reload.
fn update_passthrough(window: &Window, app: &Application, config: &Config) {
    let properties = WindowProperties::new(app, window, config);
    crate::platform::input::set_focused_passthrough(properties.passthrough_keys());
}

/// Re-applies the configuration side effects that must follow any config change:
/// the per-display menubar-height override and the focused window's passthrough
/// keys. Shared by the TOML reload trigger ([`refresh_configuration_trigger`])
/// and the Lua reload system so both config sources behave identically on reload.
pub(crate) fn apply_config_side_effects(
    config: &Config,
    displays: &mut Query<&mut Display>,
    windows: &Windows,
    applications: &Query<&Application>,
) {
    let height = config.menubar_height();
    for mut display in &mut *displays {
        display.set_menubar_height_override(height);
    }

    // Recompute passthrough keys for the currently focused window.
    if let Some((window, _, parent)) = windows
        .focused()
        .and_then(|(w, e)| windows.find_parent(w.id()).map(|(w, _, p)| (w, e, p)))
        && let Ok(app) = applications.get(parent)
    {
        update_passthrough(window, app, config);
    }
}

/// Handles the event when an application switches to the front. It updates the focused window and PSN.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the application front switched event.
/// * `processes` - A query for all processes with their children.
/// * `applications` - A query for all applications.
/// * `focused_window` - A query for the focused window.
/// * `focus_follows_mouse_id` - The resource to track focus follows mouse window ID.
/// * `commands` - Bevy commands to trigger events and manage components.
pub(super) fn front_switched_trigger(
    mut messages: MessageReader<Event>,
    processes: Query<(&BProcess, &Children)>,
    applications: Query<&Application>,
    window_manager: Res<WindowManager>,
    mut config: GlobalState,
    mut commands: Commands,
) {
    const FRONT_SWITCH_RETRY_SEC: u64 = 2;
    for event in messages.read() {
        let Event::ApplicationFrontSwitched { psn } = event else {
            continue;
        };

        let Some((BProcess(process), children)) =
            processes.iter().find(|process| &process.0.psn() == psn)
        else {
            debug!("Unable to find process with PSN {psn:?}");
            continue;
        };

        if children.len() > 1 {
            warn!("Multiple apps registered to process '{}'.", process.name());
        }
        let Some(&app_entity) = children.first() else {
            error!("No application for process '{}'.", process.name());
            continue;
        };
        let Some(app) = applications.get(app_entity).ok() else {
            error!("No application for process '{}'.", process.name());
            continue;
        };

        debug!("front switching process: {}", process.name());

        if let Ok(focused_id) = app.focused_window_id().inspect_err(|err| {
            warn!("can not get current focus: {err}");
        }) {
            if let Some(point) = window_manager.cursor_position()
                && window_manager
                    .find_window_at_point(&point)
                    .is_ok_and(|window_id| window_id != focused_id)
            {
                // Window got focus without mouse movement - probably with a Cmd-Tab.
                // If so, bring it into view.
                config.set_skip_reshuffle(false);
                config.set_ffm_flag(None);
            }
            commands.trigger(SendMessageTrigger(Event::WindowFocused {
                window_id: focused_id,
            }));
        } else {
            // Transient AX error (e.g. kAXErrorCannotComplete during app transitions).
            // Schedule a retry to query the focused window once the app is ready.
            let timeout = Timeout::new(
                Duration::from_secs(FRONT_SWITCH_RETRY_SEC),
                Some(format!(
                    "Front switch retry for '{}' timed out.",
                    process.name()
                )),
                &mut commands,
            );
            commands.spawn((timeout, RetryFrontSwitch(app_entity)));
        }
    }
}

pub(super) fn theme_change_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut theme: Option<ResMut<SystemTheme>>,
) {
    for event in messages.read() {
        let Event::ThemeChanged = event else {
            continue;
        };

        let Some(ref mut theme) = theme else {
            continue;
        };

        let is_dark = crate::util::is_dark_mode();
        if theme.is_dark == is_dark {
            continue;
        }
        theme.is_dark = is_dark;
        info!("System theme changed: dark_mode={is_dark}");

        let Some(dim_ratio) = config.window_dim_ratio(is_dark) else {
            continue;
        };

        // Re-apply dimming to all windows that are NOT focused.
        let focused_id = windows.focused().map(|(window, _)| window.id());
        let windows_to_dim: Vec<WinID> = windows
            .iter()
            .filter(|(window, _)| Some(window.id()) != focused_id)
            .map(|(window, _)| window.id())
            .collect();

        if !windows_to_dim.is_empty() {
            window_manager.dim_windows(&windows_to_dim, dim_ratio);
        }
    }
}

/// Handles the event when a window gains focus. It updates the focused window, PSN, and reshuffles windows.
/// It also centers the mouse on the focused window if focus-follows-mouse is enabled.
///
/// # Arguments
///
/// * `messages` - The event stream carrying the window focused event.
/// * `applications` - A query for all applications.
/// * `workspaces` - A query for the layout strips, to reorder the focused column.
/// * `restore_guards` - Guards absorbing the OS acknowledgment of a restored focus.
/// * `focus_history` - Per-workspace record of what was focused last.
/// * `global_state` - Focus-follows-mouse and reshuffle flags.
/// * `ctx` - Window queries, configuration and the command buffer.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::too_many_lines)]
pub(super) fn window_focused_trigger(
    mut messages: MessageReader<Event>,
    applications: Query<&Application>,
    mut workspaces: Query<(Entity, &mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    restore_guards: Query<(Entity, &RestoreFocusMarker)>,
    mut focus_history: ResMut<FocusHistory>,
    global_state: GlobalState,
    mut ctx: WindowCtx,
) {
    const STRAY_FOCUS_RETRY_SEC: u64 = 2;

    let initial_focused = ctx
        .windows
        .focused()
        .map(|(window, entity)| (entity, window.id()));
    let mut current_focused = initial_focused;

    for event in messages.read() {
        let Event::WindowFocused { window_id } = *event else {
            continue;
        };

        let Some((window, entity, parent)) = ctx.windows.find_parent(window_id) else {
            let timeout = Timeout::new(
                Duration::from_secs(STRAY_FOCUS_RETRY_SEC),
                None,
                &mut ctx.commands,
            );
            ctx.commands.spawn((timeout, StrayFocusEvent(window_id)));
            continue;
        };

        let Ok(app) = applications.get(parent) else {
            warn!("Unable to get parent for window {}.", window.id());
            continue;
        };

        // Guard against stale focus events: a delayed one (from
        // RetryFrontSwitch or a dont_focus re-assertion) must not pull
        // FocusedMarker back after focus has moved to another app.
        if !app.is_frontmost() {
            continue;
        }

        // Within the app, the window it says is focused wins over the id the
        // event carries, which may be a moment out of date. An app with native
        // tabs in particular answers with whichever member of the tab group it
        // decided to show. Follow it to that window rather than dropping the
        // event: dropping it leaves the strip parked where it was, so Cmd-Tab
        // into a tabbed terminal looks like nothing happened.
        let (window, entity, window_id) = match app.focused_window_id() {
            Ok(current) if current != window_id => {
                match ctx.windows.find_parent(current) {
                    Some((current_window, current_entity, current_parent))
                        if current_parent == parent =>
                    {
                        debug!(
                            "app {} reports window {current} focused, not {window_id}; following it",
                            app.name()
                        );
                        (current_window, current_entity, current)
                    }
                    // Nothing we track answers to that id, so the event really
                    // is stale.
                    _ => continue,
                }
            }
            _ => (window, entity, window_id),
        };

        // Always keep passthrough in sync. An internal focus_entity call races
        // with the OS WindowFocused event; without this the passthrough keys
        // remain stale from a previously focused window.
        update_passthrough(window, app, &ctx.config);

        let already_focused = current_focused.is_some_and(|(_, id)| id == window_id);

        let managed = ctx
            .windows
            .get_managed(entity)
            .and_then(|(_, _, managed)| managed);
        if matches!(managed, Some(Unmanaged::Hidden)) {
            if let Ok(mut entity_commands) = ctx.commands.get_entity(entity) {
                entity_commands.try_remove::<Unmanaged>();
            }
            ctx.commands
                .trigger(SendMessageTrigger(Event::WindowFocused { window_id }));
            continue;
        }

        // Handle tab switching: if the focused window is a tab, make it the leader.
        // Also reactivate the owning virtual strip before treating duplicate
        // focus as a no-op; the focus marker can be stale on a hidden strip.
        // Track the active workspace as a fallback so focus_history can record
        // a workspace id even when the entity hasn't been routed into a strip.
        let mut owner = None;
        let mut owning_workspace_id = None;
        let mut active_workspace_id = None;
        for (strip_entity, mut strip, active) in &mut workspaces {
            if active {
                active_workspace_id = Some(strip.id());
            }
            if owner.is_none() && strip.contains(entity) {
                // Only a real reorder counts as a layout change. A repeat
                // focus notification must not order the stack again.
                if let Ok(index) = strip.index_of(entity)
                    && let Some(column) = strip.bypass_change_detection().get_column_mut(index)
                    && column.move_to_front(entity)
                {
                    strip.set_changed();
                }
                owning_workspace_id = Some(strip.id());
                owner = Some((strip_entity, active));
            }
        }

        if owner.is_none() && managed.is_none() {
            // The window just spawned and has not yet been inserted into the strip.
            continue;
        }

        if let Some((strip_entity, active)) = owner
            && !active
            && let Ok(mut entity_commands) = ctx.commands.get_entity(strip_entity)
        {
            entity_commands.try_insert(ActiveWorkspaceMarker);
        }

        // Record before the already-focused short-circuit below: focus_entity
        // sets FocusedMarker synchronously, so OS-confirmed events for the
        // same entity would otherwise skip the write.
        if let Some(workspace_id) = owning_workspace_id.or(active_workspace_id) {
            let unmanaged = ctx.windows.get_managed(entity).and_then(|(_, _, u)| u);
            focus_history.record(workspace_id, entity, unmanaged);
        }

        // The restore guard absorbs the OS focus acknowledgment from a restore;
        // reshuffling on it would slide the strip away from where the restore
        // placed it. It may fire twice (front_switched_trigger synthesizes a
        // duplicate), so a match keeps the guard; focus moving elsewhere despawns it.
        let mut restored_focus = false;
        for (guard_entity, guard) in &restore_guards {
            if guard.entity == entity {
                restored_focus = true;
            } else if let Ok(mut entity_commands) = ctx.commands.get_entity(guard_entity) {
                entity_commands.try_despawn();
            }
        }

        if already_focused {
            if restored_focus {
                continue;
            }
            // A repeat focus event for the window that already holds focus (a
            // new browser tab, an app re-activating) is not new layout
            // information, so it must not re-derive the strip offset — that
            // threw away a manual centering. Expose the window if it has been
            // scrolled off an edge, and otherwise leave the strip alone.
            if !global_state.skip_reshuffle() && !global_state.initializing() {
                ctx.commands.ensure_visible(entity);
            }
            continue;
        }

        current_focused = Some((entity, window_id));
    }

    // Apply `FocusedMarker` at most once for the final target in this frame's
    // batch: queueing `try_insert(FocusedMarker)` for multiple entities in one
    // deferred command buffer causes `maintain_focus_singleton` to queue
    // `try_remove::<FocusedMarker>()` on both of them, stripping focus entirely.
    if let Some((entity, window_id)) = current_focused
        && current_focused != initial_focused
        && let Ok(mut entity_commands) = ctx.commands.get_entity(entity)
    {
        entity_commands.try_insert(FocusedMarker);
        debug!("window {window_id} ({entity}) focused.");
    }
}

/// Handles Mission Control events, updating the `MissionControlActive` resource.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the Mission Control event.
/// * `mission_control_active` - The `MissionControlActive` resource.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn mission_control_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut workspaces: Query<(
        Entity,
        &mut LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Option<&Scrolling>,
    )>,
    mut mission_control_active: ResMut<MissionControlActive>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::MissionControlShowAllWindows
            | Event::MissionControlShowFrontWindows
            | Event::MissionControlShowDesktop => {
                mission_control_active.as_mut().0 = true;
                for (entity, _, _, scroll) in &workspaces {
                    if scroll.is_some()
                        && let Ok(mut entity_commands) = commands.get_entity(entity)
                    {
                        entity_commands.try_remove::<Scrolling>();
                    }
                }
            }
            Event::MissionControlExit => {
                mission_control_active.as_mut().0 = false;

                // Check if some windows disappeared from the current workspace
                // - e.g. they were moved away during mission control.
                if let Some(mut active_strip) = workspaces
                    .iter_mut()
                    .find_map(|(_, strip, active, _)| active.then_some(strip))
                    && let Ok(present_windows) =
                        window_manager.windows_in_workspace(active_strip.id())
                {
                    let moved_windows = active_strip
                        .all_windows()
                        .into_iter()
                        .filter_map(|entity| windows.get(entity).zip(Some(entity)))
                        .filter(|(window, _)| !present_windows.contains(&window.id()));
                    for (window, entity) in moved_windows {
                        debug!(
                            "window {} {entity} moved, removing from workspace {}",
                            window.id(),
                            active_strip.id(),
                        );
                        // Simply removing them from the current strip is enough,
                        // they will be re-detected during the workspace change.
                        active_strip.remove(entity);
                    }
                }
            }
            _ => (),
        }
    }
}

/// Dispatches process-related messages, such as application launch and termination.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the application event.
/// * `processes` - A query for all processes.
/// * `commands` - Bevy commands to spawn or despawn entities.
pub(super) fn application_event_trigger(
    mut messages: MessageReader<Event>,
    processes: Query<(&BProcess, Entity)>,
    mut commands: Commands,
) {
    const PROCESS_READY_TIMEOUT_SEC: u64 = 5;
    let find_process = |psn| {
        processes
            .iter()
            .find(|(BProcess(process), _)| process.psn() == psn)
    };

    for event in messages.read() {
        match event {
            Event::ApplicationLaunched { psn, observer } if find_process(*psn).is_none() => {
                let process: BProcess = Process::new(psn, observer.clone()).into();
                if process.pid() == 0 {
                    debug!("Skipping process with PID 0 (likely kernel_task).");
                    continue;
                }
                let timeout = Timeout::new(
                    Duration::from_secs(PROCESS_READY_TIMEOUT_SEC),
                    Some(format!(
                        "Process '{}' did not become ready in {PROCESS_READY_TIMEOUT_SEC}s.",
                        process.name()
                    )),
                    &mut commands,
                );
                commands.spawn((FreshMarker, timeout, process));
            }

            Event::ApplicationTerminated { psn } => {
                if let Some((BProcess(process), entity)) = find_process(*psn) {
                    forget_enhanced_ui_state(process.pid());
                    if let Ok(mut entity_commands) = commands.get_entity(entity) {
                        entity_commands.try_despawn();
                    }
                }
            }
            _ => (),
        }
    }
}

/// Dispatches application-related messages, such as window creation, destruction, and resizing.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the window event.
/// * `windows` - A query for all windows.
/// * `displays` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to spawn or despawn entities.
pub(super) fn dispatch_application_messages(
    mut messages: MessageReader<Event>,
    windows: Windows,
    applications: Query<(&Application, &Children)>,
    unmanaged_query: Query<&Unmanaged>,
    mut commands: Commands,
) {
    let find_window = |window_id| windows.find(window_id);

    for event in messages.read() {
        match event {
            Event::WindowMinimized { window_id } => {
                if let Some((_, entity)) = find_window(*window_id)
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_insert(Unmanaged::Minimized);
                }
            }

            Event::WindowDeminimized { window_id } => {
                if let Some((_, entity)) = find_window(*window_id)
                    && matches!(unmanaged_query.get(entity), Ok(Unmanaged::Minimized))
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_remove::<Unmanaged>();
                }
            }

            Event::ApplicationHidden { pid } => {
                let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid)
                else {
                    warn!("Unable to find with pid {pid}");
                    continue;
                };
                for entity in children {
                    // Only hide windows that are currently managed (no Unmanaged component).
                    // Preserve existing Floating, Minimized, and Hidden states.
                    if unmanaged_query.get(*entity).is_err()
                        && let Ok(mut entity_commands) = commands.get_entity(*entity)
                    {
                        entity_commands.try_insert(Unmanaged::Hidden);
                    }
                }
            }

            Event::ApplicationVisible { pid } => {
                let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid)
                else {
                    warn!("Unable to find application with pid {pid}");
                    continue;
                };
                for entity in children {
                    // Only restore windows that were hidden by the app hide/show cycle.
                    // Preserve Floating and Minimized states.
                    if matches!(unmanaged_query.get(*entity), Ok(Unmanaged::Hidden))
                        && let Ok(mut entity_commands) = commands.get_entity(*entity)
                    {
                        entity_commands.try_remove::<Unmanaged>();
                    }
                }
            }
            _ => (),
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_unmanaged_trigger(
    trigger: On<Add, Unmanaged>,
    apps: Query<(Entity, &Application)>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    // `Option<Single>` rather than `Single`: an unresolvable display would skip the
    // whole observer, but the strip removal below must still run without one.
    active_display: Option<ActiveDisplayViewport>,
    initializing: Option<Res<Initializing>>,
    mut ctx: WindowCtx,
) {
    const UNMANAGED_MAX_SCREEN_RATIO_NUM: i32 = 4;
    const UNMANAGED_MAX_SCREEN_RATIO_DEN: i32 = 5;
    const UNMANAGED_POP_OFFSET: i32 = 32;

    fn clamp_origin_to_bounds(origin: IRect, size: Size, bounds: IRect) -> IRect {
        let max = (bounds.max - size).max(bounds.min);
        let min = origin.min.clamp(bounds.min, max);
        IRect::from_corners(min, min + size)
    }

    fn offset_frame_within_bounds(frame: IRect, bounds: IRect, offset: i32) -> IRect {
        let candidates = [
            (offset, offset),
            (offset, -offset),
            (-offset, offset),
            (-offset, -offset),
            (offset, 0),
            (-offset, 0),
            (0, offset),
            (0, -offset),
        ];

        for (dx, dy) in candidates {
            let moved = IRect::from_corners(
                Origin::new(frame.min.x + dx, frame.min.y + dy),
                Origin::new(frame.max.x + dx, frame.max.y + dy),
            );
            if moved.min.x >= bounds.min.x
                && moved.max.x <= bounds.max.x
                && moved.min.y >= bounds.min.y
                && moved.max.y <= bounds.max.y
            {
                return moved;
            }
        }

        frame
    }

    let entity = trigger.event().entity;
    let Some((_, _, Some(Unmanaged::Floating))) = ctx.windows.get_managed(entity) else {
        return;
    };

    debug!("Entity {entity} is floating.");

    // A window on a virtual row that isn't on screen is off-screen by design;
    // popping it onto the active display would paint it over the row the user
    // is actually looking at.
    let parked_out_of_view = workspaces
        .iter()
        .any(|(strip, active)| !active && strip.contains(entity));

    // Drop the strip membership first, before anything below can bail early —
    // a floating window still reserves column space in the strip otherwise,
    // leaving a gap that never closes on its own.
    for (mut strip, _) in &mut workspaces {
        if strip.contains(entity) {
            strip.remove(entity);
        }
    }

    let Some((display, dock)) = active_display.map(|display| *display) else {
        return;
    };
    let display_bounds = display.actual_display_bounds(dock, &ctx.config);

    let Some((window, frame)) = ctx.windows.get(entity).zip(ctx.windows.frame(entity)) else {
        return;
    };
    let Some((_, app)) = ctx
        .windows
        .find_parent(window.id())
        .and_then(|(_, _, parent)| apps.get(parent).ok())
    else {
        return;
    };

    let properties = WindowProperties::new(app, window, &ctx.config);

    // Skip the active-display reposition/resize during init; the strip
    // removal below still has to run.
    if parked_out_of_view {
        debug!("Entity {entity} is floating on a hidden virtual row, keeping its frame.");
    } else if let Some((rx, ry, rw, rh)) = properties.grid_ratios() {
        let x = display_bounds.min.x + round_px(f64::from(display_bounds.width()) * rx);
        let y = display_bounds.min.y + round_px(f64::from(display_bounds.height()) * ry);
        let w = round_px(f64::from(display_bounds.width()) * rw);
        let h = round_px(f64::from(display_bounds.height()) * rh);
        ctx.commands.reposition_entity(entity, Origin::new(x, y));
        ctx.commands.resize_entity(entity, Size::new(w, h));
    } else if initializing.is_none() && !properties.floating() {
        let max_width = display_bounds.width() * UNMANAGED_MAX_SCREEN_RATIO_NUM
            / UNMANAGED_MAX_SCREEN_RATIO_DEN;
        let max_height = display_bounds.height() * UNMANAGED_MAX_SCREEN_RATIO_NUM
            / UNMANAGED_MAX_SCREEN_RATIO_DEN;
        let new_width = frame.width().min(max_width);
        let new_height = frame.height().min(max_height);

        let mut target_frame =
            IRect::from_corners(frame.min, frame.min + Origin::new(new_width, new_height));
        target_frame = clamp_origin_to_bounds(target_frame, target_frame.size(), display_bounds);
        target_frame =
            offset_frame_within_bounds(target_frame, display_bounds, UNMANAGED_POP_OFFSET);

        if target_frame.size() != frame.size() {
            ctx.commands.resize_entity(
                entity,
                Size::new(target_frame.width(), target_frame.height()),
            );
        }
        if target_frame.min != frame.min {
            ctx.commands.reposition_entity(entity, target_frame.min);
        }
    }
}

fn remember_managed_strip(entity: Entity, strip: &LayoutStrip, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(PreviousManagedStrip {
            workspace_id: strip.id(),
            virtual_index: strip.virtual_index,
            index: strip.index_of(entity).unwrap_or(strip.len()),
        });
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_minimized_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    mut config: GlobalState,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    if let Some((_, _, Some(Unmanaged::Minimized | Unmanaged::Hidden))) =
        windows.get_managed(entity)
    {
        debug!("Entity {entity} is minimized or hidden.");
        let display_bounds = active_display.bounds();

        for (mut strip, active) in workspaces {
            if active {
                give_away_focus(
                    entity,
                    &windows,
                    &strip,
                    &display_bounds,
                    &mut config,
                    &mut commands,
                );
            }
            if strip.contains(entity) {
                remember_managed_strip(entity, &strip, &mut commands);
                strip.remove(entity);
            }
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_managed_trigger(
    trigger: On<Remove, Unmanaged>,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    apps: Query<(Entity, &Application)>,
    mut workspaces: Query<
        (
            Entity,
            &mut LayoutStrip,
            &mut Position,
            Has<ActiveWorkspaceMarker>,
        ),
        Without<Window>,
    >,
    previous_strips: Query<&PreviousManagedStrip>,
    initializing: Option<Res<Initializing>>,
    mut ctx: WindowCtx,
) {
    // finish_setup handles the initial strip assignment during init.
    if initializing.is_some() {
        return;
    }
    let entity = trigger.event().entity;

    if ctx
        .windows
        .get(entity)
        .is_some_and(|window| !window.is_alive())
    {
        // The marker was removed because the windows was destroyed.
        return;
    }

    debug!("Entity {entity} is managed again.");
    let (display, dock) = *active_display;
    let display_bounds = display.actual_display_bounds(dock, &ctx.config);
    let mut insert_at = previous_strips
        .get(entity)
        .ok()
        .map(|previous| previous.index);

    if let Some(window) = ctx.windows.get(entity)
        && let Some((_, app)) = ctx
            .windows
            .find_parent(window.id())
            .and_then(|(_, _, parent)| apps.get(parent).ok())
    {
        let properties = WindowProperties::new(app, window, &ctx.config);

        // Re-managing an existing window (e.g. after un-floating it) retains
        // its width under the dynamic rule. Fixed rules keep their old behavior.
        if let Some(width_ratio) = properties.fixed_width_ratio() {
            let (_, pad_right, _, pad_left) = ctx.config.edge_padding();
            let padded_width = display_bounds.width() - pad_left - pad_right;
            let width = round_px(f64::from(padded_width) * width_ratio);
            let height = display_bounds.height();
            ctx.commands.resize_entity(entity, Size::new(width, height));
        }

        insert_at = properties.insertion().or(insert_at);
    }

    let previous = previous_strips.get(entity).ok().copied();
    for (_, mut strip, _, _) in &mut workspaces {
        strip.remove(entity);
    }

    // The strip the window ended up in, and whether that strip is the one
    // currently on screen.
    let mut landed_in = None;
    if let Some(previous) = previous {
        for (strip_entity, mut strip, _, active) in &mut workspaces {
            if strip.id() == previous.workspace_id && strip.virtual_index == previous.virtual_index
            {
                strip.insert_at(insert_at.unwrap_or(previous.index), entity);
                landed_in = Some((strip_entity, active));
                break;
            }
        }
    }

    if landed_in.is_none()
        && let Some((strip_entity, mut active_strip, _, _)) =
            workspaces.iter_mut().find(|(_, _, _, active)| *active)
    {
        landed_in = Some((strip_entity, true));
        if let Some(index) = insert_at {
            active_strip.insert_at(index, entity);
        } else {
            // Insert at the column the floating window visually overlaps so the
            // strip doesn't have to scroll to the end to expose the new column.
            let insertion = ctx.windows.frame(entity).and_then(|frame| {
                let center_x = frame.center().x;
                active_strip.all_columns().into_iter().position(|top| {
                    ctx.windows
                        .frame(top)
                        .is_some_and(|col| col.center().x > center_x)
                })
            });
            let insertion = insertion.unwrap_or(active_strip.len());
            active_strip.insert_at(insertion, entity);
        }
    }

    if let Ok(mut entity_commands) = ctx.commands.get_entity(entity) {
        entity_commands.try_remove::<PreviousManagedStrip>();
    }

    if let Some((strip_entity, false)) = landed_in {
        // This strip isn't on screen, so the window's current frame (possibly
        // popped onto the active display while unmanaged) must not be kept.
        // Marking the strip's position changed forces the layout to re-derive
        // this window's frame from the strip's off-screen origin, instead of
        // painting it over the active row while it still belongs to the hidden one.
        if let Ok((_, _, mut position, _)) = workspaces.get_mut(strip_entity) {
            position.set_changed();
        }
        return;
    }

    if let Some(origin) = ctx.windows.origin(entity) {
        ctx.commands.reposition_entity(entity, origin);
    }
    if let Ok(mut entity_commands) = ctx.commands.get_entity(entity) {
        entity_commands.try_insert(VerifyWindowPosition::default());
    }
    ctx.commands.reshuffle_around(entity);
}

const HANDLE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(1);

/// Defers ambiguous SLS destruction while the window remains ordered in.
pub(super) fn claim_replaced_window_handles(
    mut messages: MessageReader<Event>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::WindowDestroyed {
            window_id,
            source: DestroySource::SpaceNotification,
        } = event
        else {
            continue;
        };
        let Some((window, entity)) = windows.find(*window_id) else {
            continue;
        };
        if !window.is_alive()
            && matches!(window_manager.window_ordered_in(*window_id), Ok(true))
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            debug!("Window {window_id} is still ordered in with a dead AX element.");
            entity_commands.try_insert_if_new(AwaitingHandle(Timer::new(
                HANDLE_RECOVERY_TIMEOUT,
                TimerMode::Once,
            )));
        }
    }
}

/// Replaces stale AX handles while preserving window entities and layout state.
///
/// Geometry committed while the handle was dead never reached the window, and
/// `Changed<Bounds>`/`Changed<Position>` will not fire again for it, so a
/// recovered managed window gets its current layout frame written directly.
/// Writing here instead of marking the components changed keeps
/// `window_resize_verifier` from reading the stale native size back into `Bounds`.
pub(super) fn recover_window_handles(
    awaiting: Populated<AwaitingWindow>,
    mut apps: Query<&mut Application>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    clock: Res<Time>,
    mut commands: Commands,
) {
    for (entity, mut window, child, mut deadline, geometry, unmanaged) in awaiting {
        deadline.0.tick(clock.delta());
        let window_id = window.id();
        let ordered_in = window_manager.window_ordered_in(window_id);
        let handle_recovered = matches!(ordered_in, Ok(true))
            && (window.is_alive()
                || apps
                    .get_mut(child.parent())
                    .is_ok_and(|mut app| adopt_fresh_handle(&mut window, &mut app, &config)));
        // A write the engine made to the window is still on its way: read
        // it once it lands rather than wait on the app.
        let frame_ready = handle_recovered
            && (unmanaged
                || geometry.is_none()
                || (!window.write_pending() && window.update_frame().is_ok()));

        if !frame_ready && !deadline.0.is_finished() && !matches!(ordered_in, Ok(false)) {
            continue;
        }
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<AwaitingHandle>();
            if frame_ready
                && !unmanaged
                && let Some((position, bounds)) = geometry
            {
                window.reposition(position.0);
                window.resize(bounds.0);
                entity_commands
                    .try_insert(VerifyWindowSize::default())
                    .try_insert_if_new(VerifyWindowPosition::default());
            }
        }
        if !handle_recovered {
            debug!("Window {window_id} has no new AX element; closing it.");
            commands.trigger(SendMessageTrigger(Event::WindowDestroyed {
                window_id,
                source: DestroySource::Accessibility { element: None },
            }));
        }
    }
}

fn adopt_fresh_handle(window: &mut Window, app: &mut Application, config: &Config) -> bool {
    let window_id = window.id();
    let Some(old) = app
        .window_list(config)
        .into_iter()
        .filter(|fresh| fresh.id() == window_id && fresh.is_alive())
        .find_map(|fresh| window.replace_handle(fresh).ok())
    else {
        return false;
    };
    debug!("Window {window_id} has a new AX element; keeping its entity.");
    app.unobserve_window(&old);
    if app.observe_window(window).is_err() {
        warn!("Error observing window {window_id}.");
    }
    true
}

/// Handles the event when a window is destroyed. The windows itself is not removed from the layout
/// strip. This happens in the On<Remove, Window> trigger.
///
/// # Arguments
///
/// * `messages` - The event stream carrying the ID of the destroyed window.
/// * `active_display` - The active display and its layout strip.
/// * `apps` - A query for all applications.
/// * `global_state` - Focus-follows-mouse and reshuffle flags.
/// * `windows` - A query for all windows with their parent.
/// * `awaiting` - Windows waiting for the app to replace their AX element.
/// * `commands` - Bevy commands to despawn entities and trigger events.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn window_destroyed_trigger(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    mut apps: Query<&mut Application>,
    mut global_state: GlobalState,
    windows: Windows,
    awaiting: Query<(), With<AwaitingHandle>>,
    mut overrides: ResMut<TileOverrides>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::WindowDestroyed { window_id, source } = event else {
            continue;
        };

        let Some((window, entity, parent)) = windows.find_parent(*window_id) else {
            debug!("Duplicate event: window {window_id} already destroyed.");
            continue;
        };

        // Only the SLS notification is ambiguous — it also fires when a window merely leaves a
        // space, so it needs confirming. A `kAXUIElementDestroyedNotification` means the AX element
        // itself has been torn down and is taken at face value: confirming it against the app is
        // not just unnecessary but actively wrong, because both signals below lag the teardown.
        // `is_alive()` keeps succeeding on the dead element for apps that outlive their windows, and
        // the app's AX window list is still warm for a moment after the close. Re-checking them
        // raced the window back to life, leaving the entity in the strip and a permanent gap where
        // the window had been.
        match source {
            DestroySource::SpaceNotification if awaiting.contains(entity) || window.is_alive() => {
                debug!(
                    "Window {} still present, this was SLS workspace change.",
                    window.id()
                );
                continue;
            }
            DestroySource::Accessibility {
                element: Some(element),
            } if !window.is_current_element(element) => {
                debug!("Window {window_id} lost a replaced AX element.");
                continue;
            }
            _ => {}
        }

        let Ok(mut app) = apps.get_mut(parent) else {
            error!("Window {} has no parent!", window.id());
            continue;
        };

        app.unobserve_window(window);
        overrides.forget(window.id());

        let has_focused_survivor = windows
            .focused()
            .and_then(|(_, focused)| windows.get_managed(focused))
            .is_some_and(|(_, focused, unmanaged)| focused != entity && unmanaged.is_none());
        if !has_focused_survivor {
            give_away_focus(
                entity,
                &windows,
                active_display.active_strip(),
                &active_display.bounds(),
                &mut global_state,
                &mut commands,
            );
        }

        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_despawn();
        }

        // The window entity will be removed from the layout strip in the On<Remove> trigger.
    }
}

/// Drops a window's cached title when its app reports the title changed.
///
/// Must run before the broadcast handler reads titles in `PostUpdate`, so a
/// subscriber sees the new title in the same frame it changed.
pub(super) fn invalidate_window_title(mut messages: MessageReader<Event>, windows: Windows) {
    for event in messages.read() {
        let Event::WindowTitleChanged { window_id } = event else {
            continue;
        };
        if let Some((window, _)) = windows.find(*window_id) {
            window.invalidate_title();
        }
    }
}

/// Moves the focus away to a neighbour window.
fn give_away_focus(
    entity: Entity,
    windows: &Windows,
    active_strip: &LayoutStrip,
    viewport: &IRect,
    config: &mut GlobalState,
    commands: &mut Commands,
) {
    if active_strip.is_tabbed_display(entity)
        && let Some(sibling) = active_strip.tab_display_sibling(entity)
    {
        // Closing/losing focus on the active tab of a paneru-tabbed-display
        // stack must focus the next tab, not fall through to the
        // nearest-column search below — that search returns one
        // representative entity per column, and this column's own entry
        // was `entity` itself, so it would otherwise jump to an unrelated
        // neighbouring column.
        config.set_ffm_flag(None);
        commands.focus_entity(sibling, true);
        return;
    }

    if active_strip.tabbed(entity) {
        // Do not give away focus for tabbed windows.
        // Remaining tab gets the focus.
        return;
    }
    let display_center = viewport.center().x;
    let closest = active_strip
        .all_columns()
        .into_iter()
        .filter(|&candidate| candidate != entity)
        .filter_map(|candidate| {
            let center = windows.moving_frame(candidate)?.center().x;
            let distance = (center - display_center).abs();
            Some((candidate, distance))
        })
        .min_by_key(|(_, dist)| *dist)
        .map(|(e, _)| e)
        .or_else(|| {
            // Fallback when no candidate has a usable frame: pick any other
            // column in the strip. Without this, losing focus on the only
            // geometrically-known window would leave FocusedMarker unset and
            // silently break keybindings.
            active_strip
                .all_columns()
                .into_iter()
                .find(|&candidate| candidate != entity)
        });

    if let Some(neighbour) = closest
        && windows.get(neighbour).is_some()
    {
        config.set_ffm_flag(None);
        // Use focus_entity instead of triggering Event::WindowFocused: the
        // OS has usually handed focus to a different app after the current
        // window closed/hid, so window_focused_trigger's frontmost/focused
        // guards would reject a fabricated event. focus_entity calls the
        // AX API to raise the neighbour and inserts FocusedMarker directly.
        commands.focus_entity(neighbour, true);
    }
}

/// Handles the event when a new window is created. It adds the window to the manager and sets focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the new windows.
/// * `windows` - A query for all windows.
/// * `apps` - A query for all applications.
/// * `active_display` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to manage components and trigger events.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn spawn_window_trigger(
    mut trigger: On<SpawnWindowTrigger>,
    windows: Query<&Window>,
    mut apps: Query<(Entity, &mut Application)>,
    active_display: ActiveDisplay,
    launch_windows: Option<Res<LaunchWindows>>,
    initializing: Option<Res<Initializing>>,
    restore: Option<Res<crate::ecs::restore::SessionRestore>>,
    mut commands: Commands,
) {
    let new_windows = &mut trigger.event_mut().0;

    while let Some(mut window) = new_windows.pop() {
        let window_id = window.id();

        if windows.iter().any(|window| window.id() == window_id) {
            continue;
        }

        let Ok(pid) = window.pid() else {
            trace!("Unable to get window pid for {window_id}");
            continue;
        };
        let Some((app_entity, mut app)) = apps.iter_mut().find(|(_, app)| app.pid() == pid) else {
            trace!("unable to find application with pid {pid}.");
            continue;
        };

        if tracing::enabled!(Level::DEBUG) {
            let title = window.title().unwrap_or_default();
            let role = window.role().unwrap_or_default();
            let subrole = window.subrole().unwrap_or_default();
            let element = window
                .element()
                .map(|element| format!("{element}"))
                .unwrap_or_default();
            debug!(
                "created {window_id} title: {title} role: {role} subrole: {subrole} element: {element}",
            );
        }

        if app.observe_window(&window).is_err() {
            warn!("Error observing window {window_id}.");
        }

        // update_frame expands the OS rect by the per-window padding, so calling it *after*
        // set_padding produces the correct logical frame for the ECS components below.
        let Ok(frame) = window.update_frame().inspect_err(|err| error!("{err}")) else {
            continue;
        };
        let window_padding = (window.horizontal_padding(), window.vertical_padding());
        let position = Position(frame.min);
        let bounds = Bounds(frame.size());
        let width_ratio =
            WidthRatio(f64::from(frame.width()) / f64::from(active_display.bounds().width()));
        let layout_position = LayoutPosition::default();

        let title = window.title().unwrap_or_default();
        let role = window.role().ok();
        let subrole = window.subrole().ok();
        let app_name = app.name().to_string();
        let bundle_id = app.bundle_id().unwrap_or_default().clone();
        let window_frame = crate::types::state::Frame {
            x: frame.min.x,
            y: frame.min.y,
            width: frame.width(),
            height: frame.height(),
        };

        // Insert the window into the internal Bevy state.
        // This insertion triggers window attributes observer.
        let mut spawned = commands.spawn((
            position,
            bounds,
            width_ratio,
            window,
            layout_position,
            ChildOf(app_entity),
        ));
        if launch_windows
            .as_ref()
            .is_some_and(|launch| launch.0.contains(&window_id))
        {
            let pre_launch = unpadded(frame, window_padding);
            spawned.insert(PreLaunchFrame(pre_launch));
        }

        commands.trigger(SendMessageTrigger(Event::WindowSpawned {
            window_id,
            pid,
            app_name,
            bundle_id,
            title,
            role,
            subrole,
            frame: window_frame,
            floating: false,
            managed: true,
        }));
    }

    if initializing.is_none() && restore.is_some() {
        commands.trigger(RestoreWindowState);
    }
}

pub(super) fn apply_window_defaults(
    mut windows: WindowDefaultsQueries,
    apps: Query<(Entity, &Application)>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    initializing: Option<Res<Initializing>>,
    overrides: Res<TileOverrides>,
    mut commands: Commands,
) {
    let focused_ratio = windows.p1().single().ok().map(|ratio| ratio.0);
    for (entity, ref mut window, mut position, mut bounds, mut width_ratio, child) in windows.p0() {
        let Ok((_, app)) = apps.get(child.parent()) else {
            continue;
        };

        let properties = WindowProperties::new(app, window, &config);
        debug!("Applying window defaults for '{}'", window.id());

        let initializing = initializing.is_some();

        // Do not add padding to floating windows.
        if properties.floating() && !overrides.contains(window, app) {
            // Skip grid_ratios during init: we don't know this window's display.
            if !initializing && let Some((rx, ry, rw, rh)) = properties.grid_ratios() {
                let bounds = active_display.actual_bounds(&config);
                let x = bounds.min.x + round_px(f64::from(bounds.width()) * rx);
                let y = bounds.min.y + round_px(f64::from(bounds.height()) * ry);
                let w = round_px(f64::from(bounds.width()) * rw);
                let h = round_px(f64::from(bounds.height()) * rh);
                window.reposition(Origin::new(x, y));
                window.resize(Size::new(w, h));
            }
            continue;
        }
        let vpadding = properties.vertical_padding();
        let hpadding = properties.horizontal_padding();
        window.set_padding(WindowPadding::Vertical(vpadding.clamp(0, 50)));
        window.set_padding(WindowPadding::Horizontal(hpadding.clamp(0, 50)));
        let display_width = f64::from(active_display.bounds().width());
        if let Ok(frame) = window.update_frame() {
            position.0 = frame.min;
            bounds.bypass_change_detection().0 = frame.size();
            width_ratio.0 = f64::from(frame.width()) / display_width;
        }

        // Apply configured width AFTER update_frame so it isn't overwritten.
        // WidthRatio and commit_window_frame use the display's raw bounds.
        // Safe during init: this only resizes, it doesn't reposition, so a
        // window on an inactive display stays put.
        if let Some(width) = properties.width_ratio(focused_ratio) {
            let new_width = round_px(display_width * width);
            let height = window.frame().height();
            window.resize(Size::new(new_width, height));
            // Re-read the actual OS size: the app may enforce a minimum width
            // that differs from our request.
            if let Ok(frame) = window.update_frame() {
                position.0 = frame.min;
                bounds.bypass_change_detection().0 = frame.size();
                width_ratio.0 = f64::from(frame.width()) / display_width;
            } else {
                // The app has not taken the resize yet: keep the requested
                // width, and check the size it takes once the write lands.
                bounds.bypass_change_detection().0 = Size::new(new_width, height);
                width_ratio.0 = f64::from(new_width) / display_width;
                commands
                    .entity(entity)
                    .try_insert(VerifyWindowSize::default());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn apply_window_positions(
    added: Populated<Entity, Added<Window>>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    apps: Query<&Application>,
    initializing: Option<Res<Initializing>>,
    restore: Option<Res<crate::ecs::restore::SessionRestore>>,
    restoration: Option<Res<PaneruState>>,
    overrides: Res<TileOverrides>,
    mut ctx: WindowCtx,
) {
    for entity in added {
        if workspaces.iter().any(|(strip, _)| strip.tabbed(entity)) {
            debug!("Ignoring tabbed {entity} attributes.");
            continue;
        }

        let Some((window, _, parent)) = ctx
            .windows
            .get(entity)
            .and_then(|window| ctx.windows.find_parent(window.id()))
        else {
            continue;
        };
        let Ok(app) = apps.get(parent) else {
            continue;
        };

        let properties = WindowProperties::new(app, window, &ctx.config);

        // A floating rule wins over a saved tiled placement, so a rule added
        // after the session was saved still applies, unless the user tiled
        // this window against the rule.
        if properties.floating() && !overrides.contains(window, app) {
            if let Some(mut strip) = workspaces
                .iter_mut()
                .find_map(|(strip, _)| strip.contains(entity).then_some(strip))
            {
                strip.remove(entity);
            }
            if let Ok(mut entity_commands) = ctx.commands.get_entity(entity) {
                // Avoid managing window if it's floating.
                entity_commands.try_insert(Unmanaged::Floating);
            }
            continue;
        }

        if crate::ecs::restore::matches_startup_restore_state(
            window,
            app,
            restore.as_deref(),
            restoration.as_deref(),
            &ctx.config,
        ) {
            continue;
        }

        // During startup, the window is already inserted into some strip by finish_setup.
        let allready_inserted = workspaces
            .iter_mut()
            .find_map(|(strip, _)| strip.contains(entity).then_some(strip));
        if initializing.is_none()
            && allready_inserted.is_none()
            && let Some(mut strip) = workspaces
                .iter_mut()
                .find_map(|(strip, active)| active.then_some(strip))
        {
            // Attempt inserting the window at a pre-defined position.
            let insert_at = properties.insertion().map_or_else(
                || {
                    // Otherwise attempt inserting it after the current focus.
                    let focused_window = ctx.windows.focused();
                    // Insert to the right of the currently focused window
                    focused_window
                        .and_then(|(_, entity)| strip.index_of(entity).ok())
                        .and_then(|insert_at| {
                            (insert_at + 1 < strip.len()).then_some(insert_at + 1)
                        })
                },
                Some,
            );

            debug!("New window {entity} adding at {}", *strip);
            match insert_at {
                Some(after) => {
                    debug!("New window inserted at {after}");
                    strip.insert_at(after, entity);
                }
                None => strip.append(entity),
            }
        }

        // During init, skip per-window reshuffles. finish_setup does a single
        // reshuffle after all windows are added.
        if initializing.is_none() {
            if properties.dont_focus() {
                if let Some((focus, prev)) = ctx.windows.focused() {
                    debug!(
                        "Not focusing new window {entity}, keeping focus on '{}'",
                        focus.title().unwrap_or_default()
                    );
                    ctx.commands.focus_entity(prev, true);
                }
            } else {
                debug!("Synthesizing WindowFocused for newly spawned window {entity}");
                ctx.commands
                    .trigger(SendMessageTrigger(Event::WindowFocused {
                        window_id: window.id(),
                    }));
            }
        }
    }
}

pub(super) fn refresh_configuration_trigger(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    mut config: ResMut<Config>,
    mut watcher: Option<NonSendMut<Box<dyn Watcher>>>,
    windows: Windows,
    mut displays: Query<&mut Display>,
    applications: Query<&Application>,
) {
    for event in messages.read() {
        let Event::ConfigRefresh(event) = event else {
            continue;
        };

        let Some(ref mut watcher) = watcher else {
            continue;
        };

        match &event.kind {
            EventKind::Modify(
                // When using the RecommendedWatcher, the event triggers on file data.
                // When using PollWatcher, it triggers on modification time.
                ModifyKind::Metadata(MetadataKind::WriteTime)
                | ModifyKind::Data(DataChange::Content),
            ) => (),
            EventKind::Remove(_) => {
                for path in &event.paths {
                    _ = watcher.unwatch(path).inspect_err(|err| {
                        error!("unwatching the config '{}': {err}", path.display());
                    });
                }
                continue;
            }
            _ => continue,
        }

        // An init.lua disables the TOML path entirely, so there is nothing here
        // to reload — see `CONFIGURATION_FILE`.
        if crate::config::CONFIGURATION_FILE.is_none() {
            continue;
        }

        for path in &event.paths {
            // The Lua init script shares this watcher and `ConfigRefresh` event;
            // it is reloaded separately by `lua_reload_system`. Skip it here so
            // we never try to TOML-parse a Lua file.
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lua"))
            {
                continue;
            }
            if let Some(symlink) = symlink_target(path) {
                debug!(
                    "symlink '{}' changed, replacing the watcher.",
                    symlink.display()
                );
                if let Some(new_watcher) =
                    crate::ecs::rewatch_configs(&window_manager, path.as_path())
                {
                    **watcher = new_watcher;
                }
            }
            info!("Reloading configuration file; {}", path.display());
            _ = config.reload_config(path.as_path()).inspect_err(|err| {
                error!("loading config '{}': {err}", path.display());
            });
        }

        apply_config_side_effects(&config, &mut displays, &windows, &applications);
    }
}

pub(super) fn window_removal_trigger(
    trigger: On<Remove, Window>,
    mut workspaces: Query<(Entity, &mut LayoutStrip)>,
    mut focus_history: ResMut<FocusHistory>,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    focus_history.forget(entity);

    if let Some((strip_entity, mut strip)) = workspaces
        .iter_mut()
        .find(|(_, strip)| strip.contains(entity))
    {
        debug!(
            "Removing despawned entity {entity} from strip {}",
            strip.id()
        );
        strip.remove(entity);
        commands
            .entity(strip_entity)
            .try_insert(ExposeFocusedSurvivorAfterRemoval);
    }
}

pub(super) fn send_message_trigger(
    trigger: On<SendMessageTrigger>,
    mut messages: MessageWriter<Event>,
) {
    let event = &trigger.event().0;
    messages.write(event.clone());
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn cleanup_timeout_trigger(
    trigger: On<Remove, Timeout>,
    all_timeouts: Query<&Timeout>,
    mut commands: Commands,
) {
    if let Ok(timeout) = all_timeouts.get(trigger.entity)
        && let Some(system_id) = timeout.system_id
    {
        commands.unregister_system(system_id);
    }
}

type NativeSizeWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Window,
        Ref<'static, Position>,
        &'static mut Bounds,
    ),
>;

#[derive(SystemParam)]
pub(super) struct SizeVerification<'w, 's> {
    windows: NativeSizeWindows<'w, 's>,
    layout_strips: Query<'w, 's, &'static LayoutStrip>,
    resizes: Query<'w, 's, &'static ResizeMarker>,
    width_limits: Query<'w, 's, &'static WidthLimits>,
    native_resizes: Query<'w, 's, &'static crate::ecs::drag::NativeResizeStart>,
    step_partners: Query<'w, 's, &'static StepPartners>,
    time: Res<'w, Time>,
}

pub(super) fn drop_step_partners_trigger(
    trigger: On<Remove, VerifyWindowSize>,
    mut commands: Commands,
) {
    if let Ok(mut entity_commands) = commands.get_entity(trigger.entity) {
        entity_commands.try_remove::<StepPartners>();
    }
}

/// How long a window may stay taller than the engine asked for before its
/// height counts as the window's minimum. Chromium-based apps apply a size
/// after the write returns, and a busy app applies it later still, so a
/// readback right after the write can show the old height.
const LATE_RESIZE_GRACE: Duration = Duration::from_millis(500);

pub(super) fn window_resize_verifier(
    mut requested: Query<(Entity, &mut VerifyWindowSize), SizeCheckReady>,
    mut verification: SizeVerification,
    mut commands: Commands,
) {
    for (entity, mut request) in &mut requested {
        let Ok((mut window, position, bounds)) = verification.windows.get_mut(entity) else {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<VerifyWindowSize>();
            }
            continue;
        };
        // Checked once the engine's own write has landed, without spending
        // an attempt on the frame the window is leaving.
        if position.is_changed() || bounds.is_changed() || window.write_pending() {
            continue;
        }
        // macOS puts a tile frame back while a restored slot holds, so a
        // readback then is not the app refusing the size.
        if let Ok(start) = verification.native_resizes.get(entity)
            && start.held_size(verification.time.elapsed()).is_some()
        {
            // An app that never reports its resize shows here that it took
            // the restored size.
            let seen = window
                .update_frame()
                .ok()
                .and_then(|frame| start.seen(frame.size()));
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<VerifyWindowSize>();
                if let Some(seen) = seen {
                    entity_commands.try_insert(seen);
                }
            }
            continue;
        }
        // After the hold, the tile restore decides whether a window that
        // took its restored size has had a tile put back.
        if verification
            .native_resizes
            .get(entity)
            .is_ok_and(crate::ecs::drag::NativeResizeStart::reached)
        {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<VerifyWindowSize>();
            }
            continue;
        }
        let now = verification.time.elapsed();
        if request
            .mismatched_since
            .is_some_and(|since| now < since + LATE_RESIZE_GRACE)
        {
            continue;
        }
        let frame = window.update_frame();
        if frame.is_err() {
            request.remaining = request.remaining.saturating_sub(1);
            if request.remaining > 0 {
                continue;
            }
        }
        // A window still taller than asked for, or of another width, may not
        // have applied the size yet, so it is read again before the size
        // counts as a limit.
        if request.mismatched_since.is_none()
            && frame.as_ref().is_ok_and(|frame| {
                fuzzy_equal(frame.height(), bounds.0.y) == Ordering::Less
                    || fuzzy_equal(frame.width(), bounds.0.x) != Ordering::Equal
            })
        {
            request.mismatched_since = Some(now);
            continue;
        }
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<VerifyWindowSize>();
        }
        if let Ok(frame) = frame {
            verification.reconcile(entity, frame.size(), &mut commands);
        }
    }
}

impl SizeVerification<'_, '_> {
    fn frame(&self, entity: Entity) -> Option<IRect> {
        self.windows
            .get(entity)
            .ok()
            .map(|(_, position, bounds)| IRect::from_corners(position.0, position.0 + bounds.0))
    }

    fn reconcile(&mut self, entity: Entity, actual_size: Size, commands: &mut Commands) {
        let Ok((window, _, mut bounds)) = self.windows.get_mut(entity) else {
            return;
        };
        let asked = bounds.0.y;
        let limits = self.width_limits.get(entity).ok().copied();
        let wider = adopt_refused_size(entity, &window, &mut bounds, actual_size, limits, commands);
        let refused = bounds.0.y - asked;
        self.return_refused_step(entity, refused, commands);
        if !wider {
            return;
        }
        let Some(column) = self.layout_strips.iter().find_map(|strip| {
            strip
                .index_of(entity)
                .ok()
                .and_then(|idx| strip.get(idx).ok())
        }) else {
            return;
        };
        widen_column_siblings(
            entity,
            &column,
            |sibling| self.frame(sibling),
            |sibling| self.resizes.get(sibling).ok().map(|resize| resize.0.y),
            commands,
        );
    }

    /// Gives the `refused` part of a height step on `entity` back to the
    /// windows the step moved height to or from, up to what each moved.
    fn return_refused_step(&mut self, entity: Entity, mut refused: i32, commands: &mut Commands) {
        let Ok(partners) = self.step_partners.get(entity) else {
            return;
        };
        for &(partner, moved) in &partners.0 {
            let back = if refused > 0 {
                refused.min(moved).max(0)
            } else {
                refused.max(moved).min(0)
            };
            if back == 0 {
                continue;
            }
            refused -= back;
            if let Ok(resize) = self.resizes.get(partner) {
                commands.resize_entity(partner, resize.0.with_y(resize.0.y - back));
            } else if let Ok((_, _, mut bounds)) = self.windows.get_mut(partner) {
                bounds.0.y -= back;
                if back > 0
                    && let Ok(mut entity_commands) = commands.get_entity(partner)
                {
                    entity_commands.try_insert(VerifyWindowSize::default());
                }
            }
        }
    }

    fn read_column(&mut self, members: &[Entity], commands: &mut Commands) -> bool {
        let mut frames = Vec::with_capacity(members.len());
        for member in members {
            if let Ok((mut window, _, _)) = self.windows.get_mut(*member)
                && let Ok(frame) = window.update_frame()
            {
                frames.push((*member, frame));
            }
        }
        if frames.len() != members.len() {
            return false;
        }
        for (member, frame) in frames {
            self.reconcile(member, frame.size(), commands);
        }
        true
    }
}

pub(crate) fn adopt_refused_size(
    entity: Entity,
    window: &Window,
    bounds: &mut Mut<Bounds>,
    actual_size: Size,
    window_limits: Option<WidthLimits>,
    commands: &mut Commands,
) -> bool {
    let expected_size = bounds.0;
    let width_ord = fuzzy_equal(actual_size.x, expected_size.x);
    let height_ord = fuzzy_equal(actual_size.y, expected_size.y);
    if width_ord == Ordering::Equal && height_ord == Ordering::Equal {
        return false;
    }
    debug!(
        "window '{}'({}) did not fully resize to {}, was {} instead",
        window.title().unwrap_or_default(),
        window.id(),
        expected_size,
        actual_size
    );
    bounds.0 = actual_size;
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        if height_ord == Ordering::Less {
            entity_commands.try_insert(MinimumHeight(actual_size.y));
        }
        if width_ord != Ordering::Equal {
            let mut limits = window_limits.unwrap_or_default();
            if width_ord == Ordering::Less {
                limits.min = Some(actual_size.x);
            } else {
                limits.max = Some(actual_size.x);
            }
            entity_commands.try_insert(limits);
        }
    }
    width_ord == Ordering::Less
}

pub(crate) fn widen_column_siblings(
    entity: Entity,
    column: &Column,
    frame: impl Fn(Entity) -> Option<IRect>,
    resizing_height: impl Fn(Entity) -> Option<i32>,
    commands: &mut Commands,
) {
    let Some(column_width) = column.width(&frame) else {
        return;
    };
    for sibling in column.window_iter().filter(|sibling| *sibling != entity) {
        if let Some(frame) = frame(sibling)
            && frame.width() < column_width
        {
            let height = resizing_height(sibling).unwrap_or(frame.height());
            commands.resize_entity(sibling, Size::new(column_width, height));
        }
    }
}

type WidthSettlementRequests<'w, 's> = Populated<
    'w,
    's,
    (
        Entity,
        &'static mut PendingWidthSettlement,
        Option<&'static WidthRun>,
        Has<FocusedMarker>,
        Has<Unmanaged>,
    ),
>;

type SettledSize = (
    Without<ResizeMarker>,
    Without<RepositionMarker>,
    Without<AwaitingHandle>,
    Without<VerifyWindowSize>,
);

pub(super) fn settle_width_requests(
    mut requests: WidthSettlementRequests,
    mut verification: SizeVerification,
    ready: Query<(), SettledSize>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for (entity, mut request, run, focused, unmanaged) in &mut requests {
        let strip = active_display.active_strip();
        let column = strip
            .index_of(entity)
            .ok()
            .and_then(|idx| strip.get(idx).ok());
        let (Some(run), Some(column)) = (run, column) else {
            commands
                .entity(entity)
                .try_remove::<PendingWidthSettlement>();
            continue;
        };
        let members: Vec<_> = column.window_iter().collect();
        if time.elapsed() >= request.expires
            || !focused
            || unmanaged
            || request.strip != active_display.active_strip_entity()
            || request.viewport != active_display.actual_bounds(&config)
            || members
                .iter()
                .any(|member| !verification.windows.contains(*member))
        {
            commands
                .entity(entity)
                .try_remove::<PendingWidthSettlement>();
            continue;
        }
        // A member whose write is still on its way has not settled either:
        // reading it would wait on its app.
        if !ready.contains(request.strip)
            || members.iter().any(|member| {
                !ready.contains(*member)
                    || verification.windows.get_mut(*member).is_ok_and(
                        |(window, position, bounds)| {
                            position.is_changed() || bounds.is_changed() || window.write_pending()
                        },
                    )
            })
        {
            continue;
        }
        let Some(width) = column.width(&|member| verification.frame(member)) else {
            commands
                .entity(entity)
                .try_remove::<PendingWidthSettlement>();
            continue;
        };
        match &mut request.phase {
            WidthSettlementPhase::InitialReadback => {
                if fuzzy_equal(width, request.requested_width) == Ordering::Equal {
                    commands
                        .entity(entity)
                        .try_remove::<PendingWidthSettlement>();
                } else {
                    request.phase = WidthSettlementPhase::Grace {
                        deadline: time.elapsed() + Duration::from_secs(1),
                        reads_remaining: 3,
                    };
                }
            }
            WidthSettlementPhase::Grace {
                deadline,
                reads_remaining,
            } => {
                if time.elapsed() < *deadline {
                    continue;
                }
                if !verification.read_column(&members, &mut commands) {
                    *reads_remaining -= 1;
                    if *reads_remaining == 0 {
                        commands
                            .entity(entity)
                            .try_remove::<PendingWidthSettlement>();
                    }
                    continue;
                }
                request.phase = WidthSettlementPhase::Reconciled;
            }
            WidthSettlementPhase::Reconciled => {
                commands
                    .entity(entity)
                    .try_remove::<PendingWidthSettlement>();
                if fuzzy_equal(width, request.requested_width) == Ordering::Equal {
                    continue;
                }
                let Some(frame) = verification.frame(entity) else {
                    continue;
                };
                let origin = anchored_origin(
                    run.anchor,
                    frame,
                    frame.size().with_x(width),
                    request.viewport,
                );
                commands.reposition_entity(entity, origin);
                commands.reshuffle_around(entity);
            }
        }
    }
}

fn fuzzy_equal<N>(actual_size: N, expected_size: N) -> Ordering
where
    N: std::ops::Sub<Output = N> + Ord + From<i8>,
{
    let diff = expected_size - actual_size;

    if diff < N::from(-1) {
        Ordering::Less
    } else if diff > N::from(1) {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TestHarness, find_window_entity};
    use bevy::ecs::system::RunSystemOnce;
    use bevy::math::IVec2;
    use std::time::Duration;

    #[test]
    fn test_persistent_frame_refresh_failure_keeps_the_live_window() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.run(vec![Event::MenuOpened { window_id: 0 }]);
        harness.advance(Duration::from_millis(2000));
        let entity = find_window_entity(0, harness.world());
        harness.mock_state.update_window(0, |window| {
            window.frame_read_failures = u8::MAX;
        });
        harness
            .world()
            .entity_mut(entity)
            .insert(AwaitingHandle(Timer::new(
                HANDLE_RECOVERY_TIMEOUT,
                TimerMode::Once,
            )));
        harness.advance(Duration::from_millis(1500));
        assert!(
            harness.world().get::<Window>(entity).is_some(),
            "an ordered-in window with a valid handle must not be closed"
        );
        assert!(harness.world().get::<AwaitingHandle>(entity).is_none());
        assert!(
            !harness
                .world()
                .run_system_once(|activity: crate::ecs::params::FrameActivity| activity.mid_frame())
                .unwrap()
        );
        crate::assert_focused!(harness.world(), 0);
    }

    #[test]
    fn test_handle_recovery_retries_a_transient_frame_refresh_failure() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.run(vec![Event::MenuOpened { window_id: 0 }]);
        harness.advance(Duration::from_millis(2000));
        harness.advance(Duration::from_millis(20));
        let entity = find_window_entity(0, harness.world());
        let mut size = harness.world().get::<Bounds>(entity).unwrap().0;
        size.x = 800;
        harness.world().get_mut::<Bounds>(entity).unwrap().0 = size;
        harness.mock_state.update_window(0, |window| {
            window.frame_read_failures = 1;
        });
        harness
            .world()
            .entity_mut(entity)
            .insert(AwaitingHandle(Timer::new(
                HANDLE_RECOVERY_TIMEOUT,
                TimerMode::Once,
            )));
        harness
            .world()
            .run_system_once(super::recover_window_handles)
            .unwrap();
        assert!(
            harness.world().get::<AwaitingHandle>(entity).is_some(),
            "a failed frame refresh must not finish recovery"
        );
        harness
            .world()
            .run_system_once(super::recover_window_handles)
            .unwrap();
        assert!(harness.world().get::<AwaitingHandle>(entity).is_none());
        assert_eq!(harness.mock_state.create_window(0).frame().width(), 800);
    }
    #[test]
    fn test_size_verification_retries_a_transient_read_failure() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.run(vec![Event::MenuOpened { window_id: 0 }]);
        harness.advance(Duration::from_millis(2000));
        let entity = find_window_entity(0, harness.world());
        let mut size = harness.world().get::<crate::ecs::Bounds>(entity).unwrap().0;
        size.x = 800;
        harness.mock_state.update_window(0, |window| {
            window.maximum_size = Some(IVec2::new(600, 2000));
            window.frame_read_failures = 1;
        });
        harness
            .world()
            .get_mut::<Window>(entity)
            .unwrap()
            .resize(size);
        harness
            .world()
            .get_mut::<crate::ecs::Bounds>(entity)
            .unwrap()
            .0 = size;
        harness
            .world()
            .entity_mut(entity)
            .insert(crate::ecs::VerifyWindowSize::default());
        let verifier = harness
            .world()
            .register_system(super::window_resize_verifier);
        // The fourth pass reads the width again after the late-resize grace.
        for _ in 0..4 {
            harness.world().run_system(verifier).unwrap();
            harness
                .world()
                .resource_mut::<Time>()
                .advance_by(super::LATE_RESIZE_GRACE);
        }
        assert_eq!(
            harness
                .world()
                .get::<crate::ecs::Bounds>(entity)
                .unwrap()
                .0
                .x,
            600,
            "a successful retry must reconcile the native size constraint"
        );
        assert!(
            harness
                .world()
                .get::<crate::ecs::VerifyWindowSize>(entity)
                .is_none()
        );
    }

    #[test]
    fn test_size_verification_stops_after_persistent_read_failures() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.run(vec![Event::MenuOpened { window_id: 0 }]);
        harness.advance(Duration::from_millis(2000));
        let entity = find_window_entity(0, harness.world());
        harness.mock_state.update_window(0, |window| {
            window.frame_read_failures = u8::MAX;
        });
        harness
            .world()
            .entity_mut(entity)
            .insert(crate::ecs::VerifyWindowSize::default());
        let verifier = harness
            .world()
            .register_system(super::window_resize_verifier);
        for _ in 0..4 {
            harness.world().run_system(verifier).unwrap();
        }
        assert!(
            harness
                .world()
                .get::<crate::ecs::VerifyWindowSize>(entity)
                .is_none()
        );
        assert!(
            !harness
                .world()
                .run_system_once(|activity: crate::ecs::params::FrameActivity| activity.mid_frame())
                .unwrap()
        );
    }
}
