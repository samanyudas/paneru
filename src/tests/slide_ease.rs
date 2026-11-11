//! A keyboard slide eases out from the key press. The engine's sleep before
//! the press is not animation time, so the first step covers the share of one
//! animation frame, as every later step does.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::scroll::{handoff_slope, settle_curve};
use crate::ecs::systems::ease_out_factor;
use crate::ecs::{RepositionMarker, Scrolling};
use crate::events::Event;
use crate::platform::WinID;

use super::*;

const SPEED: f64 = 14.0;
/// A third of the display each, so a swap moves a window one third across.
const WIDTH: i32 = TEST_DISPLAY_WIDTH / 3;
/// The pump's poll timeout while a frame is active.
const ACTIVE_FRAME: Duration = Duration::from_millis(16);
/// The pump's longest idle sleep outside low power mode.
const IDLE_SLEEP: Duration = Duration::from_millis(500);
const FOCUSED: WinID = 2;

/// Windows 0, 1 and 2 in a row, at rest with window 2 focused. They fill
/// the display, so a swipe springs back.
fn harness() -> TestHarness {
    let config: Config = (
        MainOptions {
            animation_speed: Some(SPEED),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config);
    for id in 0..3 {
        let frame = IRect::new(0, 0, WIDTH, TEST_WINDOW_HEIGHT);
        let window = harness
            .mock_state
            .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, id, frame);
        harness.existed_at_launch([id]);
        harness
            .world()
            .trigger(crate::ecs::SpawnWindowTrigger(vec![window]));
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window(Operation::Focus(Direction::Last)),
    ]);
    settle(&mut harness);
    harness
}

