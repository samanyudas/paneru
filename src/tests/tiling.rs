use std::time::Duration;

use crate::commands::{Command, Direction, Operation, ResizeDirection, Side};
use crate::config::{
    Config, InitialWindowWidth, InitialWindowWidthMode, MainOptions, WindowParams,
};
use crate::ecs::SpawnWindowTrigger;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::{assert_window_at, assert_window_size};
use bevy::math::IRect;
use bevy::prelude::*;

use super::*;

#[test]
fn new_window_starts_at_complement_of_focused_width() {
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Mode(
        InitialWindowWidthMode::ComplementFocused,
    ));
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, move |world, state| {
            world.insert_resource(config.clone());
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, 748);
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH, 748);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn new_window_starts_full_width_after_full_width_focus() {
    let mut fixed = WindowParams::new(".*", None);
    fixed.width = Some(InitialWindowWidth::Ratio(1.0));
    let fixed_config: Config = (MainOptions::default(), vec![fixed]).into();

    let mut dynamic = WindowParams::new(".*", None);
    dynamic.width = Some(InitialWindowWidth::Mode(
        InitialWindowWidthMode::ComplementFocused,
    ));
    let dynamic_config: Config = (MainOptions::default(), vec![dynamic]).into();

    TestHarness::new()
        .with_config(fixed_config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, move |world, state| {
            assert_window_size!(world, 0, TEST_DISPLAY_WIDTH, 748);
            world.insert_resource(dynamic_config.clone());
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH, 748);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn configured_width_uses_raw_display_bounds_with_edge_padding() {
    let options = MainOptions {
        padding_left: Some(20),
        padding_right: Some(30),
        ..MainOptions::default()
    };
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (options, vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(0, |world, state| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH / 2, 748);
            let mut query = world.query::<(&crate::manager::Window, &crate::ecs::WidthRatio)>();
            let ratio = query
                .iter(world)
                .find_map(|(window, ratio)| (window.id() == 1).then_some(ratio.0));
            assert_eq!(ratio, Some(0.5));
        })
        .run(vec![Event::MenuOpened { window_id: 0 }]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_window_shuffle() {
    const PADDING_LEFT: u16 = 3;
    const PADDING_RIGHT: u16 = 5;
    const PADDING_TOP: u16 = 7;
    const PADDING_BOTTOM: u16 = 9;
    const SLIVER_WIDTH: u16 = 5;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 2
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        }, // 3
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 4
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        }, // 5
        Event::Command {
            command: Command::Window(Operation::Center),
        }, // 6
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 7
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        }, // 8
        Event::Command {
            command: Command::Window(Operation::Center),
        }, // 9
        Event::Command {
            command: Command::PrintState,
        }, // 10
    ];

    // Logical width includes padding expansion on each side.
    let logical_width = TEST_WINDOW_WIDTH;
    let top_edge = TEST_MENUBAR_HEIGHT + i32::from(PADDING_TOP);
    let left_edge = i32::from(PADDING_LEFT);
    let right_edge = TEST_DISPLAY_WIDTH - i32::from(PADDING_RIGHT);
    let offscreen_right = right_edge - i32::from(SLIVER_WIDTH) + i32::from(PADDING_RIGHT);
    let offscreen_left =
        left_edge - logical_width + i32::from(SLIVER_WIDTH) - i32::from(PADDING_LEFT);
    let centered = (TEST_DISPLAY_WIDTH - logical_width) / 2;

    let mut params = WindowParams::new(".*", None);
    params.vertical_padding = Some(3);
    params.horizontal_padding = Some(2);
    let config: Config = (
        MainOptions {
            padding_left: Some(PADDING_LEFT),
            padding_right: Some(PADDING_RIGHT),
            padding_top: Some(PADDING_TOP),
            padding_bottom: Some(PADDING_BOTTOM),
            ..Default::default()
        },
        vec![params],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(2, move |world, _state| {
            assert_window_at!(world, 0, offscreen_left, top_edge);
            assert_window_at!(world, 1, offscreen_left, top_edge);
            assert_window_at!(world, 2, right_edge - 3 * logical_width, top_edge);
            assert_window_at!(world, 3, right_edge - 2 * logical_width, top_edge);
            assert_window_at!(world, 4, right_edge - logical_width, top_edge);
        })
        .on_iteration(3, move |world, _state| {
            assert_window_at!(world, 0, left_edge, top_edge);
            assert_window_at!(world, 1, left_edge + logical_width, top_edge);
            assert_window_at!(world, 2, left_edge + 2 * logical_width, top_edge);
            assert_window_at!(world, 3, offscreen_right, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .on_iteration(6, move |world, _state| {
            assert_window_at!(world, 0, centered, top_edge);
            assert_window_at!(world, 1, centered, 393);
            assert_window_at!(world, 2, centered + logical_width, top_edge);
            assert_window_at!(world, 3, offscreen_right, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .on_iteration(10, move |world, _state| {
            assert_window_at!(world, 0, centered, top_edge);
            assert_window_at!(world, 1, centered, 271);
            assert_window_at!(world, 2, centered, 515);
            assert_window_at!(world, 3, centered + logical_width, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .run(commands);
}

#[test]
fn test_window_balance() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Balance),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, _state| {
            // After grow, window 0 should be 512 (50% of 1024).
            assert_window_size!(world, 0, 512, 748);
        })
        .on_iteration(2, |world, _state| {
            // After balance, all windows should match window 0's width.
            assert_window_size!(world, 0, 512, 748);
            assert_window_size!(world, 1, 512, 748);
            assert_window_size!(world, 2, 512, 748);
        })
        .run(commands);
}

#[test]
fn test_startup_windows() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(4, |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_window_resize_grow_and_shrink_cycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
        },
    ];

    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, 512, 748);
        })
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, 768, 748);
        })
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 0, 256, 748);
        })
        .on_iteration(4, |world, _state| {
            assert_window_size!(world, 0, 768, 748);
        })
        .run(commands);
}

