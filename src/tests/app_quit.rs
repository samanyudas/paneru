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

#[test]
fn focus_on_close_parses_left_or_right() {
    use crate::config::{Config, FocusOnClose};

    assert_eq!(Config::default().focus_on_close(), FocusOnClose::Left);
    let left = Config::try_from("[options]\nfocus_on_close = \"left\"\n").unwrap();
    assert_eq!(left.focus_on_close(), FocusOnClose::Left);
    let right = Config::try_from("[options]\nfocus_on_close = \"right\"\n").unwrap();
    assert_eq!(right.focus_on_close(), FocusOnClose::Right);
    assert!(Config::try_from("[options]\nfocus_on_close = \"up\"\n").is_err());
}

fn harness_with_four_windows(focus_on_close: Option<&str>) -> TestHarness {
    let mut harness = TestHarness::new().with_app(QUITTING_PID, "quit.b", "Quit-B", |_| {});
    if let Some(side) = focus_on_close {
        let config = crate::config::Config::try_from(
            format!("[options]\nfocus_on_close = \"{side}\"\n").as_str(),
        )
        .unwrap();
        harness = harness.with_config(config);
    }
    let state = harness.mock_state.clone();
    let windows = [
        (0, TEST_PROCESS_ID),
        (1, TEST_PROCESS_ID),
        (2, QUITTING_PID),
        (3, TEST_PROCESS_ID),
    ]
    .into_iter()
    .map(|(id, pid)| {
        state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            id,
            IRect::new(id * 600, 24, id * 600 + 600, 764),
        )
    })
    .collect();
    harness.existed_at_launch(0..4);
    harness.world().trigger(SpawnWindowTrigger(windows));
    harness.advance(Duration::from_secs(3));
    harness
}

fn close_window(harness: &mut TestHarness, window_id: crate::platform::WinID) {
    harness.run(vec![Event::WindowDestroyed {
        window_id,
        source: crate::events::DestroySource::Accessibility { element: None },
    }]);
    harness.advance(Duration::from_secs(1));
}

#[test]
fn closing_rightmost_window_with_offscreen_left_columns_focuses_immediate_left_neighbor() {
    let mut harness = harness_with_four_windows(None);
    harness.mock_state.focus_window(3);
    harness.advance(Duration::from_secs(1));
    // Maximize window 3 so columns 0, 1, and 2 all park off-screen left at the same x.
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Maximize),
    }]);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 3);

    close_window(&mut harness, 3);

    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn closing_middle_window_respects_focus_on_close_left_and_right() {
    let mut left_harness = harness_with_four_windows(Some("left"));
    left_harness.mock_state.focus_window(2);
    left_harness.advance(Duration::from_secs(1));
    crate::assert_focused!(left_harness.world(), 2);
    close_window(&mut left_harness, 2);
    crate::assert_focused!(left_harness.world(), 1);

    let mut right_harness = harness_with_four_windows(Some("right"));
    right_harness.mock_state.focus_window(2);
    right_harness.advance(Duration::from_secs(1));
    crate::assert_focused!(right_harness.world(), 2);
    close_window(&mut right_harness, 2);
    crate::assert_focused!(right_harness.world(), 3);
}

#[test]
fn closing_edge_window_falls_back_to_opposite_neighbor() {
    let mut left_edge = harness_with_four_windows(Some("left"));
    left_edge.mock_state.focus_window(0);
    left_edge.advance(Duration::from_secs(1));
    close_window(&mut left_edge, 0);
    crate::assert_focused!(left_edge.world(), 1);

    let mut right_edge = harness_with_four_windows(Some("right"));
    right_edge.mock_state.focus_window(3);
    right_edge.advance(Duration::from_secs(1));
    close_window(&mut right_edge, 3);
    crate::assert_focused!(right_edge.world(), 2);
}

