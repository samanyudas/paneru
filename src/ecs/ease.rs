//! The curve that every window motion the engine animates follows.
//!
//! macOS zooms a window along `cubic-bezier(0.4, 0.1, 0.7, 1)` over about
//! 325 ms. Measured on macOS 27.0.1 from 60 fps recordings of a title-bar
//! Fill and its restore, that fit is within 0.4 percent RMS of every frame,
//! for a window that grows and one that shrinks. The motion starts gently,
//! moves at a steady pace through the middle, and lands softly.
//!
//! A motion keeps its place along the curve, from 0 at its start to 1 at its
//! end. Each pass steps a value by the share of what is left that the curve
//! covers in the pass's time, so a value that something else holds back,
//! such as pacing, still lands when the curve ends. A motion whose target
//! changes enters the curve again where the curve's speed toward the new
//! target matches the speed the value had, so it neither jumps nor stalls.

use bevy::ecs::change_detection::{DetectChanges as _, Ref};
use bevy::ecs::component::Component;
use bevy::ecs::system::{Populated, Res};
use bevy::math::{DVec2, IVec2};
use bevy::time::Time;

use crate::config::Config;
use crate::ecs::PresentedOrigin;
use crate::ecs::shared_edge::Slide;
use crate::ecs::systems::{AnimationStep, LOOP_MAX_TIMEOUT_FRAME_ACTIVE_MS};
use crate::manager::Window;

/// The control points of the curve, in the CSS order x1, y1, x2, y2.
const X1: f64 = 0.4;
const Y1: f64 = 0.1;
const X2: f64 = 0.7;
const Y2: f64 = 1.0;

/// How long a motion lasts at Miri's `animation_speed` of 14, in seconds.
const MIRI_DURATION: f64 = 0.325;
const MIRI_SPEED: f64 = 14.0;

/// One active frame, in seconds. A motion shorter than this lands in one
/// step.
fn frame_secs() -> f64 {
    f64::from(LOOP_MAX_TIMEOUT_FRAME_ACTIVE_MS) / 1000.0
}

/// How long a motion lasts, in seconds: 325 ms at Miri's `animation_speed`
/// of 14, and shorter at a faster speed. The unset default speed makes it
/// far shorter than a frame, so each motion lands in one step.
pub(crate) fn duration(config: &Config) -> f64 {
    MIRI_DURATION * MIRI_SPEED / config.animation_speed()
}

/// Whether a motion of `duration` seconds takes more than one step.
pub(crate) fn animates(duration: f64) -> bool {
    duration >= frame_secs()
}

fn bezier(p1: f64, p2: f64, u: f64) -> f64 {
    let v = 1.0 - u;
    3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u
}

fn bezier_slope(p1: f64, p2: f64, u: f64) -> f64 {
    let v = 1.0 - u;
    3.0 * v * v * p1 + 6.0 * v * u * (p2 - p1) + 3.0 * u * u * (1.0 - p2)
}

/// The Bezier parameter whose time is `s`. The time rises with the
/// parameter, so bisection always finds it, and Newton steps finish it.
fn parameter(s: f64) -> f64 {
    let (mut low, mut high) = (0.0, 1.0);
    let mut u = s;
    for _ in 0..32 {
        let error = bezier(X1, X2, u) - s;
        if error.abs() < 1e-12 {
            return u;
        }
        if error > 0.0 {
            high = u;
        } else {
            low = u;
        }
        let slope = bezier_slope(X1, X2, u);
        let next = u - error / slope;
        u = if slope > 1e-9 && next > low && next < high {
            next
        } else {
            f64::midpoint(low, high)
        };
    }
    u
}

/// How far along its distance a motion is at `s` of its time, from 0 to 1.
pub(crate) fn curve(s: f64) -> f64 {
    let s = s.clamp(0.0, 1.0);
    bezier(Y1, Y2, parameter(s))
}

/// The curve's slope at `s`: its speed as a multiple of the motion's average
/// speed.
fn curve_slope(s: f64) -> f64 {
    let u = parameter(s.clamp(0.0, 1.0));
    let dx = bezier_slope(X1, X2, u);
    if dx <= 1e-12 {
        return 0.0;
    }
    bezier_slope(Y1, Y2, u) / dx
}

/// The curve's speed at `s` toward its end, as a multiple of the average
/// speed that would cover what is left of the distance in the whole time.
/// It rises from 0.25 at the start toward infinity at the end.
fn speed_for_what_is_left(s: f64) -> f64 {
    let left = 1.0 - curve(s);
    if left <= 1e-12 {
        return f64::INFINITY;
    }
    curve_slope(s) / left
}