/// Stacks two windows, then cycles the focused one's height through the
/// presets. The window above it — the only neighbour, since the focused one is
/// last in the stack — absorbs the whole difference, and the column keeps
/// filling the viewport.
#[test]
fn test_window_vertical_resize_grow_and_shrink_cycle() {
    // The viewport is `TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT` tall and the
    // two stacked windows split it evenly to start with.
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        }, // 1
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 2
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        }, // 3
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        }, // 4
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        }, // 5
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Shrink)),
        }, // 6
    ];

    let config: Config = (
        MainOptions {
            preset_stack_heights: vec![0.3, 0.5, 0.7],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
        })
        .on_iteration(4, |world, _state| {
            // 0.7 of the viewport, with window 0 giving up exactly the delta.
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 524);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - 524);
        })
        .on_iteration(5, |world, _state| {
            // Past the last preset, so it cycles back to the smallest.
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 224);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - 224);
        })
        .on_iteration(6, |world, _state| {
            // Below the smallest preset, so shrinking cycles to the largest.
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 524);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - 524);
        })
        .run(commands);
}

/// A preset that would starve the neighbour is clamped to leave it the minimum
/// window height, rather than letting the binpacker average the column.
#[test]
fn test_window_vertical_resize_leaves_the_neighbour_its_minimum() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
    const MIN_WINDOW_HEIGHT: i32 = 200;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        },
    ];

    let config: Config = (
        MainOptions {
            // 0.9 of the viewport would leave the neighbour 75px.
            preset_stack_heights: vec![0.5, 0.9],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(4, |world, _state| {
            assert_window_size!(
                world,
                1,
                TEST_WINDOW_WIDTH,
                VIEWPORT_HEIGHT - MIN_WINDOW_HEIGHT
            );
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, MIN_WINDOW_HEIGHT);
        })
        .run(commands);
}

/// Outside a stack the binpacker always stretches a column's lone member to the
/// full viewport, so the command is a no-op there.
#[test]
fn test_window_vertical_resize_is_a_noop_outside_a_stack() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Shrink)),
        },
    ];

    let config: Config = (
        MainOptions {
            preset_stack_heights: vec![0.3, 0.5, 0.7],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
        })
        .run(commands);
}

fn step(operation: fn(ResizeDirection) -> Operation, direction: ResizeDirection) -> Event {
    Event::Command {
        command: Command::Window(operation(direction)),
    }
}