#[test]
fn closing_stacked_window_focuses_remaining_window_in_same_stack() {
    let mut harness = harness_with_four_windows(Some("left"));
    harness.mock_state.focus_window(2);
    harness.advance(Duration::from_secs(1));
    // Stack window 2 into column 1 (which holds window 1).
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Stack(crate::commands::Side::West)),
    }]);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 2);

    close_window(&mut harness, 2);

    crate::assert_focused!(harness.world(), 1);
}

#[test]
fn quitting_focused_app_focuses_neighbor_in_configured_direction() {
    let mut left_harness = harness_with_four_windows(Some("left"));
    left_harness.mock_state.focus_window(2);
    left_harness.advance(Duration::from_secs(1));
    crate::assert_focused!(left_harness.world(), 2);
    quit_app(&mut left_harness);
    crate::assert_focused!(left_harness.world(), 1);

    let mut right_harness = harness_with_four_windows(Some("right"));
    right_harness.mock_state.focus_window(2);
    right_harness.advance(Duration::from_secs(1));
    crate::assert_focused!(right_harness.world(), 2);
    quit_app(&mut right_harness);
    crate::assert_focused!(right_harness.world(), 3);
}

#[test]
fn quitting_multi_window_focused_app_skips_other_windows_of_quitting_app() {
    let config = crate::config::Config::try_from("[options]\nfocus_on_close = \"left\"\n").unwrap();
    let mut harness = TestHarness::new()
        .with_app(QUITTING_PID, "quit.b", "Quit-B", |_| {})
        .with_config(config);
    let state = harness.mock_state.clone();
    let windows = [
        (0, TEST_PROCESS_ID),
        (1, QUITTING_PID),
        (2, QUITTING_PID),
        (3, TEST_PROCESS_ID),
    ]
    .into_iter()
    .map(|(id, pid)| {
        state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            id,
            IRect::new(id * 600, 24, id * 600 + 600, 764),
        )
    })
    .collect();
    harness.existed_at_launch(0..4);
    harness.world().trigger(SpawnWindowTrigger(windows));
    harness.advance(Duration::from_secs(3));
    state.focus_window(2);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 2);

    quit_app(&mut harness);

    // Window 1 belonged to the quitting app too, so focus skips it to window 0.
    crate::assert_focused!(harness.world(), 0);
}

#[test]
fn quitting_unfocused_app_leaves_focus_unchanged() {
    let mut harness = harness_with_four_windows(Some("left"));
    harness.mock_state.focus_window(0);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 0);

    quit_app(&mut harness);

    crate::assert_focused!(harness.world(), 0);
}

#[test]
fn closing_multiple_windows_in_one_frame_skips_other_closing_windows() {
    let mut harness = harness_with_four_windows(Some("right"));
    harness.mock_state.focus_window(1);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 1);

    // Queue both destroy events in the same frame; with Right selected, focus
    // skips closing window 2 and lands directly on surviving window 3.
    harness.world().write_message(Event::WindowDestroyed {
        window_id: 1,
        source: crate::events::DestroySource::Accessibility { element: None },
    });
    harness.world().write_message(Event::WindowDestroyed {
        window_id: 2,
        source: crate::events::DestroySource::Accessibility { element: None },
    });
    harness.app.update();
    crate::assert_focused!(harness.world(), 3);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 3);
}

