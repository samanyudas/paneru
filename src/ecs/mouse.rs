use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::With;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Query, Res, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::time::{Duration, Instant};
use tracing::{debug, trace};

use super::{MouseHeldMarker, Timeout};
use crate::config::Config;
use crate::ecs::drag::DragState;
use crate::ecs::focus::PendingFocusLayout;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{GlobalState, WindowCtx, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, DockPosition, FocusedMarker, MissionControlActive, Position,
    RepositionMarker, Scrolling, SpawnCommandsExt, UpdateSet,
};
use bevy::ecs::schedule::common_conditions::on_message;

use crate::events::{Event, InputEvent, PointerMotion};
use crate::manager::{Display, Origin, WindowManager, origin_from};
use crate::platform::WinID;
use crate::util::round_px;

/// Bottom-right corner region (`NxN` pixels) where focus events are suppressed.
/// Sized to a representative macOS title bar height — see karinushka/paneru#233:
/// macOS prevents windows from being moved further down than a fully visible title bar,
/// so the parked sliver of a hidden virtual workspace lives within this region.
const CORNER_DEAD_ZONE_PX: i32 = 30;

pub struct MouseEventsPlugin;

impl Plugin for MouseEventsPlugin {
    fn build(&self, app: &mut App) {
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        // `run_if` also skips fetching each system's parameters (Windows
        // queries, config, window manager) when there's no input event, which
        // matters for perf.
        app.add_systems(
            Update,
            (
                mouse_moved_trigger,
                mouse_resize_trigger,
                mouse_down_trigger,
            )
                .run_if(mission_control_inactive)
                .in_set(UpdateSet::Input)
                .run_if(on_message::<InputEvent>),
        );
        app.add_systems(
            Update,
            horizontal_warp_mouse_trigger
                .in_set(UpdateSet::Input)
                .run_if(on_message::<PointerMotion>),
        );
        app.add_systems(
            Update,
            mouse_up_trigger
                .in_set(UpdateSet::Focus)
                .after(super::drag::drag_events)
                .run_if(on_message::<InputEvent>),
        );
    }
}

/// True when `point` sits inside the bottom-right `CORNER_DEAD_ZONE_PX`-sized
/// square of the display's working area (excluding any Dock).
fn is_in_corner_dead_zone(
    point: Origin,
    display: &Display,
    dock: Option<&DockPosition>,
    config: &Config,
) -> bool {
    let bounds = display.actual_display_bounds(dock, config);
    point.x >= bounds.max.x - CORNER_DEAD_ZONE_PX && point.y >= bounds.max.y - CORNER_DEAD_ZONE_PX
}

