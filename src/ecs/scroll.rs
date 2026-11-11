use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::Remove;
use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::SystemCondition as _;
use bevy::ecs::system::{Commands, Populated, Query, Res, ResMut, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::collections::VecDeque;
use std::time::Duration;
use tracing::{Level, instrument};

use crate::commands::{Direction, FocusSource, focus_with_verification};
use crate::config::Config;
use crate::config::swipe::{SwipeGestureDirection, focus_direction};
use crate::ecs::focus::{FocusHistory, PageHapticRequest};
use crate::ecs::layout::{Column, LayoutStrip, strip_content_range};
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, ManualStripOffset, MissionControlActive, Position, PreUpdateSet,
    RepositionMarker, ScrollInput, Scrolling, SendMessageTrigger, UpdateSet,
};
use crate::errors::Result;
use bevy::ecs::schedule::common_conditions::on_message;

use crate::events::{Event, InputEvent};
use crate::manager::{Window, WindowManager};
use crate::platform::Modifiers;
use crate::util::round_px;

pub struct ScrollEventsPlugin;

const PAGE_GESTURE_TIMEOUT: Duration = Duration::from_millis(150);

/// One gesture-earned focus step, routed to [`crate::commands::command_move_focus`].
#[derive(Message, Debug, Clone)]
pub(crate) struct PageFocusRequest {
    pub(crate) direction: Direction,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PageInFlight {
    Dispatched,
    AwaitingFocus {
        target: Entity,
        deadline: Option<Duration>,
    },
}

#[derive(Default, Resource)]
pub(crate) struct PageGestureState {
    accumulated_cm: f64,
    pending: VecDeque<(Direction, u64)>,
    pub(crate) in_flight: Option<PageInFlight>,
    last_event: Option<Duration>,
    active: bool,
    config: Option<(bool, Option<usize>, SwipeGestureDirection, f64, bool)>,
    pub(crate) generation: u64,
}

impl PageGestureState {
    pub(crate) fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.accumulated_cm = 0.0;
        self.pending.clear();
        self.in_flight = None;
        self.last_event = None;
        self.active = false;
    }

    fn has_work(&self) -> bool {
        self.active || !self.pending.is_empty() || self.in_flight.is_some()
    }

    pub(crate) fn await_focus(&mut self, target: Entity) {
        self.in_flight = Some(PageInFlight::AwaitingFocus {
            target,
            deadline: None,
        });
    }

    fn earn_steps(&mut self, threshold_cm: f64, gesture_direction: SwipeGestureDirection) {
        let travel = self.accumulated_cm.abs();
        if travel < threshold_cm {
            return;
        }
        let direction = focus_direction(gesture_direction, self.accumulated_cm);
        // Saturating float-to-integer conversion bounds an implausibly large
        // gesture while preserving the earned count for normal device input.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (travel / threshold_cm).floor() as u64;
        let remainder = travel % threshold_cm;
        self.accumulated_cm = self.accumulated_cm.signum() * remainder;
        if let Some((last_direction, count)) = self.pending.back_mut()
            && *last_direction == direction
            && let Some(merged) = count.checked_add(steps)
        {
            *count = merged;
            return;
        }
        self.pending.push_back((direction, steps));
    }

    fn next_step(&mut self) -> Option<Direction> {
        let (direction, count) = self.pending.front_mut()?;
        let direction = direction.clone();
        *count -= 1;
        if *count == 0 {
            self.pending.pop_front();
        }
        Some(direction)
    }
}

fn paging_enabled(config: Res<Config>, state: Res<PageGestureState>) -> bool {
    config.swipe_gesture_paging() || state.has_work() || state.config.as_ref().is_some_and(|c| c.0)
}