/// The width steps by `window_resize_step` of the display, stops at the full
/// display width and at one step, and never wraps around.
#[test]
fn test_window_step_resize_changes_width_by_the_step_without_wrapping() {
    use ResizeDirection::{Grow, Shrink};

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        step(Operation::ResizeStep, Grow),
        step(Operation::ResizeStep, Grow),
        step(Operation::ResizeStep, Grow),
        step(Operation::ResizeStep, Grow),
        step(Operation::ResizeStep, Shrink),
        step(Operation::ResizeStep, Shrink),
        step(Operation::ResizeStep, Shrink),
        step(Operation::ResizeStep, Shrink),
    ];

    // A quarter of the 1024px display is 256px.
    let config: Config = (
        MainOptions {
            window_resize_step: Some(0.25),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let widths = [656, 912, 1024, 1024, 768, 512, 256, 256];
    let mut harness = TestHarness::new().with_config(config).with_windows(1);
    for (index, width) in widths.into_iter().enumerate() {
        harness = harness.on_iteration(index + 1, move |world, _state| {
            assert_window_size!(world, 0, width, 748);
        });
    }
    harness.run(commands);
}

/// The default step is five percentage points of the display width.
#[test]
fn test_window_step_resize_defaults_to_five_points() {
    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            // 400px is 39.0625% of 1024px, and 44.0625% is 451.2px.
            assert_window_size!(world, 0, 451, 748);
        })
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, 400, 748);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            step(Operation::ResizeStep, ResizeDirection::Grow),
            step(Operation::ResizeStep, ResizeDirection::Shrink),
        ]);
}

#[test]
fn test_step_grow_then_shrink_keeps_a_fitting_strip_at_viewport_edges() {
    for focused in [0, 1] {
        let options = MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(true),
            animation_speed: Some(10000.0),
            ..Default::default()
        };
        let mut params = WindowParams::new(".*", None);
        params.width = Some(InitialWindowWidth::Ratio(0.5));
        let config: Config = (options, vec![params]).into();

        TestHarness::new()
            .with_config(config)
            .with_windows(2)
            .with_focused_window(focused)
            .on_iteration(0, |world, _state| {
                assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
                assert_window_size!(world, 0, 512, 748);
            })
            .on_iteration(2, move |world, _state| {
                assert_window_size!(world, focused, 563, 748);
            })
            .on_iteration(3, |world, _state| {
                assert_window_size!(world, 0, 512, 748);
                assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
                assert_window_at!(world, 1, 512, TEST_MENUBAR_HEIGHT);
            })
            .run(vec![
                Event::MenuOpened { window_id: focused },
                Event::Command {
                    command: Command::Window(Operation::Focus(if focused == 0 {
                        Direction::First
                    } else {
                        Direction::Last
                    })),
                },
                step(Operation::ResizeStep, ResizeDirection::Grow),
                step(Operation::ResizeStep, ResizeDirection::Shrink),
            ]);
    }
}

#[test]
fn test_rapid_step_resize_keeps_the_right_edge_during_animation() {
    let options = MainOptions {
        auto_center: Some(false),
        continuous_swipe: Some(true),
        animation_speed: Some(30.0),
        ..Default::default()
    };
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (options, vec![params]).into();
    let mut harness = TestHarness::new().with_config(config).with_windows(2);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ]);
    for direction in [
        ResizeDirection::Grow,
        ResizeDirection::Grow,
        ResizeDirection::Grow,
        ResizeDirection::Shrink,
        ResizeDirection::Shrink,
        ResizeDirection::Shrink,
    ] {
        harness
            .app
            .world_mut()
            .write_message(step(Operation::ResizeStep, direction));
        harness.advance(Duration::from_millis(20));
    }
    harness.advance(Duration::from_secs(3));
    assert_window_size!(harness.world(), 1, 512, 748);
    assert_window_at!(harness.world(), 0, 0, TEST_MENUBAR_HEIGHT);
    assert_window_at!(harness.world(), 1, 512, TEST_MENUBAR_HEIGHT);
}

