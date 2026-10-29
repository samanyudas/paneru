//! Heights of the windows stacked in one column.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::events::Event;
use crate::manager::{Size, Window};
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

/// Window 1 accepts no less than 500 px. By the time window 3 joins, window 1
/// already sits at that minimum, so the even split's smaller height changes
/// nothing and the app reports no resize.
#[test]
fn a_window_with_a_minimum_height_does_not_overlap_the_next() {
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
/// with a minimum height, the windows above it make room.
#[test]
fn a_last_window_with_a_minimum_height_stays_on_screen() {
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

/// A learned minimum is dropped once the window is seen shorter than it, so
/// an app that now accepts less gets an even share at the next split.
#[test]
fn a_minimum_height_is_dropped_when_the_window_is_seen_shorter() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    assert_eq!(frame(harness.world(), 1).height(), 500);

    harness
        .mock_state
        .update_window(1, |window| window.min_height = 0);
    harness
        .mock_state
        .os_resize_window(1, Size::new(TEST_WINDOW_WIDTH, 300));
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Stack(false))]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Stack(true))]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// Equalize asks every window for the same height. With animation on, the
/// window that refuses it ends at its minimum and no resize keeps running.
#[test]
fn equalizing_a_stack_with_a_minimum_height_settles() {
    use crate::config::{Config, MainOptions};
    use crate::ecs::ResizeMarker;

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
    harness.run(vec![command(Operation::Equalize)]);
    harness.advance(SETTLE);

    let world = harness.world();
    let resizing = world
        .query_filtered::<(), With<ResizeMarker>>()
        .iter(world)
        .count();
    assert_eq!(resizing, 0, "every resize has finished");
    let heights = column_heights(world, &[0, 1, 2]);
    assert_eq!(heights[0], 700, "{heights:?}");
    assert_even(&heights[1..], COLUMN - 700);
}

/// Stacks windows 1 to `count - 1` under window 0, one at a time, so one
/// column holds them all and window `count - 1` is focused.
fn stack_all(harness: &mut TestHarness, count: WinID) {
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
    ]);
    for _ in 1..count {
        harness.run(vec![
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(true)),
        ]);
        harness.advance(SETTLE);
    }
}

/// Each window's top and height, checking that they share one column.
fn rows(world: &mut World, ids: &[WinID]) -> Vec<(i32, i32)> {
    let frames = ids.iter().map(|id| frame(world, *id)).collect::<Vec<_>>();
    assert!(
        frames.windows(2).all(|pair| pair[0].min.x == pair[1].min.x),
        "one column: {frames:?}"
    );
    frames
        .iter()
        .map(|frame| (frame.min.y, frame.height()))
        .collect()
}

/// Four windows need 800 px at the 200 px minimum window height, more than
/// the 748 px column. Each lower window overlaps the one above it below that
/// window's top bar, 52 px when it is not known. The last window ends at the
/// display bottom, and the 548 px above it hold the three top bars above it
/// and an even share of the rest, 130 px each.
#[test]
fn a_stack_too_tall_for_the_minimum_window_height_overlaps_below_each_top_bar() {
    let mut harness = TestHarness::new().with_windows(4);
    stack_all(&mut harness, 4);

    assert_eq!(
        rows(harness.world(), &[0, 1, 2, 3]),
        vec![(20, 748), (203, 565), (386, 382), (568, 200)]
    );
}

/// Three windows that accept no less than 700 px each need 2100 px of a
/// 1270 px column. Stacked one under the other, the last one would start
/// below the display bottom. They overlap instead, and every window starts
/// on screen.
#[test]
fn a_stack_whose_minimum_heights_cannot_fit_keeps_every_window_on_screen() {
    let mut harness = tall_harness();
    for id in 0..3 {
        harness
            .mock_state
            .update_window(id, |window| window.min_height = 700);
    }
    stack_three(&mut harness);

    assert_eq!(
        rows(harness.world(), &[0, 1, 2]),
        vec![(20, COLUMN), (305, 985), (590, 700)]
    );
}

/// Each lower window of an overlapping stack is raised after the window above
/// it, so it is in front and the top bar above it shows. The focused window
/// is raised last, and goes back to its place once focus leaves it.
#[test]
fn an_overlapping_stack_raises_each_lower_window_in_front() {
    let mut harness = TestHarness::new().with_windows(5);
    stack_all(&mut harness, 4);
    harness.mock_state.take_raised();

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    assert_eq!(harness.mock_state.take_raised(), vec![0, 1, 3, 2]);

    harness.run(vec![command(Operation::Focus(Direction::East))]);
    assert_eq!(harness.mock_state.take_raised(), vec![0, 1, 2, 3, 4]);
}
