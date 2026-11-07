use crate::assert_focused;
use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::config::{Config, MainOptions};
use crate::events::Event;
use crate::manager::{Origin, Window};
use crate::platform::Modifiers;
use bevy::math::IRect;
use objc2_core_foundation::CGPoint;
use std::time::Duration;

use super::*;

#[test]
fn mouse_focus_resumes_after_slide_without_intermediate_mouse_events() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            focus_follows_mouse: Some(true),
            animation_speed: Some(30.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(2, |world, _state| {
            assert_focused!(world, 0);
            let previous = find_window_entity(1, world);
            let center = world.get::<Window>(previous).unwrap().frame().center();
            world.write_message(Event::MouseMoved {
                point: CGPoint {
                    x: f64::from(center.x),
                    y: f64::from(center.y),
                },
                modifiers: Modifiers::empty(),
            });
        })
        .on_iteration(3, |world, _state| assert_focused!(world, 1))
        .run(commands);
}

#[test]
fn keyboard_focus_survives_mouse_movement_during_slide() {
    for (direction, start, target) in [
        (Direction::East, Direction::First, 2),
        (Direction::West, Direction::Last, 0),
    ] {
        for (same_frame, mouse_follows_focus, auto_center) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, false),
            (false, false, true),
            (false, true, true),
            (true, false, true),
            (true, true, true),
        ] {
            let previous = 1;
            let config: Config = (
                MainOptions {
                    auto_center: Some(auto_center),
                    mouse_follows_focus: Some(mouse_follows_focus),
                    focus_follows_mouse: Some(true),
                    animation_speed: Some(12.0),
                    ..Default::default()
                },
                vec![],
            )
                .into();
            let mut harness = TestHarness::new().with_config(config).with_windows(4);
            harness.run(vec![
                Event::Command {
                    command: Command::PrintState,
                },
                Event::Command {
                    command: Command::Window(Operation::Focus(start.clone())),
                },
                Event::Command {
                    command: Command::Window(Operation::Focus(Direction::Nth(1))),
                },
            ]);
            harness.advance(Duration::from_secs(1));
            assert_focused!(harness.world(), previous);

            harness.world().write_message(Event::Command {
                command: Command::Window(Operation::Focus(direction.clone())),
            });
            if !same_frame {
                harness.advance(Duration::from_millis(20));
                assert_focused!(harness.world(), target);
                assert!(
                    harness
                        .world()
                        .query::<&crate::ecs::RepositionMarker>()
                        .iter(harness.world())
                        .next()
                        .is_some(),
                    "the mouse event must arrive during the slide"
                );
            }

            let previous_entity = find_window_entity(previous, harness.world());
            let center = harness
                .world()
                .get::<Window>(previous_entity)
                .unwrap()
                .frame()
                .center();
            harness.world().write_message(Event::MouseMoved {
                point: CGPoint {
                    x: f64::from(center.x),
                    y: f64::from(center.y),
                },
                modifiers: Modifiers::empty(),
            });
            harness.advance(Duration::from_millis(20));
            assert_focused!(harness.world(), target);
            harness.advance(Duration::from_secs(2));
            assert_focused!(harness.world(), target);

            // Mouse focus must resume once the keyboard-driven slide finishes.
            let center = harness
                .world()
                .get::<Window>(previous_entity)
                .unwrap()
                .frame()
                .center();
            harness.world().write_message(Event::MouseMoved {
                point: CGPoint {
                    x: f64::from(center.x),
                    y: f64::from(center.y),
                },
                modifiers: Modifiers::empty(),
            });
            harness.advance(Duration::from_millis(100));
            assert_focused!(harness.world(), previous);
        }
    }
}

#[test]
fn keyboard_focus_warps_mouse_to_centered_window() {
    for speed in [12.0, 30.0] {
        let config: Config = (
            MainOptions {
                auto_center: Some(true),
                mouse_follows_focus: Some(true),
                focus_follows_mouse: Some(false),
                animation_speed: Some(speed),
                ..Default::default()
            },
            vec![],
        )
            .into();
        let mut harness = TestHarness::new().with_config(config).with_windows(3);
        let mut commands = vec![Event::Command {
            command: Command::PrintState,
        }];

        // Include a jump to a window initially outside the viewport. Window 0
        // has focus from the start, and starting moves no pointer (#339), so
        // every step changes focus.
        for (direction, window_id) in [
            (Direction::Last, 2),
            (Direction::West, 1),
            (Direction::East, 2),
            (Direction::First, 0),
        ] {
            commands.push(Event::Command {
                command: Command::Window(Operation::Focus(direction)),
            });
            // Allow another command interval for the slower animation to settle.
            commands.push(Event::Command {
                command: Command::PrintState,
            });
            harness = harness.on_iteration(commands.len() - 1, move |world, state| {
                assert_focused!(world, window_id);
                let window = world.query::<&Window>().iter(world)
                    .find(|window| window.id() == window_id)
                    .expect("focused window");
                let center = window.frame().center();
                assert!((center.x - TEST_DISPLAY_WIDTH / 2).abs() <= 2,
                    "window should have finished centering: {center:?}");
                let cursor = state.cursor_position();
                assert!((cursor.x - center.x).abs() <= 2 && window.frame().contains(cursor),
                    "speed {speed}: pointer {cursor:?} was left behind after window {window_id} centered at {center:?}");
            });
        }
        harness.run(commands);
    }
}