/// In a stack, the height steps by `window_resize_step` of the display, the
/// neighbour absorbs the difference, and the neighbour keeps its minimum.
#[test]
fn test_window_vertical_step_resize_changes_height_by_the_step() {
    use ResizeDirection::{Grow, Shrink};
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        step(Operation::ResizeVerticalStep, Grow),
        step(Operation::ResizeVerticalStep, Shrink),
        step(Operation::ResizeVerticalStep, Grow),
        step(Operation::ResizeVerticalStep, Grow),
        step(Operation::ResizeVerticalStep, Grow),
        step(Operation::ResizeVerticalStep, Grow),
    ];

    // A tenth of the 748px viewport is 74.8px.
    let config: Config = (
        MainOptions {
            window_resize_step: Some(0.1),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 548 leaves the neighbour its 200px minimum.
    let heights = [449, 374, 449, 524, 548, 548];
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
        });
    for (index, height) in heights.into_iter().enumerate() {
        harness = harness.on_iteration(index + 4, move |world, _state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, height);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - height);
        });
    }
    harness.run(commands);
}

#[test]
fn test_lower_stacked_window_grows_to_its_requested_native_height() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
    let config: Config = (
        MainOptions {
            window_resize_step: Some(0.1),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    for accepted_growth in [0, 20] {
        TestHarness::new()
            .with_config(config.clone())
            .with_windows(2)
            .on_iteration(3, move |world, state| {
                crate::assert_focused!(world, 1);
                let frame = state.create_window(1).frame();
                assert_eq!(frame.height(), VIEWPORT_HEIGHT / 2);
                assert_eq!(frame.max.y, TEST_DISPLAY_HEIGHT);
                state.update_window(1, |window| {
                    window.resize_bottom_limit = Some(TEST_DISPLAY_HEIGHT + accepted_growth);
                });
            })
            .on_iteration(4, |_world, state| {
                let frame = state.create_window(1).frame();
                assert_eq!(frame.height(), 449);
                assert_eq!(frame.max.y, TEST_DISPLAY_HEIGHT);
            })
            .on_iteration(5, |_world, state| {
                let frame = state.create_window(1).frame();
                assert_eq!(frame.height(), VIEWPORT_HEIGHT / 2);
                assert_eq!(frame.max.y, TEST_DISPLAY_HEIGHT);
            })
            .run(vec![
                Event::MenuOpened { window_id: 0 },
                Event::Command {
                    command: Command::Window(Operation::Focus(Direction::First)),
                },
                Event::Command {
                    command: Command::Window(Operation::Focus(Direction::East)),
                },
                Event::Command {
                    command: Command::Window(Operation::Stack(Side::West)),
                },
                step(Operation::ResizeVerticalStep, ResizeDirection::Grow),
                step(Operation::ResizeVerticalStep, ResizeDirection::Shrink),
            ]);
    }
}

fn native_height_harness(animation_speed: f64) -> TestHarness {
    let config: Config = (
        MainOptions {
            window_resize_step: Some(0.1),
            animation_speed: Some(animation_speed),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(2);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
    ]);
    harness.advance(Duration::from_secs(2));
    harness.mock_state.update_window(1, |window| {
        window.resize_bottom_limit = Some(TEST_DISPLAY_HEIGHT);
    });
    harness
}

#[test]
fn test_native_height_verification_waits_for_layout_after_final_size_tick() {
    use crate::ecs::{Bounds, RepositionMarker, ResizeMarker, VerifyWindowSize};
    for accepted_growth in [0, 20] {
        let mut harness = native_height_harness(10000.0);
        harness.mock_state.update_window(1, |window| {
            window.resize_bottom_limit = Some(TEST_DISPLAY_HEIGHT + accepted_growth);
        });
        let entity = find_window_entity(1, harness.world());
        harness
            .world()
            .write_message(step(Operation::ResizeVerticalStep, ResizeDirection::Grow));
        harness.advance(Duration::from_millis(20));
        assert!(harness.world().get::<ResizeMarker>(entity).is_none());
        assert!(harness.world().get::<RepositionMarker>(entity).is_none());
        assert!(harness.world().get::<VerifyWindowSize>(entity).is_some());
        assert_eq!(harness.world().get::<Bounds>(entity).unwrap().0.y, 449);
        assert_eq!(
            harness.mock_state.create_window(1).frame().height(),
            374 + accepted_growth
        );
        harness
            .world()
            .write_message(Event::WindowResized { window_id: 1 });
        harness.advance(Duration::from_secs(2));
        assert_eq!(harness.mock_state.create_window(1).frame().height(), 449);
        assert!(harness.world().get::<VerifyWindowSize>(entity).is_none());
    }
}

#[test]
fn test_animated_native_height_growth_settles_for_both_stack_members() {
    use crate::ecs::{RepositionMarker, ResizeMarker, VerifyWindowSize};
    for window_id in [0, 1] {
        let mut harness = native_height_harness(30.0);
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::Focus(if window_id == 0 {
                Direction::North
            } else {
                Direction::South
            })),
        }]);
        let entity = find_window_entity(window_id, harness.world());
        harness
            .world()
            .write_message(step(Operation::ResizeVerticalStep, ResizeDirection::Grow));
        harness.advance(Duration::from_millis(20));
        assert!(harness.world().get::<ResizeMarker>(entity).is_some());
        assert!(harness.world().get::<RepositionMarker>(entity).is_none());
        assert!(harness.world().get::<VerifyWindowSize>(entity).is_some());
        harness.advance(Duration::from_secs(3));
        assert_eq!(
            harness.mock_state.create_window(window_id).frame().height(),
            449
        );
        assert!(harness.world().get::<VerifyWindowSize>(entity).is_none());
    }
}

