use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::Remove;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::time::{Duration, Instant};
use tracing::{Level, instrument};

use crate::commands::{Command, Direction, Operation, focus_with_verification};
use crate::config::Config;
use crate::config::swipe::SwipeGestureDirection;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::{Column, LayoutStrip, strip_content_range};
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, ManualStripOffset, MissionControlActive, Position, RepositionMarker,
    ScrollInput, Scrolling, SendMessageTrigger,
};
use crate::errors::Result;
use bevy::ecs::schedule::common_conditions::on_message;

use crate::events::{Event, InputEvent};
use crate::manager::{Window, WindowManager};
use crate::platform::Modifiers;
use crate::util::round_px;

pub struct ScrollEventsPlugin;

/// Upper bound on the timestep used to *advance* the strip.
///
/// `Time::<Virtual>` is deliberately given a 10s `max_delta` (see [`crate::ecs`])
/// so `elapsed()` keeps tracking wall-clock across a stalled frame — the
/// finger-lift timeout below depends on that. It is the wrong bound for
/// integration: a frame delayed by a blocking AX round trip would otherwise
/// advance the strip by `velocity * dt * viewport_width` in one step and slam
/// it into the far clamp bound. Integrate at most one 30fps frame per update,
/// however long the frame actually took.
const MAX_STEP_SECS: f64 = 1.0 / 30.0;

/// Lower bound on the timestep used to *derive* a velocity from a gesture delta.
///
/// Here the true delta is what we want — a backlog drained after a stall really
/// does represent that much finger travel over that much time. The hazard is
/// the opposite one: on a catch-up frame `dt` approaches zero, and
/// `gesture_delta / dt` diverges.
const MIN_STEP_SECS: f64 = 1.0 / 1000.0;

/// The on-screen strip, its origin, and the scroll state being applied to it.
type ScrollingStrip<'w, 's> = Single<
    'w,
    's,
    (
        &'static LayoutStrip,
        &'static mut Position,
        &'static mut Scrolling,
    ),
    (With<ActiveWorkspaceMarker>, Without<Window>),
>;

/// The active strip a swipe drives: its origin, any animation still in
/// flight, and the scroll state if a swipe is already under way.
type SwipedStrip = (
    Entity,
    &'static Position,
    Option<&'static RepositionMarker>,
    Option<&'static mut Scrolling>,
);

/// The active strip once a swipe has ended, and whether a deliberate move has
/// taken it over.
type SettlingStrip = (
    Entity,
    &'static LayoutStrip,
    &'static mut Position,
    &'static mut Scrolling,
    Has<RepositionMarker>,
);

