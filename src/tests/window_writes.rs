use std::time::{Duration, Instant};

use bevy::app::AppExit;
use bevy::ecs::query::{Or, With};
use bevy::ecs::system::RunSystemOnce;
use bevy::ecs::world::World;
use bevy::math::IRect;

use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::config::{Config, InitialWindowWidth, MainOptions, WindowParams};
use crate::ecs::{
    Bounds, DeferredMoveReport, DeferredResizeCheck, DeferredResizeReport, LANDING_LIMIT, Position,
    SpawnWindowTrigger, VerifyWindowPosition, VerifyWindowSize,
};
use crate::events::Event;
use crate::manager::{Window, WindowManager};
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

/// A window reports each move, and the engine reads its frame back. While
/// the engine's own write to that window is still queued behind a stuck app,
/// that read would wait on the app, and the write reports again once it
/// lands.
#[test]
fn a_report_from_a_stuck_app_does_not_wait_for_its_queued_write() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::WindowMoved { window_id: 2 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness_with_slow_app()
        .on_iteration(0, |_world, state| state.hold_writes(SLOW_APP))
        .on_iteration(2, |_world, state| {
            assert_eq!(
                state.reads_behind_writes(SLOW_APP),
                0,
                "the engine read a window frame while its write to the stuck app was queued"
            );
            state.release_writes(SLOW_APP);
        })
        .on_iteration(3, |_world, state| {
            wait_for_x(&state, 2, 224);
            wait_for_x(&state, 3, 624);
        })
        .run(commands);
}

fn bounds_width(world: &mut World, id: WinID) -> i32 {
    world
        .query::<(&Window, &Bounds)>()
        .iter(world)
        .find_map(|(window, bounds)| (window.id() == id).then_some(bounds.0.x))
        .expect("finding window")
}

/// A new window is resized to its configured width and read back. When the
/// app has not taken the size by the time the read gives up, the engine
/// keeps the width it asked for rather than the one the window opened with.
#[test]
fn a_busy_app_keeps_a_new_window_at_its_configured_width() {
    const NEW_WINDOW: WinID = 4;
    let mut params = WindowParams::new("^Window 4$", None);
    params.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![params]).into();
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness_with_slow_app()
        .with_config(config)
        .on_iteration(0, |world, state| {
            state.hold_writes(SLOW_APP);
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(SLOW_APP, TEST_WORKSPACE_ID, NEW_WINDOW, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, state| {
            assert_eq!(bounds_width(world, NEW_WINDOW), TEST_DISPLAY_WIDTH / 2);
            state.release_writes(SLOW_APP);
        })
        .on_iteration(2, |world, state| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while state.window_frame(NEW_WINDOW).width() != TEST_DISPLAY_WIDTH / 2
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                state.window_frame(NEW_WINDOW).width(),
                TEST_DISPLAY_WIDTH / 2
            );
            assert_eq!(bounds_width(world, NEW_WINDOW), TEST_DISPLAY_WIDTH / 2);
        })
        .run(commands);
}

/// Where each window is, as `(id, engine x, engine width, app x, app width)`.
fn layout(harness: &mut TestHarness) -> Vec<(WinID, i32, i32, i32, i32)> {
    let state = harness.mock_state.clone();
    let world = harness.world();
    let mut rows: Vec<_> = world
        .query::<(&Window, &Position, &Bounds)>()
        .iter(world)
        .map(|(window, position, bounds)| {
            let app = state.window_frame(window.id());
            (
                window.id(),
                position.0.x,
                bounds.0.x,
                app.min.x,
                app.width(),
            )
        })
        .collect();
    rows.sort_unstable();
    rows
}

/// Window 2 is focused and refuses widths below 400 px, so a Shrink to the
/// 256 px preset is refused.
fn harness_focused_on_a_400_px_minimum(animation_speed: Option<f64>) -> TestHarness {
    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            animation_speed,
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = harness_with_slow_app().with_config(config);
    harness.mock_state.update_window(2, |w| w.min_width = 400);
    let east = || Event::Command {
        command: Command::Window(Operation::Focus(Direction::East)),
    };
    harness.run(vec![Event::MenuOpened { window_id: 0 }, east(), east()]);
    crate::assert_focused!(harness.world(), 2);
    harness
}

/// Window 2 refuses a Shrink below its 400 px minimum. With `slow`, its app
/// reports the size it took before the writer is done with the write, both
/// for the Shrink and for the writes that follow the refusal.
fn layout_after_refused_shrink(slow: bool) -> Vec<(WinID, i32, i32, i32, i32)> {
    let mut harness = harness_focused_on_a_400_px_minimum(None);

    if slow {
        harness.mock_state.hold_read_back(SLOW_APP);
    }
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
    });
    harness.advance(Duration::from_millis(500));
    harness.mock_state.release_writes(SLOW_APP);
    harness.mock_state.wait_for_writes();
    if slow {
        harness.mock_state.hold_read_back(SLOW_APP);
    }
    // Past `RESIZE_REFUSAL_TIMEOUT`, so the refusal is due to settle.
    harness.advance(Duration::from_millis(1500));
    harness.mock_state.release_writes(SLOW_APP);
    harness.mock_state.wait_for_writes();
    harness.advance(Duration::from_secs(1));
    harness.mock_state.wait_for_writes();
    layout(&mut harness)
}

