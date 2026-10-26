//! Display moves take the focused window's whole column, and the edge
//! commands reach a strip's end before they cross to another display.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, MoveFocus, Operation, Side};
use crate::ecs::SelectedVirtualMarker;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::{Origin, Window};
use crate::platform::{WinID, WorkspaceId};
use crate::{assert_focused, assert_window_size};

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

/// The built-in display spans `(0, 0)` to this corner.
const BUILT_IN: IVec2 = IVec2::new(TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);

/// The external display, placed `direction` of the built-in one.
fn external_bounds(direction: &Direction) -> IRect {
    let size = IVec2::new(EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT);
    let min = match direction {
        Direction::North => IVec2::new(0, -size.y),
        Direction::South => IVec2::new(0, BUILT_IN.y),
        Direction::West => IVec2::new(-size.x, 0),
        Direction::East => IVec2::new(BUILT_IN.x, 0),
        _ => unreachable!("only a compass direction places a display"),
    };
    IRect::from_corners(min, min + size)
}

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// Windows 0 to 3 on the built-in display, stacked as `[0, 1, 2], [3]` with
/// window 1 focused, and an external display at `ext` holding one column for
/// each of `ext_windows`.
fn stacked_beside(ext: IRect, ext_windows: &[WinID]) -> TestHarness {
    let mut harness = TestHarness::new().with_windows(4).with_display(
        EXT_DISPLAY_ID,
        ext,
        vec![EXT_WORKSPACE_ID],
    );
    for (slot, id) in (0..).zip(ext_windows) {
        let min = ext.min + IVec2::new(slot * TEST_WINDOW_WIDTH, 100);
        harness = harness.with_window(*id, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
            window.frame = IRect::from_corners(min, min + IVec2::new(TEST_WINDOW_WIDTH, 800));
        });
    }
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
    harness.run(vec![command(Operation::Focus(Direction::North))]);
    assert_eq!(
        columns(harness.world(), TEST_WORKSPACE_ID),
        [vec![0, 1, 2], vec![3]]
    );
    assert_focused!(harness.world(), 1);
    harness
}

/// The window IDs of each column of the strip shown for `workspace`, left to
/// right, each column top to bottom.
fn columns(world: &mut World, workspace: WorkspaceId) -> Vec<Vec<WinID>> {
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query_filtered::<&LayoutStrip, With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|strip| strip.id() == workspace)
        .map(|strip| {
            strip
                .columns()
                .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
                .collect()
        })
        .unwrap_or_default()
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// Lets windows `ids`, which moved to the external display, settle there, as
/// macOS reports them in its space.
fn settle_on_external(harness: &mut TestHarness, ids: &[WinID]) {
    for id in ids {
        harness
            .mock_state
            .update_window(*id, |window| window.workspace_id = EXT_WORKSPACE_ID);
    }
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
}

/// Every display move takes the whole stack, in order, and keeps the focus
/// on the window that had it, whichever way the display lies.
#[test]
fn every_display_move_takes_the_whole_column() {
    let moves = [
        (Direction::North, Operation::MoveToDisplay(Direction::North)),
        (Direction::South, Operation::MoveToDisplay(Direction::South)),
        (Direction::West, Operation::MoveToDisplay(Direction::West)),
        (Direction::East, Operation::MoveToDisplay(Direction::East)),
        (
            Direction::North,
            Operation::ToNextDisplay(MoveFocus::Follow),
        ),
    ];
    for (side, operation) in moves {
        let mut harness = stacked_beside(external_bounds(&side), &[100]);
        harness.run(vec![command(operation.clone())]);
        let world = harness.world();
        assert_eq!(
            columns(world, EXT_WORKSPACE_ID),
            [vec![100], vec![0, 1, 2]],
            "{operation:?}"
        );
        assert_eq!(
            columns(world, TEST_WORKSPACE_ID),
            [vec![3]],
            "{operation:?}"
        );
        assert_focused!(world, 1);
    }
}

/// The stack arrives as one column of the destination's height, split evenly.
#[test]
fn a_moved_stack_shares_the_destination_height() {
    let ext = external_bounds(&Direction::North);
    let mut harness = stacked_beside(ext, &[100]);
    harness.run(vec![command(Operation::MoveToDisplay(Direction::North))]);
    settle_on_external(&mut harness, &[0, 1, 2]);

    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![0, 1, 2]]);
    let frames = [0, 1, 2].map(|id| frame(world, id));
    for pair in frames.windows(2) {
        assert_eq!(pair[0].min.x, pair[1].min.x, "one column: {frames:?}");
        assert_eq!(
            pair[0].max.y, pair[1].min.y,
            "no overlap or gap: {frames:?}"
        );
    }
    for frame in frames {
        assert!(
            ext.contains(frame.min) && ext.contains(frame.max - 1),
            "{frame:?} lies on the destination {ext:?}"
        );
        assert!(
            (frame.height() - frames[0].height()).abs() <= 1,
            "even split: {frames:?}"
        );
    }
    assert_eq!(
        frames[2].max.y - frames[0].min.y,
        EXT_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT
    );
    assert_focused!(world, 1);
}

