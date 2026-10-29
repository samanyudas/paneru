use bevy::math::{IRect, IVec2};
use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{ActiveWorkspaceMarker, SpawnWindowTrigger};
use crate::events::{Event, MouseButton};
use crate::manager::{Origin, Size, Window};
use crate::platform::{Modifiers, WinID};
use crate::{assert_focused, assert_window_at, assert_window_size};

use super::*;

/// The height every window of a lone column gets.
const FULL_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

fn cg(point: Origin) -> CGPoint {
    CGPoint::new(point.x.into(), point.y.into())
}

fn press(point: Origin, window: WinID) -> Event {
    Event::MouseDown {
        point: cg(point),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
        window_id: Some(window),
    }
}

fn drag_to(point: Origin) -> Event {
    Event::MouseDragged {
        point: cg(point),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    }
}

fn release(point: Origin) -> Event {
    Event::MouseUp {
        point: cg(point),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    }
}

fn settle() -> Event {
    Event::Command {
        command: Command::PrintState,
    }
}

fn window_command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

fn config(options: &str) -> Config {
    Config::try_from(format!("[options]\n{options}").as_str()).expect("config parses")
}

fn drag_enabled() -> Config {
    config("drag_to_reorder = true\n")
}

/// A harness whose window server sees the left button held down, so a
/// press lasts until its release arrives.
fn holding(harness: TestHarness) -> TestHarness {
    harness.mock_state.set_left_button_down(true);
    harness
}

/// After iteration `at`, the window server moves `window` by `by` and reports
/// it a frame after the next pointer event, as macOS does when the pointer
/// drags a window by its title bar.
fn window_follows(harness: TestHarness, at: usize, window: WinID, by: IVec2) -> TestHarness {
    harness.on_iteration(at, move |world, state| {
        state.os_move_window(window, frame(world, window).min + by);
    })
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// The window IDs of each column of the active strip, left to right, each
/// column top to bottom.
fn columns(world: &mut World) -> Vec<Vec<WinID>> {
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

/// Three columns of one window each, at x = 0, 400 and 800, the last one
/// partly off the right edge of the display.
fn assert_three_columns(world: &mut World) {
    assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
    for id in 0..3 {
        assert_window_at!(world, id, TEST_WINDOW_WIDTH * id, TEST_MENUBAR_HEIGHT);
    }
}

#[test]
fn a_window_dropped_on_the_middle_of_a_column_joins_it_below_the_pointer() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 2, to - from)
        .on_iteration(0, |world, _| assert_three_columns(world))
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 2], vec![1]]);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, FULL_HEIGHT / 2);
            assert_window_at!(world, 2, 0, TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, FULL_HEIGHT / 2);
            assert_window_at!(world, 1, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 2);
        })
        .run(commands);
}

/// Window 2 is 600 points wide, so its centre is 100 points right of the
/// pointer, over the middle of column 0.
#[test]
fn a_window_dropped_above_a_column_becomes_its_top_window_at_its_width() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(100, 100);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        settle(),
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 2, 2, to - from)
        .on_iteration(0, |world, state| {
            assert_three_columns(world);
            state.os_resize_window(2, Size::new(600, FULL_HEIGHT));
        })
        .on_iteration(1, |world, _| {
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
            assert_window_size!(world, 2, 600, FULL_HEIGHT);
        })
        .on_iteration(5, |world, _| {
            assert_eq!(columns(world), vec![vec![2, 0], vec![1]]);
            assert_window_at!(world, 2, 0, TEST_MENUBAR_HEIGHT);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, FULL_HEIGHT / 2);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, FULL_HEIGHT / 2);
        })
        .run(commands);
}

#[test]
fn a_window_dropped_on_the_outer_quarter_of_a_column_opens_a_new_column_beside_it() {
    let from = Origin::new(200, 30);
    let to = Origin::new(790, 300);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 0),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 0, to - from)
        .on_iteration(0, |world, _| assert_three_columns(world))
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![1], vec![0], vec![2]]);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, FULL_HEIGHT);
            assert_eq!(
                frame(world, 0).min.x - frame(world, 1).min.x,
                TEST_WINDOW_WIDTH,
                "window 0 is right of window 1"
            );
            assert_eq!(frame(world, 0).min.y, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

/// The window's centre, not the pointer, picks the column: a stacked window
/// grabbed near the right end of its title bar and dragged straight down
/// stays in its stack.
#[test]
fn a_drop_within_its_own_stack_reorders_it() {
    let from = Origin::new(390, 30);
    let to = Origin::new(390, 700);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::Focus(Direction::East)),
        window_command(Operation::Stack(true)),
        settle(),
        press(from, 0),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 5, 0, to - from)
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 0, TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2);
        })
        .on_iteration(8, |world, _| {
            assert_eq!(columns(world), vec![vec![1, 0], vec![2]]);
            assert_window_at!(world, 1, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, FULL_HEIGHT / 2);
        })
        .run(commands);
}