/// The engine checks a finished resize for a size the app refused. While
/// the app is still busy with the write, the check waits for it rather than
/// give up, since nothing checks again later.
#[test]
fn a_slow_apps_refused_resize_settles_like_a_fast_ones() {
    let fast = layout_after_refused_shrink(false);
    assert_eq!(
        fast.iter().find(|row| row.0 == 2).map(|row| (row.2, row.4)),
        Some((400, 400)),
        "window 2 keeps its 400 px minimum"
    );
    assert_eq!(layout_after_refused_shrink(true), fast);
}

/// A resize that comes back short of the width asked for is retried from an
/// offscreen origin, but only when it grew the window, since a fixed-size
/// window comes back short every time. Here a fixed 400 px window is asked
/// for less, then for more, within one resize animation.
#[test]
fn a_fixed_size_window_is_never_moved_offscreen() {
    let mut harness = harness_focused_on_a_400_px_minimum(Some(20.0));
    harness.mock_state.update_window(2, |w| w.max_width = 400);

    for (direction, millis) in [(ResizeDirection::Shrink, 60), (ResizeDirection::Grow, 2000)] {
        harness.world().write_message(Event::Command {
            command: Command::Window(Operation::Resize(direction)),
        });
        harness.advance(Duration::from_millis(millis));
    }
    harness.mock_state.wait_for_writes();

    assert_eq!(
        harness.mock_state.origins_not_asked(2),
        Vec::new(),
        "window 2 was moved somewhere the layout never put it"
    );
    assert_eq!(harness.mock_state.window_frame(2).width(), 400);
}

fn landing_waits(world: &mut World) -> usize {
    world
        .query_filtered::<(), Or<(
            With<DeferredMoveReport>,
            With<DeferredResizeReport>,
            With<DeferredResizeCheck>,
        )>>()
        .iter(world)
        .count()
}

/// Reads put off for a write give up once the app has been stuck for
/// `LANDING_LIMIT`, rather than wait on it forever.
#[test]
fn reads_put_off_for_a_stuck_app_give_up() {
    let mut harness = harness_focused_on_a_400_px_minimum(None);

    harness.mock_state.hold_read_back(SLOW_APP);
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
    });
    harness.advance(Duration::from_millis(500));
    assert!(landing_waits(harness.world()) > 0, "a read was put off");
    harness.advance(LANDING_LIMIT);
    assert_eq!(
        landing_waits(harness.world()),
        0,
        "every put-off read gave up"
    );
    assert_eq!(
        harness.mock_state.expired_write_holds(SLOW_APP),
        0,
        "the app was stuck throughout"
    );
    harness.mock_state.release_writes(SLOW_APP);
    harness.mock_state.wait_for_writes();
}

/// A new window with a configured width of 512 px, which its app refuses:
/// the window opens at its 600 px minimum, so the app reports no change.
/// `pace` sets how the app answers the write.
fn layout_after_new_window(pace: impl FnOnce(&MockState)) -> Vec<(WinID, i32, i32, i32, i32)> {
    const NEW_WINDOW: WinID = 4;
    let mut params = WindowParams::new("^Window 4$", None);
    params.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = harness_with_slow_app().with_config(config);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    pace(&harness.mock_state);
    // Already the height the layout gives it, so the app has no change of
    // size to report.
    let frame = IRect::new(0, 20, 600, TEST_DISPLAY_HEIGHT);
    let window = harness
        .mock_state
        .spawn_window(SLOW_APP, TEST_WORKSPACE_ID, NEW_WINDOW, frame);
    harness
        .mock_state
        .update_window(NEW_WINDOW, |w| w.min_width = 600);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(Duration::from_millis(500));
    harness.mock_state.release_writes(SLOW_APP);
    harness.mock_state.wait_for_writes();
    harness.advance(Duration::from_millis(500));
    harness.mock_state.wait_for_writes();
    layout(&mut harness)
}

/// A new window is resized to its configured width and read back. When the
/// app is still busy with the write, the engine reads the size once the
/// write lands, since a refused size brings no report.
#[test]
fn a_new_window_on_a_slow_app_settles_like_on_a_fast_one() {
    let fast = layout_after_new_window(|_| {});
    assert_eq!(
        fast.iter().find(|row| row.0 == 4).map(|row| (row.2, row.4)),
        Some((600, 600)),
        "window 4 keeps its 600 px minimum"
    );
    let slow = |state: &MockState| state.hold_read_back(SLOW_APP);
    assert_eq!(layout_after_new_window(slow), fast);
}

/// The read of a new window's size can give up just before the write lands.
/// The engine still reads the size once the write has landed.
#[test]
fn a_new_window_whose_write_lands_as_the_read_gives_up_settles() {
    let fast = layout_after_new_window(|_| {});
    let late = |state: &MockState| {
        state.hold_read_back(SLOW_APP);
        state.land_as_read_gives_up(SLOW_APP);
    };
    assert_eq!(layout_after_new_window(late), fast);
}

