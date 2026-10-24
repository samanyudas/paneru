//! Windows stacked in one column: their heights, and how many one column holds.

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
    let mut harness = harness_with("", TALL, 5);
    stack_all(&mut harness, 5);
    assert_eq!(layout(harness.world()), vec![vec![0, 1, 2, 3, 4]]);
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
    );
    harness.run(vec![Event::DisplayResized {
        display_id: TEST_DISPLAY_ID,
    }]);
    harness.advance(SETTLE);

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

#[test]
fn a_tall_lower_window_fits_when_the_top_bars_above_it_fit() {
    let mut harness = harness_with("", 1020, 4);
    set_minimum_height(&harness, 2, 850);
    stack_all(&mut harness, 4);

    assert_eq!(layout(harness.world()), vec![vec![0, 1, 2, 3]]);
    assert!(
        frame(harness.world(), 2).max.y <= 1020,
        "the tall window and the two 52-point top bars above it fit: {:?}",
        frame(harness.world(), 2)
    );
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

/// `AXRaise` on a window of the frontmost app makes it the app's focused
/// window, so ordering a stack emits focus notifications. They end on the
/// window that already holds focus, so they must not order the stack again,
/// or the stack is raised forever.
#[test]
fn ordering_an_overlapping_stack_settles_when_raising_takes_focus() {
    let mut harness = TestHarness::new().with_windows(4);
    set_minimum_height(&harness, 1, 500);
    stack_all(&mut harness, 3);
    let stack = [0, 1, 2];
    harness.mock_state.set_raise_takes_focus(true);

    let before = harness.mock_state.raise_count();
    harness.run(vec![command(Operation::Focus(Direction::North))]);
    let settled = harness.mock_state.raise_count();
    assert!(
        settled - before <= 2 * stack.len(),
        "focusing raises each window a bounded number of times, not {}",
        settled - before
    );
    assert_eq!(harness.mock_state.window_order(&stack), vec![0, 2, 1]);
    assert_eq!(
        harness.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(1)
    );

    harness.advance(SETTLE);
    assert_eq!(
        harness.mock_state.raise_count(),
        settled,
        "the ordered stack is not raised again"
    );

    // A layout pass that leaves the stack order alone does not raise it.
    let world = harness.world();
    let mut strips = world.query::<&mut crate::ecs::layout::LayoutStrip>();
    for mut strip in strips.iter_mut(world) {
        strip.set_changed();
    }
    harness.advance(SETTLE);
    assert_eq!(harness.mock_state.raise_count(), settled);
}

/// macOS shows a focused window in front a moment after the engine asks.
/// Raising a window of an app in the background puts it just below the
/// frontmost app's windows, and only once that app gets to it. A stack of
/// windows from different apps is ordered once the focused window shows in
/// front, one window at a time, so the window that had focus does not stay over
/// the rest and the raises do not land out of order.
#[test]
fn an_overlapping_stack_of_several_apps_waits_for_the_focused_window() {
    let mut harness = TestHarness::new()
        .with_app(2, "stack.b", "Stack-B", |_| {})
        .with_app(3, "stack.c", "Stack-C", |_| {})
        .with_app(4, "beside", "Beside", |_| {})
        .with_app(5, "windowless", "Windowless", |_| {});
    let state = harness.mock_state.clone();
    let windows = (0..4)
        .map(|id| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            state.spawn_window(id + 1, TEST_WORKSPACE_ID, id, frame)
        })
        .collect();
    harness.existed_at_launch(0..4);
    harness
        .world()
        .trigger(crate::ecs::SpawnWindowTrigger(windows));
    set_minimum_height(&harness, 1, 500);
    stack_all(&mut harness, 3);
    let stack = [0, 1, 2];
    assert_eq!(state.window_order(&stack), vec![0, 1, 2]);
    state.lag_like_macos(3);

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    assert_eq!(state.window_order(&stack), vec![0, 2, 1]);

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    assert_eq!(state.window_order(&stack), vec![1, 2, 0]);

    harness.run(vec![command(Operation::Focus(Direction::South))]);
    assert_eq!(state.window_order(&stack), vec![0, 2, 1]);

    harness.run(vec![command(Operation::Focus(Direction::East))]);
    assert_eq!(state.window_order(&stack), vec![0, 1, 2]);

    harness.run(vec![command(Operation::Focus(Direction::West))]);
    crate::assert_focused!(harness.world(), 0);
    assert_eq!(state.window_order(&stack), vec![1, 2, 0]);

    // While an app with no windows is frontmost, a raise goes below the
    // previous app's window, so the stack keeps its order.
    let raises = state.raise_count();
    state.activate_app(5);
    harness.advance(SETTLE);
    assert_eq!(state.window_order(&stack), vec![1, 2, 0]);
    assert_eq!(state.raise_count(), raises);
}

