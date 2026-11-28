use arc_swap::ArcSwap;
use core::ptr::NonNull;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventType, NSTouch, NSTouchPhase};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::NSSet;
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use stdext::function_name;
use tracing::{error, info, warn};

use crate::commands::{Command, Direction, Operation};
use crate::config::Config;
use crate::config::swipe::focus_direction;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender, MouseButton};
use crate::platform::slice_tap::{self, Mouse, Pointer, SliceTap, Verdict};
use crate::platform::{Modifiers, WinID, double_click_tap};

const NX_DEVICEFNKEYMASK: u64 = 0x0080_0100;

/// The currently active set of passthrough keybindings, shared lock-free with
/// the `CGEvent` tap callback thread via `ArcSwap`.
static FOCUSED_PASSTHROUGH: LazyLock<ArcSwap<Vec<(u8, Modifiers)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the passthrough keybinding set that the event tap checks on every
/// key-down. Called from the ECS thread on focus change and config reload.
pub fn set_focused_passthrough(keys: Vec<(u8, Modifiers)>) {
    FOCUSED_PASSTHROUGH.store(Arc::new(keys));
}

/// How long to suppress momentum scroll wheel events after a swipe gesture,
/// covering macOS momentum scroll that continues after finger lift.
const VERTICAL_GESTURE_SCROLL_SUPPRESS: Duration = Duration::from_millis(1200);

/// Short grace period after a swipe ends to absorb staggered finger-liftoff
/// scroll events before allowing a fresh two-finger scroll through.
const POST_SWIPE_SCROLL_GRACE: Duration = Duration::from_millis(120);

/// How soon after an active swipe ends a retouch still continues that swipe
/// without re-arming (matches `RETOUCH_WINDOW` in `ecs::scroll` for miri#494).
const SWIPE_RETOUCH_WINDOW: Duration = Duration::from_millis(40);

/// `kCGScrollPhaseBegan` and `kCGScrollPhaseMayBegin` from `CGEventTypes.h`.
const CG_SCROLL_PHASE_BEGAN: i64 = 1;
const CG_SCROLL_PHASE_MAY_BEGIN: i64 = 128;

/// Limit repeated wheel inputs while the next window moves into view.
const WINDOW_SCROLL_INTERVAL: Duration = Duration::from_millis(180);

/// Physical arming thresholds for multi-finger swipe recognition.
///
/// Requiring coordinated movement across all active fingers before committing
/// prevents a resting or lightly drifting thumb plus two scrolling fingers from
/// triggering a three-finger strip swipe.
const ARM_DISTANCE_MM: f64 = 2.2;
const MIN_FINGER_ARM_MM: f64 = 0.8;
const MIN_COMMIT_FRAME_MM: f64 = 0.15;
const RECENT_MOTION_WINDOW: Duration = Duration::from_millis(60);
const COHERENCE_RATIO: f64 = 0.25;
const AXIS_DOMINANCE: f64 = 1.5;
const REJECT_DISAGREE_MM: f64 = 8.0;

/// Fallback normalized travel per millimetre when `deviceSize` is unavailable
/// (roughly a 135 mm x 85 mm laptop trackpad).
const FALLBACK_MM_PER_NORM_X: f64 = 135.0;
const FALLBACK_MM_PER_NORM_Y: f64 = 85.0;

/// One active, non-resting finger as a single gesture event saw it.
struct Touch {
    identity: Retained<AnyObject>,
    x: f64,
    y: f64,
    device_width_points: f64,
    device_height_points: f64,
    /// Whether the finger landed this event.
    began: bool,
}

/// Result of sampling an `NSEvent`'s touches while separating empty raw events
/// from events whose touches were all resting or lifted.
struct SampledTouches {
    /// Total contacts in `allTouches()` before filtering.
    raw_count: usize,
    /// Active (`Touching`) and non-resting contacts.
    active: Vec<Touch>,
    /// Whether any non-resting contact ended or was cancelled on this event.
    any_lifted: bool,
}

impl Touch {
    /// Samples active, non-resting touches in one pass while tracking lifts
    /// and the raw touch count.
    fn sample(touches: &NSSet<NSTouch>) -> SampledTouches {
        let raw_count = touches.count();
        let mut active = Vec::with_capacity(raw_count);
        let mut any_lifted = false;
        for touch in touches {
            if touch.isResting() {
                continue;
            }
            let phase = touch.phase();
            if phase.intersects(NSTouchPhase::Ended | NSTouchPhase::Cancelled) {
                any_lifted = true;
                continue;
            }
            if !phase.intersects(NSTouchPhase::Touching) {
                continue;
            }
            let position = touch.normalizedPosition();
            let size = touch.deviceSize();
            active.push(Self {
                identity: touch.identity(),
                x: position.x,
                y: position.y,
                device_width_points: size.width,
                device_height_points: size.height,
                began: phase.contains(NSTouchPhase::Began),
            });
        }
        SampledTouches {
            raw_count,
            active,
            any_lifted,
        }
    }
}

/// An anchor for a finger's unconsumed travel. Touch identity matters because
/// `NSSet` does not preserve iteration order between events.
#[derive(Clone)]
struct SwipeSample<I> {
    identity: I,
    x: f64,
    y: f64,
    device_width_points: f64,
    device_height_points: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwipeAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, PartialEq)]
enum SwipeMovement {
    Horizontal {
        delta: f64,
        distance_cm: Option<f64>,
    },
    Vertical(f64),
}

#[derive(Debug, PartialEq)]
enum SwipeSampleOutcome {
    /// Still arming or sub-threshold while active; no movement emitted.
    Pending,
    /// Gesture was disqualified (vertical scroll when vertical is disabled, or
    /// non-coherent motion such as two scrolling fingers plus a resting thumb).
    Rejected,
    /// Resumed an active swipe within `SWIPE_RETOUCH_WINDOW` without re-arming.
    Resumed,
    /// An active swipe ended because a committed finger lifted or an extra
    /// contact started participating.
    Ended,
    /// Committed on this sample or continuing an already-committed swipe.
    Movement {
        movement: SwipeMovement,
        just_committed: bool,
    },
}

enum SwipeState<I> {
    Idle,
    /// Tracking finger travel from `start_anchors` until all fingers move
    /// coherently past the physical arming threshold. `prev_frame` holds a
    /// recent reference (bounded by `RECENT_MOTION_WINDOW`) so the commit
    /// frame checks recent motion and emits smooth velocity.
    Arming {
        start_anchors: Vec<SwipeSample<I>>,
        prev_frame: Vec<SwipeSample<I>>,
        prev_frame_at: Instant,
    },
    /// Committed to `axis` for the remainder of the touch session, while
    /// watching `resting_anchors` so an extra finger joining mid-swipe ends it.
    Active {
        axis: SwipeAxis,
        anchors: Vec<SwipeSample<I>>,
        resting_anchors: Vec<SwipeSample<I>>,
    },
    /// Disqualified for the remainder of this touch set.
    Rejected {
        identities: Vec<I>,
    },
}

struct SwipeAccumulator<I> {
    state: SwipeState<I>,
    /// When an `Active` swipe ended, so an edge-flicker retouch within
    /// `SWIPE_RETOUCH_WINDOW` can resume immediately without re-arming.
    last_active_ended: Option<(Instant, SwipeAxis)>,
}

impl<I> Default for SwipeAccumulator<I> {
    fn default() -> Self {
        Self {
            state: SwipeState::Idle,
            last_active_ended: None,
        }
    }
}

fn normalized_to_mm(delta: f64, device_points: f64, fallback_mm: f64) -> f64 {
    if device_points.is_finite() && device_points > 0.0 {
        delta * device_points * 25.4 / 72.0
    } else {
        delta * fallback_mm
    }
}

fn len_as_f64(len: usize) -> f64 {
    f64::from(u32::try_from(len).unwrap_or(u32::MAX))
}

