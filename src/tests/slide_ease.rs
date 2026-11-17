//! A keyboard slide eases along the curve every window motion follows, from
//! the key press. The engine's sleep before the press is not animation time,
//! so the first step covers one animation frame of the curve, as every later
//! step does.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::ease;
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

/// How far the passes of a slide go, as shares of the whole move, when
/// they follow each other by `deltas`.
fn ease_shares(deltas: &[Duration]) -> Vec<f64> {
    let duration = ease::duration_at(SPEED);
    let mut elapsed = 0.0;
    let mut previous = 0.0;
    deltas
        .iter()
        .map(|delta| {
            elapsed += delta.as_secs_f64();
            let at = ease::curve(elapsed / duration);
            let share = at - previous;
            previous = at;
            share
        })
        .collect()
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
    let want = ease_shares(&[ACTIVE_FRAME, ACTIVE_FRAME]);

    assert_share(steps[0], want[0], "the pass the key woke");
    assert_share(steps[1], want[1], "the next pass");
}

/// A frame that runs late during a slide still counts its whole time, so the
/// slide keeps its pace instead of stretching.
#[test]
fn a_late_frame_during_a_swap_counts_its_whole_time() {
    let late = Duration::from_millis(25);
    let steps = swap_steps(ACTIVE_FRAME, late, 2);
    let want = ease_shares(&[ACTIVE_FRAME, late]);

    assert_share(steps[1], want[1], "the late pass after the start");
}

/// Whether the fingers are on the trackpad, while a swipe or its settle runs.
fn swiping(world: &mut World) -> Option<bool> {
    world
        .query::<&Scrolling>()
        .iter(world)
        .next()
        .map(|scrolling| scrolling.is_user_swiping)
}

/// Once the fingers lift, a strip that fits the display slides back along
/// the curve every window motion follows, over about 237 ms at Miri's speed. It
/// resisted the fingers, so it starts from rest, with no jump at the lift,
/// and slows into where it was.
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

    let duration = ease::duration_at(SPEED);
    let frame = ACTIVE_FRAME.as_secs_f64();
    // 237 ms of 16 ms passes, after the pass the fingers lifted on.
    assert_eq!(left.len(), 15, "the settle lasts {duration} s: {left:?}");
    for (pass, at) in left.iter().enumerate().skip(1) {
        #[allow(clippy::cast_precision_loss, reason = "a few dozen passes")]
        let progress = ease::curve(pass as f64 * frame / duration);
        let want = left[0] * (1.0 - progress);
        assert!(
            (at - want).abs() <= 1.0,
            "pass {pass} left {at:.0}, expected {want:.0}: {left:?}"
        );
    }
}

/// How fast the curve moves at `s` toward its end, as a multiple of the
/// average speed that would cover what is left in the whole time.
fn speed_for_what_is_left(s: f64) -> f64 {
    let h = 1e-6;
    (ease::curve(s + h) - ease::curve(s)) / h / (1.0 - ease::curve(s))
}

/// A strip moving toward its landing carries its speed into the settle: it
/// enters the curve where the curve moves at that speed. One moving away,
/// or still, starts from rest, and so does one already at its landing.
#[test]
fn a_settle_carries_on_the_speed_toward_its_landing() {
    // 400 points over 0.5 s averages 800 points per second.
    for (velocity, distance) in [(800.0, 400.0), (-800.0, -400.0), (4000.0, 400.0)] {
        let start = ease::entry_for_speed(velocity, distance, 0.5);
        assert!(start > 0.0 && start < 1.0, "{velocity} enters at {start}");
        let want = velocity * 0.5 / distance;
        let speed = speed_for_what_is_left(start);
        assert!(
            (speed - want).abs() < 1e-3 * want,
            "{velocity} enters at {start}, moving at {speed}, not {want}"
        );
    }
    assert!(ease::entry_for_speed(-800.0, 400.0, 0.5).abs() < 1e-12);
    assert!(ease::entry_for_speed(0.0, 400.0, 0.5).abs() < 1e-12);
    assert!(ease::entry_for_speed(800.0, 0.0, 0.5).abs() < 1e-12);
    // The curve starts from rest, so even a slow strip enters a little way
    // in.
    let slow = ease::entry_for_speed(100.0, 400.0, 0.5);
    assert!(slow > 0.0 && slow < 0.05, "100 enters at {slow}");
}
