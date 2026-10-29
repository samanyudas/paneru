//! Heights of the windows stacked in one column.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::events::Event;
use crate::manager::Window;
use crate::platform::WinID;

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

/// A display as tall as a 14 inch laptop's. On the default 768 px test
/// display a three-window stack falls below the minimum window height, and
/// `binpack_heights` already averages it there.
const TALL: i32 = 1290;
const COLUMN: i32 = TALL - TEST_MENUBAR_HEIGHT;

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

fn tall_harness() -> TestHarness {
    tall_harness_with_windows(3)
}

fn tall_harness_with_windows(count: i32) -> TestHarness {
    let harness = TestHarness::new();
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, TALL));
    harness.with_windows(count)
}

/// Stacks windows 1 and 2 under window 0, one at a time, so one column holds
/// all three and window 2 is focused.
fn stack_three(harness: &mut TestHarness) {
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// The heights of `ids` top to bottom, checking that they share one column
/// and neither overlap nor leave a gap.
fn column_heights(world: &mut World, ids: &[WinID]) -> Vec<i32> {
    let frames = ids.iter().map(|id| frame(world, *id)).collect::<Vec<_>>();
    for pair in frames.windows(2) {
        assert_eq!(pair[0].min.x, pair[1].min.x, "one column: {frames:?}");
        assert_eq!(
            pair[0].max.y, pair[1].min.y,
            "no overlap or gap: {frames:?}"
        );
    }
    frames.iter().map(IRect::height).collect()
}

fn assert_even(heights: &[i32], total: i32) {
    assert_eq!(heights.iter().sum::<i32>(), total, "fills: {heights:?}");
    let spread = heights.iter().max().unwrap() - heights.iter().min().unwrap();
    assert!(spread <= 1, "even split: {heights:?}");
}

/// Keeping the existing heights would leave window 0 half the column and
/// the other two a quarter each.
#[test]
fn a_window_joining_a_stack_splits_the_column_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// The window leaving is the top one, so keeping heights would hand all of
/// its share to the last window.
#[test]
fn a_window_leaving_a_stack_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
        command(Operation::Stack(false)),
    ]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[1, 2]);
    assert_even(&heights, COLUMN);
}

#[test]
fn closing_a_stacked_window_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.mock_state.os_close_window(0);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[1, 2]);
    assert_even(&heights, COLUMN);
}

/// Stacking the top window has no column to join, so the heights the user
/// set stay.
#[test]
fn a_stack_command_that_changes_nothing_keeps_set_heights() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
        command(Operation::ResizeVertical(ResizeDirection::Grow)),
    ]);
    harness.advance(SETTLE);
    let before = column_heights(harness.world(), &[0, 1, 2]);
    assert!(before[0] > before[1], "the resize took effect: {before:?}");

    harness.run(vec![command(Operation::Stack(true))]);
    harness.advance(SETTLE);

    assert_eq!(column_heights(harness.world(), &[0, 1, 2]), before);
}

/// Window 1's app accepts no less than 500 px. By the time window 3 joins,
/// window 1 already sits at 500 px, so the even split's smaller height
/// changes nothing and the app reports no resize.
#[test]
fn a_window_that_keeps_a_taller_height_does_not_cover_the_next() {
    let mut harness = tall_harness_with_windows(4);
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2, 3]);
    assert_eq!(heights[1], 500, "{heights:?}");
    assert_even(&[heights[0], heights[2], heights[3]], COLUMN - 500);
}

/// The last window gets what is left of the column, so when it is the one
/// that keeps a taller height, the windows above it make room.
#[test]
fn a_last_window_that_keeps_a_taller_height_stays_on_screen() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(2, |window| window.min_height = 500);
    stack_three(&mut harness);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_eq!(heights[2], 500, "{heights:?}");
    assert_eq!(frame(harness.world(), 2).max.y, TALL, "ends at the bottom");
    assert_even(&heights[..2], COLUMN - 500);
}

/// Window 0's app refused the even split's height when the stack formed.
/// Shrinking or equalizing does not ask it for less again, so with animation
/// on, the app gets no size it refuses and no resize keeps running.
#[test]
fn shrinking_or_equalizing_sends_no_height_the_app_refused() {
    use crate::config::{Config, MainOptions};
    use crate::ecs::ResizeMarker;

    for operation in [
        Operation::ResizeVertical(ResizeDirection::Shrink),
        Operation::Equalize,
    ] {
        let config: Config = (
            MainOptions {
                animation_speed: Some(14.0),
                ..Default::default()
            },
            vec![],
        )
            .into();
        let mut harness = tall_harness().with_config(config);
        harness
            .mock_state
            .update_window(0, |window| window.min_height = 700);
        stack_three(&mut harness);
        harness.run(vec![
            command(Operation::Focus(Direction::North)),
            command(Operation::Focus(Direction::North)),
        ]);
        let writes = harness.mock_state.resize_writes(0);
        harness.run(vec![command(operation.clone())]);
        harness.advance(SETTLE);

        assert_eq!(
            harness.mock_state.resize_writes(0),
            writes,
            "{operation:?}: no size below the height the app kept"
        );
        let world = harness.world();
        let resizing = world
            .query_filtered::<(), With<ResizeMarker>>()
            .iter(world)
            .count();
        assert_eq!(resizing, 0, "{operation:?}: every resize has finished");
        let heights = column_heights(world, &[0, 1, 2]);
        assert_eq!(heights[0], 700, "{operation:?}: {heights:?}");
        assert_eq!(heights.iter().sum::<i32>(), COLUMN, "{operation:?}");
    }
}

/// Two windows that each refuse less than 400 px share a 748 px column. No
/// split fits both, so a vertical resize there leaves the heights alone.
#[test]
fn a_vertical_resize_in_a_column_too_short_for_kept_heights_changes_nothing() {
    let mut harness = TestHarness::new().with_windows(2);
    for id in [0, 1] {
        harness
            .mock_state
            .update_window(id, |window| window.min_height = 400);
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::North)),
    ]);
    harness.advance(SETTLE);
    let before = [frame(harness.world(), 0), frame(harness.world(), 1)];

    for direction in [ResizeDirection::Shrink, ResizeDirection::Grow] {
        harness.run(vec![command(Operation::ResizeVertical(direction))]);
        harness.advance(SETTLE);
        let after = [frame(harness.world(), 0), frame(harness.world(), 1)];
        assert_eq!(after, before, "{direction:?}");
    }
}