#[test]
fn test_native_height_growth_handles_both_animation_completion_orders() {
    use crate::ecs::{Position, RepositionMarker, ResizeMarker, VerifyWindowSize};
    for (window_id, position_finishes_first) in [(1, false), (0, true)] {
        let mut harness = native_height_harness(30.0);
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::Focus(if window_id == 0 {
                Direction::North
            } else {
                Direction::South
            })),
        }]);
        let entity = find_window_entity(window_id, harness.world());
        let start = harness.mock_state.create_window(window_id).frame();
        if position_finishes_first {
            harness.world().get_mut::<Position>(entity).unwrap().0.x += 20;
            harness
                .world()
                .entity_mut(entity)
                .insert(RepositionMarker(start.min));
        }
        harness
            .world()
            .write_message(step(Operation::ResizeVerticalStep, ResizeDirection::Grow));
        harness.advance(Duration::from_millis(40));
        assert!(harness.world().get::<ResizeMarker>(entity).is_some());
        assert!(harness.world().get::<RepositionMarker>(entity).is_some());
        let moving_frame = harness.mock_state.create_window(window_id).frame();
        assert!(moving_frame.height() > 374 && moving_frame.height() < 449);
        assert_ne!(moving_frame.min, start.min);

        let mut observed_completion = false;
        for _ in 0..25 {
            harness.advance(Duration::from_millis(20));
            let resizing = harness.world().get::<ResizeMarker>(entity).is_some();
            let moving = harness.world().get::<RepositionMarker>(entity).is_some();
            if resizing != moving {
                assert_eq!(resizing, position_finishes_first);
                assert!(harness.world().get::<VerifyWindowSize>(entity).is_some());
                observed_completion = true;
                break;
            }
        }
        assert!(
            observed_completion,
            "animations must finish on different ticks"
        );
        harness.advance(Duration::from_secs(3));
        let frame = harness.mock_state.create_window(window_id).frame();
        assert_eq!(frame.height(), 449);
        assert_eq!(frame.min.x, start.min.x);
        assert_eq!(frame.min.y, if window_id == 0 { 20 } else { 319 });
        assert!(harness.world().get::<ResizeMarker>(entity).is_none());
        assert!(harness.world().get::<RepositionMarker>(entity).is_none());
        assert!(harness.world().get::<VerifyWindowSize>(entity).is_none());
    }
}

