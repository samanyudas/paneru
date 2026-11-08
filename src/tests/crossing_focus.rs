//! A plain focus step that crosses to a touching display lands on the
//! destination column nearest the crossed edge, so the opposite step returns.
//! The display focus commands keep landing on the last-focused window.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::focus::FocusHistory;
use crate::events::Event;
use crate::manager::Window;
use crate::platform::WinID;

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

const A: WinID = 0;
const B: WinID = 1;
const C: WinID = 2;
const X: WinID = 100;
const Y: WinID = 101;
const Z: WinID = 102;
/// A window that joins the destination's edge column as a stack.
const W: WinID = 103;

/// Wide enough that showing Y scrolls X partly off the destination display.
const DESTINATION_COLUMN_WIDTH: i32 = 1100;

fn plain(operation: Operation, repeat: bool) -> Event {
    Event::Command {
        command: Command::Window(operation).for_keypress(repeat),
    }
}

/// The external display, touching the built-in one on `side`.
fn destination_bounds(side: Side) -> IRect {
    let extent = IVec2::new(EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT);
    let min = match side {
        Side::West => IVec2::new(-extent.x, 0),
        Side::East => IVec2::new(TEST_DISPLAY_WIDTH, 0),
    };
    IRect::from_corners(min, min + extent)
}

/// Lets macOS focus window `id` on `display`, as a click does.
fn click(harness: &mut TestHarness, id: WinID, display: u32) {
    harness.mock_state.focus_window(id);
    harness.mock_state.os_activate_display(display);
    harness.advance(SETTLE);
    assert_eq!(focused(harness), id);
}

/// Lets macOS make `display` active, as it does once the focused window is
/// there.
fn arrive(harness: &mut TestHarness, display: u32) {
    harness.mock_state.os_activate_display(display);
    harness.advance(SETTLE);
}

/// Columns A, B, C on the built-in display and X, Y, Z on a display touching
/// it on `side`, with Y last focused there and `start` focused here.
fn beside(side: Side, start: WinID) -> TestHarness {
    beside_with(side, start, &[X, Y, Z], |_| {})
}

/// Columns A, B, C on the built-in display and the columns `slots` on a
/// display touching it on `side`, which `arrange` changes while that display
/// is active. Then Y gets focus there and `start` here.
fn beside_with(
    side: Side,
    start: WinID,
    slots: &[WinID],
    arrange: impl FnOnce(&mut TestHarness),
) -> TestHarness {
    let destination = destination_bounds(side);
    let mut harness = TestHarness::new().with_windows(3).with_display(
        EXT_DISPLAY_ID,
        destination,
        vec![EXT_WORKSPACE_ID],
    );
    for (slot, &id) in (0..).zip(slots) {
        let min = destination.min + IVec2::new(slot * DESTINATION_COLUMN_WIDTH, 100);
        harness = harness.with_window(id, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
            window.frame =
                IRect::from_corners(min, min + IVec2::new(DESTINATION_COLUMN_WIDTH, 800));
        });
    }
    harness.run(vec![Event::MenuOpened { window_id: A }]);
    harness.advance(SETTLE);
    arrange(&mut harness);
    click(&mut harness, Y, EXT_DISPLAY_ID);
    click(&mut harness, start, TEST_DISPLAY_ID);
    let y = find_window_entity(Y, harness.world());
    assert_eq!(
        harness
            .world()
            .resource::<FocusHistory>()
            .last_managed(EXT_WORKSPACE_ID),
        Some(y),
        "Y is the destination's last-focused window"
    );
    harness
}

