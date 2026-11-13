use std::time::Duration;

use bevy::prelude::Entity;

use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::Window;
use crate::platform::ProcessSerialNumber;

use super::*;

/// Starts the engine on windows 0, 1 and 2 of one app, with 0 focused, and
/// lets the startup grace period pass.
fn started() -> TestHarness {
    let mut harness = TestHarness::new().with_windows(3).with_focused_window(0);
    harness.advance(Duration::from_secs(3));
    harness
}

/// Quits the test app, as macOS reports it: no destroy event per window.
fn quit_app(harness: &mut TestHarness) {
    harness.run(vec![Event::ApplicationTerminated {
        psn: ProcessSerialNumber {
            high: 0,
            low: TEST_PROCESS_ID.cast_unsigned(),
        },
    }]);
    harness.advance(Duration::from_secs(2));
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

#[test]
fn quitting_an_app_with_a_minimized_window_leaves_no_dead_strip_entry() {
    let mut harness = started();
    harness.mock_state.os_minimize_window(1, true);
    harness.advance(Duration::from_secs(2));
    assert!(dead_strip_entries(&mut harness).is_empty());

    quit_app(&mut harness);

    assert_eq!(dead_strip_entries(&mut harness), Vec::<Entity>::new());
}

#[test]
fn quitting_a_hidden_app_leaves_no_dead_strip_entry() {
    let mut harness = started();
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = true);
    harness.run(vec![Event::ApplicationHidden {
        pid: TEST_PROCESS_ID,
    }]);
    harness.advance(Duration::from_secs(2));
    assert!(dead_strip_entries(&mut harness).is_empty());

    quit_app(&mut harness);

    assert_eq!(dead_strip_entries(&mut harness), Vec::<Entity>::new());
}

#[test]
fn quitting_an_app_with_only_tiled_windows_leaves_no_dead_strip_entry() {
    let mut harness = started();

    quit_app(&mut harness);

    assert_eq!(dead_strip_entries(&mut harness), Vec::<Entity>::new());
}
