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

/// Shrinking or equalizing never asks a window for less than its known
/// minimum, so with animation on, the app gets no size it refuses and no
/// resize keeps running.
#[test]
fn shrinking_or_equalizing_below_a_minimum_height_settles() {
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
            "{operation:?}: no size below the minimum"
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

/// macOS keeps a window's bottom edge at the bottom of the screen through a
/// small shrink. When the display gets a little shorter, the window still
/// reaches its new height instead of keeping the old one as its minimum, so
/// a window stacked under it fits.
#[test]
fn a_small_shrink_at_the_screen_bottom_is_not_a_minimum() {
    const SHORTER: i32 = TALL - 16;
    let mut harness = tall_harness();
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    harness
        .mock_state
        .update_window(0, |window| window.sticky_bottom = Some(TALL));
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, SHORTER),
    );
    harness.run(vec![Event::DisplayConfigured {
        display_id: TEST_DISPLAY_ID,
    }]);
    harness.advance(SETTLE);
    assert_eq!(
        frame(harness.world(), 0).height(),
        SHORTER - TEST_MENUBAR_HEIGHT
    );

    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1]);
    assert_even(&heights, SHORTER - TEST_MENUBAR_HEIGHT);
}

/// Two windows that each refuse less than 400 px share a 748 px column. No
/// split fits both, so a vertical resize there leaves the heights alone.
#[test]
fn a_vertical_resize_in_a_column_too_short_for_its_minimums_changes_nothing() {
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

/// A learned minimum is dropped once the user makes the window shorter than
/// it, so an app that now accepts less gets an even share from Equalize.
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
    harness.run(vec![command(Operation::Equalize)]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// Window 1's app accepts any height again, for example once a sheet that
/// held it open has closed. A window leaving and joining the column asks
/// every window in it for an even share again, so window 1 gets one.
#[test]
fn a_stack_change_checks_a_minimum_height_again() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    assert_eq!(frame(harness.world(), 1).height(), 500);

    harness
        .mock_state
        .update_window(1, |window| window.min_height = 0);
    harness.run(vec![command(Operation::Stack(false))]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Stack(true))]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// An app that applies a size after the write returns, as Chromium-based
/// apps do, still shows its old height when the frame is read back. Window 0
/// is asked to go from half the column to a third when window 2 joins. Its old
/// height is not a minimum, so the stack splits evenly once the app catches
/// up, whether before the window is read again or after it was asked for less.
#[test]
fn a_window_that_resizes_late_gets_an_even_share() {
    for delay in [Duration::from_millis(200), Duration::from_millis(800)] {
        let mut harness = tall_harness();
        harness.run(vec![
            Event::MenuOpened { window_id: 0 },
            command(Operation::Focus(Direction::First)),
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(true)),
            command(Operation::Focus(Direction::Last)),
        ]);
        harness.advance(SETTLE);
        harness
            .mock_state
            .update_window(0, |window| window.late_resizes = Some(Vec::new()));
        harness
            .world()
            .write_message(command(Operation::Stack(true)));
        harness.advance(delay);
        let late = frame(harness.world(), 0);
        assert_eq!(late.height(), COLUMN / 2, "{delay:?}: not applied yet");
        harness.mock_state.os_apply_late_resizes(0);
        harness.advance(SETTLE);

        let heights = column_heights(harness.world(), &[0, 1, 2]);
        assert_even(&heights, COLUMN);
    }
}

/// When the minimums in a column add up to more than its height, no split
/// keeps every window both whole and on screen. The windows keep the split
/// they would get with no minimum known, and only the window that refuses
/// its share reaches past it.
#[test]
fn minimums_too_tall_for_the_column_keep_the_other_windows_on_screen() {
    let mut harness = TestHarness::new().with_windows(3);
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);

    for id in [0, 2] {
        let frame = frame(harness.world(), id);
        assert!(
            frame.min.y >= TEST_MENUBAR_HEIGHT && frame.max.y <= TEST_DISPLAY_HEIGHT,
            "window {id} is on screen: {frame:?}"
        );
    }
}

/// A tabbed display gives each window the whole column. A window whose app
/// refuses that height keeps the height it has, and nothing asks it again
/// and again.
#[test]
fn a_tabbed_stack_with_a_refused_height_stops_resizing() {
    let mut harness = TestHarness::new().with_windows(3);
    harness.mock_state.update_window(0, |window| {
        window.min_height = TEST_DISPLAY_HEIGHT + 100;
    });
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::ToggleTabbedDisplay),
    ]);
    harness.advance(SETTLE);

    let before = harness.mock_state.resize_writes(0);
    harness.advance(SETTLE);
    assert_eq!(harness.mock_state.resize_writes(0), before);
}

/// The user resizes window 0 while a check waits to see whether its app
/// takes a lower height: 700 ms after the stack is after the first 500 ms
/// wait and before the second ends. The height the user gives it is not a
/// minimum, so shrinking the window still takes it down to the 700 px its app
/// accepts.
#[test]
fn a_user_resize_during_a_minimum_height_check_is_not_a_minimum() {
    let mut harness = tall_harness_with_windows(2);
    harness
        .mock_state
        .update_window(0, |window| window.min_height = 700);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    harness
        .world()
        .write_message(command(Operation::Stack(true)));
    harness.advance(Duration::from_millis(700));
    harness
        .mock_state
        .os_resize_window(0, Size::new(TEST_WINDOW_WIDTH, 900));
    harness.advance(SETTLE);
    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![900, COLUMN - 900]
    );

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    for _ in 0..2 {
        harness.run(vec![command(Operation::ResizeVertical(
            ResizeDirection::Shrink,
        ))]);
        harness.advance(SETTLE);
    }

    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![700, COLUMN - 700]
    );
}