/// A tall harness whose three windows need 1100 points each and show 60
/// point top bars. They overlap, and every window ends at the display bottom.
fn overflowing_harness() -> TestHarness {
    let harness = tall_harness();
    for id in 0..3 {
        set_minimum_height(&harness, id, 1100);
        set_top_bar(&harness, id, 60);
    }
    harness
}

/// [`overflowing_harness`]'s rows with 60 point top bars: 50 points are left
/// above the last window, 25 for each strip.
const SIXTY_POINT_ROWS: [(i32, i32); 3] = [(20, COLUMN), (105, 1185), (190, 1100)];

fn top_bar_reads(harness: &TestHarness, id: WinID) -> usize {
    let mut reads = 0;
    harness
        .mock_state
        .update_window(id, |window| reads = window.top_bar_reads);
    reads
}

/// A browser can show its bookmarks bar after its top bar was measured. The
/// top bar is measured again when the window loses focus, which is when the
/// window below starts to cover it, and when the window resizes.
#[test]
fn a_top_bar_that_grows_after_it_was_measured_moves_the_window_below() {
    let mut harness = overflowing_harness();
    stack_three(&mut harness);
    assert_eq!(rows(harness.world(), &[0, 1, 2]), SIXTY_POINT_ROWS);

    // The bookmarks bar appears while window 0 is focused.
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
    ]);
    set_top_bar(&harness, 0, 96);
    harness.run(vec![
        command(Operation::Focus(Direction::South)),
        command(Operation::Focus(Direction::South)),
    ]);
    // 14 points are left above the last window, 7 for each strip.
    assert_eq!(
        rows(harness.world(), &[0, 1, 2]),
        vec![(20, COLUMN), (123, 1167), (190, 1100)]
    );

    // It disappears while window 0 is in the background, and the window
    // resizes.
    set_top_bar(&harness, 0, 60);
    let size = frame(harness.world(), 0).size();
    harness.mock_state.os_resize_window(0, size);
    harness.advance(SETTLE);
    assert_eq!(rows(harness.world(), &[0, 1, 2]), SIXTY_POINT_ROWS);
}

/// Accessibility can fail while an app is busy. The layout keeps 52 points
/// for the top bar until a later try reads it, without waiting for anything
/// else to change, and does not try on every frame.
#[test]
fn a_top_bar_that_cannot_be_read_yet_is_measured_again_later() {
    let mut harness = overflowing_harness();
    harness.mock_state.update_window(0, |window| {
        window.top_bar_unreadable = true;
    });
    stack_three(&mut harness);
    // 58 points are left above the last window, 29 for each strip.
    let fallback_rows = vec![(20, COLUMN), (101, 1189), (190, 1100)];
    assert_eq!(rows(harness.world(), &[0, 1, 2]), fallback_rows);

    let reads = top_bar_reads(&harness, 0);
    harness.advance(Duration::from_secs(10));
    let retries = top_bar_reads(&harness, 0) - reads;
    assert!(
        (1..=6).contains(&retries),
        "an unreadable top bar is tried again, but not on every frame: {retries} tries"
    );
    assert_eq!(rows(harness.world(), &[0, 1, 2]), fallback_rows);

    harness.mock_state.update_window(0, |window| {
        window.top_bar_unreadable = false;
    });
    harness.advance(Duration::from_secs(30));
    assert_eq!(rows(harness.world(), &[0, 1, 2]), SIXTY_POINT_ROWS);
}