/// Handles mouse moved events.
///
/// If "focus follows mouse" is enabled, this function finds the window under the cursor and
/// focuses it. It also handles child windows like sheets and drawers to ensure the correct
/// window receives focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse moved event.
/// * `windows` - A query for all windows.
/// * `focused_window` - A query for the currently focused window.
/// * `main_cid` - The main connection ID resource.
/// * `config` - The optional configuration resource.
#[allow(clippy::too_many_arguments)]
fn mouse_moved_trigger(
    mut messages: MessageReader<InputEvent>,
    windows: Windows,
    displays: Query<(&Display, Option<&DockPosition>)>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    time: Res<Time>,
    mut global_state: GlobalState,
    mut commands: Commands,
    mut last_find_query: Local<Option<Duration>>,
    pending_focus_layout: Query<(), With<PendingFocusLayout>>,
    moving: Query<Entity, With<RepositionMarker>>,
    active_workspaces: Query<(Entity, &LayoutStrip), With<ActiveWorkspaceMarker>>,
) {
    const FIND_WINDOW_THROTTLE: Duration = Duration::from_millis(50);

    for InputEvent(event) in messages.read() {
        let Event::MouseMoved {
            point, modifiers, ..
        } = event
        else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_some_and(|modifier| modifier.matches(*modifiers))
        {
            // Resizing is handled by a separate trigger or logic.
            // For now, let's just intercept it here to prevent focus changes during resize.
            continue;
        }

        // Corner dead zone: suppress focus events when the cursor sits in
        // the bottom-right of any display (where hidden virtual workspace
        // slivers park). See is_in_corner_dead_zone for details.
        let cursor = origin_from(*point);
        if displays.iter().any(|(display, dock)| {
            display.bounds().contains(cursor)
                && is_in_corner_dead_zone(cursor, display, dock, &config)
        }) {
            trace!("mouse moved suppressed in corner dead-zone {point:?}");
            continue;
        }

        if !config.focus_follows_mouse() {
            continue;
        }
        if global_state.ffm_flag().is_some() {
            trace!("ffm_window_id > 0");
            continue;
        }

        // Command focus is applied before auto-centering starts in PostUpdate.
        // Until the strip and its windows settle, hit-testing sees intermediate
        // geometry and can undo that focus by selecting the previous window.
        // Mouse-driven focus keeps skip_reshuffle set and remains unaffected.
        if !global_state.skip_reshuffle()
            && (!pending_focus_layout.is_empty()
                || active_workspaces.iter().any(|(entity, strip)| {
                    moving
                        .iter()
                        .any(|moving| moving == entity || strip.contains(moving))
                }))
        {
            trace!("mouse focus suppressed during command-driven slide");
            continue;
        }
        let pointer = origin_from(*point);
        if let Some((focused, _)) = windows.focused()
            && focused.frame().contains(pointer)
        {
            let has_overlap = windows
                .iter()
                .any(|(w, _)| w.id() != focused.id() && w.frame().contains(pointer));
            if !has_overlap {
                trace!("pointer unambiguously inside focused window.");
                continue;
            }
        }

        let now = time.elapsed();
        if let Some(last_time) = *last_find_query
            && now.saturating_sub(last_time) < FIND_WINDOW_THROTTLE
        {
            trace!("find_window_at_point throttled.");
            continue;
        }
        *last_find_query = Some(now);

        let Ok(window_id) = window_manager.find_window_at_point(point) else {
            debug!("can not find window at point {point:?}");
            continue;
        };
        if windows
            .focused()
            .is_some_and(|(window, _)| window.id() == window_id)
        {
            trace!("allready focused {window_id}");
            continue;
        }
        let Some((window, entity)) = windows.find(window_id) else {
            trace!("can not find focused window: {window_id}");
            continue;
        };

        // Do not reshuffle windows due to moved mouse focus.
        global_state.set_skip_reshuffle(true);
        global_state.set_ffm_flag(Some(window.id()));
        commands.focus_entity(entity, false);
    }
}

type ClickableWorkspaces<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LayoutStrip,
        &'static Position,
        Option<&'static RepositionMarker>,
        &'static ChildOf,
        Option<&'static Scrolling>,
    ),
>;

fn is_window_obscured_beyond_ratio(
    entity: Entity,
    windows: &Windows,
    workspaces: &ClickableWorkspaces,
    displays: &Query<(&Display, Option<&DockPosition>)>,
    config: &Config,
) -> bool {
    let Some(((display, dock), strip_target)) = workspaces
        .iter()
        .find(|(_, strip, ..)| strip.contains(entity))
        .and_then(|(_, _, pos, repo, child, _)| {
            let target = repo.map_or(pos.0, |r| r.0);
            Some((displays.get(child.parent()).ok()?, target))
        })
    else {
        return false;
    };
    let (Some(layout_pos), Some(frame)) = (
        windows.layout_position(entity),
        windows.moving_frame(entity),
    ) else {
        return false;
    };
    let size = frame.size();
    let viewport = display.actual_display_bounds(dock, config);
    let projected_min = layout_pos.0 + strip_target;
    let projected_frame = IRect::from_corners(projected_min, projected_min + size);
    let visible_width = viewport.intersect(projected_frame).width().max(0);
    let hidden_fraction = 1.0 - f64::from(visible_width) / f64::from(size.x.max(1));
    visible_width == 0 || hidden_fraction > config.window_hidden_ratio()
}

