//! Sending a column to the next display visits the displays in screen order:
//! left to right by left edge, then top to bottom, wrapping from the last to
//! the first. Each arrangement runs with every assignment of display ids to
//! positions, so the order the engine stores its displays in cannot pass for
//! screen order.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Direction, MoveFocus, Operation};
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::platform::{WinID, WorkspaceId};

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

const fn rect(left: i32, top: i32, right: i32, bottom: i32) -> IRect {
    IRect {
        min: IVec2::new(left, top),
        max: IVec2::new(right, bottom),
    }
}

/// The window that travels from display to display.
const TRAVELLER: WinID = 0;

/// Three displays in screen order: one above the second, which shares its
/// left edge, and one to the right of the second.
const THREE: [IRect; 3] = [
    rect(0, -700, 1000, 0),
    rect(0, 0, 1024, 768),
    rect(1024, 0, 2024, 700),
];

/// Four displays in screen order. The first touches the second only at a
/// corner, the third sits below the second, and the fourth to its right.
const FOUR: [IRect; 4] = [
    rect(-1000, -700, 0, 0),
    rect(0, 0, 1024, 768),
    rect(0, 768, 1000, 1468),
    rect(1024, 0, 2024, 700),
];

fn workspace(display: u32) -> WorkspaceId {
    if display == TEST_DISPLAY_ID {
        TEST_WORKSPACE_ID
    } else {
        u64::from(display) * 10
    }
}

fn resident(display: u32) -> WinID {
    100 + WinID::try_from(display).expect("a small display id")
}

use super::window_command as command;

/// Every order of the display ids `1..=count`.
fn orders(count: u32) -> Vec<Vec<u32>> {
    if count == 0 {
        return vec![Vec::new()];
    }
    orders(count - 1)
        .into_iter()
        .flat_map(|order| {
            (0..count as usize).map(move |slot| {
                let mut order = order.clone();
                order.insert(slot, count);
                order
            })
        })
        .collect()
}

/// Displays at `slots`, with `ids[slot]` the id of the display at each slot.
/// Each display holds one window of its own, and the focused traveller
/// starts on display 1.
fn arranged(slots: &[IRect], ids: &[u32]) -> TestHarness {
    let inside = |bounds: IRect| {
        let min = bounds.min + IVec2::new(0, 100);
        IRect::from_corners(min, min + IVec2::new(TEST_WINDOW_WIDTH, 500))
    };
    let mut harness = TestHarness::new();
    for (&id, &bounds) in ids.iter().zip(slots) {
        if id == TEST_DISPLAY_ID {
            harness.mock_state.set_display_bounds(id, bounds);
        } else {
            harness = harness.with_display(id, bounds, vec![workspace(id)]);
        }
        harness = harness.with_workspace_window(resident(id), workspace(id), |window| {
            window.frame = inside(bounds);
        });
    }
    let home = slots[ids
        .iter()
        .position(|&id| id == TEST_DISPLAY_ID)
        .expect("display 1")];
    harness = harness.with_window(TRAVELLER, |window| window.frame = inside(home));
    harness.run(vec![Event::MenuOpened {
        window_id: TRAVELLER,
    }]);
    harness.advance(SETTLE);
    harness
}

/// The display whose strip holds window `id`.
fn display_of(harness: &mut TestHarness, ids: &[u32], id: WinID) -> u32 {
    let entity = find_window_entity(id, harness.world());
    let world = harness.world();
    let strips = world
        .query_filtered::<&LayoutStrip, With<crate::ecs::SelectedVirtualMarker>>()
        .iter(world)
        .filter(|strip| strip.contains(entity))
        .map(LayoutStrip::id)
        .collect::<Vec<_>>();
    let [space] = strips[..] else {
        panic!("window {id} is in strips {strips:?}");
    };
    ids.iter()
        .copied()
        .find(|&display| workspace(display) == space)
        .unwrap_or_else(|| panic!("no display shows space {space}"))
}

/// Lets macOS report `display` active, as it does once the focused window
/// is there.
fn activate(harness: &mut TestHarness, display: u32) {
    harness.mock_state.os_activate_display(display);
    harness.advance(SETTLE);
}

/// Sends the traveller to the next display `presses` times, and returns the
/// display it is on after each press.
fn send_around(harness: &mut TestHarness, ids: &[u32], presses: usize) -> Vec<u32> {
    (0..presses)
        .map(|_| {
            harness.run(vec![command(Operation::ToNextDisplay(MoveFocus::Follow))]);
            let display = display_of(harness, ids, TRAVELLER);
            harness
                .mock_state
                .update_window(TRAVELLER, |window| window.workspace_id = workspace(display));
            activate(harness, display);
            display
        })
        .collect()
}