#[test]
fn resizing_a_busy_window_waits_for_its_top_bar_retry() {
    let mut harness = overflowing_harness();
    stack_three(&mut harness);
    harness.mock_state.update_window(0, |window| {
        window.top_bar_unreadable = true;
    });
    let size = frame(harness.world(), 0).size();
    let before = top_bar_reads(&harness, 0);

    for _ in 0..10 {
        harness.mock_state.os_resize_window(0, size);
        harness.advance(Duration::from_millis(40));
    }
    assert_eq!(
        top_bar_reads(&harness, 0) - before,
        1,
        "resizing waits for the retry after the first failed read"
    );

    harness.advance(Duration::from_secs(1));
    assert_eq!(
        top_bar_reads(&harness, 0) - before,
        2,
        "the scheduled retry still reads the top bar"
    );
}

/// A top bar that cannot be read when it is measured again keeps its last
/// measurement until a retry reads it, rather than falling back to 52 points.
#[test]
fn a_failed_refresh_keeps_the_last_top_bar_until_a_retry_reads_it() {
    let mut harness = overflowing_harness();
    set_top_bar(&harness, 0, 96);
    stack_three(&mut harness);
    let ninety_six_point_rows = vec![(20, COLUMN), (123, 1167), (190, 1100)];
    assert_eq!(rows(harness.world(), &[0, 1, 2]), ninety_six_point_rows);

    // Window 0 resizes while its app is busy.
    harness.mock_state.update_window(0, |window| {
        window.top_bar_unreadable = true;
        window.top_bar_height = Some(60);
    });
    let reads = top_bar_reads(&harness, 0);
    let size = frame(harness.world(), 0).size();
    harness.mock_state.os_resize_window(0, size);
    harness.advance(Duration::from_secs(5));
    assert!(
        top_bar_reads(&harness, 0) > reads + 1,
        "the read is retried"
    );
    // A layout pass while the retry waits still uses the last measurement.
    let world = harness.world();
    let mut strips = world.query::<&mut crate::ecs::layout::LayoutStrip>();
    for mut strip in strips.iter_mut(world) {
        strip.set_changed();
    }
    harness.advance(SETTLE);
    assert_eq!(rows(harness.world(), &[0, 1, 2]), ninety_six_point_rows);

    harness.mock_state.update_window(0, |window| {
        window.top_bar_unreadable = false;
    });
    harness.advance(Duration::from_secs(30));
    assert_eq!(rows(harness.world(), &[0, 1, 2]), SIXTY_POINT_ROWS);
}

fn harness_with(config: &str, display_height: i32, windows: i32) -> TestHarness {
    let config = crate::config::Config::try_from(config).expect("config parses");
    let harness = TestHarness::new().with_config(config);
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, display_height),
    );
    harness.with_windows(windows)
}

/// The window IDs of each column of the active strip, left to right, each
/// column top to bottom.
fn layout(world: &mut World) -> Vec<Vec<WinID>> {
    use crate::ecs::ActiveWorkspaceMarker;
    use crate::ecs::layout::LayoutStrip;

    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .expect("one active strip")
        .columns()
        .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
        .collect()
}

/// Moves the window right of the first column into it, `times` times.
fn stack_into_first_column(harness: &mut TestHarness, times: usize) {
    for _ in 0..times {
        harness.run(vec![
            command(Operation::Focus(Direction::First)),
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(Side::West)),
        ]);
        harness.advance(SETTLE);
    }
}

/// Stacks the last window onto the one left of it.
fn stack_last(harness: &mut TestHarness) {
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
}

fn stack_focused(harness: &mut TestHarness, side: Side) {
    harness.run(vec![command(Operation::Stack(side))]);
    harness.advance(SETTLE);
}