/// Selects the indices of touches moving coherently together along `primary_mm`.
///
/// A touch belongs to the moving group when it has moved at least
/// `MIN_FINGER_ARM_MM` in the dominant direction and at least
/// `COHERENCE_RATIO` of the farthest-moving touch. Stationary or passively
/// drifting palms/thumbs stay out of this group.
fn coherent_moving_indices(primary_mm: &[f64]) -> Vec<usize> {
    let Some((max_idx, &max_val)) = primary_mm.iter().enumerate().max_by(|(_, a), (_, b)| {
        a.abs()
            .partial_cmp(&b.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    }) else {
        return Vec::new();
    };
    let max_abs = max_val.abs();
    if max_abs < MIN_FINGER_ARM_MM {
        return Vec::new();
    }
    let sign = primary_mm[max_idx].signum();
    let floor = MIN_FINGER_ARM_MM.max(COHERENCE_RATIO * max_abs);
    primary_mm
        .iter()
        .enumerate()
        .filter_map(|(idx, &d)| (d * sign > 0.0 && d.abs() >= floor).then_some(idx))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitCheck {
    /// All cumulative, excluded-contact, and recent-motion checks passed.
    Ready,
    /// Cumulative and excluded-contact checks passed; only recent commit-frame
    /// coherence failed, so arming should remain pending rather than rejecting.
    NeedsRecentMotion,
    /// An excluded (non-selected) contact moved past `MIN_FINGER_ARM_MM`.
    ExcludedMoved,
    /// Cumulative threshold, axis dominance, or finger count did not pass yet.
    NotReady,
}

/// Evaluates whether the selected `indices` of per-finger physical
/// displacements (in mm) qualify to commit a swipe for `configured_fingers`.
fn subset_commits(
    indices: &[usize],
    configured_fingers: usize,
    primary_mm: &[f64],
    cross_mm: &[f64],
    frame_primary_mm: &[f64],
) -> CommitCheck {
    if indices.len() != configured_fingers || indices.is_empty() {
        return CommitCheck::NotReady;
    }
    for (idx, (&p, &c)) in primary_mm.iter().zip(cross_mm).enumerate() {
        if !indices.contains(&idx) && (p.abs() >= MIN_FINGER_ARM_MM || c.abs() >= MIN_FINGER_ARM_MM)
        {
            return CommitCheck::ExcludedMoved;
        }
    }
    let count = len_as_f64(indices.len());
    let mean_primary = indices.iter().map(|&i| primary_mm[i]).sum::<f64>() / count;
    let mean_cross = indices.iter().map(|&i| cross_mm[i]).sum::<f64>() / count;
    if mean_primary.abs() < ARM_DISTANCE_MM
        || mean_primary.abs() < AXIS_DOMINANCE * mean_cross.abs()
    {
        return CommitCheck::NotReady;
    }
    let sign = mean_primary.signum();
    let max_frame = indices
        .iter()
        .map(|&i| frame_primary_mm[i].abs())
        .fold(0.0_f64, f64::max);
    let frame_floor = MIN_COMMIT_FRAME_MM.max(COHERENCE_RATIO * max_frame);
    if indices
        .iter()
        .any(|&i| frame_primary_mm[i] * sign <= 0.0 || frame_primary_mm[i].abs() < frame_floor)
    {
        return CommitCheck::NeedsRecentMotion;
    }
    CommitCheck::Ready
}

struct ArmingMetrics {
    horiz_mm: Vec<f64>,
    vert_mm: Vec<f64>,
    horiz_norm: Vec<(f64, f64)>,
    frame_horiz: Vec<f64>,
    frame_vert: Vec<f64>,
    frame_horiz_mm: Vec<f64>,
    frame_vert_mm: Vec<f64>,
}

impl ArmingMetrics {
    fn measure<I>(
        start_anchors: &[SwipeSample<I>],
        reference: &[SwipeSample<I>],
        touches: &[SwipeSample<I>],
        same_identity: impl Fn(&I, &I) -> bool,
    ) -> Option<Self> {
        let mut horiz_mm = Vec::with_capacity(touches.len());
        let mut vert_mm = Vec::with_capacity(touches.len());
        let mut horiz_norm = Vec::with_capacity(touches.len());
        let mut frame_horiz = Vec::with_capacity(touches.len());
        let mut frame_vert = Vec::with_capacity(touches.len());
        let mut frame_horiz_mm = Vec::with_capacity(touches.len());
        let mut frame_vert_mm = Vec::with_capacity(touches.len());

        for current in touches {
            let start = start_anchors
                .iter()
                .find(|p| same_identity(&p.identity, &current.identity))?;
            let prev = reference
                .iter()
                .find(|p| same_identity(&p.identity, &current.identity))
                .unwrap_or(start);

            let horiz = start.x - current.x;
            let vert = start.y - current.y;
            let step_horiz = prev.x - current.x;
            let step_vert = prev.y - current.y;
            horiz_norm.push((horiz, current.device_width_points));
            horiz_mm.push(normalized_to_mm(
                horiz,
                current.device_width_points,
                FALLBACK_MM_PER_NORM_X,
            ));
            vert_mm.push(normalized_to_mm(
                vert,
                current.device_height_points,
                FALLBACK_MM_PER_NORM_Y,
            ));
            frame_horiz.push(step_horiz);
            frame_vert.push(step_vert);
            frame_horiz_mm.push(normalized_to_mm(
                step_horiz,
                current.device_width_points,
                FALLBACK_MM_PER_NORM_X,
            ));
            frame_vert_mm.push(normalized_to_mm(
                step_vert,
                current.device_height_points,
                FALLBACK_MM_PER_NORM_Y,
            ));
        }

        Some(Self {
            horiz_mm,
            vert_mm,
            horiz_norm,
            frame_horiz,
            frame_vert,
            frame_horiz_mm,
            frame_vert_mm,
        })
    }
}

fn partition_touches<I>(
    touches: Vec<SwipeSample<I>>,
    indices: &[usize],
) -> (Vec<SwipeSample<I>>, Vec<SwipeSample<I>>) {
    let mut selected = Vec::with_capacity(indices.len());
    let mut resting = Vec::with_capacity(touches.len().saturating_sub(indices.len()));
    for (idx, touch) in touches.into_iter().enumerate() {
        if indices.contains(&idx) {
            selected.push(touch);
        } else {
            resting.push(touch);
        }
    }
    (selected, resting)
}

fn extra_contact_participated<I>(
    resting_anchors: &mut Vec<SwipeSample<I>>,
    touches: &[SwipeSample<I>],
    committed_anchors: &[SwipeSample<I>],
    same_identity: &impl Fn(&I, &I) -> bool,
) -> bool
where
    I: Clone,
{
    for touch in touches {
        if committed_anchors
            .iter()
            .any(|c| same_identity(&c.identity, &touch.identity))
        {
            continue;
        }
        if let Some(anchor) = resting_anchors
            .iter()
            .find(|r| same_identity(&r.identity, &touch.identity))
        {
            let horiz = normalized_to_mm(
                anchor.x - touch.x,
                touch.device_width_points,
                FALLBACK_MM_PER_NORM_X,
            );
            let vert = normalized_to_mm(
                anchor.y - touch.y,
                touch.device_height_points,
                FALLBACK_MM_PER_NORM_Y,
            );
            if horiz.abs() >= ARM_DISTANCE_MM || vert.abs() >= ARM_DISTANCE_MM {
                return true;
            }
        } else {
            resting_anchors.push(touch.clone());
        }
    }
    resting_anchors.retain(|r| {
        touches
            .iter()
            .any(|t| same_identity(&r.identity, &t.identity))
    });
    false
}

fn max_abs(values: &[f64]) -> f64 {
    values.iter().map(|d| d.abs()).fold(0.0_f64, f64::max)
}

fn mean_selected(values: &[f64], indices: &[usize]) -> f64 {
    if indices.is_empty() {
        return 0.0;
    }
    indices.iter().map(|&i| values[i]).sum::<f64>() / len_as_f64(indices.len())
}

impl<I: Clone> SwipeAccumulator<I> {
    fn is_active(&self) -> bool {
        matches!(self.state, SwipeState::Active { .. })
    }

    fn is_active_compatible(&self, configured_fingers: usize, vertical_enabled: bool) -> bool {
        match &self.state {
            SwipeState::Active { axis, anchors, .. } => {
                anchors.len() == configured_fingers
                    && (*axis == SwipeAxis::Horizontal || vertical_enabled)
            }
            _ => true,
        }
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.state = SwipeState::Idle;
        self.last_active_ended = None;
    }

    fn clear_retouch(&mut self) {
        self.last_active_ended = None;
    }

    /// Ends the current gesture. If it was `Active`, remembers when it ended
    /// so an edge-flicker retouch within `SWIPE_RETOUCH_WINDOW` can resume
    /// immediately.
    fn end(&mut self, now: Instant) -> bool {
        let was_active = match self.state {
            SwipeState::Active { axis, .. } => {
                self.last_active_ended = Some((now, axis));
                true
            }
            _ => false,
        };
        self.state = SwipeState::Idle;
        was_active
    }

    /// Starts a fresh touch set. If an `Active` swipe ended within
    /// `SWIPE_RETOUCH_WINDOW` and the touch count matches `configured_fingers`,
    /// resumes `Active` immediately and returns `true` so the caller can emit
    /// `TouchpadDown` for `scroll::continues_swipe`.
    fn begin_or_retouch(
        &mut self,
        touches: Vec<SwipeSample<I>>,
        configured_fingers: usize,
        now: Instant,
    ) -> bool {
        if touches.len() == configured_fingers
            && let Some((ended_at, axis)) = self.last_active_ended
            && now.saturating_duration_since(ended_at) <= SWIPE_RETOUCH_WINDOW
        {
            self.last_active_ended = None;
            self.state = SwipeState::Active {
                axis,
                anchors: touches,
                resting_anchors: Vec::new(),
            };
            return true;
        }
        self.state = SwipeState::Arming {
            start_anchors: touches,
            prev_frame: Vec::new(),
            prev_frame_at: now,
        };
        false
    }

    fn sample(
        &mut self,
        touches: Vec<SwipeSample<I>>,
        configured_fingers: usize,
        vertical_enabled: bool,
        now: Instant,
        same_identity: impl Fn(&I, &I) -> bool,
    ) -> SwipeSampleOutcome {
        if touches.is_empty() {
            let was_active = self.end(now);
            return if was_active {
                SwipeSampleOutcome::Ended
            } else {
                SwipeSampleOutcome::Pending
            };
        }

        match &mut self.state {
            SwipeState::Idle => {
                if self.begin_or_retouch(touches, configured_fingers, now) {
                    SwipeSampleOutcome::Resumed
                } else {
                    SwipeSampleOutcome::Pending
                }
            }
            SwipeState::Rejected { identities } => {
                let same_set = identities.len() == touches.len()
                    && touches.iter().all(|t| {
                        identities
                            .iter()
                            .any(|prev| same_identity(prev, &t.identity))
                    });
                if same_set {
                    SwipeSampleOutcome::Rejected
                } else {
                    self.state = SwipeState::Arming {
                        start_anchors: touches,
                        prev_frame: Vec::new(),
                        prev_frame_at: now,
                    };
                    SwipeSampleOutcome::Pending
                }
            }
            SwipeState::Arming { .. } => self.sample_arming(
                touches,
                configured_fingers,
                vertical_enabled,
                now,
                same_identity,
            ),
            SwipeState::Active { .. } => self.sample_active(touches, now, same_identity),
        }
    }

    fn reject_touches(&mut self, touches: Vec<SwipeSample<I>>) -> SwipeSampleOutcome {
        self.state = SwipeState::Rejected {
            identities: touches.into_iter().map(|t| t.identity).collect(),
        };
        SwipeSampleOutcome::Rejected
    }

    fn advance_arming_reference(
        prev_frame: &mut Vec<SwipeSample<I>>,
        prev_frame_at: &mut Instant,
        touches: Vec<SwipeSample<I>>,
        metrics: &ArmingMetrics,
        now: Instant,
    ) {
        if max_abs(&metrics.frame_horiz_mm) >= MIN_COMMIT_FRAME_MM
            || max_abs(&metrics.frame_vert_mm) >= MIN_COMMIT_FRAME_MM
            || now.saturating_duration_since(*prev_frame_at) >= RECENT_MOTION_WINDOW
        {
            *prev_frame = touches;
            *prev_frame_at = now;
        }
    }

    fn commit_axis(
        &mut self,
        axis: SwipeAxis,
        touches: Vec<SwipeSample<I>>,
        indices: &[usize],
        metrics: &ArmingMetrics,
    ) -> SwipeSampleOutcome {
        let movement = match axis {
            SwipeAxis::Horizontal => SwipeMovement::Horizontal {
                delta: indices.iter().map(|&i| metrics.frame_horiz[i]).sum(),
                distance_cm: physical_swipe_distance_cm(
                    indices.iter().map(|&i| metrics.horiz_norm[i]),
                ),
            },
            SwipeAxis::Vertical => {
                SwipeMovement::Vertical(indices.iter().map(|&i| metrics.frame_vert[i]).sum())
            }
        };
        let (anchors, resting_anchors) = partition_touches(touches, indices);
        self.state = SwipeState::Active {
            axis,
            anchors,
            resting_anchors,
        };
        SwipeSampleOutcome::Movement {
            movement,
            just_committed: true,
        }
    }

    fn reset_arming(&mut self, touches: Vec<SwipeSample<I>>, now: Instant) -> SwipeSampleOutcome {
        self.state = SwipeState::Arming {
            start_anchors: touches,
            prev_frame: Vec::new(),
            prev_frame_at: now,
        };
        SwipeSampleOutcome::Pending
    }

    fn sample_arming(
        &mut self,
        touches: Vec<SwipeSample<I>>,
        configured_fingers: usize,
        vertical_enabled: bool,
        now: Instant,
        same_identity: impl Fn(&I, &I) -> bool,
    ) -> SwipeSampleOutcome {
        let SwipeState::Arming {
            start_anchors,
            prev_frame,
            prev_frame_at,
        } = &mut self.state
        else {
            return SwipeSampleOutcome::Pending;
        };

        if start_anchors.len() != touches.len() {
            return self.reset_arming(touches, now);
        }

        let reference_expired =
            now.saturating_duration_since(*prev_frame_at) >= RECENT_MOTION_WINDOW;
        let reference = if reference_expired {
            &touches
        } else if prev_frame.len() == touches.len() {
            &*prev_frame
        } else {
            &*start_anchors
        };
        let Some(metrics) =
            ArmingMetrics::measure(start_anchors, reference, &touches, same_identity)
        else {
            return self.reset_arming(touches, now);
        };

        let horiz_moving = coherent_moving_indices(&metrics.horiz_mm);
        let mut horiz_excluded_moved = false;
        match subset_commits(
            &horiz_moving,
            configured_fingers,
            &metrics.horiz_mm,
            &metrics.vert_mm,
            &metrics.frame_horiz_mm,
        ) {
            CommitCheck::Ready => {
                return self.commit_axis(SwipeAxis::Horizontal, touches, &horiz_moving, &metrics);
            }
            CommitCheck::NeedsRecentMotion => {
                Self::advance_arming_reference(prev_frame, prev_frame_at, touches, &metrics, now);
                return SwipeSampleOutcome::Pending;
            }
            CommitCheck::ExcludedMoved => horiz_excluded_moved = true,
            CommitCheck::NotReady => {}
        }

        let vert_moving = coherent_moving_indices(&metrics.vert_mm);
        if vert_moving.len() >= 2 {
            let mean_vert = mean_selected(&metrics.vert_mm, &vert_moving).abs();
            let mean_horiz = mean_selected(&metrics.horiz_mm, &vert_moving).abs();
            if mean_vert >= ARM_DISTANCE_MM && mean_vert > mean_horiz {
                let mut vert_excluded_moved = false;
                if vertical_enabled {
                    match subset_commits(
                        &vert_moving,
                        configured_fingers,
                        &metrics.vert_mm,
                        &metrics.horiz_mm,
                        &metrics.frame_vert_mm,
                    ) {
                        CommitCheck::Ready => {
                            return self.commit_axis(
                                SwipeAxis::Vertical,
                                touches,
                                &vert_moving,
                                &metrics,
                            );
                        }
                        CommitCheck::NeedsRecentMotion => {
                            Self::advance_arming_reference(
                                prev_frame,
                                prev_frame_at,
                                touches,
                                &metrics,
                                now,
                            );
                            return SwipeSampleOutcome::Pending;
                        }
                        CommitCheck::ExcludedMoved => vert_excluded_moved = true,
                        CommitCheck::NotReady => {}
                    }
                }
                if !vertical_enabled
                    || (max_abs(&metrics.vert_mm) >= REJECT_DISAGREE_MM
                        && (vert_moving.len() != configured_fingers || vert_excluded_moved))
                {
                    return self.reject_touches(touches);
                }
            }
        }

        if max_abs(&metrics.horiz_mm) >= REJECT_DISAGREE_MM
            && (horiz_moving.len() != configured_fingers || horiz_excluded_moved)
        {
            return self.reject_touches(touches);
        }

        Self::advance_arming_reference(prev_frame, prev_frame_at, touches, &metrics, now);
        SwipeSampleOutcome::Pending
    }

    fn sample_active(
        &mut self,
        touches: Vec<SwipeSample<I>>,
        now: Instant,
        same_identity: impl Fn(&I, &I) -> bool,
    ) -> SwipeSampleOutcome {
        let SwipeState::Active {
            axis,
            anchors,
            resting_anchors,
        } = &mut self.state
        else {
            return SwipeSampleOutcome::Pending;
        };

        if extra_contact_participated(resting_anchors, &touches, anchors, &same_identity) {
            self.last_active_ended = None;
            self.state = SwipeState::Rejected {
                identities: touches.into_iter().map(|t| t.identity).collect(),
            };
            return SwipeSampleOutcome::Ended;
        }

        // Track only the committed swipe identities so a stationary resting
        // palm landing or lifting beside them does not interrupt the swipe.
        let mut matched_touches = Vec::with_capacity(anchors.len());
        let mut deltas = Vec::with_capacity(anchors.len());
        for previous in anchors.iter() {
            let Some(current) = touches
                .iter()
                .find(|t| same_identity(&previous.identity, &t.identity))
            else {
                self.end(now);
                return SwipeSampleOutcome::Ended;
            };
            deltas.push((
                previous.x - current.x,
                previous.y - current.y,
                current.device_width_points,
            ));
        }

        // Once committed, stay locked to the winning axis and allow any finger
        // to continue driving the strip (so a lagging finger mid-swipe stays smooth).
        let movement = match *axis {
            SwipeAxis::Horizontal => {
                let horiz_sum: f64 = deltas.iter().map(|(x, _, _)| x).sum();
                let moved = deltas
                    .iter()
                    .filter(|(x, _, _)| x.abs() > SWIPE_THRESHOLD)
                    .count();
                (moved > 0).then(|| SwipeMovement::Horizontal {
                    delta: horiz_sum,
                    distance_cm: physical_swipe_distance_cm(
                        deltas.iter().map(|(x, _, width)| (*x, *width)),
                    ),
                })
            }
            SwipeAxis::Vertical => {
                let vert_sum: f64 = deltas.iter().map(|(_, y, _)| y).sum();
                let moved = deltas
                    .iter()
                    .filter(|(_, y, _)| y.abs() > SWIPE_THRESHOLD)
                    .count();
                (moved > 0).then_some(SwipeMovement::Vertical(vert_sum))
            }
        };
        if let Some(movement) = movement {
            for touch in touches {
                if anchors
                    .iter()
                    .any(|prev| same_identity(&prev.identity, &touch.identity))
                {
                    matched_touches.push(touch);
                }
            }
            *anchors = matched_touches;
            SwipeSampleOutcome::Movement {
                movement,
                just_committed: false,
            }
        } else {
            SwipeSampleOutcome::Pending
        }
    }
}

/// Keybinds registered by the Lua runtime as `(keycode, modifiers, handler_id)`,
/// shared lock-free with the event tap. Checked before the config bindings so a
/// scripted bind can override a TOML one.
static LUA_KEYBINDS: LazyLock<ArcSwap<Vec<(u8, Modifiers, u32)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the Lua keybind set that the event tap checks on every key-down.
/// Called from the main thread on script load and hot reload.
#[cfg(feature = "lua")]
pub fn set_lua_keybinds(keys: Vec<(u8, Modifiers, u32)>) {
    LUA_KEYBINDS.store(Arc::new(keys));
}

const SWIPE_THRESHOLD: f64 = 0.0005;
const GESTURE_MINIMAL_FINGERS: usize = 3;

/// Outcome of an event tap health check.
///
/// A dead tap is silent: the socket keeps answering and windows keep tiling on
/// command, so only an explicit check distinguishes it from a healthy daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TapHealth {
    Healthy,
    Reenabled,
    Rebuilt,
    Failed,
}

struct EventTap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
}