/// Where to enter the curve so that it starts at `speed`, a multiple of the
/// average speed that covers the whole distance in the whole time. A speed
/// at or below the curve's own start enters at its start.
pub(crate) fn entry(speed: f64) -> f64 {
    if speed.is_nan() || speed <= speed_for_what_is_left(0.0) {
        return 0.0;
    }
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..48 {
        let mid = f64::midpoint(low, high);
        if speed_for_what_is_left(mid) < speed {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// Where to enter the curve for a value moving at `speed` points per second
/// toward a target `distance` points away, over a motion of `duration`
/// seconds. A value moving away from the target, or at rest, enters at the
/// curve's start.
pub(crate) fn entry_for_speed(speed: f64, distance: f64, duration: f64) -> f64 {
    if distance.abs() < f64::EPSILON || duration <= 0.0 {
        return 0.0;
    }
    entry(speed * duration / distance)
}

/// The share of what is left at `from` that the curve covers by `to`, both
/// as shares of the motion's time.
pub(crate) fn share(from: f64, to: f64) -> f64 {
    if to >= 1.0 {
        return 1.0;
    }
    let start = curve(from);
    ((curve(to) - start) / (1.0 - start)).clamp(0.0, 1.0)
}

/// Moves `progress`, a motion's place along the curve, on by `dt` seconds
/// of a motion of `duration` seconds, and returns the share of what was
/// left that the step covers. A motion shorter than a frame lands at once.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a share in [0, 1], well within f32's range; only sub-pixel precision is lost"
)]
pub(crate) fn advance(progress: &mut f64, dt: f64, duration: f64) -> f32 {
    if !animates(duration) {
        *progress = 1.0;
        return 1.0;
    }
    let next = (*progress + dt.max(0.0) / duration).min(1.0);
    let t = share(*progress, next);
    *progress = next;
    t as f32
}

/// The share of the whole distance that the first active frame of a motion
/// from rest covers.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a share in [0, 1], well within f32's range; only sub-pixel precision is lost"
)]
pub(crate) fn first_share(duration: f64) -> f32 {
    let mut progress = 0.0;
    advance(&mut progress, frame_secs(), duration);
    curve(progress) as f32
}

/// How long after its last step a value still counts as moving, in
/// seconds: about three frames. A motion that starts later starts from rest.
const MOVING_WITHIN: f64 = 0.05;

/// One value's motion along the curve toward a target.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Ease {
    /// Where the motion heads. `None` before its first step.
    to: Option<IVec2>,
    /// The motion's place along the curve, from 0 to 1 of its time.
    progress: f64,
    /// The share of what was left that the last step covered.
    t: f32,
    /// Where the value was as the last step with time began, and when, in
    /// seconds of engine time, which tell its speed when a new motion
    /// starts.
    last: Option<(IVec2, f64)>,
}

impl Ease {
    /// Steps the motion by `dt` seconds of a motion of `duration` seconds,
    /// for a value at `value` that heads for `to`, at `now` seconds of
    /// engine time, and returns the share of what is left that this step
    /// covers.
    ///
    /// A new motion starts when `to` changes, or when `fresh` says the
    /// target was set afresh, such as a marker inserted again after its
    /// last motion ended. It enters the curve where the curve's speed toward
    /// `to` matches the value's speed over its last step, so a target that
    /// changes mid-motion neither jumps nor stalls. A value at rest, or
    /// moving away from `to`, starts at the curve's start.
    pub(crate) fn step(
        &mut self,
        value: IVec2,
        to: IVec2,
        fresh: bool,
        (now, dt): (f64, f64),
        duration: f64,
    ) -> f32 {
        if fresh || self.to != Some(to) {
            self.progress = self.entry_toward(value, to, now, duration);
            self.to = Some(to);
        }
        self.t = advance(&mut self.progress, dt, duration);
        if dt > 0.0 {
            self.last = Some((value, now));
        }
        self.t
    }

    /// The share of what was left that the last step covered.
    pub(crate) fn t(&self) -> f32 {
        self.t
    }

    /// Where a value at `value` enters the curve toward `to` at `now`, from
    /// how fast it moved toward `to` since its last step.
    fn entry_toward(&self, value: IVec2, to: IVec2, now: f64, duration: f64) -> f64 {
        let Some((before, then)) = self.last else {
            return 0.0;
        };
        let elapsed = now - then;
        if elapsed <= 0.0 || elapsed > MOVING_WITHIN {
            return 0.0;
        }
        let left = (to - value).as_dvec2();
        let distance = left.length();
        if distance < 1.0 {
            return 0.0;
        }
        let moved: DVec2 = (value - before).as_dvec2();
        entry_for_speed(moved.dot(left) / distance / elapsed, distance, duration)
    }
}

/// The motion of an entity's `Position` toward its `RepositionMarker`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub(crate) struct PositionEase(pub(crate) Ease);