#[test]
fn a_stacked_window_dropped_between_columns_leaves_its_stack() {
    let from = Origin::new(200, 404);
    let to = Origin::new(410, 300);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::Focus(Direction::East)),
        window_command(Operation::Stack(true)),
        settle(),
        press(from, 1),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 5, 1, to - from)
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
            assert_window_at!(world, 1, 0, TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2);
        })
        .on_iteration(8, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, FULL_HEIGHT);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, FULL_HEIGHT);
        })
        .run(commands);
}

#[test]
fn a_drop_on_its_own_lone_column_sends_the_window_back_to_its_slot() {
    let from = Origin::new(600, 30);
    let to = Origin::new(740, 230);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 1),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 1, to - from)
        .on_iteration(2, |world, _| {
            assert_window_at!(world, 1, 540, 220);
        })
        .on_iteration(4, |world, _| {
            assert_three_columns(world);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, FULL_HEIGHT);
        })
        .run(commands);
}

/// On a display 500 points tall, 480 of them below the menu bar, a column
/// fits two windows at the 200 point minimum height. A third is refused, and
/// the window goes back to its slot.
#[test]
fn a_column_without_room_for_another_window_refuses_the_drop() {
    const DISPLAY_HEIGHT: i32 = 500;
    let from = Origin::new(600, 30);
    let to = Origin::new(200, 400);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::Focus(Direction::East)),
        window_command(Operation::Stack(true)),
        settle(),
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = TestHarness::new().with_config(drag_enabled());
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, DISPLAY_HEIGHT),
    );
    let harness = holding(harness.with_windows(3));
    window_follows(harness, 5, 2, to - from)
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
            assert_window_at!(world, 2, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(6, |world, _| {
            assert_window_at!(world, 2, 0, 390);
        })
        .on_iteration(8, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
            assert_window_at!(world, 2, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            assert_window_size!(
                world,
                2,
                TEST_WINDOW_WIDTH,
                DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT
            );
        })
        .run(commands);
}

/// A tabbed stack shows one window at a time over the whole column, so it
/// has room for a third window even where a split column fits only two.
#[test]
fn a_tabbed_stack_takes_a_window_a_split_column_has_no_room_for() {
    const DISPLAY_HEIGHT: i32 = 500;
    let from = Origin::new(600, 30);
    let to = Origin::new(200, 400);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::Focus(Direction::East)),
        window_command(Operation::Stack(true)),
        window_command(Operation::ToggleTabbedDisplay),
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = TestHarness::new().with_config(drag_enabled());
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, DISPLAY_HEIGHT),
    );
    let harness = holding(harness.with_windows(3));
    window_follows(harness, 5, 2, to - from)
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
            assert_window_at!(world, 2, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(8, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1, 2]]);
            for id in 0..3 {
                assert_window_at!(world, id, 0, TEST_MENUBAR_HEIGHT);
            }
        })
        .run(commands);
}

/// A move of 3 points is too small to tell a drag from a click, so the
/// release is a click, which puts the window back in its slot.
#[test]
fn a_small_jitter_is_not_a_drag() {
    let from = Origin::new(600, 30);
    let to = from + IVec2::new(3, 1);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 1),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 1, to - from)
        .on_iteration(2, |world, _| {
            assert_window_at!(world, 1, 403, 21);
        })
        .on_iteration(4, |world, _| assert_three_columns(world))
        .run(commands);
}

/// A press near the top edge can start a resize. The window changes size as
/// it moves, so it is not following the pointer.
#[test]
fn a_resize_from_the_top_edge_is_not_a_drag() {
    let from = Origin::new(1000, 22);
    let to = Origin::new(200, 300);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    )
    .on_iteration(1, move |_, state| {
        state.os_move_window(2, Origin::new(800, 300));
        state.os_resize_window(2, Size::new(TEST_WINDOW_WIDTH, FULL_HEIGHT - 280));
    })
    .on_iteration(4, |world, _| {
        assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
    })
    .run(commands);
}