impl Plugin for ScrollEventsPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(finish_settle_on_deactivate);
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        // Only the two gesture systems are gated on an input event. The rest of
        // the chain (inertia, snap force, integrator) must keep running after
        // the fingers stop sending events, since that's when they take over.
        app.add_systems(
            Update,
            (
                vertical_swipe_gesture
                    .run_if(mission_control_inactive)
                    .run_if(on_message::<InputEvent>),
                (
                    swipe_gesture
                        .run_if(mission_control_inactive)
                        .run_if(on_message::<InputEvent>),
                    apply_inertia,
                    apply_snap_force,
                    settle_strip,
                    scrolling_integrator,
                    apply_scrolling_constraints,
                    swiping_timeout,
                )
                    .chain(),
            ),
        );
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn swipe_gesture(
    mut messages: MessageReader<InputEvent>,
    active_display: ActiveDisplay,
    mut active_workspace: Single<SwipedStrip, With<ActiveWorkspaceMarker>>,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let swipe_sensitivity = config.swipe_sensitivity();
    let mut total_delta = 0.0;
    let mut gesture_delta = 0.0;
    let mut touchpad_down = false;
    let mut has_scroll_event = false;
    let mut has_gesture_event = false;

    // Normalization: Touchpad deltas are typically small fractions.
    // Scroll wheel deltas can be larger. We scale it down slightly
    // to match the "feel" of a finger swipe.
    const SCROLL_SCALE_UPPER: f64 = 0.15;
    const SCROLL_SCALE_LOWER: f64 = 0.005;
    const SCROLL_FULL_RANGE: f64 = 2.0;
    let scroll_scale = SCROLL_SCALE_LOWER
        + ((SCROLL_SCALE_UPPER - SCROLL_SCALE_LOWER) / SCROLL_FULL_RANGE) * swipe_sensitivity;

    for InputEvent(event) in messages.read() {
        match event {
            Event::TouchpadDown => {
                touchpad_down = true;
                total_delta = 0.0;
            }
            Event::Scroll { delta } => {
                total_delta += *delta * scroll_scale;
                has_scroll_event = true;
            }
            Event::Swipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers) =>
            {
                total_delta += delta;
                gesture_delta += delta;
                has_scroll_event = true;
                has_gesture_event = true;
            }
            _ => (),
        }
    }

    if !touchpad_down && !has_scroll_event {
        return;
    }

    let (entity, position, reposition, scrolling) = &mut *active_workspace;
    let input = if has_gesture_event {
        ScrollInput::Gesture
    } else {
        ScrollInput::Wheel
    };

    // The user is driving the strip by hand now, so an earlier deliberate
    // placement (center, snap) no longer describes where they want it.
    if let Ok(mut entity_commands) = commands.get_entity(*entity) {
        entity_commands.try_remove::<ManualStripOffset>();
    }

    if touchpad_down && let Some(scrolling) = scrolling.as_mut() {
        scrolling.velocity = 0.0;
        scrolling.is_user_swiping = true;
        scrolling.last_event = time.elapsed();
        scrolling.settle = None;
    }

    if has_scroll_event {
        let viewport_width = f64::from(active_display.bounds().width());
        let direction = direction_modifier(&config);

        // Floored, not capped: dividing by the real elapsed time is correct
        // here, but a catch-up frame can drive `dt` arbitrarily close to zero.
        let dt = time.delta_secs_f64().max(MIN_STEP_SECS);
        let new_velocity = if has_gesture_event {
            gesture_delta * swipe_sensitivity / dt
        } else {
            0.0
        };

        if let Some(scrolling) = scrolling.as_mut() {
            // Native modifier-scroll events already include macOS momentum.
            // Add synthetic inertia only for raw multi-finger gestures.
            scrolling.velocity = if has_gesture_event {
                // Smoothen gesture velocity changes using EMA.
                0.3 * new_velocity + 0.7 * scrolling.velocity
            } else {
                0.0
            };
            scrolling.is_user_swiping = true;
            scrolling.last_event = time.elapsed();
            scrolling.input = input;
            scrolling.settle = None;
            scrolling.position += total_delta * viewport_width * direction * swipe_sensitivity;
        } else if let Ok(mut entity_commands) = commands.get_entity(*entity) {
            entity_commands.try_insert(Scrolling {
                velocity: new_velocity,
                position: f64::from(position.0.x)
                    + total_delta * viewport_width * direction * swipe_sensitivity,
                is_user_swiping: true,
                last_event: time.elapsed(),
                input,
                // A strip still animating is headed for its target, so that is
                // where it rests.
                anchor: f64::from(reposition.map_or(position.0.x, |target| target.0.x)),
                settle: None,
            });
        }
    }
}

#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swiping_timeout(
    strips: Populated<(Entity, &mut Scrolling, Has<ActiveWorkspaceMarker>), With<LayoutStrip>>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut commands: Commands,
) {
    const MIN_VELOCITY_PX: f64 = 5.0;
    let settling = settles(&config, &active_display);
    // Predicts the distance the integrator is about to move, so it has to use
    // the same bounded step the integrator does.
    let dt = time.delta_secs_f64().min(MAX_STEP_SECS);
    let viewport_width = f64::from(active_display.bounds().width());

    for (entity, mut scroll, active) in strips {
        if time.elapsed().abs_diff(scroll.last_event) > scroll.input.idle_threshold() {
            scroll.is_user_swiping = false;
            // `settle_strip` owns the active strip from here, and focus goes to
            // the column it lands on, not to whatever slid under the pointer.
            if settling && active {
                continue;
            }

            if scroll.velocity.abs() * dt * viewport_width < MIN_VELOCITY_PX
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_remove::<Scrolling>();
            }
            if let Some(point) = window_manager.cursor_position() {
                commands.trigger(SendMessageTrigger(Event::MouseMoved {
                    point,
                    modifiers: Modifiers::empty(),
                }));
            }
        }
    }
}