/// The default limit of five stops a sixth window from joining, even on a
/// display that fits six.
#[test]
fn the_default_limit_holds_five_windows() {
    let mut harness = harness_with("", TALL, 7);
    stack_into_first_column(&mut harness, 5);

    assert_eq!(
        layout(harness.world()),
        vec![vec![0, 1, 2, 3, 4], vec![5], vec![6]]
    );
}

/// A stacked window that cannot join a full stack leaves its own stack for a
/// new column next to the full one.
#[test]
fn stacking_onto_a_full_stack_starts_a_new_column() {
    let mut harness = harness_with("[options]\nstack_limit = 2\n", TALL, 4);
    stack_into_first_column(&mut harness, 1);
    stack_last(&mut harness);
    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2, 3]]);

    stack_focused(&mut harness, Side::West);

    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![3], vec![2]]);
}

/// The same applies stacking east, where the new column opens on the full
/// stack's west side. A column jump returns to window 1, which joined the
/// first column last.
#[test]
fn stacking_east_onto_a_full_stack_starts_a_new_column() {
    let mut harness = harness_with("[options]\nstack_limit = 2\n", TALL, 4);
    stack_into_first_column(&mut harness, 1);
    stack_last(&mut harness);
    harness.run(vec![command(Operation::Focus(Direction::First))]);

    stack_focused(&mut harness, Side::East);

    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2, 3]]);
}

/// A window that is already its own column stays where it is when the stack
/// it would join is full.
#[test]
fn a_single_window_beside_a_full_stack_stays_its_own_column() {
    let mut harness = harness_with("[options]\nstack_limit = 2\n", TALL, 3);
    stack_into_first_column(&mut harness, 1);

    stack_last(&mut harness);

    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2]]);
}

/// "fit" allows as many windows as fit at the minimum height: three on a
/// 748 point column at 200 points each.
#[test]
fn fit_allows_as_many_windows_as_fit_the_display() {
    let mut harness = harness_with("[options]\nstack_limit = \"fit\"\n", TEST_DISPLAY_HEIGHT, 5);
    stack_into_first_column(&mut harness, 2);
    stack_last(&mut harness);
    assert_eq!(layout(harness.world()), vec![vec![0, 1, 2], vec![3, 4]]);

    stack_focused(&mut harness, Side::West);

    assert_eq!(
        layout(harness.world()),
        vec![vec![0, 1, 2], vec![4], vec![3]]
    );
}

/// "fit" on a tall display allows more: six on a 1270 point column.
#[test]
fn fit_allows_six_windows_on_a_tall_display() {
    let mut harness = harness_with("[options]\nstack_limit = \"fit\"\n", TALL, 7);
    stack_into_first_column(&mut harness, 6);

    assert_eq!(
        layout(harness.world()),
        vec![vec![0, 1, 2, 3, 4, 5], vec![6]]
    );
}

/// A number above what fits is capped at what fits.
#[test]
fn a_limit_above_what_fits_is_capped() {
    let mut harness = harness_with("[options]\nstack_limit = 10\n", TEST_DISPLAY_HEIGHT, 5);
    stack_into_first_column(&mut harness, 2);
    stack_last(&mut harness);

    stack_focused(&mut harness, Side::West);

    assert_eq!(
        layout(harness.world()),
        vec![vec![0, 1, 2], vec![4], vec![3]]
    );
}