/// Pressing a window's title bar and moving the pointer without the window
/// following it, as when selecting text in a toolbar field, is not a drag.
#[test]
fn a_pointer_drag_that_leaves_the_window_still_is_not_a_drag() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    )
    .on_iteration(4, |world, _| {
        assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
    })
    .run(commands);
}

/// Window 3 joins window 2 as a native tab. Dragging the tab on top moves
/// the whole group, even when macOS reports the move for the other tab.
#[test]
fn a_native_tab_group_moves_as_one() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        settle(),
        press(from, 3),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 2, 2, to - from)
        .on_iteration(0, |world, state| {
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame(world, 2));
            state.window_visible(2, false);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _| {
            let columns = columns(world);
            assert_eq!(columns.len(), 3, "{columns:?}");
            assert_eq!(columns[2].len(), 2, "one tab group: {columns:?}");
        })
        .on_iteration(5, |world, _| {
            let columns = columns(world);
            assert_eq!(columns.len(), 2, "{columns:?}");
            assert_eq!(columns[0][0], 0, "{columns:?}");
            let mut tabs = columns[0][1..].to_vec();
            tabs.sort_unstable();
            assert_eq!(tabs, vec![2, 3], "{columns:?}");
            assert_eq!(columns[1], vec![1]);
            assert_eq!(frame(world, 2), frame(world, 3), "the tabs share a frame");
        })
        .run(commands);
}

/// The tap can hand a whole gesture over in one update, after the window
/// has already moved to where it was released.
#[test]
fn a_gesture_batched_into_one_update_still_drops() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![Event::MenuOpened { window_id: 0 }, settle(), settle()];

    holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    )
    .on_iteration(0, move |world, state| {
        let start = frame(world, 2).min;
        world.write_message(press(from, 2));
        world.write_message(drag_to(to));
        state.os_move_window(2, start + (to - from));
        for event in state.drain_events() {
            world.write_message(event);
        }
        world.write_message(release(to));
    })
    .on_iteration(2, |world, _| {
        assert_eq!(columns(world), vec![vec![0, 2], vec![1]]);
        assert_focused!(world, 2);
    })
    .run(commands);
}

/// When the window server reports the button up but the tap never delivers
/// the release, the dragged window goes back to its slot.
#[test]
fn a_release_the_tap_never_reports_returns_the_window() {
    let from = Origin::new(600, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 1),
        drag_to(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 1, to - from)
        .on_iteration(2, |world, state| {
            assert_window_at!(world, 1, 0, 590);
            state.set_left_button_down(false);
        })
        .on_iteration(3, |world, _| {
            assert_three_columns(world);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, FULL_HEIGHT);
        })
        .run(commands);
}

/// A dragged window that closes leaves nothing to drop, and the next drag
/// works as usual.
#[test]
fn a_held_window_closing_ends_the_drag() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let next_from = Origin::new(600, 30);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        settle(),
        press(next_from, 1),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    let harness = window_follows(harness, 1, 2, to - from)
        .on_iteration(2, |_, state| state.os_close_window(2))
        .on_iteration(3, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![1]]);
            assert_window_at!(world, 1, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        });
    window_follows(harness, 4, 1, to - next_from)
        .on_iteration(7, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1]]);
        })
        .run(commands);
}

/// A window scrolled off the right edge is parked as a 5 point sliver over
/// its neighbour. Its visible title bar still starts a drag, and the
/// window's centre, 198 points right of the pointer, picks the column.
#[test]
fn a_parked_window_is_dragged_by_its_visible_title_bar() {
    let from = Origin::new(TEST_DISPLAY_WIDTH - 3, 30);
    let to = Origin::new(2, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 3),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(4),
    );
    window_follows(harness, 1, 3, to - from)
        .on_iteration(0, |world, _| {
            assert_window_at!(world, 3, TEST_DISPLAY_WIDTH - 5, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 3], vec![1], vec![2]]);
        })
        .run(commands);
}

/// With the option off, which is the default, a title-bar drag works as it
/// always has: macOS moves the window, Paneru keeps it in its column, and
/// the release only scrolls the strip to show it.
#[test]
fn without_the_option_a_title_bar_drag_leaves_the_layout_alone() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(TestHarness::new().with_windows(3));
    window_follows(harness, 1, 2, to - from)
        .on_iteration(2, |world, _| {
            assert_window_at!(world, 2, 0, 590);
        })
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
            assert_window_at!(world, 0, -176, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 224, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 624, TEST_MENUBAR_HEIGHT);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, FULL_HEIGHT);
        })
        .run(commands);
}

