use std::time::Duration;

use bevy::math::IRect;

use crate::assert_window_size;
use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::config::{Config, MainOptions};
use crate::events::Event;
use crate::manager::Window;

use super::*;

/// Settles one window at the full height of the display, applies `setup` to
/// it, then makes the display `shrink` points shorter. Returns the window's
/// frame before and after, and how many size writes its app got for it.
fn shrink_display_under_window(
    shrink: i32,
    setup: impl FnOnce(&mut MockWindowData),
) -> (IRect, IRect, usize) {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let before = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();
    assert_eq!(
        before.max.y, TEST_DISPLAY_HEIGHT,
        "the window starts at the bottom of the display"
    );
    harness.mock_state.update_window(0, setup);
    let writes = harness.mock_state.size_writes(0);

    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT - shrink),
    );
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });
    harness.advance(Duration::from_millis(1200));

    let after = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();
    (before, after, harness.mock_state.size_writes(0) - writes)
}

#[test]
fn a_small_shrink_at_the_screen_bottom_reaches_the_asked_height() {
    let (before, after, _) = shrink_display_under_window(16, |w| w.holds_bottom_edge = true);

    assert_eq!(after.min, before.min);
    assert_eq!(
        after.height(),
        TEST_DISPLAY_HEIGHT - 16 - TEST_MENUBAR_HEIGHT,
        "the window shrinks with the display"
    );
}

/// With animation on, a shrink reaches the window in steps, and the last steps
/// are small.
#[test]
fn an_animated_shrink_of_a_stack_bottom_window_ends_at_the_screen_bottom() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Shrink)),
        },
    ];
    let config: Config = (
        MainOptions {
            animation_speed: Some(15.0),
            preset_stack_heights: vec![0.3, 0.5, 0.7],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(4, |world, state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 524);
            state.update_window(1, |w| w.holds_bottom_edge = true);
        })
        .on_iteration(5, |world, _state| {
            let window_entity = find_window_entity(1, world);
            let frame = world.get::<Window>(window_entity).unwrap().frame();
            assert_eq!(frame.height(), 374, "the window reaches the asked height");
            assert_eq!(
                frame.max.y, TEST_DISPLAY_HEIGHT,
                "the window ends at the bottom of the display"
            );
        })
        .run(commands);
}

#[test]
fn a_large_shrink_at_the_screen_bottom_takes_one_write() {
    let (_, after, writes) = shrink_display_under_window(200, |w| w.holds_bottom_edge = true);

    assert_eq!(
        after.height(),
        TEST_DISPLAY_HEIGHT - 200 - TEST_MENUBAR_HEIGHT
    );
    assert_eq!(writes, 1);
}

#[test]
fn a_refused_shrink_at_the_screen_bottom_leaves_the_window_where_it_was() {
    let kept_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
    let (before, after, writes) = shrink_display_under_window(16, |w| w.min_height = kept_height);

    assert_eq!(after, before, "the app keeps its height");
    assert!(
        (1..=3).contains(&writes),
        "the app got {writes} size writes for one refused height"
    );
}

#[test]
fn a_refused_shrink_above_the_screen_bottom_takes_one_write() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
    ];

    TestHarness::new()
        .with_window(0, |w| w.min_height = 500)
        .with_window(1, |_| {})
        .on_iteration(1, |_world, state| {
            assert_eq!(state.size_writes(0), 1, "the first layout fits the app");
        })
        .on_iteration(2, |world, state| {
            let window_entity = find_window_entity(0, world);
            let frame = world.get::<Window>(window_entity).unwrap().frame();
            assert_eq!(frame.height(), 500, "the app keeps its minimum height");
            assert!(frame.max.y < TEST_DISPLAY_HEIGHT);
            assert_eq!(state.size_writes(0), 2, "stacking sends one write");
        })
        .run(commands);
}