/// The app reports a move while the writer is still busy with the write,
/// and has put the window somewhere other than asked. The engine reads
/// where once the write lands.
#[test]
fn a_move_report_during_a_slow_write_is_read_once_it_lands() {
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
        .on_iteration(0, |_world, state| state.hold_read_back(SLOW_APP))
        .on_iteration(1, |_world, state| {
            for id in SLOW_WINDOWS {
                state.update_window(id, |w| {
                    w.frame.min.y += 30;
                    w.frame.max.y += 30;
                });
            }
            state.release_writes(SLOW_APP);
            state.wait_for_writes();
        })
        .on_iteration(2, |world, state| {
            for id in SLOW_WINDOWS {
                let entity = find_window_entity(id, world);
                assert_eq!(
                    world.get::<Position>(entity).expect("position").0,
                    state.window_frame(id).min,
                    "window {id} is where its app put it"
                );
            }
        })
        .run(commands);
}

/// The app reports a resize of its own while the engine's write to the
/// window is still on its way. The engine reads the size once the write
/// lands, since no later report is coming.
#[test]
fn a_resize_report_during_a_slow_write_is_read_once_it_lands() {
    const WIDTH: i32 = 500;
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness_with_slow_app()
        .on_iteration(0, |_world, state| state.hold_read_back(SLOW_APP))
        .on_iteration(1, |world, state| {
            for id in SLOW_WINDOWS {
                state.update_window(id, |w| w.frame.max.x = w.frame.min.x + WIDTH);
                world.write_message(Event::WindowResized { window_id: id });
            }
        })
        .on_iteration(2, |_world, state| {
            state.release_writes(SLOW_APP);
            state.wait_for_writes();
        })
        .on_iteration(3, |world, _state| {
            for id in SLOW_WINDOWS {
                assert_eq!(bounds_width(world, id), WIDTH, "window {id} width");
            }
        })
        .run(commands);
}

/// A window that becomes managed again has its position checked a few
/// times. A write still on its way does not use up those checks, and the
/// check does not wait on the app.
#[test]
fn a_position_check_waits_for_a_slow_write_without_using_attempts() {
    let manage = || Event::Command {
        command: Command::Window(Operation::Manage),
    };
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        manage(),
        manage(),
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness_with_slow_app()
        .on_iteration(2, |_world, state| state.hold_writes(SLOW_APP))
        .on_iteration(3, |world, state| {
            assert_eq!(
                state.reads_behind_writes(SLOW_APP),
                0,
                "the position check waited on the slow app"
            );
            let entity = find_window_entity(3, world);
            assert!(world.get::<VerifyWindowPosition>(entity).is_some());
            state.release_writes(SLOW_APP);
            state.wait_for_writes();
        })
        .on_iteration(4, |world, state| {
            let entity = find_window_entity(3, world);
            assert!(world.get::<VerifyWindowPosition>(entity).is_none());
            assert_eq!(
                world.get::<Position>(entity).expect("position").0,
                state.window_frame(3).min
            );
        })
        .run(commands);
}

/// After a display change, focusing a window re-reads its size once. While
/// a write to it is still on its way, the read waits for a later focus
/// rather than on the app.
#[test]
fn a_size_check_on_focus_does_not_wait_for_a_slow_write() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::DisplayChanged,
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
    ];

    harness_with_slow_app()
        .on_iteration(1, |_world, state| state.hold_writes(SLOW_APP))
        .on_iteration(3, |world, state| {
            crate::assert_focused!(world, 2);
            assert_eq!(
                state.reads_behind_writes(SLOW_APP),
                0,
                "the size check waited on the slow app"
            );
            let entity = find_window_entity(2, world);
            assert!(world.get::<VerifyWindowSize>(entity).is_some());
            state.release_writes(SLOW_APP);
            state.wait_for_writes();
        })
        .run(commands);
}

/// The process ends once exit cleanup returns, so cleanup waits for the
/// windows it puts back to reach their apps.
#[test]
fn exit_waits_for_queued_window_writes() {
    let mut harness = harness_with_slow_app();
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    let state = harness.mock_state.clone();
    let mut window_manager = state.create_window_manager();
    window_manager.expect_dim_windows().return_const(());
    let world = harness.world();
    world.insert_resource(WindowManager(Box::new(window_manager)));

    state.hold_writes(SLOW_APP);
    world.write_message(AppExit::Success);
    let releaser = std::thread::spawn({
        let state = state.clone();
        move || {
            std::thread::sleep(Duration::from_millis(200));
            state.release_writes(SLOW_APP);
        }
    });
    world
        .run_system_once(crate::ecs::systems::cleanup_on_exit)
        .expect("exit cleanup runs");

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    for id in SLOW_WINDOWS {
        assert_eq!(
            state.window_frame(id).min.x,
            centered,
            "window {id} reached its app before exit returned"
        );
    }
    releaser.join().expect("releasing the slow app");
}