#[test]
fn a_right_button_drag_never_drags_a_window() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let right = |event: Event| match event {
        Event::MouseDown {
            point,
            modifiers,
            window_id,
            ..
        } => Event::MouseDown {
            point,
            modifiers,
            button: MouseButton::Right,
            window_id,
        },
        Event::MouseDragged {
            point, modifiers, ..
        } => Event::MouseDragged {
            point,
            modifiers,
            button: MouseButton::Right,
        },
        Event::MouseUp {
            point, modifiers, ..
        } => Event::MouseUp {
            point,
            modifiers,
            button: MouseButton::Right,
        },
        event => event,
    };
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        right(press(from, 2)),
        right(drag_to(to)),
        right(release(to)),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 2, to - from)
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
        })
        .run(commands);
}

/// Mission Control moves windows on its own, so a whole gesture made while
/// it shows, even one the tap hands over in one update, drags nothing.
#[test]
fn no_window_is_dragged_while_mission_control_is_showing() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::MissionControlShowAllWindows,
        settle(),
    ];

    holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    )
    .on_iteration(1, move |world, state| {
        let start = frame(world, 2).min;
        world.write_message(press(from, 2));
        world.write_message(drag_to(to));
        state.os_move_window(2, start + (to - from));
        for event in state.drain_events() {
            world.write_message(event);
        }
        world.write_message(release(to));
    })
    .on_iteration(2, |world, _| {
        assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
        assert_window_at!(world, 2, 0, 590);
    })
    .run(commands);
}

/// Mission Control opening mid-drag ends the drag, and the window goes back
/// to its slot.
#[test]
fn mission_control_opening_mid_drag_returns_the_window() {
    let from = Origin::new(600, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 1),
        drag_to(to),
        Event::MissionControlShowAllWindows,
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 1, to - from)
        .on_iteration(2, |world, _| assert_window_at!(world, 1, 0, 590))
        .on_iteration(5, |world, _| {
            assert_three_columns(world);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, FULL_HEIGHT);
        })
        .run(commands);
}

/// Window 1 closes while window 2 is dragged. The strip closes the gap, but
/// window 2 stays under the pointer until the drop.
#[test]
fn a_layout_change_mid_drag_leaves_the_dragged_window_under_the_pointer() {
    let from = Origin::new(1000, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        settle(),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 1, 2, to - from)
        .on_iteration(2, |_, state| state.os_close_window(1))
        .on_iteration(3, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![2]]);
            assert_window_at!(world, 2, 0, 590);
        })
        .on_iteration(5, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 2]]);
        })
        .run(commands);
}

/// With `auto_center`, focusing a window centres it. A dropped window is
/// focused too, but its drop only scrolls the strip as far as needed to show
/// it. Window 2, parked at the right edge, joins window 1, whose column then
/// ends at the right edge of the display instead of moving to the middle.
#[test]
fn a_drop_scrolls_only_as_far_as_needed_even_with_auto_center() {
    let from = Origin::new(TEST_DISPLAY_WIDTH - 3, 30);
    let to = Origin::new(714, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let options = "drag_to_reorder = true\nauto_center = true\n";
    let harness = holding(
        TestHarness::new()
            .with_config(config(options))
            .with_windows(3),
    );
    window_follows(harness, 1, 2, to - from)
        .on_iteration(0, |world, _| {
            assert_focused!(world, 0);
            assert_window_at!(world, 0, 312, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 712, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, TEST_DISPLAY_WIDTH - 5, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![1, 2]]);
            assert_focused!(world, 2);
            assert_window_at!(
                world,
                1,
                TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH,
                TEST_MENUBAR_HEIGHT
            );
            assert_window_at!(
                world,
                2,
                TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH,
                TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2
            );
        })
        .run(commands);
}