#[test]
fn keyboard_focus_warps_mouse_without_auto_center() {
    for speed in [12.0, 30.0] {
        let config: Config = (
            MainOptions {
                auto_center: Some(false),
                mouse_follows_focus: Some(true),
                focus_follows_mouse: Some(false),
                animation_speed: Some(speed),
                ..Default::default()
            },
            vec![],
        )
            .into();
        // Window 0 has focus from the start. Only a command moves the pointer
        // (#339), so starting leaves it where it was.
        let mut harness = TestHarness::new()
            .with_config(config)
            .with_windows(4)
            .on_iteration(0, |world, state| {
                assert_focused!(world, 0);
                assert_eq!(state.cursor_position(), Origin::ZERO);
            });
        let mut commands = vec![Event::Command {
            command: Command::PrintState,
        }];

        // Include jumps to windows partially and completely outside the viewport.
        for (direction, window_id) in [
            (Direction::Last, 3),
            (Direction::West, 2),
            (Direction::East, 3),
            (Direction::First, 0),
        ] {
            commands.push(Event::Command {
                command: Command::Window(Operation::Focus(direction)),
            });
            // Allow another command interval for the slower animation to settle.
            commands.push(Event::Command {
                command: Command::PrintState,
            });
            harness = harness.on_iteration(commands.len() - 1, move |world, state| {
                assert_focused!(world, window_id);
                let window = world.query::<&Window>().iter(world)
                    .find(|window| window.id() == window_id)
                    .expect("focused window");
                let center = window.frame().center();
                let cursor = state.cursor_position();
                assert!((cursor - center).abs().max_element() <= 2,
                    "speed {speed}: pointer {cursor:?} missed focused window {window_id} at {center:?}");
            });
        }
        harness.run(commands);
    }
}

#[test]
fn keyboard_focus_does_not_warp_mouse_when_disabled() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            mouse_follows_focus: Some(false),
            focus_follows_mouse: Some(false),
            animation_speed: Some(30.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ];
    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(1, |world, state| {
            assert_focused!(world, 2);
            assert_eq!(state.cursor_position(), Origin::ZERO);
        })
        .run(commands);
}

#[test]
fn virtual_workspace_focus_warps_mouse_to_restored_window() {
    for animations in [false, true] {
        let config: Config = (
            MainOptions {
                auto_center: Some(true),
                mouse_follows_focus: Some(true),
                focus_follows_mouse: Some(false),
                animation_speed: Some(30.0),
                virtual_workspace_animations: Some(animations),
                ..Default::default()
            },
            vec![],
        )
            .into();
        let mut harness = TestHarness::new().with_config(config).with_windows(3);
        let mut commands = vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
            },
        ];
        for (iteration, (direction, window_id)) in [
            (Direction::South, 0),
            (Direction::North, 1),
            (Direction::South, 0),
        ]
        .into_iter()
        .enumerate()
        {
            commands.push(Event::Command {
                command: Command::Window(Operation::FocusOrVirtual(direction)),
            });
            harness = harness.on_iteration(iteration + 2, move |world, state| {
                assert_focused!(world, window_id);
                let window = world.query::<&Window>().iter(world)
                    .find(|window| window.id() == window_id)
                    .expect("restored window");
                let center = window.frame().center();
                let cursor = state.cursor_position();
                assert!((cursor - center).abs().max_element() <= 2,
                    "animations {animations}: pointer {cursor:?} missed restored window {window_id} at {center:?}");
            });
        }
        harness.run(commands);
    }
}

/// Focuses window 0 by command, then lets macOS focus window 1 the way a click
/// or Cmd-Tab does, with the pointer resting at a spot `rest` picks. With
/// `mouse_follows_focus` on, the pointer must stay at that spot (#339).
fn assert_outside_focus_keeps_pointer(click: bool, rest: fn(&Window, &Window) -> Origin) {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            mouse_follows_focus: Some(true),
            focus_follows_mouse: Some(false),
            animation_speed: Some(30.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    let resting = std::rc::Rc::new(std::cell::Cell::new(Origin::ZERO));
    let set = resting.clone();
    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, move |world, state| {
            assert_focused!(world, 0);
            let first = find_window_entity(0, world);
            let second = find_window_entity(1, world);
            let point = rest(
                world.get::<Window>(first).unwrap(),
                world.get::<Window>(second).unwrap(),
            );
            set.set(point);
            state.set_cursor_position(point);
            if click {
                state.simulate_click(point);
            }
            state.focus_window(1);
        })
        .on_iteration(3, move |world, state| {
            assert_focused!(world, 1);
            assert_eq!(
                state.cursor_position(),
                resting.get(),
                "click {click}: the pointer moved after an outside focus change"
            );
        })
        .run(commands);
}

#[test]
fn clicking_a_window_leaves_the_pointer_where_it_clicked() {
    // Near the top-left corner of the visible part of window 1, away from
    // the centre that a warp would pick.
    assert_outside_focus_keeps_pointer(true, |_, second| {
        let display = IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
        let visible = display.intersect(second.frame());
        assert!(
            visible.width() > 40,
            "window 1 must be visible: {visible:?}"
        );
        visible.min + Origin::new(20, 20)
    });
}

#[test]
fn an_app_focusing_its_window_leaves_the_pointer_in_place() {
    // Cmd-Tab, or an app that brings its own window forward, while the
    // pointer rests on window 0.
    // Away from the screen centre, where a warp to the newly centred window 1
    // would land.
    assert_outside_focus_keeps_pointer(false, |first, _| {
        let display = IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
        display.intersect(first.frame()).min + Origin::new(20, 20)
    });
}