/// The displays after display 1 in screen order, wrapping round to display 1.
fn screen_order_from_display_1(ids: &[u32]) -> Vec<u32> {
    let home = ids
        .iter()
        .position(|&id| id == TEST_DISPLAY_ID)
        .expect("display 1");
    (1..=ids.len())
        .map(|step| ids[(home + step) % ids.len()])
        .collect()
}

#[test]
fn three_displays_are_visited_once_each_in_screen_order() {
    for ids in orders(3) {
        let mut harness = arranged(&THREE, &ids);
        assert_eq!(
            send_around(&mut harness, &ids, 3),
            screen_order_from_display_1(&ids),
            "displays {ids:?} at {THREE:?}"
        );
    }
}

#[test]
fn four_displays_with_a_corner_only_one_are_each_visited_once_per_cycle() {
    for ids in orders(4) {
        let mut harness = arranged(&FOUR, &ids);
        assert_eq!(
            send_around(&mut harness, &ids, 4),
            screen_order_from_display_1(&ids),
            "displays {ids:?} at {FOUR:?}"
        );
    }
}

fn focused(harness: &mut TestHarness) -> WinID {
    let world = harness.world();
    world
        .query_filtered::<&crate::manager::Window, With<crate::ecs::FocusedMarker>>()
        .iter(world)
        .map(|window| window.id())
        .next()
        .expect("a focused window")
}

/// Focusing the other displays first with the directional focus commands, then
/// clicking back on the traveller, leaves the order of the next display as it
/// was.
#[test]
fn focusing_displays_by_other_commands_keeps_the_screen_order() {
    let tours = [
        [
            Direction::East,
            Direction::West,
            Direction::North,
            Direction::South,
        ],
        [
            Direction::North,
            Direction::South,
            Direction::East,
            Direction::West,
        ],
        [
            Direction::West,
            Direction::North,
            Direction::East,
            Direction::South,
        ],
    ];
    for ids in orders(3) {
        let mut reached = std::collections::BTreeSet::new();
        for tour in &tours {
            let mut harness = arranged(&THREE, &ids);
            for direction in tour {
                harness.run(vec![command(Operation::FocusDisplay(direction.clone()))]);
                let window = focused(&mut harness);
                let display = display_of(&mut harness, &ids, window);
                activate(&mut harness, display);
                reached.insert(display);
            }
            harness.mock_state.focus_window(TRAVELLER);
            activate(&mut harness, TEST_DISPLAY_ID);
            assert_eq!(
                focused(&mut harness),
                TRAVELLER,
                "the click focuses the traveller"
            );
            assert_eq!(
                send_around(&mut harness, &ids, 3),
                screen_order_from_display_1(&ids),
                "displays {ids:?} after focusing {tour:?}"
            );
        }
        assert_eq!(reached.len(), 3, "the tours focus every display of {ids:?}");
    }
}

/// Two displays at the same origin, as mirrored displays are, follow in id
/// order, so each is still visited once per cycle whichever display the
/// cycle starts from.
#[test]
fn displays_at_the_same_origin_are_each_visited_once_per_cycle() {
    let slots = [
        rect(0, 0, 1024, 768),
        rect(0, 0, 1024, 768),
        rect(1024, 0, 2024, 700),
    ];
    for ids in orders(3) {
        let mut order = ids.clone();
        order.sort_by_key(|id| {
            let slot = ids.iter().position(|other| other == id).expect("slot");
            (slots[slot].min.x, slots[slot].min.y, *id)
        });
        let mut harness = arranged(&slots, &ids);
        assert_eq!(
            send_around(&mut harness, &ids, 3),
            screen_order_from_display_1(&order),
            "displays {ids:?} at {slots:?}"
        );
    }
}

/// Screen order follows each display's physical top edge, not the top of its
/// area below the menu bar: a display whose taller menu bar ends lower still
/// comes first when its top edge is higher.
#[test]
fn screen_order_ignores_menu_bar_heights() {
    let slots = [
        rect(0, 0, 1024, 768),
        rect(0, 10, 1024, 778),
        rect(1024, 0, 2024, 700),
    ];
    let menu_bars = [40, 20, 24];
    for ids in orders(3) {
        let mut harness = arranged(&slots, &ids);
        let world = harness.world();
        let mut displays = world.query::<&mut crate::manager::Display>();
        for mut display in displays.iter_mut(world) {
            let slot = ids.iter().position(|&id| id == display.id()).expect("slot");
            display.set_menubar_height_override(Some(menu_bars[slot]));
        }
        assert_eq!(
            send_around(&mut harness, &ids, 3),
            screen_order_from_display_1(&ids),
            "displays {ids:?} at {slots:?} with menu bars {menu_bars:?}"
        );
    }
}