/// Sending the column away keeps the focus on this display, on the column
/// it leaves behind.
#[test]
fn sending_a_column_focuses_the_one_left_behind() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100]);
    harness.run(vec![command(Operation::ToNextDisplay(MoveFocus::Stay))]);
    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![0, 1, 2]]);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![3]]);
    assert_focused!(world, 3);
}

/// With destination columns X, Y, Z and Y active, the column lands between
/// Y and Z.
#[test]
fn a_column_lands_after_the_destination_active_column() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100, 101, 102]);
    let y = find_window_entity(101, harness.world());
    harness
        .world()
        .resource_mut::<FocusHistory>()
        .record(EXT_WORKSPACE_ID, y, None);
    harness.run(vec![command(Operation::MoveToDisplay(Direction::North))]);
    let world = harness.world();
    assert_eq!(
        columns(world, EXT_WORKSPACE_ID),
        [vec![100], vec![101], vec![0, 1, 2], vec![102]]
    );
    assert_focused!(world, 1);
}

/// An empty destination takes the column as its first column.
#[test]
fn an_empty_destination_takes_the_column_first() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[]);
    harness.run(vec![command(Operation::MoveToDisplay(Direction::North))]);
    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![0, 1, 2]]);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![3]]);
    assert_focused!(world, 1);
}

/// With no display in that direction, nothing moves.
#[test]
fn a_move_toward_no_display_changes_nothing() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100]);
    harness.run(vec![
        command(Operation::MoveToDisplay(Direction::West)),
        command(Operation::MoveToDisplay(Direction::South)),
    ]);
    let world = harness.world();
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2], vec![3]]);
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100]]);
    assert_focused!(world, 1);
}

/// A maximized column stays maximized on the display it moves to.
#[test]
fn a_maximized_column_stays_maximized_across_displays() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100]);
    harness.run(vec![
        command(Operation::Focus(Direction::East)),
        command(Operation::Maximize),
    ]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::MoveToDisplay(Direction::North))]);
    settle_on_external(&mut harness, &[3]);

    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![3]]);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2]]);
    let maximized = find_window_entity(3, world);
    let still_maximized = world
        .query::<&LayoutStrip>()
        .iter(world)
        .any(|strip| strip.maximized(maximized).is_some());
    assert!(still_maximized, "window 3 stays maximized");
    assert_focused!(world, 3);
}

/// A destination too short for the stack at the stack limit still takes it
/// whole: the layout overlaps the windows below each top bar, as in any stack
/// that does not fit, instead of splitting it.
#[test]
fn a_stack_too_tall_for_the_destination_arrives_whole() {
    let ext = IRect::new(0, -500, EXT_DISPLAY_WIDTH, 0);
    let mut harness = stacked_beside(ext, &[100]);
    let limit = crate::ecs::layout::effective_stack_limit(
        harness.world().resource::<crate::config::Config>(),
        ext.height() - TEST_MENUBAR_HEIGHT,
    );
    assert!(limit < 3, "the destination holds fewer than three: {limit}");

    harness.run(vec![command(Operation::MoveToDisplay(Direction::North))]);
    settle_on_external(&mut harness, &[0, 1, 2]);

    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![0, 1, 2]]);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![3]]);
    let frames = [0, 1, 2].map(|id| frame(world, id));
    for pair in frames.windows(2) {
        assert_eq!(pair[0].min.x, pair[1].min.x, "one column: {frames:?}");
        assert!(pair[0].min.y < pair[1].min.y, "in order: {frames:?}");
    }
    for frame in frames {
        assert!(
            frame.min.y >= ext.min.y && frame.max.y <= ext.max.y,
            "{frame:?} lies on the destination {ext:?}"
        );
    }
    assert_focused!(world, 1);
}

/// Away from the first column, the west edge focus reaches it, on the
/// window that last had focus there. From there it crosses to the display
/// on the left, without moving a window or the pointer.
#[test]
fn edge_focus_reaches_the_first_column_then_the_display_beside() {
    let mut harness = stacked_beside(external_bounds(&Direction::West), &[100]);
    harness.run(vec![command(Operation::Focus(Direction::East))]);
    assert_focused!(harness.world(), 3);

    harness.run(vec![command(Operation::FocusEdge(Side::West))]);
    assert_focused!(harness.world(), 1);

    harness.run(vec![command(Operation::FocusEdge(Side::West))]);
    let world = harness.world();
    assert_focused!(world, 100);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2], vec![3]]);
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100]]);
    assert_eq!(harness.mock_state.cursor_position(), Origin::ZERO);
    assert_eq!(harness.mock_state.focused_desktop(), None);
}

