//! Heights of the windows stacked in one column.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation, Side};
use crate::events::Event;
use crate::manager::{Size, Window};
use crate::platform::WinID;

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// Stacks windows 1 and 2 under window 0, so one column holds all three.
fn stack_three(harness: &mut TestHarness) {
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    // The two-window stack settles before the third window joins it.
    harness.advance(SETTLE);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
}

/// Each window's frame top to bottom, checking they stay in one column and
/// neither overlap nor leave a gap.
fn column(world: &mut World, ids: &[WinID]) -> Vec<IRect> {
    let frames = ids.iter().map(|id| frame(world, *id)).collect::<Vec<_>>();
    for pair in frames.windows(2) {
        assert_eq!(pair[0].min.x, pair[1].min.x, "one column: {frames:?}");
        assert_eq!(
            pair[0].max.y, pair[1].min.y,
            "no overlap or gap: {frames:?}"
        );
    }
    frames
}

/// A display as tall as a 14 inch laptop's, where an uneven split stays above
/// the minimum window height instead of falling back to an even one.
const TALL: i32 = 1290;
const COLUMN: i32 = TALL - TEST_MENUBAR_HEIGHT;

fn tall_harness() -> TestHarness {
    let harness = TestHarness::new();
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, TALL));
    harness.with_windows(3)
}

fn heights(frames: &[IRect]) -> Vec<i32> {
    frames.iter().map(IRect::height).collect()
}

fn assert_even(heights: &[i32], total: i32) {
    assert_eq!(heights.iter().sum::<i32>(), total, "{heights:?}");
    assert!(
        heights.iter().max().unwrap() - heights.iter().min().unwrap() <= 1,
        "even split: {heights:?}"
    );
}

/// A window joining a stack gives every window in it an equal share. Keeping
/// the existing heights would leave the first window half the column and the
/// other two a quarter each.
#[test]
fn a_window_joining_a_stack_splits_the_column_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);

    let frames = column(harness.world(), &[0, 1, 2]);
    assert_even(&heights(&frames), COLUMN);
}

/// A window leaving a stack gives the windows that stay an equal share.
#[test]
fn a_window_leaving_a_stack_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![command(Operation::Unstack(Side::East))]);
    harness.advance(SETTLE);

    let frames = column(harness.world(), &[0, 1]);
    assert_even(&heights(&frames), COLUMN);
}

/// A window that will not shrink to its share keeps the height it accepts,
/// and the windows below it move down instead of being covered.
#[test]
fn a_window_with_a_minimum_height_does_not_overlap_the_next() {
    let mut harness = tall_harness();
    harness.mock_state.update_window(1, |window| {
        window.minimum_size = Some(Size::new(0, 500));
    });
    stack_three(&mut harness);

    let frames = column(harness.world(), &[0, 1, 2]);
    assert_eq!(frames[1].height(), 500, "{frames:?}");
    assert_even(&[frames[0].height(), frames[2].height()], COLUMN - 500);
}

/// The last window gets what is left of the column, so when it is the one
/// with a minimum height, the windows above it must make room.
#[test]
fn a_last_window_with_a_minimum_height_stays_on_screen() {
    let mut harness = tall_harness();
    harness.mock_state.update_window(2, |window| {
        window.minimum_size = Some(Size::new(0, 500));
    });
    stack_three(&mut harness);

    let frames = column(harness.world(), &[0, 1, 2]);
    assert_eq!(frames[2].height(), 500, "{frames:?}");
    assert_eq!(
        frames[2].max.y, TALL,
        "it ends at the display bottom: {frames:?}"
    );
    assert_even(&heights(&frames[..2]), COLUMN - 500);
}

/// A learned minimum is dropped once the window is seen smaller than it, so
/// an app that now accepts less gets an even share at the next split.
#[test]
fn a_minimum_height_is_dropped_when_the_window_is_seen_smaller() {
    let mut harness = tall_harness();
    harness.mock_state.update_window(1, |window| {
        window.minimum_size = Some(Size::new(0, 500));
    });
    stack_three(&mut harness);
    assert_eq!(frame(harness.world(), 1).height(), 500);

    harness
        .mock_state
        .update_window(1, |window| window.minimum_size = None);
    harness
        .mock_state
        .os_resize_window(1, Size::new(TEST_WINDOW_WIDTH, 300));
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Unstack(Side::East))]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Stack(Side::West))]);
    harness.advance(SETTLE);

    let frames = column(harness.world(), &[0, 1, 2]);
    assert_even(&heights(&frames), COLUMN);
}

/// Closing a stacked window leaves the others an equal share.
#[test]
fn closing_a_stacked_window_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.mock_state.os_close_window(2);
    harness.advance(SETTLE);

    let frames = column(harness.world(), &[0, 1]);
    assert_even(&heights(&frames), COLUMN);
}

/// Stacking with no column to join changes nothing, so heights the user set
/// stay.
#[test]
fn a_stack_command_that_changes_nothing_keeps_set_heights() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
        command(Operation::ResizeVerticalStep(
            crate::commands::ResizeDirection::Grow,
        )),
    ]);
    harness.advance(SETTLE);
    let before = heights(&column(harness.world(), &[0, 1, 2]));
    assert!(before[0] > before[1], "the resize took effect: {before:?}");

    harness.run(vec![command(Operation::Stack(Side::West))]);
    harness.advance(SETTLE);

    assert_eq!(heights(&column(harness.world(), &[0, 1, 2])), before);
}

/// Minimizing a stacked window leaves the others an equal share, like
/// closing it.
#[test]
fn minimizing_a_stacked_window_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.mock_state.os_minimize_window(2, true);
    harness.advance(SETTLE);

    let frames = column(harness.world(), &[0, 1]);
    assert_even(&heights(&frames), COLUMN);
}

/// With animation on, a stack holding a window with a known minimum
/// settles: the split already leaves that window its minimum, so no resize
/// keeps fighting the layout.
#[test]
fn an_animated_split_with_a_minimum_height_settles() {
    use crate::config::{Config, MainOptions};
    use crate::ecs::{ResizeMarker, VerifyWindowSize};

    let config: Config = (
        MainOptions {
            animation_speed: Some(14.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let harness = TestHarness::new().with_config(config);
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, TALL));
    let mut harness = harness.with_windows(3);
    harness.mock_state.update_window(0, |window| {
        window.minimum_size = Some(Size::new(0, 700));
    });
    stack_three(&mut harness);
    harness.advance(SETTLE);

    let world = harness.world();
    assert!(
        world
            .query_filtered::<(), Or<(With<ResizeMarker>, With<VerifyWindowSize>)>>()
            .iter(world)
            .next()
            .is_none(),
        "every resize has finished"
    );
    let frames = column(world, &[0, 1, 2]);
    assert_eq!(frames[0].height(), 700, "{frames:?}");
    assert_even(&heights(&frames[1..]), COLUMN - 700);
}