/// Handles mouse down events.
///
/// This function finds the window at the click point. If the click changes focus,
/// stops a settling scroll, or targets a window obscured beyond
/// `window_hidden_ratio`, it defers a reshuffle until mouse-up.
fn mouse_down_trigger(
    mut messages: MessageReader<InputEvent>,
    workspaces: ClickableWorkspaces,
    displays: Query<(&Display, Option<&DockPosition>)>,
    window_manager: Res<WindowManager>,
    mouse_held: Query<Entity, With<MouseHeldMarker>>,
    mut ctx: WindowCtx,
) {
    for InputEvent(event) in messages.read() {
        let Event::MouseDown {
            point,
            window_id,
            clicks,
            ..
        } = event
        else {
            continue;
        };
        trace!("{point:?}");

        // The window the tap saw under the press, because by now the pressed
        // window may have moved off the point.
        let Some((_, entity)) = window_id
            .and_then(|window_id| ctx.windows.find(window_id))
            .or_else(|| {
                window_manager
                    .find_window_at_point(point)
                    .ok()
                    .and_then(|window_id| ctx.windows.find(window_id))
            })
        else {
            continue;
        };

        // Stop the scroll of the clicked window's strip, which the mouse-up
        // reshuffle then places. A strip settling on another display keeps
        // settling, so it never stays stretched past its end.
        let mut stopped_scrolling = false;
        for (strip_entity, strip, _, _, _, scrolling) in &workspaces {
            if scrolling.is_some()
                && strip.contains(entity)
                && let Ok(mut entity_commands) = ctx.commands.get_entity(strip_entity)
            {
                entity_commands.try_remove::<Scrolling>();
                stopped_scrolling = true;
            }
        }

        // Clean up any stale marker from a previous click.
        for held in &mouse_held {
            if let Ok(mut entity_commands) = ctx.commands.get_entity(held) {
                entity_commands.try_despawn();
            }
        }

        let focused_changed = ctx
            .windows
            .focused()
            .is_none_or(|(_, focused)| focused != entity);

        if *clicks <= 1
            && ctx.config.window_hidden_ratio() < 1.0
            && (stopped_scrolling
                || focused_changed
                || is_window_obscured_beyond_ratio(
                    entity,
                    &ctx.windows,
                    &workspaces,
                    &displays,
                    &ctx.config,
                ))
        {
            // Defer reshuffle until mouse-up so the window doesn't shift
            // mid-click. The Timeout auto-despawns if mouse-up is lost.
            let timeout = Timeout::new(Duration::from_secs(5), None, &mut ctx.commands);
            ctx.commands.spawn((MouseHeldMarker(entity), timeout));
        }
    }
}

/// Handles mouse-up events. Triggers the deferred reshuffle so the clicked
/// window slides into view after the user releases the button. A title-bar
/// drop that awaits confirmation or just finished owns its placement, and a
/// window that lost focus while held leaves the strip to the window that has it.
pub(super) fn mouse_up_trigger(
    mut messages: MessageReader<InputEvent>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    focused: Query<(), With<FocusedMarker>>,
    drag: Option<Res<DragState>>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        if !matches!(event, Event::MouseUp { .. }) {
            continue;
        }

        let dropped = drag.as_ref().and_then(|drag| match &**drag {
            DragState::Released { press, .. } => Some(press.window),
            _ => drag.settling_window(),
        });
        for (held_entity, marker) in &mouse_held {
            if dropped != Some(marker.0) && focused.contains(marker.0) {
                commands.reshuffle_around(marker.0);
            }
            if let Ok(mut entity_commands) = commands.get_entity(held_entity) {
                entity_commands.try_despawn();
            }
        }
    }
}

#[derive(Default)]
pub(super) struct MouseResizeState {
    last_point: Option<Origin>,
    window_id: Option<WinID>,
}