#[test]
fn same_frame_window_close_and_neighbor_app_quit_focuses_surviving_neighbor() {
    // Direction 1: focused window 1 closes while right neighbor (window 2, QUITTING_PID) quits
    // in the same frame.
    let mut harness = harness_with_four_windows(Some("right"));
    harness.mock_state.focus_window(1);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 1);

    harness.world().write_message(Event::WindowDestroyed {
        window_id: 1,
        source: crate::events::DestroySource::Accessibility { element: None },
    });
    harness.world().write_message(Event::ApplicationTerminated {
        psn: ProcessSerialNumber {
            high: 0,
            low: QUITTING_PID.cast_unsigned(),
        },
    });
    harness.app.update();
    crate::assert_focused!(harness.world(), 3);
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 3);

    // Direction 2: focused window 2 (QUITTING_PID) quits while right neighbor (window 3) closes
    // in the same frame.
    let mut harness2 = harness_with_four_windows(Some("right"));
    harness2.mock_state.focus_window(2);
    harness2.advance(Duration::from_secs(1));
    crate::assert_focused!(harness2.world(), 2);

    let raises_before = harness2.mock_state.raise_count();
    harness2
        .world()
        .write_message(Event::ApplicationTerminated {
            psn: ProcessSerialNumber {
                high: 0,
                low: QUITTING_PID.cast_unsigned(),
            },
        });
    harness2.world().write_message(Event::WindowDestroyed {
        window_id: 3,
        source: crate::events::DestroySource::Accessibility { element: None },
    });
    // After the single departure frame, focus lands directly on surviving
    // window 1 with exactly one raise (closing window 3 is never raised).
    harness2.app.update();
    crate::assert_focused!(harness2.world(), 1);
    assert_eq!(harness2.mock_state.raise_count(), raises_before + 1);
    assert_eq!(
        harness2.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(1)
    );
    harness2.advance(Duration::from_secs(1));
    crate::assert_focused!(harness2.world(), 1);
}

#[test]
fn closing_unfocused_floating_window_does_not_steal_focus_from_focused_floating_window() {
    let mut harness = started();
    let state = harness.mock_state.clone();
    let floating_settings = state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        10,
        IRect::new(200, 100, 1080, 740),
    );
    let floating_transient = state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        11,
        IRect::new(500, 30, 800, 280),
    );
    harness.world().trigger(SpawnWindowTrigger(vec![
        floating_settings,
        floating_transient,
    ]));
    harness.advance(Duration::from_millis(200));

    // Mark both windows floating, as Miri Settings and its helper windows are.
    let world = harness.world();
    let mut to_float = Vec::new();
    for (window, entity) in world.query::<(&Window, Entity)>().iter(world) {
        if window.id() == 10 || window.id() == 11 {
            to_float.push(entity);
        }
    }
    for entity in to_float {
        world
            .entity_mut(entity)
            .insert(crate::ecs::Unmanaged::Floating);
    }
    harness.advance(Duration::from_millis(200));

    state.focus_window(10);
    harness.advance(Duration::from_millis(200));
    crate::assert_focused!(harness.world(), 10);

    let raises_before = state.raise_count();
    close_window(&mut harness, 11);
    crate::assert_focused!(harness.world(), 10);
    assert_eq!(state.raise_count(), raises_before);

    // Closing an unfocused tiled window must also leave the focused floating window alone.
    close_window(&mut harness, 1);
    crate::assert_focused!(harness.world(), 10);
    assert_eq!(state.raise_count(), raises_before);
}

#[test]
fn closing_unfocused_floating_window_with_no_focused_marker_does_not_steal_focus() {
    let mut harness = started();
    let state = harness.mock_state.clone();
    let floating_transient = state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        11,
        IRect::new(500, 30, 800, 280),
    );
    harness
        .world()
        .trigger(SpawnWindowTrigger(vec![floating_transient]));
    harness.advance(Duration::from_millis(200));

    let world = harness.world();
    let entity_11 = world
        .query::<(&Window, Entity)>()
        .iter(world)
        .find_map(|(w, e)| (w.id() == 11).then_some(e))
        .expect("window 11");
    world
        .entity_mut(entity_11)
        .insert(crate::ecs::Unmanaged::Floating);
    let focused_entities: Vec<Entity> = world
        .query_filtered::<Entity, bevy::prelude::With<crate::ecs::FocusedMarker>>()
        .iter(world)
        .collect();
    for entity in focused_entities {
        world
            .entity_mut(entity)
            .remove::<crate::ecs::FocusedMarker>();
    }
    harness.advance(Duration::from_millis(200));

    let raises_before = state.raise_count();
    close_window(&mut harness, 11);

    assert_eq!(state.raise_count(), raises_before);
}