impl EventTap {
    /// The handler must stay pinned and alive until this tap is dropped.
    unsafe fn create(
        location: CGEventTapLocation,
        mask: CGEventMask,
        handler: NonNull<InputHandler>,
    ) -> Result<Self> {
        let port = unsafe {
            CGEvent::tap_create(
                location,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(InputHandler::callback),
                handler.as_ptr().cast(),
            )
        }
        .ok_or_else(|| {
            Error::PermissionDenied(format!("{}: Can not create EventTap.", function_name!()))
        })?;

        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0).ok_or_else(|| {
            CGEvent::tap_enable(&port, false);
            CFMachPort::invalidate(&port);
            Error::PermissionDenied(format!(
                "{}: Unable to create run loop source",
                function_name!()
            ))
        })?;

        Ok(Self { port, source })
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        if let Some(main_loop) = CFRunLoop::main() {
            CFRunLoop::remove_source(&main_loop, Some(&self.source), unsafe {
                kCFRunLoopCommonModes
            });
        }
        if self.port.is_valid() {
            CGEvent::tap_enable(&self.port, false);
            CFMachPort::invalidate(&self.port);
        }
    }
}

/// Maps a native `CGEvent` timestamp (Mach ticks for physical HID devices or
/// nanoseconds for software-posted events, normalized via `slice_tap::made_at`)
/// directly onto Rust's monotonic `Instant` clock via `slice_tap::uptime()`.
/// That keeps event timestamps comparable with `Instant::now()` across threads
/// and preserves true inter-event intervals through delayed dispatch bursts.
/// Synthetic events with timestamp 0 fall back to `dispatch_now`.
pub(crate) fn cg_event_timestamp_to_instant(cg_timestamp: u64, dispatch_now: Instant) -> Instant {
    if cg_timestamp == 0 {
        return dispatch_now;
    }
    event_timestamp_from_uptime(
        super::slice_tap::made_at(cg_timestamp),
        super::slice_tap::uptime(),
        dispatch_now,
    )
}

pub(crate) fn event_timestamp_from_uptime(
    event_time: Duration,
    now_uptime: Duration,
    dispatch_now: Instant,
) -> Instant {
    dispatch_now
        .checked_sub(now_uptime.saturating_sub(event_time))
        .unwrap_or(dispatch_now)
}

fn read_motion_fields(event: &CGEvent) -> (objc2_core_foundation::CGPoint, i64, Instant) {
    let point = CGEvent::location(Some(event));
    let delta_y = CGEvent::integer_value_field(Some(event), CGEventField::MouseEventDeltaY);
    let timestamp = cg_event_timestamp_to_instant(CGEvent::timestamp(Some(event)), Instant::now());
    (point, delta_y, timestamp)
}

/// `InputHandler` manages low-level input events from macOS event taps.
/// It intercepts keyboard and mouse events, processes gestures, and dispatches them as higher-level `Event`s.
pub(super) struct InputHandler {
    /// The `EventSender` for dispatching input events.
    events: Option<EventSender>,
    /// The application `Config` for looking up keybindings.
    config: Config,
    slice_tap: SliceTap,
    /// Tracks finger motion since the last swipe sent to the ECS.
    swipe_accumulator: SwipeAccumulator<Retained<AnyObject>>,
    taps: Option<[EventTap; 2]>,
    /// Timestamp of the last swipe gesture event. Scroll wheel events
    /// are suppressed for a short window after this to prevent the OS from
    /// scrolling windows underneath (including momentum scroll after finger lift).
    last_swipe_time: Option<Instant>,
    last_window_scroll: Option<(Instant, Direction)>,
    invalid_device_size_warned: bool,
    // Prevents from being Unpin automatically
    _pin: PhantomPinned,
}

pub(super) type PinnedInputHandler =
    ScopeGuard<Pin<Box<InputHandler>>, Box<dyn FnOnce(Pin<Box<InputHandler>>)>>;

impl InputHandler {
    /// Creates a new `InputHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send input-related events.
    /// * `config` - The `Config` object for looking up keybindings.
    ///
    /// # Returns
    ///
    /// A new `InputHandler`.
    pub(super) fn new(events: EventSender, config: Config) -> Self {
        InputHandler {
            events: Some(events),
            config,
            slice_tap: SliceTap::default(),
            swipe_accumulator: SwipeAccumulator::default(),
            taps: None,
            last_swipe_time: None,
            last_window_scroll: None,
            invalid_device_size_warned: false,
            _pin: PhantomPinned,
        }
    }

    /// Starts the input handler by creating and enabling its event taps. It also sets up a cleanup hook.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event tap is created and started successfully, otherwise `Err(Error)`.
    pub(super) fn start(self) -> Result<PinnedInputHandler> {
        let mut pinned = Box::pin(self);
        // Safety: the handler stays pinned in its Box for the life of the
        // daemon, so the pointer handed to the tap callback stays valid.
        unsafe { pinned.as_mut().get_unchecked_mut() }.install_tap()?;

        Ok(scopeguard::guard(
            pinned,
            Box::new(|mut handler: Pin<Box<Self>>| {
                info!("Unregistering event_handler");
                // Safety: as above. Tearing the tap down does not move it.
                unsafe { handler.as_mut().get_unchecked_mut() }.uninstall_tap();
            }),
        ))
    }

    /// Creates both event taps and wires their mach ports into the main run loop.
    ///
    /// The caller must keep `self` pinned: the tap callback holds a raw pointer
    /// back to this handler and dereferences it on every event.
    fn install_tap(&mut self) -> Result<()> {
        let main_loop = CFRunLoop::main().ok_or_else(|| {
            Error::PermissionDenied(format!("{}: Unable to get main run loop", function_name!()))
        })?;
        let hid_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0)
            | (1 << CGEventType::ScrollWheel.0)
            | (1 << NSEventType::Gesture.0)
            | (1 << CGEventType::KeyDown.0);

        let this = NonNull::from(&mut *self);
        // AppleScript posts keys after HID. Handled HID keys are swallowed,
        // so the session tap catches later input without dispatching them twice.
        // Safety: the caller keeps this handler pinned until both taps drop.
        let taps = unsafe {
            [
                EventTap::create(CGEventTapLocation::HIDEventTap, hid_event_mask, this)?,
                EventTap::create(
                    CGEventTapLocation::SessionEventTap,
                    1 << CGEventType::KeyDown.0,
                    this,
                )?,
            ]
        };

