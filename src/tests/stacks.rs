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

fn animated_tall_harness() -> TestHarness {
    use crate::config::{Config, MainOptions};

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
    harness.with_windows(3)
}

fn assert_settled(world: &mut World) {
    use crate::ecs::{ResizeMarker, VerifyWindowSize};
    assert!(
        world
            .query_filtered::<(), Or<(With<ResizeMarker>, With<VerifyWindowSize>)>>()
            .iter(world)
            .next()
            .is_none(),
        "every resize has finished"
    );
}

/// Shrinking or equalizing never asks a window for less than its known
/// minimum, so the animation ends instead of fighting the layout.
#[test]
fn shrinking_and_equalizing_below_a_minimum_settle() {
    for operation in [
        Operation::ResizeVerticalStep(crate::commands::ResizeDirection::Shrink),
        Operation::Equalize,
    ] {
        let mut harness = animated_tall_harness();
        harness.mock_state.update_window(0, |window| {
            window.minimum_size = Some(Size::new(0, 700));
        });
        stack_three(&mut harness);
        harness.advance(SETTLE);
        harness.run(vec![
            command(Operation::Focus(Direction::North)),
            command(Operation::Focus(Direction::North)),
            command(operation),
        ]);
        harness.advance(SETTLE);

        let world = harness.world();
        assert_settled(world);
        let frames = column(world, &[0, 1, 2]);
        assert!(frames[0].height() >= 700, "{frames:?}");
        assert_eq!(heights(&frames).iter().sum::<i32>(), COLUMN, "{frames:?}");
    }
}

/// With animation on, a stack holding a window with a known minimum
/// settles: the split already leaves that window its minimum, so no resize
/// keeps fighting the layout.
#[test]
fn an_animated_split_with_a_minimum_height_settles() {
    let mut harness = animated_tall_harness();
    harness.mock_state.update_window(0, |window| {
        window.minimum_size = Some(Size::new(0, 700));
    });
    stack_three(&mut harness);
    harness.advance(SETTLE);

    let world = harness.world();
    assert_settled(world);
    let frames = column(world, &[0, 1, 2]);
    assert_eq!(frames[0].height(), 700, "{frames:?}");
    assert_even(&heights(&frames[1..]), COLUMN - 700);
}

/// Stacks windows 1 to `count - 1` under window 0, one Stack command each.
fn stack_all(harness: &mut TestHarness, count: WinID) {
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
    ]);
    for _ in 1..count {
        harness.run(vec![
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(Side::West)),
        ]);
        harness.advance(SETTLE);
    }
}

/// Each window's top and height.
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

fn set_minimum_height(harness: &TestHarness, id: WinID, height: i32) {
    harness.mock_state.update_window(id, |window| {
        window.minimum_size = Some(Size::new(0, height));
    });
}

fn set_top_bar(harness: &TestHarness, id: WinID, height: i32) {
    harness.mock_state.update_window(id, |window| {
        window.top_bar_height = Some(height);
    });
}

/// More windows than fit at the minimum window height used to drop the whole
/// column from the layout, leaving every window where it was. They overlap
/// instead: the last window ends at the display bottom, and each window above
/// shows its top bar and an even share of the rest.
#[test]
fn a_stack_too_tall_for_the_minimum_window_height_overlaps() {
    let mut harness = TestHarness::new().with_windows(5);
    stack_all(&mut harness, 5);

    // A 748 point column: 548 above the last window, 208 of it the four
    // fallback top bars and 340 shared, 85 each.
    assert_eq!(
        rows(harness.world(), &[0, 1, 2, 3, 4]),
        vec![(20, 748), (157, 611), (294, 474), (431, 337), (568, 200)]
    );
}

/// Windows whose minimum heights exceed the column overlap, each covering
/// the window above only below that window's measured top bar. No window
/// extends past the display bottom while the top bars fit.
#[test]
fn an_overflowing_stack_overlaps_only_below_each_top_bar() {
    let mut harness = tall_harness();
    set_minimum_height(&harness, 1, 900);
    for (id, top_bar) in [(0, 60), (1, 40), (2, 52)] {
        set_top_bar(&harness, id, top_bar);
    }
    stack_three(&mut harness);

    // Window 1 starts high enough to fit its 900 points above the bottom.
    assert_eq!(
        rows(harness.world(), &[0, 1, 2]),
        vec![(20, COLUMN), (390, 900), (1090, 200)]
    );
    for id in 0..3 {
        let mut reads = 0;
        harness
            .mock_state
            .update_window(id, |window| reads = window.top_bar_reads);
        assert_eq!(reads, 1, "window {id}'s top bar is measured once");
    }
}

/// A top bar Accessibility cannot measure keeps 52 points visible. Here the
/// three 52 point top bars and the last window's minimum do not fit, so only
/// that window extends past the display bottom.
#[test]
fn an_unmeasured_top_bar_keeps_52_points() {
    let mut harness = tall_harness();
    for id in 0..3 {
        set_minimum_height(&harness, id, 1200);
    }
    stack_three(&mut harness);

    assert_eq!(
        rows(harness.world(), &[0, 1, 2]),
        vec![(20, COLUMN), (72, 1218), (124, 1200)]
    );
    assert_eq!(frame(harness.world(), 2).max.y, TALL + 34);
}

/// The same windows with 30 point top bars fit above the display bottom, so
/// the stack does not extend past it: every window ends at the bottom.
#[test]
fn a_stack_extends_past_the_bottom_only_when_its_top_bars_do_not_fit() {
    let mut harness = tall_harness();
    for id in 0..3 {
        set_minimum_height(&harness, id, 1200);
        set_top_bar(&harness, id, 30);
    }
    stack_three(&mut harness);

    // 10 points are left above the last window, 5 for each strip.
    assert_eq!(
        rows(harness.world(), &[0, 1, 2]),
        vec![(20, COLUMN), (55, 1235), (90, 1200)]
    );
}

/// In an overlapping stack each lower window is in front of the window above
/// it, so every top bar shows. The focused window comes to the front, and
/// returns to its place when focus leaves it.
#[test]
fn focus_brings_an_overlapped_window_to_the_front_until_it_leaves() {
    let mut harness = TestHarness::new().with_windows(4);
    set_minimum_height(&harness, 1, 500);
    stack_all(&mut harness, 3);
    let stack = [0, 1, 2];
    assert!(
        frame(harness.world(), 2).min.y < frame(harness.world(), 1).max.y,
        "the stack overlaps"
    );
    assert_eq!(harness.mock_state.window_order(&stack), vec![0, 1, 2]);

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    assert_eq!(harness.mock_state.window_order(&stack), vec![0, 2, 1]);

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    assert_eq!(harness.mock_state.window_order(&stack), vec![1, 2, 0]);

    harness.run(vec![command(Operation::Focus(Direction::East))]);
    assert_eq!(harness.mock_state.window_order(&stack), vec![0, 1, 2]);
}