#[test]
fn test_interrupted_native_height_resize_uses_the_latest_target() {
    use crate::ecs::{RepositionMarker, ResizeMarker, VerifyWindowSize};
    let mut harness = native_height_harness(30.0);
    let entity = find_window_entity(1, harness.world());
    harness
        .world()
        .write_message(step(Operation::ResizeVerticalStep, ResizeDirection::Grow));
    harness.advance(Duration::from_millis(40));
    assert!(harness.world().get::<ResizeMarker>(entity).is_some());
    assert!(harness.world().get::<RepositionMarker>(entity).is_some());
    let moving_frame = harness.mock_state.create_window(1).frame();
    assert!(moving_frame.height() > 374 && moving_frame.height() < 449);
    assert!(moving_frame.min.y < 394 && moving_frame.min.y > 319);
    harness
        .world()
        .write_message(step(Operation::ResizeVerticalStep, ResizeDirection::Shrink));
    harness.advance(Duration::from_millis(20));
    assert_eq!(
        harness.world().get::<ResizeMarker>(entity).unwrap().0.y,
        374
    );
    harness.advance(Duration::from_secs(3));
    let frame = harness.mock_state.create_window(1).frame();
    assert_eq!(
        frame.height(),
        (TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT) / 2
    );
    assert_eq!(frame.max.y, TEST_DISPLAY_HEIGHT);
    assert!(harness.world().get::<ResizeMarker>(entity).is_none());
    assert!(harness.world().get::<RepositionMarker>(entity).is_none());
    assert!(harness.world().get::<VerifyWindowSize>(entity).is_none());
}

#[test]
fn test_fixed_native_height_settles_without_continuing_writes() {
    use crate::ecs::{Bounds, VerifyWindowSize};
    for window_id in [0, 1] {
        let mut harness = native_height_harness(30.0);
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::Focus(if window_id == 0 {
                Direction::North
            } else {
                Direction::South
            })),
        }]);
        let entity = find_window_entity(window_id, harness.world());
        let height = harness.mock_state.create_window(window_id).frame().height();
        harness.mock_state.update_window(window_id, |window| {
            window.maximum_size = Some(IVec2::new(TEST_DISPLAY_WIDTH, height));
        });
        harness.run(vec![step(
            Operation::ResizeVerticalStep,
            ResizeDirection::Grow,
        )]);
        harness.advance(Duration::from_secs(3));
        assert_eq!(
            harness.mock_state.create_window(window_id).frame().height(),
            height
        );
        if window_id == 0 {
            assert_eq!(harness.world().get::<Bounds>(entity).unwrap().0.y, height);
        }
        assert!(harness.world().get::<VerifyWindowSize>(entity).is_none());
        let mut writes = (0, 0);
        harness.mock_state.update_window(window_id, |window| {
            writes = (window.resize_writes, window.position_writes);
        });
        harness.advance(Duration::from_secs(3));
        harness.mock_state.update_window(window_id, |window| {
            assert_eq!((window.resize_writes, window.position_writes), writes);
        });
    }
}

/// Outside a stack a column's lone window always fills the viewport height.
#[test]
fn test_window_vertical_step_resize_is_a_noop_outside_a_stack() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            step(Operation::ResizeVerticalStep, ResizeDirection::Shrink),
        ]);
}

#[test]
fn test_window_can_resize_to_two_display_widths_and_scroll() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::SetWidth(2.0)),
        },
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
        Event::Command {
            command: Command::Window(Operation::Snap),
        },
    ];

    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, 2048, 748);
            assert_oversized_window_is_pannable(world, 0);
        })
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, 2048, 748);
            assert_oversized_window_is_pannable(world, 0);
        })
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 0, 2048, 748);
            assert_oversized_window_is_pannable(world, 0);
        })
        .run(commands);
}

/// A `LayoutOp::SetWidth` handed back by a Lua handler must resize the focused
/// window just like the interactive `Operation::SetWidth` path, instead of only
/// storing a `WidthRatio` that nothing consumes and the next bounds sync
/// overwrites from the live frame.
#[test]
fn test_lua_set_width_layout_op_resizes_the_focused_window() {
    use crate::types::windowset::LayoutOp;

    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![Event::Command {
        command: Command::Layout(vec![LayoutOp::SetWidth {
            window: 0,
            ratio: 0.5,
        }]),
    }];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, TEST_DISPLAY_WIDTH / 2, VIEWPORT_HEIGHT);
        })
        .run(commands);
}

fn assert_oversized_window_is_pannable(world: &mut World, id: i32) {
    let mut query = world.query::<&crate::manager::Window>();
    let window = query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found");
    let x = window.frame().min.x;
    assert!(
        (-TEST_DISPLAY_WIDTH..=0).contains(&x),
        "oversized window must stay within its pannable range, got x={x}"
    );
}