/// The tap hands over a parked window's press, drag, and move in one
/// update, and the release in a later one. By the time the press is handled,
/// the sliver has moved away and window 2 is under the press point, so the
/// press must keep the window the tap saw. Otherwise the release would scroll
/// the strip to show window 2.
#[test]
fn a_batched_press_of_a_parked_window_holds_that_window() {
    let from = Origin::new(TEST_DISPLAY_WIDTH - 3, 30);
    let to = Origin::new(2, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        settle(),
        release(to),
        settle(),
    ];

    holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(4),
    )
    .on_iteration(0, move |world, state| {
        let start = frame(world, 3).min;
        assert_eq!(start.x, TEST_DISPLAY_WIDTH - 5);
        world.write_message(press(from, 3));
        world.write_message(drag_to(to));
        state.os_move_window(3, start + (to - from));
        for event in state.drain_events() {
            world.write_message(event);
        }
    })
    .on_iteration(3, |world, _| {
        assert_eq!(columns(world), vec![vec![0, 3], vec![1], vec![2]]);
        assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 3, 0, TEST_MENUBAR_HEIGHT + FULL_HEIGHT / 2);
    })
    .run(commands);
}

/// Windows 0 and 1 move to a virtual workspace that is not shown, which
/// parks them in the bottom-right corner, with window 1 as a sliver at its
/// edge. A press there is not on a window of the layout on screen, so it
/// starts no drag, and the parked workspace keeps its order.
#[test]
fn a_window_of_a_parked_workspace_is_not_dragged() {
    let from = Origin::new(TEST_DISPLAY_WIDTH - 3, TEST_DISPLAY_HEIGHT - 8);
    let to = Origin::new(200, 400);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        window_command(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        settle(),
        press(from, 1),
        drag_to(to),
        release(to),
        settle(),
    ];

    let parked = |world: &mut World| {
        let ids = world
            .query::<(Entity, &Window)>()
            .iter(world)
            .map(|(entity, window)| (entity, window.id()))
            .collect::<std::collections::HashMap<_, _>>();
        world
            .query_filtered::<&LayoutStrip, With<PreviousStripPosition>>()
            .single(world)
            .expect("one parked strip")
            .columns()
            .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
            .collect::<Vec<Vec<WinID>>>()
    };
    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(4),
    );
    window_follows(harness, 4, 1, to - from)
        .on_iteration(3, move |world, _| {
            assert_eq!(parked(world), vec![vec![0], vec![1]]);
            assert_window_at!(world, 1, TEST_DISPLAY_WIDTH - 5, TEST_DISPLAY_HEIGHT - 10);
        })
        .on_iteration(7, move |world, _| {
            assert_eq!(parked(world), vec![vec![0], vec![1]]);
            assert_eq!(columns(world), vec![vec![2], vec![3]]);
        })
        .run(commands);
}

/// Window 2 hangs past the right edge of the main display, over an external
/// display. A press on its title bar there is on the external display, not
/// on the strip's own, so it starts no drag.
#[test]
fn a_press_on_another_display_than_the_windows_strip_starts_no_drag() {
    let from = Origin::new(TEST_DISPLAY_WIDTH + 76, 30);
    let to = Origin::new(200, 600);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_display(
                EXT_DISPLAY_ID,
                IRect::new(
                    TEST_DISPLAY_WIDTH,
                    0,
                    TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
                    EXT_DISPLAY_HEIGHT,
                ),
                vec![EXT_WORKSPACE_ID],
            )
            .with_windows(3),
    );
    window_follows(harness, 1, 2, to - from)
        .on_iteration(0, |world, _| assert_three_columns(world))
        .on_iteration(4, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![1], vec![2]]);
        })
        .run(commands);
}

/// A full-width window that joins a column takes the column's width, so it
/// is no longer full width. Toggling full width on it again makes it full
/// width, instead of restoring the width it had before.
#[test]
fn a_full_width_window_that_joins_a_column_can_be_made_full_width_again() {
    let from = Origin::new(900, 30);
    let to = Origin::new(150, 300);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::Last)),
        window_command(Operation::FullWidth),
        window_command(Operation::Focus(Direction::First)),
        settle(),
        press(from, 2),
        drag_to(to),
        release(to),
        settle(),
        window_command(Operation::FullWidth),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    window_follows(harness, 5, 2, to - from)
        .on_iteration(4, |world, _| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
            assert_window_size!(world, 2, TEST_DISPLAY_WIDTH, FULL_HEIGHT);
        })
        .on_iteration(8, |world, _| {
            assert_eq!(columns(world), vec![vec![0], vec![2, 1]]);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, FULL_HEIGHT / 2);
        })
        .on_iteration(10, |world, _| {
            assert_window_size!(world, 2, TEST_DISPLAY_WIDTH, FULL_HEIGHT);
        })
        .run(commands);
}
