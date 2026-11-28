//! Tests for `window_consume` and `window_expel`, adapted for Miri's
//! stack limit and maximized-column semantics.

use std::num::NonZeroUsize;

use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::{Config, MainOptions, StackLimit};
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;

use super::*;

fn stack_first_three(commands: &mut Vec<Event>) {
    commands.extend([
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
    ]);
}

fn same_column(world: &mut World, id_a: i32, id_b: i32) -> bool {
    let entity_a = find_window_entity(id_a, world);
    let entity_b = find_window_entity(id_b, world);
    let mut query = world.query::<&LayoutStrip>();
    let strip = query.single(world).expect("a single active strip");
    strip.index_of(entity_a).ok() == strip.index_of(entity_b).ok()
}

#[test]
fn test_consume_pulls_the_right_neighbor_into_the_current_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(
                same_column(world, 0, 1),
                "window 1 must have been consumed into window 0's column"
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_consume_is_a_noop_with_no_right_neighbor() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(
                !same_column(world, 0, 1),
                "there is nothing to the right of window 1; consume must be a no-op"
            );
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_consume_respects_stack_limit() {
    let config: Config = (
        MainOptions {
            stack_limit: Some(StackLimit::Count(NonZeroUsize::new(2).unwrap())),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(
                same_column(world, 0, 1),
                "first consume stays within the limit of 2"
            );
            assert!(
                !same_column(world, 0, 2),
                "second consume would exceed the stack limit of 2 and must be refused"
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_consume_restores_maximized_column_before_merging() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Maximize),
        },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(
                same_column(world, 0, 1),
                "consuming from a maximized column restores it and merges window 1"
            );
            let e0 = find_window_entity(0, world);
            let strip = world.query::<&LayoutStrip>().single(world).unwrap();
            assert!(
                strip.maximized(e0).is_none(),
                "merged column is no longer maximized"
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_consume_into_tabbed_display_keeps_all_members_tabbed() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(same_column(world, 0, 1));
            assert!(same_column(world, 0, 2));
            let e2 = find_window_entity(2, world);
            let strip = world.query::<&LayoutStrip>().single(world).unwrap();
            assert!(
                strip.is_tabbed_display(e2),
                "consumed window must join the tabbed stack"
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_expel_falls_back_to_the_previous_item_when_expelling_the_last_one() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::Expel),
    });
    let after_expel = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_expel, |world, _state| {
            assert!(
                !same_column(world, 1, 2),
                "window 2 must have been ejected into its own column"
            );
            assert!(
                same_column(world, 0, 1),
                "windows 0 and 1 must still share the remaining stack"
            );
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_expel_focuses_the_next_item_when_expelling_a_middle_one() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Expel),
    });
    let after_expel = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_expel, |world, _state| {
            assert!(
                !same_column(world, 0, 1),
                "window 1 must have been ejected into its own column"
            );
            assert!(
                same_column(world, 0, 2),
                "windows 0 and 2 must still share the remaining stack"
            );
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn test_expel_is_a_noop_on_a_single_window_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Expel),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(!same_column(world, 0, 1));
            assert_focused!(world, 0);
        })
        .run(commands);
}