fn mouse_resize_trigger(
    mut messages: MessageReader<InputEvent>,
    windows: Windows,
    active_workspace: Single<(Entity, &LayoutStrip, &Position), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<MouseResizeState>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        let Event::MouseMoved {
            point, modifiers, ..
        } = event
        else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_none_or(|modifier| !modifier.matches(*modifiers))
        {
            state.last_point = None;
            state.window_id = None;
            continue;
        }
        let pointer = origin_from(*point);

        let Some(last_point) = state.last_point else {
            state.last_point = Some(pointer);
            continue;
        };
        state.last_point = Some(pointer);

        let dx = (pointer.x - last_point.x) * 5;
        if dx.abs() < 1 {
            continue;
        }

        let window_id = if let Some(window_id) = state.window_id {
            window_id
        } else {
            let Ok(window_id) = window_manager.find_window_at_point(point) else {
                continue;
            };
            state.window_id = Some(window_id);
            window_id
        };

        let Some((window, entity)) = windows.find(window_id) else {
            continue;
        };
        let (strip_entity, strip, strip_position) = *active_workspace;
        let floating = !strip.contains(entity);

        let mut frame = window.frame();
        let center = frame.center();

        if pointer.x < center.x {
            if floating && let Some(mut origin) = windows.origin(entity) {
                // For floating windows, move the window itself.
                origin.x += dx;
                commands.reposition_entity(entity, origin);
            } else {
                // Resize Left Edge: increase/decrease width AND shift the strip so the right edge stays
                // anchored.
                let mut origin = strip_position.0;
                origin.x += dx;
                commands.reposition_entity(strip_entity, origin);
            }

            frame.min.x += dx;
        } else {
            frame.max.x += dx;
        }
        commands.resize_entity(entity, frame.size());
    }
}

#[derive(Default)]
pub(super) struct WarpVelocityState {
    last: Option<(Origin, Instant)>,
}

fn rect_contains(rect: IRect, point: Origin) -> bool {
    (rect.min.x..rect.max.x).contains(&point.x) && (rect.min.y..rect.max.y).contains(&point.y)
}

#[derive(Clone, Copy)]
struct RowDisplay {
    frame: IRect,
    row_offset_x: i32,
}

struct WarpDesk {
    row: Vec<RowDisplay>,
    lowest: IRect,
}

impl WarpDesk {
    fn from_displays(
        displays: &Query<&Display>,
        config: &Config,
        warp_direction: i16,
    ) -> Option<Self> {
        if !config.horizontal_mouse_warp_lowest_below() {
            return None;
        }
        // The pointer can be in a menu bar, which `Display::bounds` leaves out.
        let frames = displays
            .iter()
            .map(Display::physical_bounds)
            .collect::<Vec<_>>();
        Self::new(&frames, warp_direction)
    }