        for tap in self.taps.insert(taps) {
            CFRunLoop::add_source(&main_loop, Some(&tap.source), unsafe {
                kCFRunLoopCommonModes
            });
        }
        Ok(())
    }

    fn reset_tap_state(&mut self) {
        self.slice_tap.reset();
        double_click_tap::reset();
        self.clear_swipe_on_tap_reset();
    }

    fn uninstall_tap(&mut self) {
        self.reset_tap_state();
        self.taps = None;
    }

    /// Verifies the tap still delivers events, and revives it when it does not.
    ///
    /// macOS can invalidate the tap's mach port across a sleep cycle without
    /// ever delivering `TapDisabled` to the callback. No further event then
    /// reaches the callback, so the re-arm branch there can never fire and the
    /// tap stays dead until the daemon restarts. Re-enabling an invalid port is
    /// a no-op, so that case needs a full rebuild.
    pub(super) fn ensure_tap_alive(&mut self) -> TapHealth {
        let Some(taps) = self
            .taps
            .as_ref()
            .filter(|taps| taps.iter().all(|tap| tap.port.is_valid()))
        else {
            return self.rebuild_tap();
        };

        if taps.iter().all(|tap| CGEvent::tap_is_enabled(&tap.port)) {
            return TapHealth::Healthy;
        }

        for tap in taps {
            if !CGEvent::tap_is_enabled(&tap.port) {
                CGEvent::tap_enable(&tap.port, true);
            }
        }
        let reenabled = taps.iter().all(|tap| CGEvent::tap_is_enabled(&tap.port));
        self.reset_tap_state();
        if reenabled {
            warn!("event tap was disabled, re-enabled it");
            TapHealth::Reenabled
        } else {
            self.rebuild_tap()
        }
    }

    fn rebuild_tap(&mut self) -> TapHealth {
        self.uninstall_tap();
        match self.install_tap() {
            Ok(()) => {
                warn!("event tap was unusable, rebuilt it");
                TapHealth::Rebuilt
            }
            Err(err) => {
                error!("unable to rebuild event tap: {err}");
                TapHealth::Failed
            }
        }
    }

    extern "C-unwind" fn callback(
        _: CGEventTapProxy,
        event_type: CGEventType,
        mut event_ref: NonNull<CGEvent>,
        this: *mut c_void,
    ) -> *mut CGEvent {
        if let Some(this) =
            NonNull::new(this).map(|this| unsafe { this.cast::<InputHandler>().as_mut() })
        {
            let intercept = this.input_handler(event_type, unsafe { event_ref.as_ref() });
            if intercept {
                return null_mut();
            }
        } else {
            error!("Zero passed to Event Handler.");
        }
        unsafe { event_ref.as_mut() }
    }

    fn rearm_taps(&mut self) {
        warn!("Tap Disabled, re-enabling");
        self.reset_tap_state();
        for tap in self.taps.iter().flatten() {
            if tap.port.is_valid() {
                CGEvent::tap_enable(&tap.port, true);
            }
        }
    }

    fn handle_slice_tap(&mut self, mouse: Mouse, event: &CGEvent) -> Option<bool> {
        slice_tap::apply_cancellations(&mut self.slice_tap);
        let pointer = Pointer::read(mouse, event);
        match self.slice_tap.handle(&pointer) {
            Verdict::Pass => None,
            Verdict::Quiet => Some(false),
            Verdict::Swallow => Some(true),
            Verdict::Move(to) => {
                CGEvent::set_location(Some(event), to);
                None
            }
            Verdict::Hold(press) => {
                if let Some(events) = &self.events
                    && let Err(err) = events.send(Event::SlicePress { press })
                {
                    error!("error sending event: {err}");
                    self.events = None;
                }
                Some(true)
            }
            Verdict::PassAndRelease { id, button } => {
                slice_tap::post(id, button, false, pointer.point, pointer.flags);
                None
            }
        }
    }

    /// Handles various input events received from the `CGEventTap` callback. It sends corresponding `Event`s.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event` - A reference to the `CGEvent`.
    ///
    /// # Returns
    ///
    /// `true` if the event should be intercepted (not passed further), `false` otherwise.
    fn input_handler(&mut self, event_type: CGEventType, event: &CGEvent) -> bool {
        // Re-arm before any early return can swallow it. This is the only
        // notice macOS gives that it switched the tap off, and dropping it
        // leaves paneru deaf for the rest of the process lifetime.
        if matches!(
            event_type,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            self.rearm_taps();
            return false;
        }

        if double_click_tap::is_replayed(event) {
            return false;
        }

        if let Some(mouse) = Mouse::of(event_type)
            && let Some(intercept) = self.handle_slice_tap(mouse, event)
        {
            return intercept;
        }
        let Some(events) = &self.events else {
            return false;
        };

        let hold = match double_click_tap::intercept(event_type, event) {
            double_click_tap::TapAction::Pass => None,
            double_click_tap::TapAction::PassQuiet => return false,
            double_click_tap::TapAction::SwallowQuiet => return true,
            double_click_tap::TapAction::HoldForEngine(token) => Some(token),
        };

        let flags = CGEvent::flags(Some(event));
        let modifiers = get_modifiers(flags);

        let result = match event_type {
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = CGEvent::location(Some(event));
                let button = mouse_button(event_type);
                let window_id = CGEvent::integer_value_field(
                    Some(event),
                    CGEventField::MouseEventWindowUnderMousePointer,
                );
                events.send(Event::MouseDown {
                    point,
                    modifiers,
                    button,
                    window_id: WinID::try_from(window_id).ok().filter(|id| *id != 0),
                    clicks: CGEvent::integer_value_field(
                        Some(event),
                        CGEventField::MouseEventClickState,
                    ),
                    hold,
                })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = CGEvent::location(Some(event));
                let button = mouse_button(event_type);
                events.send(Event::MouseUp {
                    point,
                    modifiers,
                    button,
                })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let (point, delta_y, timestamp) = read_motion_fields(event);
                events.send(Event::MouseDragged {
                    point,
                    delta_y,
                    timestamp,
                    modifiers,
                    button: mouse_button(event_type),
                })
            }
            CGEventType::MouseMoved => {
                let (point, delta_y, timestamp) = read_motion_fields(event);
                events.send(Event::MouseMoved {
                    point,
                    delta_y,
                    timestamp,
                    modifiers,
                })
            }
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
                let repeat = CGEvent::integer_value_field(
                    Some(event),
                    CGEventField::KeyboardEventAutorepeat,
                ) != 0;
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, flags, repeat);
            }
            CGEventType::ScrollWheel => {
                return self.handle_scroll_wheel(event);
            }
            // Returns directly: handle_swipe returns bool (intercept flag)
            // rather than Result like the other arms.
            _ => {
                return self.handle_swipe(event);
            }
        };
        if let Err(err) = result {
            error!("error sending event: {err}");
            // The socket is dead, so no use trying to send to it.
            // Trigger cleanup destructor, unregistering the handler.
            self.events = None;
        }
        // Intercept only when the title-bar double-click gate is holding this
        // press for `maximize::double_click_toggle`; otherwise fall through.
        hold.is_some()
    }

    fn configured_swipe_fingers(&self) -> Option<usize> {
        self.config
            .swipe_gesture_fingers()
            .filter(|fingers| *fingers >= GESTURE_MINIMAL_FINGERS)
    }

    fn cancel_disabled_swipe(&mut self, now: Instant) {
        if self.swipe_accumulator.end(now) {
            self.send_touchpad_up();
        }
        self.swipe_accumulator.clear_retouch();
        self.last_swipe_time = None;
    }

    fn clear_swipe_on_tap_reset(&mut self) {
        if self.swipe_accumulator.end(Instant::now()) {
            self.send_touchpad_up();
        }
        self.swipe_accumulator.clear_retouch();
    }

    fn ensure_active_swipe_compatible(&mut self, now: Instant) {
        let Some(configured_fingers) = self.configured_swipe_fingers() else {
            self.cancel_disabled_swipe(now);
            return;
        };
        if !self
            .swipe_accumulator
            .is_active_compatible(configured_fingers, self.config.swipe_vertical())
        {
            self.cancel_disabled_swipe(now);
        }
    }

    /// Use the configured modifier to scroll the strip or select an adjacent window.
    fn handle_scroll_wheel(&mut self, event: &CGEvent) -> bool {
        let now = Instant::now();
        self.ensure_active_swipe_compatible(now);

        let is_continuous =
            CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventIsContinuous)
                != 0;
        let scroll_phase =
            CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventScrollPhase);
        let momentum_phase =
            CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventMomentumPhase);

        if should_suppress_scroll_wheel(
            self.swipe_accumulator.is_active(),
            &mut self.last_swipe_time,
            is_continuous,
            scroll_phase,
            momentum_phase,
            now,
        ) {
            return true;
        }

        let flags = CGEvent::flags(Some(event));
        let modifiers = get_binding_modifiers(flags);

        let target_modifier = self.config.swipe_scroll_modifier();
        let vertical_mod = self.config.swipe_scroll_vertical_modifier();

        // Check the combined modifier (base + vertical) first, then fall back to
        // base-only. matches() rejects extra modifier groups, so cmd+shift held
        // would fail a cmd-only check. We need to accept both.
        let base_match = target_modifier.matches(modifiers);
        let combined_match =
            vertical_mod.is_some_and(|vm| (target_modifier | vm).matches(modifiers));
        if !combined_match && !base_match {
            return false;
        }

        if let Some(events) = &self.events {
            let h_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
            );
            let v_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
            );

            // Vertical workspace switching when vertical modifier is also held.
            // Don't set last_vertical_gesture here: the suppress timer is for
            // trackpad momentum scroll, not discrete wheel ticks.
            if combined_match && v_delta.abs() > 0.001 {
                _ = events.send(Event::VerticalScrollTick { delta: v_delta });
                return true;
            }

            // If we have any horizontal delta, or if there's only vertical delta, use it.
            let delta = if h_delta.abs() > 0.001 {
                h_delta
            } else if v_delta.abs() > 0.001 {
                v_delta
            } else {
                0.0
            };

            if delta.abs() > 0.001 {
                if self.config.swipe_scroll_window_step() {
                    let direction = scroll_step_direction(&self.config, delta);
                    let momentum = CGEvent::integer_value_field(
                        Some(event),
                        CGEventField::ScrollWheelEventMomentumPhase,
                    );
                    if accept_window_scroll(
                        &mut self.last_window_scroll,
                        &direction,
                        momentum != 0,
                        Instant::now(),
                    ) {
                        _ = events.send(Event::Command {
                            command: Command::Window(Operation::Focus(direction)),
                        });
                    }
                } else {
                    _ = events.send(Event::Scroll { delta });
                }
                return true; // Intercept: don't let the window scroll
            }
        }
        false
    }

    fn send_touchpad_down(&self) {
        if let Some(events) = &self.events {
            _ = events.send(Event::TouchpadDown);
        }
    }

    fn send_touchpad_up(&self) {
        if let Some(events) = &self.events {
            _ = events.send(Event::TouchpadUp);
        }
    }

    fn finish_active_swipe(&mut self, now: Instant) -> bool {
        let was_active = self.swipe_accumulator.end(now);
        if was_active {
            self.send_touchpad_up();
            self.last_swipe_time = Some(now);
        }
        was_active
    }

    /// Handles swipe gesture events. Routes to horizontal `Swipe` or vertical
    /// `VerticalSwipe` based on coordinated multi-finger arming. Returns true
    /// to intercept the event once the gesture has committed.
    fn handle_swipe(&mut self, event: &CGEvent) -> bool {
        const NS_EVENT_PHASE_ENDED: usize = 1 << 3; // 8
        const NS_EVENT_PHASE_CANCELLED: usize = 1 << 4; // 16

        let now = Instant::now();
        let Some(configured_fingers) = self.configured_swipe_fingers() else {
            // No valid gesture is configured, so cancel any stale active swipe
            // and leave native macOS gestures alone.
            self.cancel_disabled_swipe(now);
            return false;
        };
        if !self
            .swipe_accumulator
            .is_active_compatible(configured_fingers, self.config.swipe_vertical())
        {
            self.cancel_disabled_swipe(now);
        }

        objc2::rc::autoreleasepool(|_| {
            let Some(ns_event) = NSEvent::eventWithCGEvent(event) else {
                error!("{}: Unable to convert CGEvent to NSEvent", function_name!());
                return false;
            };
            if ns_event.r#type() != NSEventType::Gesture {
                return false;
            }

            // Fingers lifted off touchpad or gesture cancelled.
            let phase = ns_event.phase();
            let cancelled = phase.0 & NS_EVENT_PHASE_CANCELLED != 0;
            if cancelled || phase.0 & NS_EVENT_PHASE_ENDED != 0 {
                self.finish_active_swipe(now);
                if cancelled {
                    self.swipe_accumulator.clear_retouch();
                }
                self.invalid_device_size_warned = false;
                return false;
            }

            let touch_batch = Touch::sample(&ns_event.allTouches());
            // AppKit interleaves phase events with an empty raw touch set
            // between usable samples. They do not mean the fingers lifted.
            // A non-empty raw set whose contacts all filtered out (resting or
            // ended) is NOT an empty intermediate frame and must end the swipe.
            if touch_batch.raw_count == 0 {
                return false;
            }

            if touch_batch.active.len() < configured_fingers {
                let was_active = self.finish_active_swipe(now);
                return was_active && touch_batch.any_lifted;
            }

            let any_began = touch_batch.active.iter().any(|finger| finger.began);
            let any_lifted = touch_batch.any_lifted;
            let samples = touch_batch
                .active
                .into_iter()
                .map(|finger| SwipeSample {
                    identity: finger.identity,
                    x: finger.x,
                    y: finger.y,
                    device_width_points: finger.device_width_points,
                    device_height_points: finger.device_height_points,
                })
                .collect();

            if !self.swipe_accumulator.is_active() && any_began {
                self.invalid_device_size_warned = false;
                let outcome =
                    if self
                        .swipe_accumulator
                        .begin_or_retouch(samples, configured_fingers, now)
                    {
                        SwipeSampleOutcome::Resumed
                    } else {
                        SwipeSampleOutcome::Pending
                    };
                return self.dispatch_swipe_outcome(
                    outcome,
                    configured_fingers,
                    self.config.swipe_vertical(),
                    any_lifted,
                    now,
                );
            }

            let vertical_enabled = self.config.swipe_vertical();
            let outcome = self.swipe_accumulator.sample(
                samples,
                configured_fingers,
                vertical_enabled,
                now,
                |previous, current| {
                    // SAFETY: NSTouch identities conform to NSObject and answer isEqual:.
                    unsafe { msg_send![&**previous, isEqual: &**current] }
                },
            );
            self.dispatch_swipe_outcome(
                outcome,
                configured_fingers,
                vertical_enabled,
                any_lifted,
                now,
            )
        })
    }

    fn dispatch_swipe_outcome(
        &mut self,
        outcome: SwipeSampleOutcome,
        configured_fingers: usize,
        vertical_enabled: bool,
        any_lifted: bool,
        now: Instant,
    ) -> bool {
        match outcome {
            SwipeSampleOutcome::Resumed => {
                self.send_touchpad_down();
                true
            }
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Horizontal { delta, distance_cm },
                just_committed,
            } => {
                if just_committed {
                    self.send_touchpad_down();
                }
                if distance_cm.is_none() && !self.invalid_device_size_warned {
                    warn!("trackpad did not provide a valid device size for paging");
                    self.invalid_device_size_warned = true;
                }
                if let Some(events) = &self.events {
                    let _ = events.send(Event::Swipe {
                        delta,
                        fingers: configured_fingers,
                        distance_cm: distance_cm.unwrap_or(0.0),
                    });
                    self.last_swipe_time = Some(now);
                }
                true
            }
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Vertical(delta),
                just_committed,
            } if vertical_enabled => {
                if just_committed {
                    self.send_touchpad_down();
                }
                if let Some(events) = &self.events {
                    let _ = events.send(Event::VerticalSwipe {
                        delta,
                        fingers: configured_fingers,
                    });
                    self.last_swipe_time = Some(now);
                }
                true
            }
            SwipeSampleOutcome::Ended => {
                self.send_touchpad_up();
                self.last_swipe_time = Some(now);
                any_lifted
            }
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Vertical(_),
                ..
            }
            | SwipeSampleOutcome::Rejected => false,
            SwipeSampleOutcome::Pending => self.swipe_accumulator.is_active(),
        }
    }

    /// Handles key press events. It determines the modifier mask and attempts to find a matching keybinding in the configuration.
    /// If a binding is found, it sends a `Command` event and intercepts the key press.
    ///
    /// # Arguments
    ///
    /// * `keycode` - The key code of the pressed key.
    /// * `eventflags` - The `CGEventFlags` representing active modifiers.
    /// * `repeat` - Whether macOS sent the press as a key auto-repeat.
    ///
    /// # Returns
    ///
    /// `true` if the key press was handled and should be intercepted, `false` otherwise.
    fn handle_keypress(&self, keycode: i64, eventflags: CGEventFlags, repeat: bool) -> bool {
        let Some(events) = &self.events else {
            return false;
        };

        let mask = get_binding_modifiers(eventflags);

        // On a native fullscreen space, keybindings are still intercepted so
        // that paneru can actively switch back to the previous workspace.
        // Non-paneru keys pass through naturally (find_keybind returns None).

        let keycode = keycode.try_into().ok();
        keycode
            .and_then(|keycode| {
                let passthrough = FOCUSED_PASSTHROUGH.load();
                if passthrough
                    .iter()
                    .any(|(c, m)| *c == keycode && m.matches(mask))
                {
                    return None;
                }
                // Lua-registered binds take precedence over the TOML config.
                let lua_binds = LUA_KEYBINDS.load();
                if let Some((_, _, id)) = lua_binds
                    .iter()
                    .find(|(c, m, _)| *c == keycode && m.matches(mask))
                {
                    return Some(Command::Lua(*id));
                }
                self.config.find_keybind(keycode, mask)
            })
            .and_then(|command| {
                if repeat && matches!(command, Command::Window(Operation::FocusPrevious)) {
                    return Some(());
                }
                events
                    .send(Event::Command {
                        command: command.for_keypress(repeat),
                    })
                    .inspect_err(|err| error!("Error sending command: {err}"))
                    .ok()
            })
            .is_some()
    }
}

