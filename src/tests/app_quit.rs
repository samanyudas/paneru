//! Quitting an app leaves no dead column in a strip (miri#478).

use std::time::Duration;

use bevy::math::IRect;
use bevy::prelude::Entity;

use crate::commands::{Command, Direction, Operation};
use crate::ecs::SpawnWindowTrigger;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::Window;
use crate::platform::{Pid, ProcessSerialNumber};

use super::*;

/// The app that quits. It owns window 1, between windows 0 and 2 of the
/// app that keeps running.
const QUITTING_PID: Pid = 2;

/// Starts the engine on columns 0, 1 and 2, with window 1 from the quitting
/// app, focuses window 0, and lets the startup grace period pass.
fn started() -> TestHarness {
    let mut harness = TestHarness::new().with_app(QUITTING_PID, "quit.b", "Quit-B", |_| {});
    let state = harness.mock_state.clone();
    let windows = [
        (0, TEST_PROCESS_ID),
        (1, QUITTING_PID),
        (2, TEST_PROCESS_ID),
    ]
    .into_iter()
    .map(|(id, pid)| {
        state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            id,
            IRect::new(id * 300, 24, id * 300 + 300, 764),
        )
    })
    .collect();
    harness.existed_at_launch(0..3);
    harness.world().trigger(SpawnWindowTrigger(windows));
    state.focus_window(0);
    harness.advance(Duration::from_secs(3));
    crate::assert_focused!(harness.world(), 0);
    harness
}

/// Quits the app of window 1, as macOS reports it: no destroy event per
/// window.
fn quit_app(harness: &mut TestHarness) {
    harness.run(vec![Event::ApplicationTerminated {
        psn: ProcessSerialNumber {
            high: 0,
            low: QUITTING_PID.cast_unsigned(),
        },
    }]);
    harness.advance(Duration::from_secs(2));
}

/// Moves focus one column right, as Control-Option-Right does.
fn focus_east(harness: &mut TestHarness) {
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Focus(Direction::East)),
    }]);
    harness.advance(Duration::from_secs(1));
}

/// The strip entries that are no longer windows.
fn dead_strip_entries(harness: &mut TestHarness) -> Vec<Entity> {
    let world = harness.world();
    let entries: Vec<Entity> = world
        .query::<&LayoutStrip>()
        .iter(world)
        .flat_map(LayoutStrip::all_windows)
        .collect();
    entries
        .into_iter()
        .filter(|entity| world.get::<Window>(*entity).is_none())
        .collect()
}

/// After the quit, the strip holds only live windows, and a focus move from
/// window 0 reaches window 2 past the quit app's old column.
fn assert_focus_passes_the_quit_column(harness: &mut TestHarness) {
    assert_eq!(dead_strip_entries(harness), Vec::<Entity>::new());
    // Hiding or minimizing window 1 can move focus first; start from 0.
    harness.mock_state.focus_window(0);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 0);
    focus_east(harness);
    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn quitting_an_app_with_a_minimized_window_leaves_no_dead_column() {
    let mut harness = started();
    harness.mock_state.os_minimize_window(1, true);
    harness.advance(Duration::from_secs(2));

    quit_app(&mut harness);

    assert_focus_passes_the_quit_column(&mut harness);
}

#[test]
fn quitting_a_hidden_app_leaves_no_dead_column() {
    let mut harness = started();
    harness
        .mock_state
        .update_app(QUITTING_PID, |app| app.is_hidden = true);
    harness.run(vec![Event::ApplicationHidden { pid: QUITTING_PID }]);
    harness.advance(Duration::from_secs(2));

    quit_app(&mut harness);

    assert_focus_passes_the_quit_column(&mut harness);
}

#[test]
fn quitting_an_app_with_only_tiled_windows_leaves_no_dead_column() {
    let mut harness = started();

    quit_app(&mut harness);

    assert_focus_passes_the_quit_column(&mut harness);
}

/// The safety net: an entry that a future leak leaves behind is pruned, so
/// focus moves past its column again.
#[test]
fn a_dead_strip_entry_is_pruned() {
    let mut harness = started();
    let world = harness.world();
    let dead = world.spawn_empty().id();
    world.despawn(dead);
    let mut strips = world.query::<&mut LayoutStrip>();
    let mut strip = strips
        .iter_mut(world)
        .find(|strip| strip.len() == 3)
        .expect("the strip of windows 0, 1 and 2");
    strip.insert_at(1, dead);
    assert_eq!(dead_strip_entries(&mut harness), vec![dead]);

    harness.advance(Duration::from_millis(100));

    assert_eq!(dead_strip_entries(&mut harness), Vec::<Entity>::new());
    crate::assert_focused!(harness.world(), 0);
    focus_east(&mut harness);
    crate::assert_focused!(harness.world(), 1);
}
