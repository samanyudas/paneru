//! Window writes reach each app on that app's own writer, so a slow app
//! holds up neither the engine frame nor any other app's windows.

use std::time::{Duration, Instant};

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::SpawnWindowTrigger;
use crate::events::Event;
use crate::manager::Window;
use crate::platform::{Pid, WinID};

use super::*;

/// Owns windows 0 and 2.
const SLOW_APP: Pid = TEST_PROCESS_ID;
/// Owns window 1.
const OTHER_APP: Pid = 2;

const WINDOW_WIDTH: i32 = 700;
/// How long each of the slow app's writes takes, like Helium mid-swipe.
const SLOW_WRITE: Duration = Duration::from_millis(200);
/// Well under one slow write, and well over an engine frame.
const FRAME_BUDGET: Duration = Duration::from_millis(100);
/// Simulated time per frame, as the harness runs it.
const FRAME: Duration = Duration::from_millis(20);
const FLUSH: Duration = Duration::from_secs(5);

/// Three columns, wider together than the display, whose apps take their
/// writes through the engine's per-app writers, settled at the strip start.
fn harness() -> TestHarness {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness =
        TestHarness::new()
            .with_config(config)
            .with_app(OTHER_APP, "other", "OtherApp", |_| {});
    harness.mock_state.queue_writes(SLOW_APP);
    harness.mock_state.queue_writes(OTHER_APP);
    for (id, pid) in [(0, SLOW_APP), (1, OTHER_APP), (2, SLOW_APP)] {
        let frame = IRect::new(0, 0, WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = harness
            .mock_state
            .spawn_window(pid, TEST_WORKSPACE_ID, id, frame);
        harness.existed_at_launch([id]);
        harness.world().trigger(SpawnWindowTrigger(vec![window]));
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ]);
    assert!(harness.mock_state.flush_writes(FLUSH));
    harness
}

fn swipe(harness: &mut TestHarness) {
    harness.world().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
}

/// The frame the engine asked window `id` to have.
fn engine_frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .frame()
}

fn position_writes(mock_state: &MockState, id: WinID) -> usize {
    let mut writes = 0;
    mock_state.update_window(id, |window| writes = window.position_writes);
    writes
}

/// Runs `frames` engine frames and returns the slowest one, in wall time.
fn run_frames(harness: &mut TestHarness, frames: usize) -> Duration {
    (0..frames)
        .map(|_| {
            let started = Instant::now();
            harness.advance(FRAME);
            started.elapsed()
        })
        .max()
        .unwrap_or_default()
}

/// Waits up to `timeout` for `done`, polling.
fn eventually(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Every write to the slow app takes 200ms, yet no engine frame of the swipe
/// waits for one, and once the app catches up every window is where the
/// engine put it.
#[test]
fn a_slow_app_does_not_hold_up_the_engine_frame() {
    let mut harness = harness();
    harness.mock_state.set_write_delay(SLOW_APP, SLOW_WRITE);
    let before = engine_frame(harness.world(), 0);

    swipe(&mut harness);
    let slowest = run_frames(&mut harness, 15);

    assert_ne!(
        engine_frame(harness.world(), 0),
        before,
        "the swipe moved the slow app's window"
    );
    assert!(
        slowest < FRAME_BUDGET,
        "an engine frame waited {slowest:?} on the slow app"
    );
    assert!(harness.mock_state.flush_writes(FLUSH));
    for id in 0..3 {
        assert_eq!(
            harness.mock_state.ax_frame(id),
            engine_frame(harness.world(), id),
            "window {id} landed where the engine put it"
        );
    }
}

/// While the slow app is stuck inside a write, the other app's window still
/// gets its frame at once.
#[test]
fn other_apps_get_their_writes_while_one_app_is_stuck() {
    let mut harness = harness();
    let stuck_at = harness.mock_state.ax_frame(0);
    harness.mock_state.hold_writes(SLOW_APP);

    swipe(&mut harness);
    run_frames(&mut harness, 3);

    let other = engine_frame(harness.world(), 1);
    let landed = eventually(FRAME_BUDGET, || harness.mock_state.ax_frame(1) == other);
    let slow_app_frame = harness.mock_state.ax_frame(0);
    harness.mock_state.release_writes(SLOW_APP);

    assert!(landed, "the other app's window reached {other:?}");
    assert_eq!(
        slow_app_frame, stuck_at,
        "the stuck app had not applied its write yet"
    );
    assert!(harness.mock_state.flush_writes(FLUSH));
    assert_eq!(
        harness.mock_state.ax_frame(0),
        engine_frame(harness.world(), 0)
    );
}

/// The engine asks for a new frame every swipe frame while the slow app is
/// stuck in its first write. Once it answers, it gets the newest frame only,
/// not every frame it missed.
#[test]
fn a_stuck_app_gets_only_the_newest_frame_once_it_answers() {
    let mut harness = harness();
    let before = position_writes(&harness.mock_state, 0);
    harness.mock_state.hold_writes(SLOW_APP);

    swipe(&mut harness);
    let mut asked = Vec::new();
    for _ in 0..10 {
        run_frames(&mut harness, 1);
        let frame = engine_frame(harness.world(), 0);
        if asked.last() != Some(&frame) {
            asked.push(frame);
        }
    }
    harness.mock_state.release_writes(SLOW_APP);
    assert!(harness.mock_state.flush_writes(FLUSH));

    assert!(
        asked.len() > 2,
        "the swipe asked for {} frames",
        asked.len()
    );
    assert_eq!(
        position_writes(&harness.mock_state, 0) - before,
        2,
        "the write in flight, then the newest frame"
    );
    assert_eq!(harness.mock_state.ax_frame(0), *asked.last().unwrap());
}