/// With two displays the command switches between them, whether they sit
/// side by side or one above the other.
#[test]
fn two_displays_take_turns() {
    let arrangements = [
        [rect(0, 0, 1024, 768), rect(1024, 0, 2024, 700)],
        [rect(0, 0, 1024, 768), rect(100, 768, 1100, 1468)],
    ];
    for slots in arrangements {
        for ids in orders(2) {
            let mut harness = arranged(&slots, &ids);
            let other = ids
                .iter()
                .copied()
                .find(|&id| id != TEST_DISPLAY_ID)
                .expect("a second display");
            assert_eq!(
                send_around(&mut harness, &ids, 4),
                [other, TEST_DISPLAY_ID, other, TEST_DISPLAY_ID],
                "displays {ids:?} at {slots:?}"
            );
        }
    }
}

/// `mouse_to_next_display` recognizes a cursor on the menu bar via
/// `physical_bounds()` and steps through 3+ displays in screen order.
#[test]
fn mouse_to_next_display_follows_screen_order_and_handles_menubar_cursor() {
    use crate::commands::{Command, MouseMove};

    let ids = [3, 1, 2];
    let mut harness = arranged(&THREE, &ids);
    {
        let world = harness.world();
        let mut displays = world.query::<&mut crate::manager::Display>();
        for mut display in displays.iter_mut(world) {
            display.set_menubar_height_override(Some(30));
        }
    }

    // Place the cursor inside slot 0's menu bar (outside `bounds()`, inside `physical_bounds()`).
    harness
        .mock_state
        .set_cursor_position(THREE[0].min + IVec2::new(50, 5));

    let mut visited = Vec::new();
    for _ in 0..3 {
        harness.run(vec![Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        }]);
        harness.advance(SETTLE);
        let cursor = harness.mock_state.cursor_position();
        let slot = THREE
            .iter()
            .position(|bounds| bounds.contains(cursor))
            .unwrap_or_else(|| panic!("cursor {cursor:?} not inside any display"));
        visited.push(slot);
    }
    assert_eq!(visited, vec![1, 2, 0]);
}

/// Equal-width ties (such as a two-window stack) keep focusing the first
/// window on the target display, and an empty/off-screen strip warps the
/// cursor to the display center without focusing.
#[test]
fn mouse_to_next_display_keeps_first_window_on_tie_and_warps_empty_strip_to_center() {
    use crate::commands::{Command, MouseMove};

    let display_2_bounds = rect(1024, 0, 2024, 700);
    let mut harness = TestHarness::new()
        .with_windows(1)
        .with_display(2, display_2_bounds, vec![20])
        .with_workspace_window(10, 20, |window| {
            window.frame = IRect::new(1024, 40, 1524, 340);
        })
        .with_workspace_window(11, 20, |window| {
            window.frame = IRect::new(1024, 340, 1524, 640);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);

    // Stack window 11 into window 10's column on display 2 so the stack is [10, 11] with equal widths.
    let e11 = find_window_entity(11, harness.world());
    {
        let world = harness.world();
        let mut strips = world.query::<&mut LayoutStrip>();
        for mut strip in strips.iter_mut(world) {
            if strip.id() == 20 {
                strip
                    .stack(e11, crate::types::commands::Side::West)
                    .expect("stack e11 onto e10");
            }
        }
    }
    harness.advance(SETTLE);

    harness.mock_state.set_cursor_position(IVec2::new(100, 100));
    harness.run(vec![Event::Command {
        command: Command::Mouse(MouseMove::ToNextDisplay),
    }]);
    harness.advance(SETTLE);
    assert_eq!(
        harness.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(10),
        "equal-width stack should focus its first window"
    );

    // Move to a third display whose only window sits wholly off-screen: cursor
    // warps to the display center without focusing the off-screen window.
    let display_3_bounds = rect(2024, 0, 3024, 700);
    harness
        .mock_state
        .add_display(3, display_3_bounds, vec![30]);
    let offscreen =
        harness
            .mock_state
            .spawn_window(TEST_PROCESS_ID, 30, 30, IRect::new(5000, 40, 5500, 540));
    harness.run(vec![Event::DisplayAdded { display_id: 3 }]);
    harness
        .world()
        .trigger(crate::ecs::SpawnWindowTrigger(vec![offscreen]));
    harness.advance(SETTLE);
    // Move window 30 off-screen in mock_state and sync its cached ECS frame so
    // `other.bounds().intersect(frame)` is empty.
    harness.mock_state.update_window(30, |window| {
        window.frame = IRect::new(5000, 40, 5500, 540);
    });
    let e30 = find_window_entity(30, harness.world());
    if let Some(mut window) = harness.world().get_mut::<crate::manager::Window>(e30) {
        let _ = window.update_frame();
    }
    harness.run(vec![Event::Command {
        command: Command::Mouse(MouseMove::ToNextDisplay),
    }]);
    harness.advance(SETTLE);
    let cursor = harness.mock_state.cursor_position();
    assert!(
        display_3_bounds.contains(cursor),
        "cursor {cursor:?} should warp inside display {display_3_bounds:?}"
    );
    assert_eq!(
        harness.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(10),
        "off-screen strip should not change focus"
    );
}