fn paging_active(config: Res<Config>, state: Res<PageGestureState>) -> bool {
    state.has_work()
        || (!config.swipe_gesture_paging() && state.config.as_ref().is_some_and(|c| c.0))
}

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
        app.init_resource::<PageGestureState>();
        app.add_message::<PageFocusRequest>();
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        app.add_systems(
            PreUpdate,
            paging_gesture
                .in_set(PreUpdateSet::Demux)
                .after(super::systems::demux_input_events)
                .run_if(mission_control_inactive)
                .run_if(paging_enabled)
                .run_if(on_message::<InputEvent>.or_else(paging_active)),
        );

        // Only the gesture system is gated on an input event. The rest of the
        // chain (inertia, snap force, integrator) must keep running after the
        // fingers stop sending events, since that's when they take over.
        app.add_systems(
            Update,
            (
                swipe_gesture
                    .run_if(mission_control_inactive)
                    .run_if(on_message::<InputEvent>),
                apply_inertia,
                apply_snap_force,
                settle_strip,
                scrolling_integrator,
                apply_scrolling_constraints,
                landing_detents,
                swiping_timeout,
            )
                .chain()
                .in_set(UpdateSet::Input),
        );
    }
}

fn paging_gesture(
    mut messages: MessageReader<InputEvent>,
    active_workspace: Single<Entity, With<ActiveWorkspaceMarker>>,
    config: Res<Config>,
    time: Res<Time>,
    mut state: ResMut<PageGestureState>,
    mut events: MessageWriter<PageFocusRequest>,
    mut commands: Commands,
) {
    let current_config = config.swipe_paging_config();
    if state.config.as_ref() != Some(&current_config) {
        state.reset();
        state.config = Some(current_config.clone());
    }
    if !current_config.0 {
        return;
    }

    if state
        .last_event
        .is_some_and(|last| time.elapsed().abs_diff(last) > PAGE_GESTURE_TIMEOUT)
    {
        state.accumulated_cm = 0.0;
        state.last_event = None;
        state.active = false;
    }

    for InputEvent(event) in messages.read() {
        match event {
            Event::TouchpadDown => {
                state.reset();
                state.active = true;
            }
            Event::TouchpadUp => {
                state.accumulated_cm = 0.0;
                state.last_event = None;
                state.active = false;
            }
            Event::Swipe {
                distance_cm,
                fingers,
                ..
            } if Some(*fingers) == current_config.1 => {
                state.last_event = Some(time.elapsed());
                state.active = true;
                if !distance_cm.is_finite() {
                    continue;
                }
                state.accumulated_cm += *distance_cm;
                if !state.accumulated_cm.is_finite() {
                    state.accumulated_cm = 0.0;
                    continue;
                }
                commands.entity(*active_workspace).try_remove::<Scrolling>();
                state.earn_steps(current_config.3, current_config.2.clone());
            }
            Event::Swipe { .. } => state.reset(),
            _ => {}
        }
    }

    if state.in_flight.is_none()
        && let Some(direction) = state.next_step()
    {
        state.in_flight = Some(PageInFlight::Dispatched);
        events.write(PageFocusRequest {
            direction,
            generation: state.generation,
        });
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
            Event::Swipe { delta, fingers, .. }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers)
                    && !config.swipe_gesture_paging() =>
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
        // Fingers that catch a settle start a new swipe from where the strip
        // was headed, so a second quick swipe steps on from there.
        if let Some(target) = scrolling.settle {
            scrolling.anchor = target;
        }
        scrolling.origin = scrolling.position;
        scrolling.detent = None;
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
                detent: None,
                origin: f64::from(position.0.x),
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
///
/// A window-landing `step` instead lands one column on from the column the
/// swipe began on, whatever the column widths, or springs back to where the
/// swipe began.
fn landing(
    resting: i32,
    step: Option<Step>,
    strip: &LayoutStrip,
    windows: &Windows,
    viewport: IRect,
) -> Option<(Column, i32)> {
    let content = strip_content_range(strip, windows)?;
    let edges = (viewport.max.x - content.end).min(viewport.min.x - content.start)
        ..=(viewport.min.x - content.start);
    let center = viewport.center().x;
    let columns: Vec<_> = strip
        .columns()
        .filter_map(|column| {
            let top = column.top()?;
            let slot = windows.layout_position(top)?.x;
            let width = windows.moving_frame(top)?.width();
            Some((column, slot, width))
        })
        .collect();
    let nearest = |resting: i32| {
        (0..columns.len()).min_by_key(|&index| {
            let (_, slot, width) = &columns[index];
            let min = resting + slot;
            let max = min + width;
            let outside = (min - center).max(center - max).max(0);
            (outside, (min + width / 2 - center).abs())
        })
    };
    let index = match step {
        None => nearest(resting)?,
        Some(step) => {
            let start = nearest(step.from)?;
            let index = match step.toward_end {
                Some(true) => (start + 1).min(columns.len() - 1),
                Some(false) => start.saturating_sub(1),
                None => start,
            };
            if index == start {
                return Some((
                    columns[start].0.clone(),
                    step.from.clamp(*edges.start(), *edges.end()),
                ));
            }
            index
        }
    };
    let (column, slot, width) = &columns[index];
    let far_edge = viewport.max.x - width;
    let shown = (resting + slot).clamp(viewport.min.x.min(far_edge), viewport.min.x.max(far_edge));
    let offset = (shown - slot).clamp(*edges.start(), *edges.end());
    Some(((*column).clone(), offset))
}

/// Where a window-landing swipe goes: one column on from the column it began
/// on, or back to it.
#[derive(Clone, Copy, Debug)]
struct Step {
    /// The strip offset the swipe began at.
    from: i32,
    /// Toward the last column, toward the first, or neither.
    toward_end: Option<bool>,
}

impl Step {
    /// The step a window-landing swipe takes from the column it began on: in
    /// the direction the fingers moved the strip once they have moved it
    /// `STEP_FRACTION` of the viewport, or in the direction of a `flick`,
    /// which wins since it is the latest intent.
    fn of(scroll: &Scrolling, position: f64, flick: Option<f64>, viewport_width: f64) -> Self {
        let moved = position - scroll.origin;
        let toward_end = match flick {
            Some(velocity) => Some(velocity < 0.0),
            None => (moved.abs() >= STEP_FRACTION * viewport_width).then_some(moved < 0.0),
        };
        Self {
            from: round_px(scroll.anchor),
            toward_end,
        }
    }
}

/// How far, as a fraction of the viewport width, the fingers move the strip
/// before a window-landing swipe steps to the next column. On a 2560-point
/// display at the default sensitivity this is about 2 cm of finger travel.
const STEP_FRACTION: f64 = 0.05;

/// The strip speed, in viewport widths per second, above which a lifted
/// window-landing swipe counts as a flick.
const FLICK_SPEED: f64 = 0.25;

/// Whether a swipe ends on one window instead of gliding on with momentum.
fn lands_on_window(config: &Config, input: ScrollInput) -> bool {
    input == ScrollInput::Gesture && config.swipe_gesture_window_landing()
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
///
/// A window-landing swipe ignores the momentum: it lands one column on from
/// where it began, or back there, as [`Step`] decides, and pulses if that is
/// not the column the last detent pulse named.
#[instrument(level = Level::TRACE, skip_all)]
#[allow(clippy::too_many_arguments)]
fn settle_strip(
    strip: Single<SettlingStrip, (With<ActiveWorkspaceMarker>, Without<Window>)>,
    active_display: ActiveDisplay,
    windows: Windows,
    focus_history: Res<FocusHistory>,
    config: Res<Config>,
    time: Res<Time>,
    mut haptics: MessageWriter<PageHapticRequest>,
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
        let window_landing = lands_on_window(&config, scroll.input);
        let (natural, step) = if window_landing {
            let flick =
                (scroll.velocity.abs() > FLICK_SPEED).then(|| scroll.velocity * px_per_velocity);
            (
                scroll.position,
                Some(Step::of(&scroll, scroll.position, flick, viewport_width)),
            )
        } else {
            (
                scroll.position + scroll.velocity * px_per_velocity / deceleration,
                None,
            )
        };
        let get_window_frame = |entity| windows.moving_frame(entity);
        let Some((column, target)) = clamp_viewport_offset(
            round_px(natural),
            strip,
            &windows,
            &get_window_frame,
            &viewport,
            &config,
        )
        .and_then(|resting| landing(resting, step, strip, &windows, viewport)) else {
            finish(&mut commands);
            return;
        };
        let target = f64::from(target);
        scroll.settle = Some(target);
        if window_landing {
            // The spring overshoots when it starts toward the target faster
            // than it would close the gap itself.
            let offset = scroll.position - target;
            let limit = omega_for(deceleration) * offset.abs() / px_per_velocity.abs();
            if scroll.velocity * px_per_velocity * offset < 0.0 {
                scroll.velocity = scroll.velocity.clamp(-limit, limit);
            }
            if config.swipe_gesture_haptics() && column.top() != scroll.detent {
                haptics.write(PageHapticRequest);
            }
        }
        if let Some(window) = focus_history.last_focused_in(&column) {
            let focused = windows.focused().map(|(_, focused)| focused);
            if focused != Some(window) {
                focus_with_verification(window, focused, None, FocusSource::Other, &mut commands);
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

    let omega = omega_for(deceleration);
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

/// The settle spring's rate: twice the deceleration rate, so a settle that
/// starts at the glide's velocity toward its projected rest never overshoots.
fn omega_for(deceleration: f64) -> f64 {
    2.0 * deceleration
}

/// Pulses while the fingers move a window-landing swipe, each time the
/// column the strip would land on changes: once the fingers have moved it far
/// enough to step, and again if they bring it back. A strip that fits the
/// viewport springs back instead, so it has no detents.
fn landing_detents(
    mut strip: ScrollingStrip,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    mut haptics: MessageWriter<PageHapticRequest>,
) {
    let (strip, ref position, ref mut scroll) = *strip;
    if !scroll.is_user_swiping
        || !lands_on_window(&config, scroll.input)
        || !settles(&config, &active_display)
    {
        return;
    }
    let viewport = active_display.actual_bounds(&config);
    if fitted_offsets(strip, &windows, viewport).is_some() {
        return;
    }
    let step = Step::of(
        scroll,
        f64::from(position.x),
        None,
        f64::from(viewport.width()),
    );
    let column_top = |step| {
        landing(position.x, Some(step), strip, &windows, viewport)
            .and_then(|(column, _)| column.top())
    };
    // A swipe starts on the column it began on, even when its first frame
    // already moved the strip far enough to step.
    if scroll.detent.is_none() {
        scroll.detent = column_top(Step {
            toward_end: None,
            ..step
        });
    }
    let top = column_top(step);
    if top.is_none() || top == scroll.detent {
        return;
    }
    if config.swipe_gesture_haptics() {
        haptics.write(PageHapticRequest);
    }
    scroll.detent = top;
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
    // NOTE: this snaps to the second window on purpose.
    // For a single colum workspace this will return None
    let right_snap = strip_position(layout_strip.get(1));

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

#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn large_swipe_preserves_every_earned_step_without_expanding_the_queue() {
        let mut state = PageGestureState {
            accumulated_cm: 80.5,
            ..Default::default()
        };
        state.earn_steps(2.0, SwipeGestureDirection::Natural);
        assert_eq!(state.pending.len(), 1);
        assert!((state.accumulated_cm - 0.5).abs() < f64::EPSILON);

        let expected = focus_direction(SwipeGestureDirection::Natural, 1.0);
        for _ in 0..40 {
            assert_eq!(state.next_step(), Some(expected.clone()));
        }
        assert_eq!(state.next_step(), None);
    }

    #[test]
    fn in_flight_focus_keeps_paging_system_active_after_finger_lift() {
        let state = PageGestureState {
            in_flight: Some(PageInFlight::Dispatched),
            ..Default::default()
        };
        assert!(state.has_work());
    }
}
