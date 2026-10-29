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

fn top_bar_reads(harness: &TestHarness, id: WinID) -> usize {
    let mut reads = 0;
    harness
        .mock_state
        .update_window(id, |window| reads = window.top_bar_reads);
    reads
}

/// Window 1 accepts no less than 900 px, so windows 0 to 2 need 1300 px of a
/// 1270 px column. Each overlaps the one above it only below that window's
/// measured top bar.
fn overflowing_tall_harness() -> TestHarness {
    let mut harness = tall_harness_with_windows(4);
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 900);
    for (id, top_bar) in [(0, 60), (1, 40), (2, 52)] {
        harness
            .mock_state
            .update_window(id, |window| window.top_bar_height = Some(top_bar));
    }
    stack_all(&mut harness, 3);
    harness
}

/// Window 1 starts high enough to fit its 900 px above the display bottom,
/// and window 2 starts below window 1's 40 px top bar.
#[test]
fn an_overflowing_stack_overlaps_only_below_each_measured_top_bar() {
    let mut harness = overflowing_tall_harness();

    assert_eq!(
        rows(harness.world(), &[0, 1, 2]),
        vec![(20, COLUMN), (390, 900), (1090, 200)]
    );
    for id in 0..3 {
        assert_eq!(top_bar_reads(&harness, id), 1, "window {id} measured once");
    }
    assert_eq!(top_bar_reads(&harness, 3), 0, "a window outside the stack");
}

/// A window keeps its measured top bar only while its stack overlaps, so a
/// top bar that changed in the meantime is measured afresh. Window 0's top
/// bar grows from 52 to 96 px while the stack fits, and window 1 then starts
/// below it.
#[test]
fn a_stack_that_overlaps_again_measures_its_top_bars_again() {
    let mut harness = TestHarness::new().with_windows(4);
    for id in 0..4 {
        harness
            .mock_state
            .update_window(id, |window| window.top_bar_height = Some(52));
    }
    stack_all(&mut harness, 4);
    harness.run(vec![command(Operation::Stack(false))]);
    harness.advance(SETTLE);
    let world = harness.world();
    let cached = world
        .query_filtered::<(), With<crate::ecs::TopBarHeight>>()
        .iter(world)
        .count();
    assert_eq!(cached, 0, "a stack that fits keeps no top bars");

    harness
        .mock_state
        .update_window(0, |window| window.top_bar_height = Some(96));
    harness.run(vec![command(Operation::Stack(true))]);
    harness.advance(SETTLE);

    for id in 0..4 {
        assert_eq!(top_bar_reads(&harness, id), 2, "window {id} measured again");
    }
    // 548 px above the last window: 200 of top bars and 348 shared, 116 each.
    assert_eq!(
        rows(harness.world(), &[0, 1, 2, 3]),
        vec![(20, 748), (232, 536), (400, 368), (568, 200)]
    );
}

/// Raising a stack in order changes nothing the next pass would order
/// differently, so an idle stack and a layout pass that keeps its order
/// raise nothing.
#[test]
fn an_ordered_overlapping_stack_is_not_raised_again() {
    let mut harness = TestHarness::new().with_windows(4);
    stack_all(&mut harness, 4);
    assert_eq!(harness.mock_state.take_raised(), vec![0, 1, 2, 3]);

    harness.advance(SETTLE);
    let world = harness.world();
    for mut strip in world
        .query::<&mut crate::ecs::layout::LayoutStrip>()
        .iter_mut(world)
    {
        strip.set_changed();
    }
    harness.advance(SETTLE);
    assert_eq!(harness.mock_state.take_raised(), Vec::<WinID>::new());
}

/// Raising a window of the frontmost app makes it the app's focused window,
/// so ordering a stack sends a focus notification for each window it raises.
/// The focused window is raised last, so focus stays on it and the
/// notifications do not order the stack again.
#[test]
fn ordering_a_stack_keeps_focus_when_raising_takes_it() {
    let mut harness = TestHarness::new().with_windows(5);
    stack_all(&mut harness, 4);
    harness.mock_state.set_raise_takes_focus(true);
    harness.mock_state.take_raised();

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    harness.advance(SETTLE);

    crate::assert_focused!(harness.world(), 2);
    assert_eq!(harness.mock_state.take_raised(), vec![0, 1, 3, 2]);
}

/// Focus that enters an overlapping stack from outside it, such as when the
/// app focuses one of its windows, brings that window in front of the stack.
#[test]
fn focus_entering_an_overlapping_stack_raises_the_focused_window() {
    let mut harness = TestHarness::new().with_windows(5);
    stack_all(&mut harness, 4);
    harness.run(vec![command(Operation::Focus(Direction::East))]);
    harness.advance(SETTLE);
    harness.mock_state.take_raised();

    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);

    crate::assert_focused!(harness.world(), 1);
    assert_eq!(harness.mock_state.take_raised(), vec![0, 2, 3, 1]);
}