/// Lowering the limit on a running engine leaves a stack over the new limit
/// intact, and the next Stack command into it starts a new column.
#[test]
fn lowering_the_limit_keeps_existing_stacks() {
    let path = std::env::temp_dir().join(format!(
        "paneru-lowering-the-limit-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, "[options]\nstack_limit = 5\n").expect("write config");
    let mut config = crate::config::Config::new(&path).expect("config parses");
    let harness = TestHarness::new().with_config(config.clone());
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, TALL));
    let mut harness = harness.with_windows(5);
    stack_into_first_column(&mut harness, 2);
    stack_last(&mut harness);
    assert_eq!(layout(harness.world()), vec![vec![0, 1, 2], vec![3, 4]]);

    std::fs::write(&path, "[options]\nstack_limit = 2\n").expect("write config");
    config.reload_config(&path).expect("reload parses");
    std::fs::remove_file(&path).expect("remove config");
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 1, 2], vec![3, 4]]);

    stack_focused(&mut harness, Side::West);

    assert_eq!(
        layout(harness.world()),
        vec![vec![0, 1, 2], vec![4], vec![3]]
    );
}

#[test]
fn stack_limit_parses_a_number_or_fit() {
    use crate::config::{Config, StackLimit};
    use std::num::NonZeroUsize;

    let limit = |input: &str| Config::try_from(input).map(|config| config.stack_limit());

    assert_eq!(
        limit("").unwrap(),
        StackLimit::Count(NonZeroUsize::new(5).unwrap())
    );
    assert_eq!(
        limit("[options]\nstack_limit = 5\n").unwrap(),
        StackLimit::Count(NonZeroUsize::new(5).unwrap())
    );
    assert_eq!(
        limit("[options]\nstack_limit = 1\n").unwrap(),
        StackLimit::Count(NonZeroUsize::new(1).unwrap())
    );
    assert_eq!(
        limit("[options]\nstack_limit = \"fit\"\n").unwrap(),
        StackLimit::Fit
    );
    for (bad, problem) in [
        ("0", "invalid value: integer `0`"),
        ("-2", "invalid value: integer `-2`"),
        ("\"bogus\"", "invalid value: string \"bogus\""),
        ("2.5", "invalid type: floating point `2.5`"),
        ("true", "invalid type: boolean `true`"),
    ] {
        let error = limit(&format!("[options]\nstack_limit = {bad}\n"))
            .expect_err(bad)
            .to_string();
        assert!(
            error.contains(&format!(
                "{problem}, expected a whole number from 1 or \"fit\""
            )),
            "{bad}: {error}"
        );
    }
}

#[test]
fn the_effective_limit_never_exceeds_what_fits() {
    use crate::config::Config;
    use crate::ecs::layout::effective_stack_limit;

    let effective = |input: &str, height: i32| {
        effective_stack_limit(&Config::try_from(input).expect("config parses"), height)
    };

    assert_eq!(effective("", 1270), 5);
    assert_eq!(effective("", 748), 3);
    assert_eq!(effective("[options]\nstack_limit = 2\n", 1270), 2);
    assert_eq!(effective("[options]\nstack_limit = 10\n", 748), 3);
    assert_eq!(effective("[options]\nstack_limit = \"fit\"\n", 1270), 6);
    assert_eq!(effective("[options]\nstack_limit = \"fit\"\n", 1399), 6);
    assert_eq!(effective("[options]\nstack_limit = \"fit\"\n", 1400), 7);
    assert_eq!(effective("[options]\nstack_limit = \"fit\"\n", 150), 1);
}

#[test]
fn test_width_settlement_unfocused_sibling_refusal_preserves_unequal_heights() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
        command(Operation::ResizeVerticalStep(
            crate::commands::ResizeDirection::Grow,
        )),
        command(Operation::Center),
    ]);
    harness.advance(SETTLE);
    let before = column(harness.world(), &[0, 1, 2]);
    let before_heights = heights(&before);
    assert!(before_heights[0] > before_heights[1]);
    crate::assert_focused!(harness.world(), 0);
    harness
        .mock_state
        .update_window(2, |window| window.minimum_size = Some(Size::new(400, 0)));
    harness.run(vec![command(Operation::SetWidth(0.25))]);
    harness.advance(Duration::from_secs(3));
    let after = column(harness.world(), &[0, 1, 2]);
    assert_eq!(heights(&after), before_heights);
    for frame in after {
        assert_eq!(frame.width(), 400);
        assert_eq!(frame.min.x, 312);
    }
}