/// The motion of an entity's `Bounds` toward its `ResizeMarker`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub(crate) struct SizeEase(pub(crate) Ease);

/// The motion of a sliding photo, or of a window that leaves for its tuck,
/// toward where its slide heads.
#[derive(Component, Clone, Copy, Debug, Default)]
pub(crate) struct SlideEase(pub(crate) Ease);

/// Where a slide shows now, and where it heads. `None` while it lands, at
/// rest under its photo.
pub(crate) fn slide_heading(slide: &Slide, presented: IVec2) -> Option<(IVec2, IVec2)> {
    match *slide {
        Slide::Moving { at, .. } => Some((at, presented)),
        Slide::Leaving { at, to, .. } | Slide::Tucking { at, to, .. } => Some((at, to)),
        Slide::Landing { .. } => None,
    }
}

/// Steps the ease of each slide, before pacing reads how far each photo
/// steps this frame and the photos step by it. A photo that waits for its
/// window to tuck under it holds its place on the curve, as it does not
/// move. A slide that lands forgets its motion, so the next one starts from
/// rest.
pub(crate) fn step_slides(
    slides: Populated<(&Window, Ref<Slide>, &PresentedOrigin, &mut SlideEase)>,
    step: Res<AnimationStep>,
    time: Res<Time>,
    config: Res<Config>,
) {
    let duration = duration(&config);
    for (window, slide, presented, mut ease) in slides {
        let waits = matches!(*slide, Slide::Tucking { until, .. }
            if window.write_pending() && time.elapsed() < until);
        let dt = if waits { 0.0 } else { step.secs() };
        match slide_heading(&slide, presented.0) {
            Some((at, to)) => {
                let now = time.elapsed_secs_f64();
                ease.0.step(at, to, slide.is_added(), (now, dt), duration);
            }
            None => ease.0 = Ease::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the Bezier at parameter `u`, as (time, progress).
    fn point(u: f64) -> (f64, f64) {
        (bezier(X1, X2, u), bezier(Y1, Y2, u))
    }

    /// The curve's progress at `s`, found by bisecting the Bezier's
    /// parameter on its own, apart from the solver under test.
    fn reference(s: f64) -> f64 {
        let (mut low, mut high) = (0.0, 1.0);
        for _ in 0..200 {
            let mid = f64::midpoint(low, high);
            if point(mid).0 < s {
                low = mid;
            } else {
                high = mid;
            }
        }
        point(f64::midpoint(low, high)).1
    }

    #[test]
    fn the_curve_follows_the_measured_bezier() {
        // Computed with an independent bisection in Python of
        // cubic-bezier(0.4, 0.1, 0.7, 1).
        for (s, want) in [
            (0.25, 0.163_500_577_448_484_8),
            (0.5, 0.482_896_656_134_697_9),
            (0.75, 0.830_666_168_233_599_6),
        ] {
            assert!(
                (curve(s) - want).abs() < 1e-9,
                "at {s}: {} not {want}",
                curve(s)
            );
        }
        for step in 0..=100 {
            let s = f64::from(step) / 100.0;
            assert!((curve(s) - reference(s)).abs() < 1e-9, "at {s}");
        }
        assert!(curve(0.0).abs() < 1e-12);
        assert!((curve(1.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_curve_starts_at_a_quarter_of_its_average_speed_and_lands_at_rest() {
        assert!((curve_slope(0.0) - Y1 / X1).abs() < 1e-9);
        assert!(curve_slope(1.0).abs() < 1e-9);
        assert!((speed_for_what_is_left(0.0) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn the_curve_speed_for_what_is_left_only_rises() {
        let mut previous = 0.0;
        for step in 0..1000 {
            let speed = speed_for_what_is_left(f64::from(step) / 1000.0);
            assert!(
                speed >= previous,
                "at step {step}: {speed} after {previous}"
            );
            previous = speed;
        }
    }

    #[test]
    fn a_motion_lasts_325_ms_at_miri_speed_and_scales_with_speed() {
        let at = |speed: f64| {
            let config: Config = (
                crate::config::MainOptions {
                    animation_speed: Some(speed),
                    ..Default::default()
                },
                vec![],
            )
                .into();
            duration(&config)
        };
        assert!((at(14.0) - 0.325).abs() < 1e-12);
        assert!((at(28.0) - 0.1625).abs() < 1e-12);
        assert!((at(7.0) - 0.65).abs() < 1e-12);
        assert!(animates(at(14.0)));
        assert!(!animates(at(10_000.0)));
    }

    /// With animation off, the unset default speed, the first step lands.
    #[test]
    fn the_default_speed_lands_in_one_step() {
        let config = Config::default();
        assert!(config.options().animation_speed.is_none());
        let duration = duration(&config);
        assert!(!animates(duration));
        for dt in [0.0, 0.004, 0.016] {
            let mut ease = Ease::default();
            let t = ease.step(IVec2::ZERO, IVec2::new(900, 0), true, (0.0, dt), duration);
            assert!(
                (t - 1.0).abs() < f32::EPSILON,
                "a step of {dt} s covers {t}"
            );
        }
    }

    const DURATION: f64 = 0.325;
    const DT: f64 = 1.0 / 60.0;

    /// A window that an ease steps at 60 fps, as each pass moves one.
    struct Moving {
        ease: Ease,
        at: IVec2,
        now: f64,
    }

    impl Moving {
        fn new(at: IVec2) -> Self {
            Self {
                ease: Ease::default(),
                at,
                now: 0.0,
            }
        }

        /// One pass toward `to`, which covers how far the window moved.
        fn step(&mut self, to: IVec2, fresh: bool) -> i32 {
            let t = self.ease.step(self.at, to, fresh, (self.now, DT), DURATION);
            let next = self.at.as_vec2().lerp(to.as_vec2(), t).round().as_ivec2();
            let moved = next.x - self.at.x;
            self.at = next;
            self.now += DT;
            moved
        }

        /// Passes toward `to` until the window lands, with where each put it.
        fn run(&mut self, to: IVec2) -> Vec<IVec2> {
            let mut path = Vec::new();
            for _ in 0..1000 {
                self.step(to, false);
                path.push(self.at);
                if self.at == to {
                    break;
                }
            }
            path
        }
    }

    /// Stepping by the share of what is left keeps a window on the curve,
    /// and it lands as the curve ends.
    #[test]
    fn steps_follow_the_curve_and_land_on_time() {
        let mut window = Moving::new(IVec2::ZERO);
        let path = window.run(IVec2::new(1000, 0));
        assert_eq!(path.len(), 20, "{path:?}");
        for (index, at) in path.iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "a few dozen passes")]
            let want = 1000.0 * curve((index + 1) as f64 * DT / DURATION);
            // Each pass rounds to whole points from the last.
            assert!(
                (f64::from(at.x) - want).abs() <= 2.0,
                "pass {index}: {at} not {want:.1}"
            );
        }
    }

    /// A target that changes mid-motion carries the window's speed on: the
    /// next step moves about as far as the last, with no jump and no stall.
    #[test]
    fn a_new_target_mid_motion_carries_the_speed_on() {
        let mut window = Moving::new(IVec2::ZERO);
        let mut speed = 0;
        for _ in 0..8 {
            speed = window.step(IVec2::new(600, 0), false);
        }
        assert!(speed > 20, "moving at {speed} a pass");
        let to = IVec2::new(1200, 0);
        let step = window.step(to, false);
        // The curve still gathers speed, so the step can be a little longer.
        assert!(
            step >= speed && step - speed <= speed / 8 + 1,
            "the step after the new target moved {step}, the one before {speed}"
        );
        let from = window.at.x;
        let rest = window.run(to);
        assert_eq!(rest.last(), Some(&to));
        let mut previous = from;
        for x in rest.iter().map(|at| at.x) {
            assert!(x >= previous, "turned back: {rest:?}");
            previous = x;
        }
        assert!(
            rest.len() < 20,
            "lands sooner than a whole motion: {rest:?}"
        );
    }

    /// A target behind the window starts a new motion from rest.
    #[test]
    fn a_target_behind_starts_from_rest() {
        let mut window = Moving::new(IVec2::ZERO);
        for _ in 0..8 {
            window.step(IVec2::new(600, 0), false);
        }
        let path = window.run(IVec2::new(-400, 0));
        assert_eq!(path.len(), 20, "a whole motion: {path:?}");
    }

    /// A motion that ends and has a new target set at once carries its speed
    /// on, as a window that chases a moving target does. One set after the
    /// window has rested starts from rest.
    #[test]
    fn a_target_set_afresh_starts_from_the_speed_the_window_had() {
        let mut window = Moving::new(IVec2::ZERO);
        let mut speed = 0;
        for _ in 0..8 {
            speed = window.step(IVec2::new(600, 0), false);
        }
        let step = window.step(IVec2::new(1200, 0), true);
        assert!(step >= speed, "moved {step} after {speed}");

        let mut window = Moving::new(IVec2::ZERO);
        window.run(IVec2::new(600, 0));
        window.now += 0.5;
        let t = window.ease.step(
            window.at,
            IVec2::new(1200, 0),
            true,
            (window.now, DT),
            DURATION,
        );
        assert!((f64::from(t) - share(0.0, DT / DURATION)).abs() < 1e-6);
    }
}
