//! Where windows hidden toward an edge shared with another display show a
//! slice, and which of them show it.

use std::time::Duration;

use bevy::math::{IRect, IVec2};
use bevy::prelude::World;

use super::*;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::MainOptions;
use crate::ecs::SpawnWindowTrigger;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::shared_edge::{EdgeSlice, showing_edge_slices, showing_may_change};
use crate::platform::WinID;

const SETTLE: Duration = Duration::from_secs(2);
const PAD: i32 = 4;
const OWN: IRect = IRect {
    min: IVec2::new(0, 0),
    max: IVec2::new(TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
};
const RIGHT: IRect = IRect {
    min: IVec2::new(TEST_DISPLAY_WIDTH, 0),
    max: IVec2::new(TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
};
const LEFT: IRect = IRect {
    min: IVec2::new(-EXT_DISPLAY_WIDTH, 0),
    max: IVec2::new(0, EXT_DISPLAY_HEIGHT),
};
const BELOW: IRect = IRect {
    min: IVec2::new(0, TEST_DISPLAY_HEIGHT),
    max: IVec2::new(EXT_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT),
};

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// The test display holding `count` windows with gaps of `2 * PAD`, beside
/// the empty displays `others`. Nothing is laid out until the first run.
fn arranged(others: &[IRect], count: i32) -> TestHarness {
    let options = MainOptions {
        padding_top: Some(4),
        padding_right: Some(4),
        padding_bottom: Some(4),
        padding_left: Some(4),
        ..MainOptions::default()
    };
    let mut harness = TestHarness::new().with_config((options, vec![]).into());
    for (offset, bounds) in (0..).zip(others) {
        harness = harness.with_display(
            EXT_DISPLAY_ID + offset,
            *bounds,
            vec![EXT_WORKSPACE_ID + u64::from(offset)],
        );
    }
    let harness = harness.with_windows(count);
    for id in 0..count {
        harness.mock_state.update_window(id, |window| {
            window.horizontal_padding = PAD;
            window.vertical_padding = PAD;
        });
    }
    harness
}

/// `arranged`, laid out with focus moved by `focus` from the first window.
fn focused(others: &[IRect], count: i32, focus: Option<Direction>) -> TestHarness {
    let mut harness = arranged(others, count);
    let mut events = vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
    ];
    events.extend(focus.map(|direction| command(Operation::Focus(direction))));
    harness.run(events);
    harness.advance(SETTLE);
    harness
}

/// Stacks windows 0 to 2 in the first column, with focus on window 2.
fn stack_first_three(harness: &mut TestHarness) {
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
    harness.run(vec![
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
}

fn focus_last(harness: &mut TestHarness) {
    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
}

/// Windows 0 to 2 stacked in the first of `count` columns, with focus on
/// the last column.
fn stacked_first(others: &[IRect], count: i32) -> TestHarness {
    let mut harness = arranged(others, count);
    stack_first_three(&mut harness);
    focus_last(&mut harness);
    harness
}

/// `stacked_first` with six windows, each stacked window refusing less than
/// `minimum` points of height.
fn stacked_first_refusing(others: &[IRect], minimum: i32) -> TestHarness {
    let mut harness = arranged(others, 6);
    for id in 0..3 {
        harness.mock_state.update_window(id, |window| {
            window.minimum_size = Some(crate::manager::Size::new(0, minimum));
        });
    }
    stack_first_three(&mut harness);
    focus_last(&mut harness);
    harness
}

/// Windows 0 to 2 stacked as tabs in the first of six columns, window 1
/// focused last among them, with focus on the last column.
fn tabbed_first(others: &[IRect]) -> TestHarness {
    let mut harness = arranged(others, 6);
    stack_first_three(&mut harness);
    harness.run(vec![
        command(Operation::ToggleTabbedDisplay),
        command(Operation::Focus(Direction::North)),
    ]);
    harness.advance(SETTLE);
    focus_last(&mut harness);
    harness
}

fn slice_of(world: &mut World, id: WinID) -> Option<EdgeSlice> {
    let entity = find_window_entity(id, world);
    world.get::<EdgeSlice>(entity).copied()
}

fn showing(world: &mut World) -> Vec<(WinID, EdgeSlice)> {
    world
        .run_system_cached(showing_edge_slices)
        .expect("the showing slices")
}

fn showing_ids(world: &mut World) -> Vec<WinID> {
    showing(world).into_iter().map(|(id, _)| id).collect()
}

/// The frame window `id` has in the mock OS, its padding included.
fn os_frame(harness: &TestHarness, id: WinID) -> IRect {
    let mut frame = None;
    harness
        .mock_state
        .update_window(id, |window| frame = Some(window.frame));
    frame.expect("window exists")
}

/// The part of window `id` its display shows, in absolute and in the
/// window's own points.
fn sliver(harness: &mut TestHarness, id: WinID) -> (IRect, IRect) {
    let entity = find_window_entity(id, harness.world());
    let shown = harness
        .world()
        .get::<crate::manager::Window>(entity)
        .expect("a window")
        .shown_frame();
    let on_screen = shown.intersect(OWN);
    let in_window = IRect::from_corners(on_screen.min - shown.min, on_screen.max - shown.min);
    (on_screen, in_window)
}

#[test]
fn a_slice_toward_a_display_beside_matches_the_sliver_with_that_display_below() {
    let mut beside = focused(&[RIGHT], 4, None);
    let mut below = focused(&[BELOW], 4, None);

    let slice = slice_of(beside.world(), 3).expect("window 3 tucks off the display beside");
    assert!(
        os_frame(&beside, 3).intersect(RIGHT).is_empty(),
        "window 3 stays off the display beside"
    );
    assert!(slice_of(below.world(), 3).is_none(), "nothing tucks");
    let (on_screen, in_window) = sliver(&mut below, 3);
    let sliver_width = below
        .world()
        .resource::<crate::config::Config>()
        .sliver_width();
    assert_eq!(
        (on_screen.max.x, on_screen.width()),
        (TEST_DISPLAY_WIDTH, sliver_width),
        "window 3 shows its sliver at the right edge with the display below: {on_screen:?}"
    );
    assert_eq!(
        (slice.on_screen, slice.in_window, slice.side),
        (on_screen, in_window, Side::East)
    );
    assert_eq!(showing(beside.world()), vec![(3, slice)]);
}

#[test]
fn a_hidden_stack_shows_one_slice_per_window_with_its_gaps() {
    let mut beside = stacked_first(&[LEFT], 6);
    let mut below = stacked_first(&[BELOW], 6);

    let slices = showing(beside.world());
    assert_eq!(
        slices.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    let twins = [0, 1, 2].map(|id| sliver(&mut below, id).0);
    assert_eq!(
        slices
            .iter()
            .map(|(_, slice)| slice.on_screen)
            .collect::<Vec<_>>(),
        twins.to_vec(),
        "each slice is the sliver its window shows with the display below"
    );
    for pair in slices.windows(2) {
        let (above, under) = (pair[0].1.on_screen, pair[1].1.on_screen);
        assert_eq!(
            under.min.y - above.max.y,
            2 * PAD,
            "the stack's gap: {above:?} {under:?}"
        );
    }
    assert!(slices.iter().all(|(_, slice)| slice.side == Side::West));
}

#[test]
fn only_the_hidden_column_nearest_a_shared_edge_shows() {
    let mut harness = focused(&[RIGHT], 5, None);

    assert!(slice_of(harness.world(), 3).is_some());
    assert!(
        slice_of(harness.world(), 4).is_some(),
        "window 4 also tucks off the display beside"
    );
    assert_eq!(showing_ids(harness.world()), vec![3]);
}

#[test]
fn a_display_between_two_others_shows_one_column_per_side() {
    let mut harness = focused(&[LEFT, RIGHT], 7, Some(Direction::Nth(4)));

    let slices = showing(harness.world());
    let sides = slices
        .iter()
        .map(|(id, slice)| (*id, slice.side))
        .collect::<Vec<_>>();
    assert_eq!(sides, vec![(1, Side::West), (5, Side::East)]);
    assert!(
        slice_of(harness.world(), 0).is_some() && slice_of(harness.world(), 6).is_some(),
        "farther columns tuck too"
    );
}

#[test]
fn a_free_edge_shows_no_slice() {
    let mut harness = focused(&[RIGHT], 4, Some(Direction::Last));

    assert!(
        os_frame(&harness, 0).max.x <= OWN.min.x + 10,
        "window 0 is hidden past the free left edge"
    );
    assert!(slice_of(harness.world(), 0).is_none());
    assert_eq!(showing(harness.world()), vec![]);
}

#[test]
fn stacked_displays_show_no_slice() {
    let mut harness = focused(&[BELOW], 4, None);

    assert!(
        os_frame(&harness, 3).min.x >= OWN.max.x - 10,
        "window 3 is hidden past the right edge"
    );
    assert!(slice_of(harness.world(), 3).is_none());
    assert_eq!(showing(harness.world()), vec![]);
}

#[test]
fn a_strip_the_display_stopped_showing_shows_no_slice() {
    let mut harness = focused(&[RIGHT], 4, None);
    assert_eq!(showing_ids(harness.world()), vec![3]);

    harness.run(vec![command(Operation::VirtualNumber(1))]);
    harness.advance(SETTLE);
    assert_eq!(showing(harness.world()), vec![]);

    harness.run(vec![command(Operation::VirtualNumber(0))]);
    harness.advance(SETTLE);
    assert_eq!(showing_ids(harness.world()), vec![3]);
}

#[test]
fn scrolling_a_hidden_column_into_view_removes_its_slice() {
    let mut harness = focused(&[RIGHT], 4, None);
    assert_eq!(showing_ids(harness.world()), vec![3]);

    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);

    assert!(
        !os_frame(&harness, 3).intersect(OWN).is_empty() && slice_of(harness.world(), 3).is_none(),
        "window 3 is in view: {:?}",
        os_frame(&harness, 3)
    );
    assert_eq!(showing(harness.world()), vec![]);
}

#[test]
fn closing_a_hidden_stack_member_resizes_the_slices_of_the_rest() {
    let mut beside = stacked_first(&[LEFT], 6);
    let mut below = stacked_first(&[BELOW], 6);

    for harness in [&mut beside, &mut below] {
        harness.mock_state.os_close_window(2);
        harness.run(vec![]);
        harness.advance(SETTLE);
    }

    let slices = showing(beside.world());
    assert_eq!(
        slices.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![0, 1]
    );
    let twins = [0, 1].map(|id| sliver(&mut below, id));
    assert_eq!(
        slices
            .iter()
            .map(|(_, slice)| (slice.on_screen, slice.in_window))
            .collect::<Vec<_>>(),
        twins.to_vec(),
        "each slice is the sliver its window shows with the display below"
    );
}

#[test]
fn an_overflowing_hidden_stack_shows_what_each_window_shows_of_itself() {
    let mut beside = stacked_first_refusing(&[LEFT], 300);
    let mut below = stacked_first_refusing(&[BELOW], 300);

    let twins = [0, 1, 2].map(|id| sliver(&mut below, id));
    for pair in twins.windows(2) {
        assert!(
            !pair[0].0.intersect(pair[1].0).is_empty(),
            "the stack overlaps with the display below: {twins:?}"
        );
    }
    let union = twins
        .iter()
        .fold(twins[0].0, |union, (on_screen, _)| union.union(*on_screen));

    let slices = showing(beside.world());
    assert!(!slices.is_empty());
    for (index, (id, slice)) in slices.iter().enumerate() {
        let (twin, twin_in_window) = twins[usize::try_from(*id).expect("a stacked window")];
        assert_eq!(
            slice.in_window.min - slice.on_screen.min,
            twin_in_window.min - twin.min,
            "window {id} crops its slice where its sliver is"
        );
        assert_eq!(slice.in_window.size(), slice.on_screen.size());
        assert_eq!(slice.on_screen.intersect(twin), slice.on_screen);
        for (other, later) in &slices[index + 1..] {
            assert!(
                slice.on_screen.intersect(later.on_screen).is_empty(),
                "windows {id} and {other} overlap: {slices:?}"
            );
        }
    }
    let area = |rect: IRect| rect.width() * rect.height();
    assert_eq!(
        slices
            .iter()
            .map(|(_, slice)| area(slice.on_screen))
            .sum::<i32>(),
        area(union),
        "the slices cover the stack's sliver: {slices:?} {union:?}"
    );
}

#[test]
fn a_hidden_tabbed_stack_shows_only_the_tab_it_shows_last() {
    let mut beside = tabbed_first(&[LEFT]);
    let mut below = tabbed_first(&[BELOW]);

    let slices = showing(beside.world());
    assert_eq!(
        slices.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        (slices[0].1.on_screen, slices[0].1.in_window),
        sliver(&mut below, 1)
    );
}

#[test]
fn switching_the_front_tab_of_a_hidden_tab_group_shows_the_new_front_tab() {
    let mut harness = focused(&[RIGHT], 4, None);
    let tab = harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        4,
        os_frame(&harness, 3),
    );
    harness.mock_state.update_window(4, |window| {
        window.horizontal_padding = PAD;
        window.vertical_padding = PAD;
    });
    harness.mock_state.window_visible(3, false);
    harness.world().trigger(SpawnWindowTrigger(vec![tab]));
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Focus(Direction::First))]);
    harness.advance(SETTLE);

    let (front, back) = match showing_ids(harness.world()).as_slice() {
        [4] => (4, 3),
        [3] => (3, 4),
        ids => panic!("one tab of the group shows: {ids:?}"),
    };
    let geometry = |world: &mut World| [slice_of(world, 3), slice_of(world, 4)];
    let before = geometry(harness.world());
    assert!(before[0].is_some() && before[0] == before[1], "{before:?}");
    harness
        .world()
        .run_system_cached(showing_may_change)
        .expect("the condition runs");

    let back_entity = find_window_entity(back, harness.world());
    let world = harness.world();
    let mut strips = world.query::<&mut LayoutStrip>();
    for mut strip in strips.iter_mut(world) {
        if let Ok(index) = strip.index_of(back_entity) {
            let column = strip.get_column_mut(index).expect("the tab group");
            assert!(
                column.move_to_front(back_entity),
                "{back} comes to the front"
            );
        }
    }
    harness.advance(SETTLE);

    assert_eq!(
        geometry(harness.world()),
        before,
        "the tabs keep their slices"
    );
    assert!(
        harness
            .world()
            .run_system_cached(showing_may_change)
            .expect("the condition runs"),
        "the showing set is due for a refresh"
    );
    assert_eq!(
        showing_ids(harness.world()),
        vec![back],
        "{front} went back"
    );
}