/// The east edge focus mirrors the west one with the last column.
#[test]
fn edge_focus_reaches_the_last_column_then_the_display_beside() {
    let mut harness = stacked_beside(external_bounds(&Direction::East), &[100]);
    harness.run(vec![command(Operation::FocusEdge(Side::East))]);
    assert_focused!(harness.world(), 3);
    harness.run(vec![command(Operation::FocusEdge(Side::East))]);
    assert_focused!(harness.world(), 100);
    assert_eq!(harness.mock_state.cursor_position(), Origin::ZERO);
}

/// At the first column with no display on the left, the focus stays.
#[test]
fn edge_focus_without_a_display_beside_stays() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100]);
    harness.run(vec![command(Operation::FocusEdge(Side::West))]);
    assert_focused!(harness.world(), 1);
    assert_eq!(harness.mock_state.focused_desktop(), None);
}

/// Away from the last column, the east edge move takes the whole column
/// there, and the focus stays on its window.
#[test]
fn edge_move_takes_the_whole_column_to_the_strip_end() {
    let mut harness = stacked_beside(external_bounds(&Direction::East), &[100]);
    harness.run(vec![command(Operation::MoveEdge(Side::East))]);
    let world = harness.world();
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![3], vec![0, 1, 2]]);
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100]]);
    assert_focused!(world, 1);

    harness.run(vec![command(Operation::MoveEdge(Side::West))]);
    let world = harness.world();
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2], vec![3]]);
    assert_focused!(world, 1);
}

/// At the strip's end, one edge move takes the whole column to the display
/// on that side.
#[test]
fn edge_move_at_the_strip_end_crosses_to_the_display_beside() {
    let mut harness = stacked_beside(external_bounds(&Direction::West), &[100]);
    harness.run(vec![command(Operation::MoveEdge(Side::West))]);
    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![0, 1, 2]]);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![3]]);
    assert_focused!(world, 1);
}

/// At the strip's end with no display on that side, the column stays.
#[test]
fn edge_move_without_a_display_beside_stays_at_the_end() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100]);
    harness.run(vec![command(Operation::MoveEdge(Side::West))]);
    let world = harness.world();
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2], vec![3]]);
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100]]);
    assert_focused!(world, 1);
}

/// A single window follows the same two stages as a stack.
#[test]
fn edge_move_of_a_single_window_reaches_the_end_then_crosses() {
    let mut harness = stacked_beside(external_bounds(&Direction::West), &[100]);
    harness.run(vec![
        command(Operation::Focus(Direction::East)),
        command(Operation::MoveEdge(Side::West)),
    ]);
    assert_eq!(
        columns(harness.world(), TEST_WORKSPACE_ID),
        [vec![3], vec![0, 1, 2]]
    );
    harness.run(vec![command(Operation::MoveEdge(Side::West))]);
    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![3]]);
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2]]);
    assert_focused!(world, 3);
}

/// A floating window has no column, so the edge move leaves it and the
/// strip alone.
#[test]
fn edge_move_of_a_floating_window_does_nothing() {
    let mut harness = stacked_beside(external_bounds(&Direction::West), &[100]);
    harness.run(vec![
        command(Operation::Focus(Direction::East)),
        command(Operation::Manage),
    ]);
    harness.advance(SETTLE);
    assert_eq!(columns(harness.world(), TEST_WORKSPACE_ID), [vec![0, 1, 2]]);
    let before = frame(harness.world(), 3);
    harness.run(vec![command(Operation::MoveEdge(Side::West))]);
    let world = harness.world();
    assert_eq!(columns(world, TEST_WORKSPACE_ID), [vec![0, 1, 2]]);
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100]]);
    assert_window_size!(world, 3, before.width(), before.height());
    assert_eq!(frame(world, 3), before);
}

/// From a floating window, the edge focus goes to the strip's end column.
#[test]
fn edge_focus_from_a_floating_window_focuses_the_end_column() {
    let mut harness = stacked_beside(external_bounds(&Direction::West), &[100]);
    harness.run(vec![
        command(Operation::Focus(Direction::East)),
        command(Operation::Manage),
    ]);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 3);
    harness.run(vec![command(Operation::FocusEdge(Side::West))]);
    assert_focused!(harness.world(), 1);
}

/// A stack shown as tabs stays tabbed on the display it moves to.
#[test]
fn a_tabbed_stack_stays_tabbed_across_displays() {
    let mut harness = stacked_beside(external_bounds(&Direction::North), &[100]);
    harness.run(vec![command(Operation::ToggleTabbedDisplay)]);
    harness.run(vec![command(Operation::MoveToDisplay(Direction::North))]);
    let world = harness.world();
    assert_eq!(columns(world, EXT_WORKSPACE_ID), [vec![100], vec![0, 1, 2]]);
    let members = [0, 1, 2].map(|id| find_window_entity(id, world));
    let tabbed = world
        .query::<&LayoutStrip>()
        .iter(world)
        .find(|strip| strip.id() == EXT_WORKSPACE_ID)
        .is_some_and(|strip| {
            members
                .iter()
                .all(|member| strip.is_tabbed_display(*member))
        });
    assert!(tabbed, "every member stays tabbed");
    assert_focused!(world, 1);
}
