use std::time::{Duration, Instant};

use bevy::math::IRect;

use crate::commands::{Command, Direction, Operation};
use crate::ecs::SpawnWindowTrigger;
use crate::events::Event;
use crate::platform::{Pid, WinID};

use super::*;

/// A second app whose windows share the strip with the harness's windows 0
/// and 1.
pub(super) const SLOW_APP: Pid = TEST_PROCESS_ID + 1;
pub(super) const SLOW_WINDOWS: [WinID; 2] = [2, 3];

pub(super) fn harness_with_slow_app() -> TestHarness {
    let mut harness = TestHarness::new()
        .with_windows(2)
        .with_app(SLOW_APP, "slow", "Slow", |_| {});
    let windows = SLOW_WINDOWS
        .iter()
        .map(|&id| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            harness
                .mock_state
                .spawn_window(SLOW_APP, TEST_WORKSPACE_ID, id, frame)
        })
        .collect();
    harness.world().trigger(SpawnWindowTrigger(windows));
    harness
}

/// Writes land on another thread, so wait a bounded time for one to arrive.
fn wait_for_x(state: &MockState, id: WinID, x: i32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.window_frame(id).min.x != x && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(state.window_frame(id).min.x, x, "window {id} x position");
}

/// The engine thread also runs the input event tap, so a window write that
/// waits on an unresponsive app holds up every keystroke and every other
/// app's windows with it.
#[test]
fn a_stuck_app_does_not_hold_up_the_engine() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness_with_slow_app()
        .on_iteration(0, |_world, state| {
            assert_eq!(state.window_frame(2).min.x, 800);
            assert_eq!(state.window_frame(3).min.x, 1019);
            state.hold_writes(SLOW_APP);
        })
        .on_iteration(1, |_world, state| {
            assert_eq!(
                state.expired_write_holds(SLOW_APP),
                0,
                "the engine waited for the stuck app to answer a window write"
            );
            assert_eq!(state.window_frame(0).min.x, -395);
            assert_eq!(state.window_frame(1).min.x, -176);
            state.release_writes(SLOW_APP);
        })
        .on_iteration(2, |_world, state| {
            wait_for_x(&state, 2, 224);
            wait_for_x(&state, 3, 624);
        })
        .run(commands);
}