/// A floating window is out of the tiling layout, so it must not keep a slot in
/// the strip: the tiler lays columns out left to right by accumulated width, so
/// a floating member reserves space no tiled window occupies — a gap.
#[test]
fn test_floating_window_does_not_hold_a_slot_in_the_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 1 — float the focused window
        Event::Command {
            command: Command::PrintState,
        }, // 2
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            // Window 0 holds the focus, so it is the one about to float.
            assert_eq!(window_x(world, 0), 0);
            assert_eq!(window_x(world, 1), TEST_WINDOW_WIDTH);
            assert_eq!(window_x(world, 2), 2 * TEST_WINDOW_WIDTH);
        })
        .on_iteration(2, |world, _state| {
            let entity = find_window_entity(0, world);
            let mut query = world.query::<&LayoutStrip>();
            assert!(
                !query.iter(world).any(|strip| strip.contains(entity)),
                "a floating window must not stay in any layout strip"
            );
            // The slot it vacated has to close up: window 1 slides to the left
            // edge rather than leaving an empty column where window 0 was.
            assert_eq!(
                window_x(world, 1),
                0,
                "the tiled windows must close the gap the floating one left"
            );
            assert_eq!(window_x(world, 2), TEST_WINDOW_WIDTH);
        })
        .run(commands);
}

#[test]
fn test_unordered_window_releases_its_layout_space() {
    let mut harness = TestHarness::new().with_windows(3);

    // Allow initial layout to settle.
    harness.advance(Duration::from_millis(100));
    assert_eq!(window_x(harness.app.world_mut(), 0), 0);
    assert_eq!(window_x(harness.app.world_mut(), 1), TEST_WINDOW_WIDTH);
    assert_eq!(window_x(harness.app.world_mut(), 2), TEST_WINDOW_WIDTH * 2);

    // Window 1 disappears silently from the OS (unordered on WindowServer and dead in AX).
    harness.mock_state.os_vanish_window(1);

    // Step past the 1-second periodic CLOSED_WINDOW_CHECK_FREQ timer.
    harness.advance(Duration::from_millis(1100));

    let mut query = harness.app.world_mut().query::<&crate::manager::Window>();
    assert!(
        query
            .iter(harness.app.world())
            .all(|window| window.id() != 1),
        "unordered window 1 should be removed within periodic check"
    );
    assert_eq!(window_x(harness.app.world_mut(), 0), 0);
    assert_eq!(window_x(harness.app.world_mut(), 2), TEST_WINDOW_WIDTH);
}

/// The same invariant for a window floated by a config rule rather than by the
/// toggle. This is the path that runs while the window is being spawned, so it
/// races the strip insertion the tiling path is doing at the same time.
#[test]
fn test_rule_floated_window_does_not_hold_a_slot_in_the_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    // Only window 1 floats; 0 and 2 tile as usual.
    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(1, |world, _state| {
            let entity = find_window_entity(1, world);
            let mut query = world.query::<&LayoutStrip>();
            assert!(
                !query.iter(world).any(|strip| strip.contains(entity)),
                "a rule-floated window must not stay in any layout strip"
            );
            assert_eq!(
                window_x(world, 2) - window_x(world, 0),
                TEST_WINDOW_WIDTH,
                "the tiled windows must close the gap the floating one left"
            );
        })
        .run(commands);
}

/// Closing a window while its application stays alive must free its slot in
/// the strip. The AX element of such a window often keeps answering queries
/// after the window is gone, which used to make `window_destroyed_trigger`
/// mistake the destroy event for a space change and leave a gap where the
/// window had been.
#[test]
fn test_closing_window_of_live_app_closes_the_gap() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::PrintState,
        }, // 1
        Event::Command {
            command: Command::PrintState,
        }, // 2
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, state| {
            let left = window_x(world, 0);
            assert_eq!(
                window_x(world, 2) - left,
                2 * TEST_WINDOW_WIDTH,
                "three windows should tile side by side before the close"
            );
            state.os_close_window(1);
        })
        .on_iteration(2, |world, _state| {
            assert!(
                !window_exists(world, 1),
                "closed window must be dropped from the world"
            );
            assert_eq!(
                window_x(world, 2) - window_x(world, 0),
                TEST_WINDOW_WIDTH,
                "surviving windows must close the gap left by window 1"
            );
        })
        .run(commands);
}