fn window(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// Runs one pass of the engine that follows a sleep of `delta`.
fn pass(harness: &mut TestHarness, delta: Duration) {
    *harness.world().resource_mut::<TimeUpdateStrategy>() =
        TimeUpdateStrategy::ManualDuration(delta);
    harness.app.update();
    for event in harness.mock_state.drain_events() {
        harness.world().write_message::<Event>(event);
    }
}

fn moving(world: &mut World) -> bool {
    world
        .query_filtered::<(), With<RepositionMarker>>()
        .iter(world)
        .next()
        .is_some()
}

fn settle(harness: &mut TestHarness) {
    for _ in 0..200 {
        pass(harness, ACTIVE_FRAME);
        if !moving(harness.world()) {
            return;
        }
    }
    panic!("the row did not settle");
}

fn x(harness: &TestHarness) -> f64 {
    f64::from(harness.mock_state.ax_frame(FOCUSED).min.x)
}

/// The share of a whole swap that each of the first `frames` passes after
/// the key press covered, where the key woke the engine from `idle` and each
/// later pass followed the one before by `frame`.
fn swap_steps(idle: Duration, frame: Duration, frames: usize) -> Vec<f64> {
    let mut harness = harness();
    pass(&mut harness, IDLE_SLEEP);
    let start = x(&harness);

    harness
        .world()
        .write_message(window(Operation::Swap(Direction::West)));
    let mut positions = Vec::new();
    for index in 0..frames {
        pass(&mut harness, if index == 0 { idle } else { frame });
        positions.push(x(&harness));
    }
    settle(&mut harness);
    let distance = x(&harness) - start;
    assert!(
        distance.abs() >= f64::from(WIDTH) - 1.0,
        "window {FOCUSED} swapped a column west, moved {distance}"
    );

    let mut previous = start;
    positions
        .into_iter()
        .map(|position| {
            let share = (position - previous) / distance;
            previous = position;
            share
        })
        .collect()
}

/// How far an ease-out of `delta` goes, as a share of the whole move, after
/// `covered` of it is behind.
fn ease_share(delta: Duration, covered: f64) -> f64 {
    f64::from(ease_out_factor(SPEED, delta.as_secs_f64())) * (1.0 - covered)
}

/// Rounding to whole points can move a step by one point.
fn assert_share(step: f64, want: f64, what: &str) {
    let point = 1.0 / f64::from(WIDTH);
    assert!(
        (step - want).abs() <= point,
        "{what}: covered {step:.3} of the swap, expected {want:.3}"
    );
}

#[test]
fn a_swap_after_an_idle_sleep_starts_with_one_frame_of_ease() {
    let steps = swap_steps(IDLE_SLEEP, ACTIVE_FRAME, 2);

    assert_share(
        steps[0],
        ease_share(ACTIVE_FRAME, 0.0),
        "the pass the key woke",
    );
    assert_share(
        steps[1],
        ease_share(ACTIVE_FRAME, steps[0]),
        "the next pass",
    );
}

/// A frame that runs late during a slide still counts its whole time, so the
/// slide keeps its pace instead of stretching.
#[test]
fn a_late_frame_during_a_swap_counts_its_whole_time() {
    let late = Duration::from_millis(25);
    let steps = swap_steps(ACTIVE_FRAME, late, 2);

    assert_share(
        steps[1],
        ease_share(late, steps[0]),
        "the late pass after the start",
    );
}

/// Whether the fingers are on the trackpad, while a swipe or its settle runs.
fn swiping(world: &mut World) -> Option<bool> {
    world
        .query::<&Scrolling>()
        .iter(world)
        .next()
        .map(|scrolling| scrolling.is_user_swiping)
}

/// Once the fingers lift, a strip that fits the display slides back over
/// 7 / `animation_speed` seconds. It resisted the fingers, so it starts from
/// rest, with no jump at the lift, and slows into where it was.
#[test]
fn a_spring_back_eases_in_and_out() {
    let mut harness = harness();
    let rest = x(&harness);
    harness.world().write_message(Event::Swipe {
        delta: 0.3,
        fingers: 3,
        distance_cm: 0.0,
    });
    let mut left = Vec::new();
    for _ in 0..200 {
        pass(&mut harness, ACTIVE_FRAME);
        match swiping(harness.world()) {
            None => break,
            Some(true) => {}
            Some(false) => left.push(x(&harness) - rest),
        }
    }
    // The windows follow the strip on the next pass.
    pass(&mut harness, ACTIVE_FRAME);
    assert_eq!(x(&harness), rest, "the strip springs back to where it was");
    assert!(left[0].abs() > 10.0, "the swipe moved the strip: {left:?}");

    let duration = 7.0 / SPEED;
    let frame = ACTIVE_FRAME.as_secs_f64();
    // 0.5 s of 16 ms passes.
    assert_eq!(left.len(), 32, "the settle lasts {duration} s: {left:?}");
    for (pass, at) in left.iter().enumerate().skip(1) {
        #[allow(clippy::cast_precision_loss, reason = "a few dozen passes")]
        let progress = settle_curve(pass as f64 * frame / duration, 0.0);
        let want = left[0] * (1.0 - progress);
        assert!(
            (at - want).abs() <= 1.0,
            "pass {pass} left {at:.0}, expected {want:.0}: {left:?}"
        );
    }
}

/// The settle curve runs from 0 to 1 without overshooting, for every slope
/// a hand-off can give it, and starts at that slope.
#[test]
fn the_settle_curve_starts_at_its_slope_and_never_overshoots() {
    for slope in [0.0, 0.5, 1.0, 2.0, 3.0] {
        assert!(settle_curve(0.0, slope).abs() < 1e-12);
        assert!((settle_curve(1.0, slope) - 1.0).abs() < 1e-12);
        let h = 1e-6;
        let start = settle_curve(h, slope) / h;
        assert!(
            (start - slope).abs() < 1e-3,
            "slope {slope} starts at {start}"
        );
        let mut previous = 0.0;
        for step in 1..=1000 {
            let at = settle_curve(f64::from(step) / 1000.0, slope);
            assert!(
                at >= previous && at <= 1.0 + 1e-12,
                "slope {slope}: {at} after {previous}"
            );
            previous = at;
        }
    }
}

/// A strip moving toward its landing carries its speed into the settle. One
/// moving away, or still, starts from rest, and a fast one is capped where
/// the curve would overshoot.
#[test]
fn a_settle_carries_on_the_speed_toward_its_landing() {
    // 400 points over 0.5 s averages 800 points per second.
    assert!((handoff_slope(800.0, 400.0, 0.5) - 1.0).abs() < 1e-12);
    assert!((handoff_slope(-800.0, -400.0, 0.5) - 1.0).abs() < 1e-12);
    assert!(handoff_slope(-800.0, 400.0, 0.5).abs() < 1e-12);
    assert!(handoff_slope(0.0, 400.0, 0.5).abs() < 1e-12);
    assert!(handoff_slope(800.0, 0.0, 0.5).abs() < 1e-12);
    assert!((handoff_slope(10_000.0, 400.0, 0.5) - 3.0).abs() < 1e-12);
}