/// The one focused window.
fn focused(harness: &mut TestHarness) -> WinID {
    let world = harness.world();
    let ids = world
        .query_filtered::<&Window, With<crate::ecs::FocusedMarker>>()
        .iter(world)
        .map(|window| window.id())
        .collect::<Vec<_>>();
    let [id] = ids[..] else {
        panic!("focused windows {ids:?}");
    };
    id
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// Crossing right from C focuses X, and the step back left focuses C.
#[test]
fn crossing_right_focuses_the_first_column_and_the_step_back_returns() {
    let mut harness = beside(Side::East, C);
    harness.run(vec![plain(Operation::Focus(Direction::East), false)]);
    assert_eq!(focused(&mut harness), X);
    arrive(&mut harness, EXT_DISPLAY_ID);
    let x = frame(harness.world(), X);
    assert!(
        destination_bounds(Side::East).contains(x.min)
            && destination_bounds(Side::East).contains(x.max - 1),
        "X {x:?} is in view"
    );

    harness.run(vec![plain(Operation::Focus(Direction::West), false)]);
    assert_eq!(focused(&mut harness), C);
}

/// Crossing left from A focuses Z, the destination's last column, and the
/// step back right focuses A.
#[test]
fn crossing_left_focuses_the_last_column_and_the_step_back_returns() {
    let mut harness = beside(Side::West, A);
    harness.run(vec![plain(Operation::Focus(Direction::West), false)]);
    assert_eq!(focused(&mut harness), Z);
    arrive(&mut harness, EXT_DISPLAY_ID);
    let z = frame(harness.world(), Z);
    assert!(
        destination_bounds(Side::West).contains(z.min)
            && destination_bounds(Side::West).contains(z.max - 1),
        "Z {z:?} is in view"
    );

    harness.run(vec![plain(Operation::Focus(Direction::East), false)]);
    assert_eq!(focused(&mut harness), A);
}

/// The display focus commands still land on the last-focused window.
#[test]
fn display_focus_still_lands_on_the_last_focused_window() {
    for (side, start) in [(Side::East, C), (Side::West, A)] {
        let mut harness = beside(side, start);
        harness.run(vec![plain(
            Operation::FocusDisplay(side.direction()),
            false,
        )]);
        assert_eq!(focused(&mut harness), Y);
    }
}

/// A held key steps within the strip and stops at its end. Its repeats never
/// cross, whether the hold starts inside the strip or at its end.
#[test]
fn a_held_focus_key_never_crosses() {
    for (side, inside, end) in [(Side::East, A, C), (Side::West, C, A)] {
        for start in [inside, end] {
            let mut harness = beside(side, start);
            harness.run(vec![plain(Operation::Focus(side.direction()), true)]);
            for _ in 0..8 {
                harness.run(vec![plain(Operation::Focus(side.direction()), true)]);
            }
            assert_eq!(focused(&mut harness), end);
        }
        let mut harness = beside(side, inside);
        harness.run(vec![plain(Operation::Focus(side.direction()), false)]);
        assert_eq!(focused(&mut harness), B);
        for _ in 0..8 {
            harness.run(vec![plain(Operation::Focus(side.direction()), true)]);
        }
        assert_eq!(focused(&mut harness), end);
    }
}

/// With `mouse_follows_focus`, the pointer follows a crossing to the window
/// it lands on.
#[test]
fn the_pointer_follows_a_crossing_to_the_window_it_lands_on() {
    let config: Config = (
        MainOptions {
            mouse_follows_focus: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = beside(Side::East, C).with_config(config);
    harness.run(vec![plain(Operation::Focus(Direction::East), false)]);
    arrive(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(focused(&mut harness), X);
    let pointer = harness.mock_state.cursor_position();
    let x = frame(harness.world(), X);
    assert!(x.contains(pointer), "pointer {pointer:?} is on X {x:?}");
}

/// The window IDs of each column of the destination strip, each column top
/// to bottom.
fn destination_columns(harness: &mut TestHarness) -> Vec<Vec<WinID>> {
    let world = harness.world();
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query::<&crate::ecs::layout::LayoutStrip>()
        .iter(world)
        .find(|strip| strip.id() == EXT_WORKSPACE_ID)
        .map(|strip| {
            strip
                .columns()
                .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
                .collect()
        })
        .unwrap_or_default()
}

/// The destination's edge column `edge` holds W stacked under it, and
/// `last` had focus there before Y did.
fn stacked_edge(side: Side, start: WinID, edge: WinID, last: WinID) -> TestHarness {
    let mut harness = beside_with(side, start, &[X, Y, Z, W], |harness| {
        click(harness, W, EXT_DISPLAY_ID);
        let joins = match side {
            Side::East => vec![
                Operation::Swap(Direction::First),
                Operation::Stack(Side::East),
            ],
            Side::West => vec![Operation::Stack(Side::West)],
        };
        for operation in joins {
            harness.run(vec![Event::Command {
                command: Command::Window(operation),
            }]);
            harness.advance(SETTLE);
        }
        click(harness, last, EXT_DISPLAY_ID);
    });
    let expected = match side {
        Side::East => vec![vec![edge, W], vec![Y], vec![Z]],
        Side::West => vec![vec![X], vec![Y], vec![edge, W]],
    };
    assert_eq!(destination_columns(&mut harness), expected);
    harness
}

/// Crossing into a stacked edge column focuses the window that last had focus
/// in that stack, whether it is the stack's top or a lower window, and the
/// step back returns.
#[test]
fn crossing_into_a_stacked_edge_column_focuses_its_last_focused_window() {
    for (side, start, edge, last) in [(Side::East, C, X, W), (Side::West, A, Z, Z)] {
        let mut harness = stacked_edge(side, start, edge, last);
        harness.run(vec![plain(Operation::Focus(side.direction()), false)]);
        assert_eq!(focused(&mut harness), last, "{side:?}");
        arrive(&mut harness, EXT_DISPLAY_ID);
        let back = match side {
            Side::East => Direction::West,
            Side::West => Direction::East,
        };
        harness.run(vec![plain(Operation::Focus(back), false)]);
        assert_eq!(focused(&mut harness), start, "{side:?} back");
    }
}
