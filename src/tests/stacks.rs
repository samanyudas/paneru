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