    fn new(frames: &[IRect], warp_direction: i16) -> Option<Self> {
        if frames.len() < 3 {
            return None;
        }
        let lowest = *frames.iter().max_by_key(|frame| frame.min.y)?;
        let mut upward = frames
            .iter()
            .copied()
            .filter(|frame| *frame != lowest)
            .collect::<Vec<_>>();
        upward.sort_by_key(|frame| std::cmp::Reverse((frame.min.y, frame.min.x)));
        upward.dedup_by_key(|frame| (frame.min.y, frame.min.x));
        // The display above the lowest one keeps its macOS x.
        // Thus the native crossing between these two displays agrees with the row.
        let anchor = *upward.first()?;
        let row = upward
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let row_min_x = if warp_direction > 0 {
                    anchor.min.x + upward[..index].iter().map(IRect::width).sum::<i32>()
                } else {
                    anchor.max.x - upward[..=index].iter().map(IRect::width).sum::<i32>()
                };
                RowDisplay {
                    frame: *frame,
                    row_offset_x: row_min_x - frame.min.x,
                }
            })
            .collect();
        Some(Self { row, lowest })
    }

    fn is_lowest(&self, display: &Display) -> bool {
        self.lowest == display.physical_bounds()
    }

    fn landing(
        &self,
        previous: Option<Origin>,
        point: Origin,
        delta_y: i64,
        edge: i32,
        inset: i32,
    ) -> Option<Origin> {
        previous
            .and_then(|previous| self.row_crossing(previous, point, inset))
            .or_else(|| self.down_from_row_overhang(previous, point, delta_y, edge, inset))
            .or_else(|| self.up_from_lowest(previous, point, delta_y, edge, inset))
    }

    fn member(&self, point: Origin) -> Option<RowDisplay> {
        self.row
            .iter()
            .copied()
            .find(|display| rect_contains(display.frame, point))
    }

    // macOS moves the pointer across a shared edge without a stop at the edge.
    // Thus only the previous point shows that the pointer left a row display.
    fn row_crossing(&self, previous: Origin, point: Origin, inset: i32) -> Option<Origin> {
        // An event from before a sideways warp still has the old position.
        // It jumps by the width of a display, which a real crossing does not.
        const MAX_CROSSING_STEP_X: i32 = 400;
        let from = self.member(previous)?;
        let to = self.member(point)?;
        if to.frame == from.frame || (point.x - previous.x).abs() >= MAX_CROSSING_STEP_X {
            return None;
        }
        let row_x = previous.x + from.row_offset_x;
        let over_lowest = (self.lowest.min.x..self.lowest.max.x).contains(&row_x);
        if to.frame.min.y >= from.frame.max.y {
            Some(if over_lowest {
                Origin::new(
                    row_x.clamp(self.lowest.min.x + inset, self.lowest.max.x - 1 - inset),
                    self.lowest.min.y + inset,
                )
            } else {
                Origin::new(previous.x, from.frame.max.y - 1)
            })
        } else {
            (to.frame.max.y <= from.frame.min.y).then(|| Origin::new(previous.x, from.frame.min.y))
        }
    }

    /// When an upper row display overhangs the monitor beneath it in macOS's
    /// virtual layout, downward motion clamps at its bottom edge without
    /// entering another row display. Detect downward pushes via `delta_y`
    /// (which stays positive even when macOS clamps the Y coordinate) or a
    /// downward position step within the same display.
    fn down_from_row_overhang(
        &self,
        previous: Option<Origin>,
        point: Origin,
        delta_y: i64,
        edge: i32,
        inset: i32,
    ) -> Option<Origin> {
        let from = self.member(point)?;
        if previous.is_some_and(|prev| !rect_contains(from.frame, prev)) {
            return None;
        }
        let at_bottom_edge = from.frame.max.y - point.y <= edge;
        let moving_down =
            delta_y > 0 || (delta_y == 0 && previous.is_some_and(|prev| point.y > prev.y));
        let below = Origin::new(point.x, from.frame.max.y);
        let native_below = rect_contains(self.lowest, below) || self.member(below).is_some();
        if !at_bottom_edge || !moving_down || native_below {
            return None;
        }
        let row_x = point.x + from.row_offset_x;
        (self.lowest.min.x..self.lowest.max.x)
            .contains(&row_x)
            .then(|| {
                Origin::new(
                    row_x.clamp(self.lowest.min.x + inset, self.lowest.max.x - 1 - inset),
                    self.lowest.min.y + inset,
                )
            })
    }

    fn up_from_lowest(
        &self,
        previous: Option<Origin>,
        point: Origin,
        delta_y: i64,
        edge: i32,
        inset: i32,
    ) -> Option<Origin> {
        if previous.is_some_and(|prev| !rect_contains(self.lowest, prev)) {
            return None;
        }
        let on_top_edge = rect_contains(self.lowest, point) && point.y - self.lowest.min.y < edge;
        let moving_up =
            delta_y < 0 || (delta_y == 0 && previous.is_some_and(|prev| point.y < prev.y));
        let native_above = self
            .member(Origin::new(point.x, self.lowest.min.y - 1))
            .is_some();
        if !on_top_edge || !moving_up || native_above {
            return None;
        }
        self.row
            .iter()
            .find(|display| {
                (display.frame.min.x + display.row_offset_x
                    ..display.frame.max.x + display.row_offset_x)
                    .contains(&point.x)
            })
            .map(|display| {
                Origin::new(
                    point.x - display.row_offset_x,
                    display.frame.max.y - 1 - inset,
                )
            })
    }
}