/// A strip that stops being active mid-settle lands at once. `settle_strip`
/// only drives the active strip, so otherwise its settle would never finish.
fn finish_settle_on_deactivate(
    trigger: On<Remove, ActiveWorkspaceMarker>,
    mut strips: Query<(&mut Position, &Scrolling), With<LayoutStrip>>,
    mut commands: Commands,
) {
    let entity = trigger.entity;
    let Ok((mut position, scroll)) = strips.get_mut(entity) else {
        return;
    };
    let Some(target) = scroll.settle else {
        return;
    };
    position.x = round_px(target);
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_remove::<Scrolling>();
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn apply_inertia(
    mut strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    time: Res<Time>,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    for (_, mut scroll) in &mut strips {
        if scroll.is_user_swiping || scroll.settle.is_some() {
            continue;
        }

        if scroll.velocity.abs() > 0.001 {
            // Unbounded on purpose: decay is monotonic toward zero, so a long
            // frame can only shed more momentum, never overshoot. After a stall
            // that is what we want.
            let decay_rate = config.swipe_deceleration();
            scroll.velocity *= (-decay_rate * dt).exp();
        } else {
            scroll.velocity = 0.0;
        }
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn apply_snap_force(
    mut strip: Single<(&LayoutStrip, &Position, &mut Scrolling)>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    time: Res<Time>,
) {
    const CENTER_MAGNETIC_FORCE: f64 = 10.0;
    const SNAP_DISPLAY_RATIO: f64 = 0.45;

    if !config.auto_center() {
        return;
    }

    let viewport = active_display.actual_bounds(&config);
    let viewport_center = viewport.center().x;
    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

    let (strip, position, ref mut scroll) = *strip;
    if scroll.is_user_swiping || scroll.velocity.abs() > 0.5 {
        return;
    }

    let target_offset = strip
        .all_columns()
        .into_iter()
        .filter_map(|entity| {
            windows
                .layout_position(entity)
                .map(|p| p.0.x)
                .zip(Some(entity))
        })
        .map(|(position, entity)| {
            let col_width = windows.moving_frame(entity).map_or(0, |f| f.width());
            viewport_center - (position + col_width / 2)
        })
        .min_by_key(|target| (position.x - target).abs())
        .unwrap_or(position.x);

    let dist_to_snap = f64::from(position.x - target_offset);
    if dist_to_snap.abs() < snap_threshold {
        // This is an exponential approach, and it only pulls *toward* the
        // target while the factor stays under 1. Past that it overshoots to the
        // far side; at a 10s delta it would fling the strip 100x the distance
        // the wrong way. Clamping the factor rather than `dt` keeps that true
        // for any timestep and any `CENTER_MAGNETIC_FORCE`.
        let approach = (time.delta_secs_f64() * CENTER_MAGNETIC_FORCE).min(1.0);
        scroll.position -= dist_to_snap * approach;
    }
}

/// Whether a swipe ends by settling on a column. `auto_center` keeps
/// upstream's centering snap instead, and a native fullscreen Space has no
/// strip to settle.
fn settles(config: &Config, active_display: &ActiveDisplay) -> bool {
    !config.auto_center() && active_display.fullscreen().is_none()
}

/// The range of strip offsets that show the whole strip, when it fits the
/// viewport.
fn fitted_offsets(strip: &LayoutStrip, windows: &Windows, viewport: IRect) -> Option<(f64, f64)> {
    let content = strip_content_range(strip, windows)?;
    (content.end - content.start <= viewport.width()).then(|| {
        (
            f64::from(viewport.min.x - content.start),
            f64::from(viewport.max.x - content.end),
        )
    })
}

/// Rubber-band resistance: the strip follows a stretch almost one to one at
/// first, and never travels more than an eighth of the viewport.
fn resist(stretch: f64, viewport_width: f64) -> f64 {
    let limit = viewport_width / 8.0;
    stretch * limit / (limit + stretch.abs())
}

/// The column a strip resting at offset `resting` lands on, and the offset
/// that shows it fully. The column nearest the viewport center wins; a
/// partly hidden one ends with its edge on the viewport edge, and a fully
/// visible one leaves the strip where it rests. This is the projection
/// `ensure_visible_in_strip` makes for keyboard focus. Continuous swipe lets
/// the fingers pull an end column to the far edge, so the offset is then
/// held to the range that leaves no empty space beside the overflowing strip.
fn landing(
    resting: i32,
    strip: &LayoutStrip,
    windows: &Windows,
    viewport: IRect,
) -> Option<(Column, i32)> {
    let content = strip_content_range(strip, windows)?;
    let edges = (viewport.max.x - content.end).min(viewport.min.x - content.start)
        ..=(viewport.min.x - content.start);
    let center = viewport.center().x;
    strip
        .columns()
        .filter_map(|column| {
            let top = column.top()?;
            let slot = windows.layout_position(top)?.x;
            let width = windows.moving_frame(top)?.width();
            Some((column, slot, width))
        })
        .min_by_key(|(_, slot, width)| {
            let min = resting + slot;
            let max = min + width;
            let outside = (min - center).max(center - max).max(0);
            (outside, (min + width / 2 - center).abs())
        })
        .map(|(column, slot, width)| {
            let far_edge = viewport.max.x - width;
            let shown =
                (resting + slot).clamp(viewport.min.x.min(far_edge), viewport.min.x.max(far_edge));
            let offset = (shown - slot).clamp(*edges.start(), *edges.end());
            (column.clone(), offset)
        })
}

/// Ends a swipe on a column once the fingers lift, with `auto_center` off.
///
/// On lift it projects the momentum to where the strip would come to rest,
/// picks the column [`landing`] gives for that offset, and focuses the window
/// in it that had focus last. A strip that fits the viewport springs back to
/// where the swipe began instead, and focus stays. The strip then travels
/// there on a critically damped spring that starts at the glide's velocity:
/// with the target at the projected rest, at twice the deceleration rate,
/// it begins without a jolt and lands without overshoot.
#[instrument(level = Level::TRACE, skip_all)]
fn settle_strip(
    strip: Single<SettlingStrip, (With<ActiveWorkspaceMarker>, Without<Window>)>,
    active_display: ActiveDisplay,
    windows: Windows,
    focus_history: Res<FocusHistory>,
    config: Res<Config>,
    time: Res<Time>,
    mut commands: Commands,
) {
    const DONE_PX: f64 = 0.5;

    if !settles(&config, &active_display) {
        return;
    }
    let (entity, strip, mut position, mut scroll, repositioning) = strip.into_inner();
    if scroll.is_user_swiping {
        return;
    }
    let finish = |commands: &mut Commands| {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<Scrolling>();
        }
    };
    // A deliberate move (keyboard focus, Center) has taken the strip over.
    if repositioning {
        finish(&mut commands);
        return;
    }

    let viewport = active_display.actual_bounds(&config);
    let viewport_width = f64::from(viewport.width());
    let px_per_velocity = viewport_width * direction_modifier(&config);
    let deceleration = config.swipe_deceleration();

    let target = if let Some(target) = scroll.settle {
        target
    } else if let Some(fitted) = fitted_offsets(strip, &windows, viewport) {
        // Start from where the resisting strip shows, not the raw fingers.
        scroll.position = f64::from(position.x);
        scroll.velocity = 0.0;
        let target = scroll.anchor.clamp(fitted.0, fitted.1);
        scroll.settle = Some(target);
        target
    } else {
        let natural = scroll.position + scroll.velocity * px_per_velocity / deceleration;
        let get_window_frame = |entity| windows.moving_frame(entity);
        let Some((column, target)) = clamp_viewport_offset(
            round_px(natural),
            strip,
            &windows,
            &get_window_frame,
            &viewport,
            &config,
        )
        .and_then(|resting| landing(resting, strip, &windows, viewport)) else {
            finish(&mut commands);
            return;
        };
        let target = f64::from(target);
        scroll.settle = Some(target);
        if let Some(window) = focus_history.last_focused_in(&column) {
            let focused = windows.focused().map(|(_, focused)| focused);
            if focused != Some(window) {
                focus_with_verification(window, focused, &mut commands);
            }
        }
        target
    };

    // An unset animation speed means instant movement, which is how Miri
    // applies Reduce Motion and its disable-animation setting.
    if config.options().animation_speed.is_none() {
        position.x = round_px(target);
        finish(&mut commands);
        return;
    }

    let omega = 2.0 * deceleration;
    let dt = time.delta_secs_f64().min(MAX_STEP_SECS);
    let offset = scroll.position - target;
    let velocity = scroll.velocity * px_per_velocity;
    let decay = (-omega * dt).exp();
    let drive = velocity + omega * offset;
    let offset = (offset + drive * dt) * decay;
    let velocity = (velocity - omega * drive * dt) * decay;

    if offset.abs() < DONE_PX && velocity.abs() * MAX_STEP_SECS < DONE_PX {
        position.x = round_px(target);
        finish(&mut commands);
        return;
    }
    scroll.position = target + offset;
    scroll.velocity = velocity / px_per_velocity;
}

/// Natural direction moves the strip left (a negative offset) for a positive
/// delta, the fingers moving left.
fn direction_modifier(config: &Config) -> f64 {
    match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn scrolling_integrator(
    mut strip: Single<&mut Scrolling, With<LayoutStrip>>,
    time: Res<Time>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64().min(MAX_STEP_SECS);
    let viewport = active_display.actual_bounds(&config);
    let viewport_width = f64::from(viewport.width());

    let direction = direction_modifier(&config);

    let scroll = &mut *strip;
    if scroll.settle.is_none() && scroll.velocity.abs() > 0.0001 {
        scroll.position += scroll.velocity * dt * viewport_width * direction;
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn apply_scrolling_constraints(
    mut strip: ScrollingStrip,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
) {
    let viewport = active_display.actual_bounds(&config);
    let (strip, ref mut position, ref mut scroll) = *strip;

    if scroll.is_user_swiping
        && settles(&config, &active_display)
        && let Some(fitted) = fitted_offsets(strip, &windows, viewport)
    {
        // Keep the raw finger position in `scroll.position`, so the stretch
        // eases off along the same curve when the fingers turn back.
        let anchor = scroll.anchor.clamp(fitted.0, fitted.1);
        let stretch = scroll.position - anchor;
        position.x = round_px(anchor + resist(stretch, f64::from(viewport.width())));
        return;
    }

    let get_window_frame = |entity| windows.moving_frame(entity);
    if let Some(clamped_offset) = clamp_viewport_offset(
        round_px(scroll.position),
        strip,
        &windows,
        &get_window_frame,
        &viewport,
        &config,
    ) {
        position.x = clamped_offset;
        // A settle's last pixels move less than one per frame, so it keeps
        // the fraction rounding would discard unless the clamp bit.
        if scroll.settle.is_none() || clamped_offset != round_px(scroll.position) {
            scroll.position = f64::from(clamped_offset);
        }
    } else {
        scroll.velocity = 0.0;
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn clamp_viewport_offset<W>(
    current_offset: i32,
    layout_strip: &LayoutStrip,
    windows: &Windows,
    get_window_frame: &W,
    viewport: &IRect,
    config: &Config,
) -> Option<i32>
where
    W: Fn(Entity) -> Option<IRect>,
{
    let total_strip_width = layout_strip
        .last()
        .ok()
        .and_then(|column| column.top())
        .and_then(|entity| {
            windows
                .layout_position(entity)
                .zip(get_window_frame(entity))
        })
        .map(|(position, frame)| position.x + frame.width())?;

    let continuous_swipe = config.continuous_swipe();
    let strip_position = |column: Result<Column>| {
        column
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.layout_position(entity))
            .map(|position| position.0.x)
    };

    let left_snap = strip_position(layout_strip.last());
    let right_snap = strip_position(layout_strip.first());

    Some(
        if continuous_swipe && let Some((left_snap, right_snap)) = left_snap.zip(right_snap) {
            // Allow to scroll away until the last or first window snaps.
            current_offset.clamp(viewport.min.x - left_snap, viewport.max.x - right_snap)
        } else if viewport.width() < total_strip_width {
            // Snap the strip directly to the edges.
            current_offset.clamp(viewport.max.x - total_strip_width, viewport.min.x)
        } else {
            // Snap the strip directly to the edges.
            current_offset.clamp(viewport.min.x, viewport.max.x - total_strip_width)
        },
    )
}

#[derive(Default)]
struct VerticalGestureState {
    accumulated: f64,
    last_event: Option<Instant>,
    fired: bool,
}

#[instrument(level = Level::TRACE, skip_all)]
fn vertical_swipe_gesture(
    mut messages: MessageReader<InputEvent>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
    mut state: Local<VerticalGestureState>,
) {
    const GESTURE_TIMEOUT: Duration = Duration::from_millis(150);

    if active_display.fullscreen().is_some() {
        return;
    }

    // Reset state when the gesture times out (fingers lifted).
    if let Some(last) = state.last_event
        && last.elapsed() > GESTURE_TIMEOUT
    {
        state.accumulated = 0.0;
        state.fired = false;
    }

    for InputEvent(event) in messages.read() {
        match event {
            Event::VerticalScrollTick { delta } => {
                switch_virtual_workspace(*delta, &config, &mut commands);
            }
            Event::VerticalSwipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers) =>
            {
                state.last_event = Some(Instant::now());

                if !state.fired {
                    state.accumulated += delta;
                }
            }
            _ => {}
        }
    }

    // Threshold needs to be high enough that incidental vertical movement
    // during horizontal swipes doesn't trigger a workspace switch.
    let threshold = 0.15 / config.swipe_sensitivity();
    if state.accumulated.abs() >= threshold {
        switch_virtual_workspace(state.accumulated, &config, &mut commands);
        state.accumulated = 0.0;
        state.fired = true;
    }
}

fn switch_virtual_workspace(delta: f64, config: &Config, commands: &mut Commands) {
    let physical_finger_direction = if delta > 0.0 {
        Direction::South
    } else {
        Direction::North
    };
    let direction = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => physical_finger_direction.reverse(),
        SwipeGestureDirection::Reversed => physical_finger_direction,
    };
    commands.trigger(SendMessageTrigger(Event::Command {
        command: Command::Window(Operation::Virtual(direction)),
    }));
}