/// Decides whether to suppress a `ScrollWheel` event while or after a strip
/// swipe is active. Discrete mouse wheels always pass through; trackpad
/// momentum tails are suppressed for `VERTICAL_GESTURE_SCROLL_SUPPRESS` unless
/// a fresh non-momentum scroll sequence (`Began` / `MayBegin`) starts after
/// `POST_SWIPE_SCROLL_GRACE`; and non-momentum `Changed` events pass after
/// grace while preserving momentum suppression for the prior swipe.
fn should_suppress_scroll_wheel(
    swipe_active: bool,
    last_swipe_time: &mut Option<Instant>,
    is_continuous: bool,
    scroll_phase: i64,
    momentum_phase: i64,
    now: Instant,
) -> bool {
    if !is_continuous {
        return false;
    }
    if swipe_active {
        return true;
    }
    let Some(last) = *last_swipe_time else {
        return false;
    };
    let elapsed = now.saturating_duration_since(last);
    if elapsed >= VERTICAL_GESTURE_SCROLL_SUPPRESS {
        *last_swipe_time = None;
        return false;
    }
    if momentum_phase != 0 {
        return true;
    }
    if elapsed < POST_SWIPE_SCROLL_GRACE {
        return true;
    }
    if scroll_phase == CG_SCROLL_PHASE_BEGAN || scroll_phase == CG_SCROLL_PHASE_MAY_BEGIN {
        *last_swipe_time = None;
    }
    false
}

// Automation and remote input can send group flags without left or right flags.
// Use the group only when the event does not specify a side.
fn get_binding_modifiers(eventflags: CGEventFlags) -> Modifiers {
    const GROUP_FLAGS: [(Modifiers, u64); 4] = [
        (Modifiers::ALT, CGEventFlags::MaskAlternate.0),
        (Modifiers::SHIFT, CGEventFlags::MaskShift.0),
        (Modifiers::CMD, CGEventFlags::MaskCommand.0),
        (Modifiers::CTRL, CGEventFlags::MaskControl.0),
    ];

    GROUP_FLAGS
        .iter()
        .fold(get_modifiers(eventflags), |modifiers, (group, flag)| {
            if !modifiers.intersects(*group) && eventflags.0 & flag != 0 {
                modifiers | *group
            } else {
                modifiers
            }
        })
}

/// The button behind a mouse press, release, or drag event type.
fn mouse_button(event_type: CGEventType) -> MouseButton {
    match event_type {
        CGEventType::RightMouseDown
        | CGEventType::RightMouseUp
        | CGEventType::RightMouseDragged => MouseButton::Right,
        _ => MouseButton::Left,
    }
}

fn get_modifiers(eventflags: CGEventFlags) -> Modifiers {
    const MODIFIER_MASKS: [(Modifiers, u64); 8] = [
        (Modifiers::LALT, 0x0000_0020),
        (Modifiers::RALT, 0x0000_0040),
        (Modifiers::LSHIFT, 0x0000_0002),
        (Modifiers::RSHIFT, 0x0000_0004),
        (Modifiers::LCMD, 0x0000_0008),
        (Modifiers::RCMD, 0x0000_0010),
        (Modifiers::LCTRL, 0x0000_0001),
        (Modifiers::RCTRL, 0x0000_2000),
    ];

    // Fn key should be checked for separately, because pressing
    // some keys (i.e. leftarrow) seems to inadvertently toggling it.
    if eventflags.0 == NX_DEVICEFNKEYMASK {
        tracing::debug!("event flags {:#x}", eventflags.0);
        return Modifiers::FN;
    }

    MODIFIER_MASKS
        .iter()
        .fold(Modifiers::empty(), |modifiers, (modifier, mask)| {
            if eventflags.0 & mask != 0 {
                modifiers | *modifier
            } else {
                modifiers
            }
        })
}

fn scroll_step_direction(config: &Config, delta: f64) -> Direction {
    focus_direction(config.swipe_gesture_direction(), delta)
}

fn physical_swipe_distance_cm(deltas: impl IntoIterator<Item = (f64, f64)>) -> Option<f64> {
    let (total, count) = deltas.into_iter().try_fold(
        (0.0, 0_u32),
        |(total, count), (normalized_delta, width_points)| {
            (normalized_delta.is_finite() && width_points.is_finite() && width_points > 0.0)
                .then_some((
                    total + normalized_delta * width_points * 2.54 / 72.0,
                    count + 1,
                ))
        },
    )?;
    (count > 0).then_some(total / f64::from(count))
}