const EDGE_THRESHOLD: i32 = 3;
/// Inset from the destination display's edge so the cursor doesn't land
/// directly on the threshold and immediately re-warp back.
const LANDING_INSET: i32 = 6;
/// Extrapolate pre-warp horizontal motion by this duration so the cursor
/// does not feel like it starts from rest on the target display.
const CARRY_DURATION: Duration = Duration::from_millis(30);
/// Cap on how far the carry-over can push past the inset, in pixels.
const MAX_CARRY_PX: i32 = 80;
/// Stale velocity samples (e.g. from a prior gesture) shouldn't carry.
const VELOCITY_FRESHNESS: Duration = Duration::from_millis(80);

fn horizontal_warp_mouse_trigger(
    mut messages: MessageReader<PointerMotion>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<WarpVelocityState>,
) {
    let mut desk: Option<Option<WarpDesk>> = None;
    for motion in messages.read() {
        let now = Instant::now();
        let point = origin_from(motion.point);
        let delta_y = motion.delta_y;

        // Compute velocity from the previous sample before deciding whether to
        // warp, then refresh the sample so subsequent events build on this one.
        let recent_previous = state
            .last
            .filter(|(_, t)| now.saturating_duration_since(*t) <= VELOCITY_FRESHNESS);
        let velocity_x = recent_previous.and_then(|(prev, t)| {
            let dt = now.saturating_duration_since(t);
            if dt.is_zero() {
                return None;
            }
            let dx = f64::from(point.x - prev.x);
            Some(dx / dt.as_secs_f64())
        });
        let previous = recent_previous.map(|(previous, _)| previous);
        state.last = Some((point, now));

        let Some(warp_direction) = config.horizontal_mouse_warp() else {
            return;
        };
        if displays.count() < 2 {
            return;
        }

        let desk = desk
            .get_or_insert_with(|| WarpDesk::from_displays(&displays, &config, warp_direction))
            .as_ref();

        if let Some(landing) = desk
            .and_then(|desk| desk.landing(previous, point, delta_y, EDGE_THRESHOLD, LANDING_INSET))
        {
            window_manager.warp_mouse(landing);
            state.last = Some((landing, now));
            continue;
        }

        let Some(current_display) = displays
            .iter()
            .find(|display| rect_contains(display.bounds(), point))
        else {
            continue;
        };
        if desk.is_some_and(|desk| desk.is_lowest(current_display)) {
            continue;
        }

        let on_left_edge = (point.x - current_display.bounds().min.x).abs() < EDGE_THRESHOLD;
        let on_right_edge = (current_display.bounds().max.x - point.x).abs() < EDGE_THRESHOLD;
        if !on_left_edge && !on_right_edge {
            continue;
        }

        let mut target_displays = displays
            .iter()
            .filter(|display| desk.is_none_or(|desk| !desk.is_lowest(display)))
            .filter(|display| {
                let above = display.bounds().min.y < current_display.bounds().min.y;
                let below = display.bounds().min.y > current_display.bounds().min.y;
                if on_left_edge {
                    if warp_direction > 0 { below } else { above }
                } else if warp_direction > 0 {
                    above
                } else {
                    below
                }
            })
            .collect::<Vec<_>>();

        target_displays
            .sort_by_key(|display| (display.bounds().min.y - current_display.bounds().min.y).abs());
        let Some(warp_to) = target_displays.first() else {
            continue;
        };
        let target = warp_to.bounds();

        // Land at the *opposite* edge so the cursor flow is continuous: leaving
        // the right edge appears at the left edge of the target, and vice versa.
        // Carry over horizontal velocity so the cursor does not feel "stuck" at
        // the edge - extrapolate motion forward into the target display.
        let carry = velocity_x
            .map_or(0, |v| round_px(v * CARRY_DURATION.as_secs_f64()))
            .clamp(-MAX_CARRY_PX, MAX_CARRY_PX);
        let target_x = if on_left_edge {
            // Cursor was moving leftward; carry is negative. Push further from
            // the right edge of the target.
            (target.max.x - LANDING_INSET + carry).clamp(target.min.x + 1, target.max.x - 1)
        } else {
            // Cursor was moving rightward; carry is positive. Push further from
            // the left edge of the target.
            (target.min.x + LANDING_INSET + carry).clamp(target.min.x + 1, target.max.x - 1)
        };

        // Preserve relative Y offset from the source display's top so vertical
        // motion feels continuous (matches macOS's behavior for side-by-side
        // displays). Apply the configured offset signed by warp direction:
        // positive offset pushes the cursor lower when warping downward, and
        // raises it when warping upward - matching the user's physical desk
        // arrangement (e.g. monitor sitting below the laptop).
        // If the equivalent position falls outside the target's Y range (e.g. a
        // tall portrait monitor's bottom region maps off a shorter laptop's
        // bottom), skip the warp - matches macOS native side-by-side behavior
        // where the cursor can only cross at Y values where both displays exist.
        let relative_y = point.y - current_display.bounds().min.y;
        let direction_sign = if target.min.y > current_display.bounds().min.y {
            1
        } else {
            -1
        };
        let signed_offset = config.horizontal_mouse_warp_offset() * direction_sign;
        let target_y = target.min.y + relative_y + signed_offset;
        if target_y < target.min.y || target_y >= target.max.y {
            continue;
        }

        let landing = Origin::new(target_x, target_y);
        window_manager.warp_mouse(landing);
        // Reset the velocity sample to the landing point so the next motion
        // event computes velocity from the new position, not the pre-warp one.
        state.last = Some((landing, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ecs::DockPosition;
    use crate::manager::{Display, Origin};
    use bevy::math::IRect;

    fn make_display() -> Display {
        // 1024x768 test display with a 20px menubar, mirrors the values in src/tests.rs.
        Display::new(
            1,
            IRect {
                min: Origin::new(0, 0),
                max: Origin::new(1024, 768),
            },
            20,
        )
    }

    #[test]
    fn corner_dead_zone_no_dock() {
        let display = make_display();
        let config = Config::default();

        // Inside the 30x30 bottom-right corner.
        assert!(is_in_corner_dead_zone(
            Origin::new(1000, 750),
            &display,
            None,
            &config
        ));
        assert!(is_in_corner_dead_zone(
            Origin::new(1024, 768),
            &display,
            None,
            &config
        ));

        // Just outside the corner (one pixel above/left).
        assert!(!is_in_corner_dead_zone(
            Origin::new(993, 750),
            &display,
            None,
            &config
        ));
        assert!(!is_in_corner_dead_zone(
            Origin::new(1000, 737),
            &display,
            None,
            &config
        ));
    }

    #[test]
    fn corner_dead_zone_with_bottom_dock() {
        let display = make_display();
        let config = Config::default();
        let dock = DockPosition::Bottom(80);

        // With an 80px dock at the bottom, actual_display_bounds.max.y = 768 - 80 = 688.
        // Corner zone is now y >= 658.
        assert!(is_in_corner_dead_zone(
            Origin::new(1000, 680),
            &display,
            Some(&dock),
            &config
        ));
        // Just outside the corner zone (point within display bounds but outside corner).
        assert!(!is_in_corner_dead_zone(
            Origin::new(1000, 657),
            &display,
            Some(&dock),
            &config
        ));
    }
}