fn window_x(world: &mut World, id: i32) -> i32 {
    let mut query = world.query::<&crate::manager::Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .unwrap_or_else(|| panic!("window {id} not found"))
        .frame()
        .min
        .x
}

fn window_exists(world: &mut World, id: i32) -> bool {
    let mut query = world.query::<&crate::manager::Window>();
    query.iter(world).any(|window| window.id() == id)
}

/// A last column that ends short of the display edge grows past it at 75% and
/// is clamped onto it. When the preset cycle wraps to 25%, the column must
/// return around its original center instead of staying pinned to the edge.
#[test]
fn test_preset_cycle_returns_a_clamped_last_column_to_its_center() {
    let options = MainOptions {
        auto_center: Some(false),
        preset_column_widths: vec![0.25, 0.5, 0.75],
        ..Default::default()
    };
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Ratio(0.45));
    let config: Config = (options, vec![params]).into();
    let mut harness = TestHarness::new().with_config(config).with_windows(2);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ]);
    harness.advance(Duration::from_secs(1));
    assert_window_at!(harness.world(), 1, 461, TEST_MENUBAR_HEIGHT);
    for _ in 0..2 {
        harness.app.world_mut().write_message(Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        });
        harness.advance(Duration::from_secs(1));
    }
    assert_window_size!(harness.world(), 1, 256, 748);
    assert_window_at!(harness.world(), 1, 563, TEST_MENUBAR_HEIGHT);
}

fn clamped_last_column_harness(animation_speed: Option<f64>) -> TestHarness {
    let options = MainOptions {
        auto_center: Some(false),
        preset_column_widths: vec![0.25, 0.5, 0.75],
        animation_speed,
        ..Default::default()
    };
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Ratio(0.45));
    let config: Config = (options, vec![params]).into();
    TestHarness::new().with_config(config).with_windows(2)
}

fn grow_width(harness: &mut TestHarness, wait: Duration) {
    harness.app.world_mut().write_message(Event::Command {
        command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
    });
    harness.advance(wait);
}

/// Focusing another column and coming back ends the width run, so the next
/// change anchors where the column now stands: flush on the right edge.
#[test]
fn test_a_focus_change_ends_the_width_run() {
    let mut harness = clamped_last_column_harness(None);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ]);
    harness.advance(Duration::from_secs(1));
    grow_width(&mut harness, Duration::from_secs(1));
    assert_window_at!(harness.world(), 1, 256, TEST_MENUBAR_HEIGHT);
    for direction in [Direction::First, Direction::Last] {
        harness.app.world_mut().write_message(Event::Command {
            command: Command::Window(Operation::Focus(direction)),
        });
        harness.advance(Duration::from_secs(1));
    }
    grow_width(&mut harness, Duration::from_secs(1));
    assert_window_size!(harness.world(), 1, 256, 748);
    assert_window_at!(harness.world(), 1, 768, TEST_MENUBAR_HEIGHT);
}

/// Width changes pressed while the previous one still animates keep the run's
/// center instead of reading an intermediate frame.
#[test]
fn test_rapid_animated_width_changes_keep_the_run_center() {
    let mut harness = clamped_last_column_harness(Some(30.0));
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ]);
    harness.advance(Duration::from_secs(2));
    grow_width(&mut harness, Duration::from_millis(20));
    grow_width(&mut harness, Duration::from_secs(3));
    assert_window_size!(harness.world(), 1, 256, 748);
    assert_window_at!(harness.world(), 1, 563, TEST_MENUBAR_HEIGHT);
}

/// An app that refuses the requested width does not end the run, so cycling
/// back still returns the column around its original center.
#[test]
fn test_a_refused_width_keeps_the_run_center() {
    let mut harness = clamped_last_column_harness(None);
    harness.mock_state.update_window(1, |window| {
        window.maximum_size = Some(crate::manager::Size::new(750, 1312));
    });
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ]);
    harness.advance(Duration::from_secs(1));
    grow_width(&mut harness, Duration::from_secs(2));
    grow_width(&mut harness, Duration::from_secs(2));
    assert_window_size!(harness.world(), 1, 256, 748);
    assert_window_at!(harness.world(), 1, 563, TEST_MENUBAR_HEIGHT);
}