fn accept_window_scroll(
    last: &mut Option<(Instant, Direction)>,
    direction: &Direction,
    momentum: bool,
    now: Instant,
) -> bool {
    if momentum
        || last.as_ref().is_some_and(|(time, previous)| {
            previous == direction && now.duration_since(*time) < WINDOW_SCROLL_INTERVAL
        })
    {
        return false;
    }
    *last = Some((now, direction.clone()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_fingers(x: f64) -> Vec<SwipeSample<usize>> {
        // 360 pt wide (~127 mm), 240 pt tall (~84.7 mm): typical MacBook trackpad.
        (0..3)
            .map(|identity| SwipeSample {
                identity,
                x,
                y: 0.0,
                device_width_points: 360.0,
                device_height_points: 240.0,
            })
            .collect()
    }

    fn three_fingers_144(x: f64) -> Vec<SwipeSample<usize>> {
        // 144 pt wide = 50.8 mm; 0.05 normalized = 2.54 mm (> ARM_DISTANCE_MM).
        (0..3)
            .map(|identity| SwipeSample {
                identity,
                x,
                y: 0.0,
                device_width_points: 144.0,
                device_height_points: 144.0,
            })
            .collect()
    }

    fn assert_horizontal(outcome: SwipeSampleOutcome, expected: f64) {
        let SwipeSampleOutcome::Movement {
            movement: SwipeMovement::Horizontal { delta, .. },
            ..
        } = outcome
        else {
            panic!("expected horizontal swipe, got {outcome:?}");
        };
        assert!((delta - expected).abs() < 1e-12, "{delta} != {expected}");
    }

    /// Commits a horizontal swipe on `swipe` using `three_fingers_144` so
    /// subsequent assertions can test active-state continuation behavior.
    fn commit_horizontal(swipe: &mut SwipeAccumulator<usize>, now: Instant) -> f64 {
        assert_eq!(
            swipe.sample(three_fingers_144(0.0), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        // 0.05 * 50.8 mm = 2.54 mm >= ARM_DISTANCE_MM (2.2 mm)
        let outcome = swipe.sample(three_fingers_144(0.05), 3, false, now, |a, b| a == b);
        assert!(matches!(
            outcome,
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Horizontal { .. },
                just_committed: true
            }
        ));
        0.05
    }

    #[test]
    fn commit_emits_smooth_frame_delta_and_full_arming_physical_distance() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        assert_eq!(
            swipe.sample(three_fingers_144(0.0), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        // 0.04 * 50.8 mm = 2.032 mm (still arming)
        assert_eq!(
            swipe.sample(three_fingers_144(0.04), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        // 0.05 * 50.8 mm = 2.54 mm (commits on this 0.01 step)
        let outcome = swipe.sample(three_fingers_144(0.05), 3, false, now, |a, b| a == b);
        let SwipeSampleOutcome::Movement {
            movement: SwipeMovement::Horizontal { delta, distance_cm },
            just_committed,
        } = outcome
        else {
            panic!("expected horizontal commit, got {outcome:?}");
        };
        assert!(just_committed);
        // Continuous delta is only the final frame (prev 0.04 -> cur 0.05), avoiding a velocity spike.
        assert!((delta + 0.03).abs() < 1e-12, "got {delta}");
        // Paging physical distance preserves the full 2.54 mm = 0.254 cm travel from start_anchors.
        let cm = distance_cm.expect("valid distance_cm");
        assert!((cm + 0.254).abs() < 1e-12, "got {cm}");
    }

    #[test]
    fn slow_finger_motion_accumulates_after_commit() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let base = commit_horizontal(&mut swipe, now);
        assert_eq!(
            swipe.sample(three_fingers_144(base + 0.0002), 3, false, now, |a, b| a
                == b),
            SwipeSampleOutcome::Pending
        );
        assert_eq!(
            swipe.sample(three_fingers_144(base + 0.0004), 3, false, now, |a, b| a
                == b),
            SwipeSampleOutcome::Pending
        );
        assert_horizontal(
            swipe.sample(three_fingers_144(base + 0.0006), 3, false, now, |a, b| {
                a == b
            }),
            -0.0018,
        );
        assert_eq!(
            swipe.sample(three_fingers_144(base + 0.0008), 3, false, now, |a, b| a
                == b),
            SwipeSampleOutcome::Pending
        );
        assert_horizontal(
            swipe.sample(three_fingers_144(base + 0.0012), 3, false, now, |a, b| {
                a == b
            }),
            -0.0018,
        );
    }

    #[test]
    fn subthreshold_reversal_cancels_without_emitting_a_swipe() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        swipe.sample(three_fingers(0.0), 3, false, now, |a, b| a == b);
        assert_eq!(
            swipe.sample(three_fingers(0.005), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert_eq!(
            swipe.sample(three_fingers(0.001), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert_eq!(
            swipe.sample(three_fingers(0.0), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
    }

    #[test]
    fn new_gesture_discards_unconsumed_motion() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        swipe.sample(three_fingers_144(0.0), 3, false, now, |a, b| a == b);
        assert_eq!(
            swipe.sample(three_fingers_144(0.04), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        swipe.reset();
        assert_eq!(
            swipe.sample(three_fingers_144(0.04), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert_eq!(
            swipe.sample(three_fingers_144(0.05), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
    }

    #[test]
    fn finger_replacement_ends_active_swipe() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let base = commit_horizontal(&mut swipe, now);
        let mut replacement = three_fingers_144(base + 0.002);
        replacement[2].identity = 3;
        assert_eq!(
            swipe.sample(replacement, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Ended
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn finger_order_changes_do_not_reset_a_slow_swipe() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let base = commit_horizontal(&mut swipe, now);
        let mut first = three_fingers_144(base + 0.0003);
        first.reverse();
        assert_eq!(
            swipe.sample(first, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert_horizontal(
            swipe.sample(three_fingers_144(base + 0.0006), 3, false, now, |a, b| {
                a == b
            }),
            -0.0018,
        );
    }

    #[test]
    fn sustained_swipe_continues_when_one_finger_lags() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let base = commit_horizontal(&mut swipe, now);

        let mut next = three_fingers_144(base + 0.002);
        next[2].x = base;
        assert_horizontal(swipe.sample(next, 3, false, now, |a, b| a == b), -0.004);

        let mut next = three_fingers_144(base + 0.004);
        next[2].x = base;
        assert_horizontal(swipe.sample(next, 3, false, now, |a, b| a == b), -0.004);
    }

    #[test]
    fn stationary_thumb_with_two_moving_fingers_never_arms() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        // Thumb is finger 0 at top-left (0.05, 0.92), stationary.
        let mut start = three_fingers(0.5);
        start[0].x = 0.05;
        start[0].y = 0.92;
        assert_eq!(
            swipe.sample(start.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );

        // Two scrolling fingers drift 4 mm horizontally while thumb stays still.
        let mut step1 = start.clone();
        step1[1].x += 0.032; // ~4.1 mm on 127 mm pad
        step1[2].x += 0.032;
        assert_eq!(
            swipe.sample(step1, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert!(!swipe.is_active());

        // Continued 2-finger horizontal scroll (> 8 mm) latches Rejected.
        let mut step2 = start;
        step2[1].x += 0.08; // ~10.2 mm
        step2[2].x += 0.08;
        assert_eq!(
            swipe.sample(step2, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn passive_thumb_roll_below_coherence_ratio_never_arms() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let start = three_fingers(0.5);
        swipe.sample(start.clone(), 3, false, now, |a, b| a == b);

        // Two fingers move 6 mm horizontally while thumb passively rolls 1 mm (< 25% of 6 mm).
        let mut next = start;
        next[0].x += 1.0 / 127.0;
        next[1].x += 6.0 / 127.0;
        next[2].x += 6.0 / 127.0;
        assert_eq!(
            swipe.sample(next, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn vertical_two_finger_scroll_with_thumb_latches_rejected_against_later_wobble() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let start = three_fingers(0.5);
        swipe.sample(start.clone(), 3, false, now, |a, b| a == b);

        // Vertical scroll: two fingers move 5 mm vertically with vertical disabled.
        let mut vertical = start.clone();
        vertical[1].y += 5.0 / 84.7;
        vertical[2].y += 5.0 / 84.7;
        assert_eq!(
            swipe.sample(vertical.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );

        // Subsequent horizontal wobble of all 3 fingers stays Rejected until lift.
        let mut wobble = vertical;
        for f in &mut wobble {
            f.x += 5.0 / 127.0;
        }
        assert_eq!(
            swipe.sample(wobble, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn resting_palm_plus_three_moving_fingers_commits_and_ignores_palm() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        // Palm is identity 99 at bottom-left, stationary; fingers 0, 1, 2 swipe.
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.05,
            y: 0.05,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        assert_eq!(
            swipe.sample(start.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );

        // Three fingers move 3 mm horizontally; palm stays still.
        let mut moved = start.clone();
        for f in &mut moved[0..3] {
            f.x += 3.0 / 127.0;
        }
        let outcome = swipe.sample(moved.clone(), 3, false, now, |a, b| a == b);
        assert!(matches!(
            outcome,
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Horizontal { .. },
                just_committed: true
            }
        ));
        assert!(swipe.is_active());

        // Palm lifts mid-swipe while the 3 committed fingers keep moving: swipe continues.
        let mut palm_lifted = moved[0..3].to_vec();
        for f in &mut palm_lifted {
            f.x += 2.0 / 127.0;
        }
        assert!(matches!(
            swipe.sample(palm_lifted, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Horizontal { .. },
                just_committed: false
            }
        ));
    }

    #[test]
    fn four_coherent_moving_fingers_do_not_trigger_three_finger_swipe() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let four = |x| {
            (0..4)
                .map(|identity| SwipeSample {
                    identity,
                    x,
                    y: 0.5,
                    device_width_points: 360.0,
                    device_height_points: 240.0,
                })
                .collect::<Vec<_>>()
        };
        swipe.sample(four(0.5), 3, false, now, |a, b| a == b);
        assert_eq!(
            swipe.sample(four(0.5 + 4.0 / 127.0), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert!(!swipe.is_active());
        assert_eq!(
            swipe.sample(four(0.5 + 10.0 / 127.0), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn edge_flicker_retouch_within_window_recommits_immediately() {
        let t0 = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        commit_horizontal(&mut swipe, t0);
        assert!(swipe.end(t0 + Duration::from_millis(10)));

        // Fingers return 20 ms later (within 40 ms SWIPE_RETOUCH_WINDOW).
        let t_retouch = t0 + Duration::from_millis(30);
        assert!(swipe.begin_or_retouch(three_fingers_144(0.2), 3, t_retouch));
        assert!(swipe.is_active());
        assert_horizontal(
            swipe.sample(three_fingers_144(0.201), 3, false, t_retouch, |a, b| a == b),
            -0.003,
        );
    }

    #[test]
    fn retouch_after_window_requires_arming_again() {
        let t0 = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        commit_horizontal(&mut swipe, t0);
        assert!(swipe.end(t0));

        // Fingers return 100 ms later (past 40 ms SWIPE_RETOUCH_WINDOW).
        let t_later = t0 + Duration::from_millis(100);
        assert!(!swipe.begin_or_retouch(three_fingers_144(0.2), 3, t_later));
        assert!(!swipe.is_active());
    }

    #[test]
    fn fast_vertical_motion_commits_when_vertical_enabled() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let vertical = |y| {
            (0..3)
                .map(|identity| SwipeSample {
                    identity,
                    x: 0.0,
                    y,
                    device_width_points: 144.0,
                    device_height_points: 144.0,
                })
                .collect()
        };
        swipe.sample(vertical(0.0), 3, true, now, |a, b| a == b);
        let outcome = swipe.sample(vertical(0.05), 3, true, now, |a, b| a == b);
        let SwipeSampleOutcome::Movement {
            movement: SwipeMovement::Vertical(delta),
            just_committed,
        } = outcome
        else {
            panic!("expected vertical swipe, got {outcome:?}");
        };
        assert!(just_committed);
        assert!((delta + 0.15).abs() < 1e-12);
    }

    #[test]
    fn thumb_that_rolls_then_stops_does_not_commit_with_two_scrolling_fingers() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let start = three_fingers(0.5);
        swipe.sample(start.clone(), 3, false, now, |a, b| a == b);

        // Frame 1: thumb rolls 1.0 mm, two fingers move 1.5 mm.
        let mut step1 = start.clone();
        step1[0].x += 1.0 / 127.0;
        step1[1].x += 1.5 / 127.0;
        step1[2].x += 1.5 / 127.0;
        assert_eq!(
            swipe.sample(step1.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );

        // Frame 2: thumb barely drifts 0.01 mm while two scrolling fingers reach 3.0 mm
        // (mean 2.34 mm >= 2.2 mm, min 1.01 mm >= 0.8 mm and >= 25% of 3.0 mm, but
        // frame drift 0.01 mm < 25% of the scrolling fingers' 1.5 mm frame step).
        let mut step2 = step1;
        step2[0].x += 0.01 / 127.0;
        step2[1].x = start[1].x + 3.0 / 127.0;
        step2[2].x = start[2].x + 3.0 / 127.0;
        assert_eq!(
            swipe.sample(step2.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert!(!swipe.is_active());

        // Frames 3..18: as the 2-finger scroll slows, all three contacts drift 0.01 mm
        // together every 10 ms for 16 frames. Because RECENT_MOTION_WINDOW (60 ms)
        // bounds accumulation, it never reaches MIN_COMMIT_FRAME_MM and never commits.
        let mut step = step2;
        for frame in 1..=16 {
            for f in &mut step {
                f.x += 0.01 / 127.0;
            }
            assert_eq!(
                swipe.sample(
                    step.clone(),
                    3,
                    false,
                    now + Duration::from_millis(frame * 10),
                    |a, b| a == b
                ),
                SwipeSampleOutcome::Pending
            );
            assert!(!swipe.is_active());
        }

        // Frame 19: after a 160 ms gap, a single 0.16 mm drift on all three contacts
        // expires the stale reference before checking commit, so it still does not commit.
        for f in &mut step {
            f.x += 0.16 / 127.0;
        }
        assert_eq!(
            swipe.sample(step, 3, false, now + Duration::from_millis(320), |a, b| a
                == b),
            SwipeSampleOutcome::Pending
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn fourth_contact_moving_vertically_blocks_three_finger_horizontal_commit() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.2,
            y: 0.5,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        swipe.sample(start.clone(), 3, false, now, |a, b| a == b);

        let mut moved = start;
        for f in &mut moved[0..3] {
            f.x += 3.0 / 127.0;
        }
        moved[3].y += 3.0 / 84.7;
        assert_eq!(
            swipe.sample(moved, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn extra_contact_joining_mid_swipe_ends_active_swipe() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.05,
            y: 0.05,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        swipe.sample(start.clone(), 3, false, now, |a, b| a == b);

        let mut moved = start;
        for f in &mut moved[0..3] {
            f.x += 3.0 / 127.0;
        }
        assert!(matches!(
            swipe.sample(moved.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Movement {
                just_committed: true,
                ..
            }
        ));

        // Now the 4th contact starts moving >= ARM_DISTANCE_MM.
        moved[3].x += 3.0 / 127.0;
        assert_eq!(
            swipe.sample(moved.clone(), 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Ended
        );
        assert!(!swipe.is_active());

        // Same four contacts stay rejected even if the 4th pauses while the other 3 keep moving.
        for f in &mut moved[0..3] {
            f.x += 4.0 / 127.0;
        }
        assert_eq!(
            swipe.sample(moved, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn retouch_without_began_flag_emits_resumed() {
        let t0 = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        commit_horizontal(&mut swipe, t0);
        assert!(swipe.end(t0 + Duration::from_millis(10)));

        let t_retouch = t0 + Duration::from_millis(25);
        assert_eq!(
            swipe.sample(three_fingers_144(0.2), 3, false, t_retouch, |a, b| a == b),
            SwipeSampleOutcome::Resumed
        );
        assert!(swipe.is_active());
    }

    #[test]
    fn uneven_vertical_frame_stays_pending_and_commits_on_next_coherent_frame() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.05,
            y: 0.05,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        swipe.sample(start.clone(), 3, true, now, |a, b| a == b);

        // Frame 1: all 3 fingers move 2.0 mm vertically (still < 2.2 mm).
        let mut step1 = start.clone();
        for f in &mut step1[0..3] {
            f.y += 2.0 / 84.7;
        }
        assert_eq!(
            swipe.sample(step1, 3, true, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );

        // Frame 2: uneven jump to [8.1, 8.1, 2.8] mm with 8.1 mm sideways on finger 0;
        // still vertically dominant, fails only recent-motion coherence, so it must stay
        // Pending (not fall through to horizontal rejection).
        let mut step2 = start;
        step2[0].x += 8.1 / 127.0;
        step2[0].y += 8.1 / 84.7;
        step2[1].y += 8.1 / 84.7;
        step2[2].y += 2.8 / 84.7;
        assert_eq!(
            swipe.sample(step2.clone(), 3, true, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );

        // Frame 3: all 3 fingers move 1.0 mm vertically together -> commits cleanly.
        let mut step3 = step2;
        for f in &mut step3[0..3] {
            f.y += 1.0 / 84.7;
        }
        assert!(matches!(
            swipe.sample(step3, 3, true, now, |a, b| a == b),
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Vertical(_),
                just_committed: true
            }
        ));
    }

    #[test]
    fn participating_fourth_contact_rejects_vertical_swipe_even_if_it_returns() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.2,
            y: 0.5,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        swipe.sample(start.clone(), 3, true, now, |a, b| a == b);

        // Four contacts at vertical offsets [9, 9, 9, -4] mm after a 100 ms gap
        // (past RECENT_MOTION_WINDOW) -> 4th contact is moving, so this must still
        // Reject rather than returning Pending on reference expiry.
        let mut step1 = start.clone();
        for f in &mut step1[0..3] {
            f.y += 9.0 / 84.7;
        }
        step1[3].y -= 4.0 / 84.7;
        assert_eq!(
            swipe.sample(step1, 3, true, now + Duration::from_millis(100), |a, b| a
                == b),
            SwipeSampleOutcome::Rejected
        );

        // Even if the 4th contact returns to 0 on the next frame ([10, 10, 10, 0] mm),
        // the rejected touch set stays rejected.
        let mut step2 = start;
        for f in &mut step2[0..3] {
            f.y += 10.0 / 84.7;
        }
        assert_eq!(
            swipe.sample(step2, 3, true, now + Duration::from_millis(110), |a, b| a
                == b),
            SwipeSampleOutcome::Rejected
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn ambiguous_diagonal_vertical_candidate_stays_pending_then_commits() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.05,
            y: 0.05,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        swipe.sample(start.clone(), 3, true, now, |a, b| a == b);

        // 3 fingers reach 7 mm sideways and 9 mm vertically with a stationary palm:
        // fails AXIS_DOMINANCE (9 < 1.5 * 7 = 10.5), must stay Pending (not Rejected).
        let mut step1 = start;
        for f in &mut step1[0..3] {
            f.x += 7.0 / 127.0;
            f.y += 9.0 / 84.7;
        }
        assert_eq!(
            swipe.sample(step1.clone(), 3, true, now, |a, b| a == b),
            SwipeSampleOutcome::Pending
        );

        // Next frame moves 3 mm further vertically (12 mm >= 1.5 * 7 = 10.5 mm) -> commits.
        let mut step2 = step1;
        for f in &mut step2[0..3] {
            f.y += 3.0 / 84.7;
        }
        assert!(matches!(
            swipe.sample(step2, 3, true, now, |a, b| a == b),
            SwipeSampleOutcome::Movement {
                movement: SwipeMovement::Vertical(_),
                just_committed: true
            }
        ));
    }

    #[test]
    fn participating_fourth_contact_rejects_horizontal_swipe_even_if_it_returns() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let mut start = three_fingers(0.5);
        start.push(SwipeSample {
            identity: 99,
            x: 0.2,
            y: 0.5,
            device_width_points: 360.0,
            device_height_points: 240.0,
        });
        swipe.sample(start.clone(), 3, false, now, |a, b| a == b);

        // Horizontal offsets [9, 9, 9, -4] mm -> 4th contact moved, must Reject.
        let mut step1 = start.clone();
        for f in &mut step1[0..3] {
            f.x += 9.0 / 127.0;
        }
        step1[3].x -= 4.0 / 127.0;
        assert_eq!(
            swipe.sample(step1, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );

        // Even if the 4th contact returns to 0 on the next frame ([10, 10, 10, 0] mm),
        // the rejected touch set stays rejected.
        let mut step2 = start;
        for f in &mut step2[0..3] {
            f.x += 10.0 / 127.0;
        }
        assert_eq!(
            swipe.sample(step2, 3, false, now, |a, b| a == b),
            SwipeSampleOutcome::Rejected
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn scroll_suppression_scopes_to_active_swipe_grace_and_momentum() {
        let t0 = Instant::now();
        let mut last = Some(t0);

        // Discrete mouse wheel always passes through.
        assert!(!should_suppress_scroll_wheel(
            true,
            &mut last,
            false,
            0,
            0,
            t0 + Duration::from_millis(10)
        ));

        // Active swipe suppresses continuous trackpad scroll.
        assert!(should_suppress_scroll_wheel(
            true,
            &mut last,
            true,
            CG_SCROLL_PHASE_BEGAN,
            0,
            t0 + Duration::from_millis(20)
        ));

        // Within 120 ms grace after lift, non-momentum scroll (e.g. Began) is suppressed.
        assert!(should_suppress_scroll_wheel(
            false,
            &mut last,
            true,
            CG_SCROLL_PHASE_BEGAN,
            0,
            t0 + Duration::from_millis(50)
        ));

        // Non-momentum Changed at 150 ms (after grace) passes without clearing momentum guard.
        assert!(!should_suppress_scroll_wheel(
            false,
            &mut last,
            true,
            2,
            0,
            t0 + Duration::from_millis(150)
        ));
        assert!(last.is_some());

        // Momentum tail from prior swipe at 200 ms is still suppressed inside the 1200 ms window.
        assert!(should_suppress_scroll_wheel(
            false,
            &mut last,
            true,
            0,
            2,
            t0 + Duration::from_millis(200)
        ));

        // A fresh non-momentum Began at 250 ms (after grace) clears the guard so its own
        // momentum at 400 ms passes through.
        assert!(!should_suppress_scroll_wheel(
            false,
            &mut last,
            true,
            CG_SCROLL_PHASE_BEGAN,
            0,
            t0 + Duration::from_millis(250)
        ));
        assert!(last.is_none());
        assert!(!should_suppress_scroll_wheel(
            false,
            &mut last,
            true,
            0,
            2,
            t0 + Duration::from_millis(400)
        ));

        // Without a fresh Began, momentum stays suppressed until 1200 ms expiry.
        let mut last2 = Some(t0);
        assert!(!should_suppress_scroll_wheel(
            false,
            &mut last2,
            true,
            0,
            2,
            t0 + Duration::from_millis(1250)
        ));
        assert!(last2.is_none());
    }

    fn horizontal_key(verb: &str, side: crate::commands::Side, repeat: bool) -> Event {
        let chord = if verb == "swap" {
            "ctrl+alt+shift"
        } else {
            "ctrl+alt"
        };
        let (key, code) = if side == crate::commands::Side::West {
            ("leftarrow", 123)
        } else {
            ("rightarrow", 124)
        };
        let config = Config::try_from(
            format!(
                "[options]\n[bindings]\nwindow_{verb}_{} = '{chord}-{key}'\n",
                side.token()
            )
            .as_str(),
        )
        .expect("valid keyboard config");
        let (sender, receiver) = EventSender::new();
        let handler = InputHandler::new(sender, config);
        let flags = if verb == "swap" {
            0x00ae_0023
        } else {
            0x00ac_0021
        };
        assert!(handler.handle_keypress(code, CGEventFlags(flags), repeat));
        receiver.try_recv().expect("keyboard command")
    }

    #[test]
    fn plain_focus_key_crosses_a_touching_display_at_the_strip_end() {
        use crate::commands::Side;
        use crate::tests::*;
        use bevy::math::{IRect, IVec2};
        for side in [Side::West, Side::East] {
            let x = if side == Side::West {
                -EXT_DISPLAY_WIDTH
            } else {
                TEST_DISPLAY_WIDTH
            };
            let bounds = IRect::from_corners(
                IVec2::new(x, 0),
                IVec2::new(x + EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
            );
            let mut harness = TestHarness::new()
                .with_windows(1)
                .with_display(EXT_DISPLAY_ID, bounds, vec![EXT_WORKSPACE_ID])
                .with_window(100, |window| {
                    window.workspace_id = EXT_WORKSPACE_ID;
                    window.frame = bounds;
                });
            harness.run(vec![
                Event::MenuOpened { window_id: 0 },
                horizontal_key("focus", side, false),
            ]);
            crate::assert_focused!(harness.world(), 100);
        }
    }

    #[test]
    fn plain_move_key_crosses_a_touching_display_at_the_strip_end() {
        use crate::commands::Side;
        use crate::ecs::layout::LayoutStrip;
        use crate::tests::*;
        use bevy::math::{IRect, IVec2};
        for side in [Side::West, Side::East] {
            let x = if side == Side::West {
                -EXT_DISPLAY_WIDTH
            } else {
                TEST_DISPLAY_WIDTH
            };
            let bounds = IRect::from_corners(
                IVec2::new(x, 0),
                IVec2::new(x + EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
            );
            let mut harness = TestHarness::new().with_windows(1).with_display(
                EXT_DISPLAY_ID,
                bounds,
                vec![EXT_WORKSPACE_ID],
            );
            harness.run(vec![
                Event::MenuOpened { window_id: 0 },
                horizontal_key("swap", side, false),
            ]);
            let world = harness.world();
            let entity = find_window_entity(0, world);
            assert!(
                world
                    .query::<&LayoutStrip>()
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.contains(entity)),
                "plain move crosses {side:?}"
            );
            crate::assert_focused!(world, 0);
        }
    }

    #[test]
    fn repeated_plain_keyboard_events_do_not_cross_a_touching_display() {
        use crate::commands::Side;
        use crate::ecs::layout::LayoutStrip;
        use crate::tests::*;
        use bevy::math::{IRect, IVec2};
        for side in [Side::West, Side::East] {
            for verb in ["focus", "swap"] {
                let x = if side == Side::West {
                    -EXT_DISPLAY_WIDTH
                } else {
                    TEST_DISPLAY_WIDTH
                };
                let bounds = IRect::from_corners(
                    IVec2::new(x, 0),
                    IVec2::new(x + EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
                );
                let mut harness = TestHarness::new()
                    .with_windows(1)
                    .with_display(EXT_DISPLAY_ID, bounds, vec![EXT_WORKSPACE_ID])
                    .with_window(100, |window| {
                        window.workspace_id = EXT_WORKSPACE_ID;
                        window.frame = bounds;
                    });
                harness.run(vec![
                    Event::MenuOpened { window_id: 0 },
                    horizontal_key(verb, side, true),
                ]);
                let world = harness.world();
                let entity = find_window_entity(0, world);
                assert!(
                    world
                        .query::<&LayoutStrip>()
                        .iter(world)
                        .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.contains(entity))
                );
                crate::assert_focused!(world, 0);
            }
        }
    }

    fn step_config(extra: &str) -> Config {
        Config::try_from(format!(
            "[options]\n[bindings]\n[swipe.scroll]\nmodifier = 'alt'\nwindow_step = true\n{extra}"
        ).as_str()).expect("valid test config")
    }

    #[test]
    fn wheel_steps_use_delta_sign_and_configured_direction() {
        for reversed in [false, true] {
            let config = step_config(if reversed {
                "[swipe.gesture]\ndirection = 'Reversed'\n"
            } else {
                ""
            });
            for delta in [0.1, 1.0, 3.0, 120.0] {
                for signed_delta in [delta, -delta] {
                    let direction = scroll_step_direction(&config, signed_delta);
                    let expected = if (signed_delta > 0.0) == reversed {
                        Direction::West
                    } else {
                        Direction::East
                    };
                    assert_eq!(direction, expected);
                }
            }
        }
    }

    #[test]
    fn physical_swipe_distance_averages_finger_travel_and_preserves_sign() {
        // 144 device points at 72 ppi is 5.08 cm wide; half its width is 2.54 cm.
        let left = [(0.5, 144.0); 3];
        assert_eq!(physical_swipe_distance_cm(left), Some(2.54));
        let right = [(-0.5, 144.0); 4];
        assert_eq!(physical_swipe_distance_cm(right), Some(-2.54));
        assert_eq!(physical_swipe_distance_cm([(0.5, 0.0)]), None);
    }

    #[test]
    fn wheel_scroll_stays_free_unless_step_mode_is_enabled() {
        assert!(!Config::default().swipe_scroll_window_step());
        let disabled = Config::try_from("[swipe.scroll]\nwindow_step = false\n").unwrap();
        assert!(!disabled.swipe_scroll_window_step());
        let enabled = Config::try_from("[swipe.scroll]\nwindow_step = true\n").unwrap();
        assert!(enabled.swipe_scroll_window_step());
    }

    #[test]
    fn wheel_steps_limit_repeats_but_allow_immediate_reversal() {
        let now = Instant::now();
        let mut last = None;
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now
        ));
        assert!(!accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now + Duration::from_millis(100)
        ));
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now + WINDOW_SCROLL_INTERVAL
        ));
        assert!(accept_window_scroll(
            &mut last,
            &Direction::West,
            false,
            now + WINDOW_SCROLL_INTERVAL
        ));
    }

    #[test]
    fn wheel_momentum_does_not_select_a_window_or_delay_the_next_input() {
        let now = Instant::now();
        let mut last = None;
        assert!(!accept_window_scroll(
            &mut last,
            &Direction::East,
            true,
            now
        ));
        assert!(last.is_none());
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now
        ));
        assert!(!accept_window_scroll(
            &mut last,
            &Direction::West,
            true,
            now
        ));
        assert!(accept_window_scroll(
            &mut last,
            &Direction::East,
            false,
            now + WINDOW_SCROLL_INTERVAL
        ));
    }

    #[test]
    fn wheel_steps_focus_adjacent_windows_and_stop_at_strip_edges() {
        use crate::tests::TestHarness;

        let config = step_config("");
        let mut events = vec![Event::MenuOpened { window_id: 0 }];
        events.extend(
            [120.0, 3.0, 3.0, -120.0, -3.0, -3.0].map(|delta| Event::Command {
                command: Command::Window(Operation::Focus(scroll_step_direction(&config, delta))),
            }),
        );
        let mut harness = TestHarness::new().with_config(config).with_windows(3);
        for (iteration, expected) in [0, 1, 2, 2, 1, 0, 0].into_iter().enumerate() {
            harness = harness.on_iteration(iteration, move |world, _| {
                crate::assert_focused!(world, expected);
                assert_eq!(
                    world.query::<&crate::ecs::Scrolling>().iter(world).count(),
                    0
                );
            });
        }
        harness.run(events);
    }

    const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
    const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

    #[test]
    fn no_modifiers() {
        assert_eq!(get_modifiers(CGEventFlags(0)), Modifiers::empty());
    }

    #[test]
    fn fn_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICEFNKEYMASK)),
            Modifiers::FN
        );
    }

    #[test]
    fn single_left_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELALTKEYMASK)),
            Modifiers::LALT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELSHIFTKEYMASK)),
            Modifiers::LSHIFT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELCMDKEYMASK)),
            Modifiers::LCMD
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELCTLKEYMASK)),
            Modifiers::LCTRL
        );
    }

    #[test]
    fn single_right_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERALTKEYMASK)),
            Modifiers::RALT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERSHIFTKEYMASK)),
            Modifiers::RSHIFT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERCMDKEYMASK)),
            Modifiers::RCMD
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERCTLKEYMASK)),
            Modifiers::RCTRL
        );
    }

    #[test]
    fn both_sides_of_same_modifier() {
        let flags = NX_DEVICELALTKEYMASK | NX_DEVICERALTKEYMASK;
        assert_eq!(get_modifiers(CGEventFlags(flags)), Modifiers::ALT);
    }

    #[test]
    fn multiple_different_modifiers() {
        let flags = NX_DEVICELCTLKEYMASK | NX_DEVICELALTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::LCTRL | Modifiers::LALT
        );
    }

    #[test]
    fn mixed_sides_across_groups() {
        let flags = NX_DEVICELCMDKEYMASK | NX_DEVICERALTKEYMASK | NX_DEVICERSHIFTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::LCMD | Modifiers::RALT | Modifiers::RSHIFT
        );
    }

    #[test]
    fn device_independent_flags_ignored() {
        let generic_alt: u64 = 0x0008_0000;
        assert_eq!(get_modifiers(CGEventFlags(generic_alt)), Modifiers::empty());
    }

    // kCGEventFlagMaskControl and kCGEventFlagMaskAlternate, with no device bit.
    const GENERIC_CONTROL: u64 = 0x0004_0000;
    const GENERIC_OPTION: u64 = 0x0008_0000;

    #[test]
    fn generic_control_option_flags_match_a_control_option_shortcut() {
        let event = get_binding_modifiers(CGEventFlags(GENERIC_CONTROL | GENERIC_OPTION));
        let shortcut = Modifiers::CTRL | Modifiers::ALT;
        assert!(
            shortcut.matches(event),
            "a Control-Option shortcut must match a key event that carries only the generic flags, got {event:?}"
        );
    }

    #[test]
    fn device_bits_still_tell_modifier_sides_apart() {
        let left = get_binding_modifiers(CGEventFlags(GENERIC_CONTROL | NX_DEVICELCTLKEYMASK));
        assert!(Modifiers::LCTRL.matches(left), "{left:?}");
        assert!(!Modifiers::RCTRL.matches(left), "{left:?}");

        let right = get_binding_modifiers(CGEventFlags(GENERIC_CONTROL | NX_DEVICERCTLKEYMASK));
        assert!(Modifiers::RCTRL.matches(right), "{right:?}");
        assert!(!Modifiers::LCTRL.matches(right), "{right:?}");
    }

    #[test]
    fn shared_mouse_alt_scroll_matches_generic_binding() {
        // Deskflow uses the group flag for scroll events from Windows Alt.
        let modifiers = get_binding_modifiers(CGEventFlags(0x2008_0000));
        assert!(Modifiers::ALT.matches(modifiers));
        // The event does not identify a side. Do not select one.
        assert!(!Modifiers::LALT.matches(modifiers));
        assert!(!Modifiers::RALT.matches(modifiers));
    }

    #[test]
    fn shared_mouse_extra_modifier_does_not_trigger_alt_scroll() {
        let modifiers = get_binding_modifiers(CGEventFlags(0x200c_0000));
        assert!(!Modifiers::ALT.matches(modifiers));
        assert!((Modifiers::ALT | Modifiers::CTRL).matches(modifiers));
    }

    #[test]
    fn physical_scroll_keeps_the_reported_modifier_side() {
        let modifiers = get_binding_modifiers(CGEventFlags(0x0008_0000 | NX_DEVICERALTKEYMASK));
        assert!(Modifiers::ALT.matches(modifiers));
        assert!(Modifiers::RALT.matches(modifiers));
        assert!(!Modifiers::LALT.matches(modifiers));
    }

    #[test]
    fn ordinary_scroll_does_not_trigger_alt_scroll() {
        assert!(!Modifiers::ALT.matches(get_binding_modifiers(CGEventFlags(0x2000_0000))));
    }

    #[test]
    fn shared_mouse_scroll_supports_other_modifier_groups() {
        for (group, flag) in [
            (Modifiers::SHIFT, 0x0002_0000),
            (Modifiers::CTRL, 0x0004_0000),
            (Modifiers::CMD, 0x0010_0000),
        ] {
            assert!(group.matches(get_binding_modifiers(CGEventFlags(flag))));
        }
    }

    #[test]
    fn secondary_fn_flag_is_not_ignored() {
        // Ensure we don't accidentally filter out the fn mask as "device independent"
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICEFNKEYMASK)),
            Modifiers::FN
        );
    }

    #[test]
    fn empty_intermediate_gesture_event_preserves_pending_movement() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        let base = commit_horizontal(&mut swipe, now);
        // Skipping an empty intermediate raw event leaves the active accumulator intact.
        assert_horizontal(
            swipe.sample(three_fingers_144(base + 0.0006), 3, false, now, |a, b| {
                a == b
            }),
            -0.0018,
        );
        assert_horizontal(
            swipe.sample(three_fingers_144(base + 0.0012), 3, false, now, |a, b| {
                a == b
            }),
            -0.0018,
        );
    }

    #[test]
    fn finger_count_change_ends_active_swipe() {
        let now = Instant::now();
        let mut swipe = SwipeAccumulator::default();
        commit_horizontal(&mut swipe, now);
        assert!(swipe.is_active());
        assert!(swipe.end(now));
        assert!(!swipe.is_active());
    }
}
