//! Apps the engine finds before they can be managed: not yet regular apps,
//! or too busy to answer Accessibility.

use std::time::Duration;

use bevy::prelude::*;

use crate::ecs::layout::LayoutStrip;
use crate::ecs::systems::launch_watch;
use crate::ecs::{ActiveWorkspaceMarker, BProcess};
use crate::events::{Event, EventSender};
use crate::manager::Window;
use crate::platform::{Pid, WinID, WorkspaceObserver};

use super::*;

const MENU_BAR_APP: Pid = 7;
const DASHBOARD: WinID = 70;

fn tiled(harness: &mut TestHarness, id: WinID) -> bool {
    let world = harness.world();
    let Some(entity) = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .find_map(|(entity, window)| (window.id() == id).then_some(entity))
    else {
        return false;
    };
    world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .is_ok_and(|strip| strip.contains(entity))
}

/// Docker Desktop runs as a menu-bar app, with no Dock icon, until its
/// dashboard opens. Its dashboard is tiled, however long the app took to
/// become a regular app.
#[test]
fn a_menu_bar_app_that_becomes_regular_later_is_tiled() {
    let mut harness = TestHarness::new().with_windows(1);
    let state = harness.mock_state.clone();
    state.spawn_app(MENU_BAR_APP, "com.electron.dockerdesktop", "Docker Desktop");
    state.update_app(MENU_BAR_APP, |app| app.regular = false);

    let world = harness.world();
    let mut commands = world.commands();
    let watch = launch_watch("Docker Desktop", &mut commands);
    commands.spawn((
        watch,
        BProcess(Box::new(state.create_process(MENU_BAR_APP))),
    ));
    world.flush();
    harness.advance(Duration::from_secs(10));

    state.update_app(MENU_BAR_APP, |app| app.regular = true);
    state.spawn_window(
        MENU_BAR_APP,
        TEST_WORKSPACE_ID,
        DASHBOARD,
        IRect::new(200, 100, 900, 600),
    );
    let mut psn = None;
    state.update_app(MENU_BAR_APP, |app| psn = Some(app.psn));
    let (events, _receiver) = EventSender::new();
    harness.world().write_message(Event::ApplicationLaunched {
        psn: psn.expect("app exists"),
        observer: WorkspaceObserver::new(events),
    });
    harness.advance(Duration::from_secs(2));

    assert!(tiled(&mut harness, DASHBOARD), "the dashboard is tiled");
}

const BUSY_APP: Pid = 8;
const RESTORED: WinID = 80;

/// A browser restoring many tabs blocks its main thread right after it
/// finishes launching, for longer than the engine's 5 s launch watch. Its
/// window is tiled soon after the app answers again, without the user
/// activating it, and the engine asks the busy app only once from its main
/// loop while it waits. (#63)
#[test]
fn an_app_busy_past_the_launch_watch_is_tiled_once_it_answers() {
    let mut harness = TestHarness::new().with_windows(1);
    let state = harness.mock_state.clone();
    state.spawn_app(BUSY_APP, "com.example.busy", "Busy");
    state.update_app(BUSY_APP, |app| app.ax_busy = true);
    state.spawn_window(
        BUSY_APP,
        TEST_WORKSPACE_ID,
        RESTORED,
        IRect::new(200, 100, 900, 600),
    );

    let world = harness.world();
    let mut commands = world.commands();
    let watch = launch_watch("Busy", &mut commands);
    commands.spawn((watch, BProcess(Box::new(state.create_process(BUSY_APP)))));
    world.flush();

    harness.advance(Duration::from_secs(10));
    assert!(!tiled(&mut harness, RESTORED), "a busy app is not tiled");
    assert!(
        state.observe_calls(BUSY_APP) <= 1,
        "the main loop asked a busy app once"
    );

    state.update_app(BUSY_APP, |app| app.ax_busy = false);
    harness.advance(Duration::from_secs(2));
    assert!(
        tiled(&mut harness, RESTORED),
        "the window is tiled once the app answers"
    );
}

/// An app that blocks its main thread for most of every second, with short
/// free gaps, can answer the engine's probe in a gap and then time out the
/// observer registration in its next block. The engine asks it from its main
/// loop less and less often, so a busy app does not keep blocking the main
/// loop, and its window is still tiled once the app stops being busy. (#63)
#[test]
fn an_app_that_answers_probes_but_stays_busy_is_asked_less_often() {
    let mut harness = TestHarness::new().with_windows(1);
    let state = harness.mock_state.clone();
    state.spawn_app(BUSY_APP, "com.example.busy", "Busy");
    state.update_app(BUSY_APP, |app| {
        app.ax_busy = true;
        app.answers_probes = true;
    });
    state.spawn_window(
        BUSY_APP,
        TEST_WORKSPACE_ID,
        RESTORED,
        IRect::new(200, 100, 900, 600),
    );

    let world = harness.world();
    let mut commands = world.commands();
    let watch = launch_watch("Busy", &mut commands);
    commands.spawn((watch, BProcess(Box::new(state.create_process(BUSY_APP)))));
    world.flush();

    harness.advance(Duration::from_secs(60));
    assert!(!tiled(&mut harness, RESTORED), "a busy app is not tiled");
    let calls = state.observe_calls(BUSY_APP);
    assert!(
        calls <= 7,
        "the main loop asked a busy app {calls} times in 60 s, at most 7 expected"
    );

    state.update_app(BUSY_APP, |app| app.ax_busy = false);
    harness.advance(Duration::from_secs(32));
    assert!(
        tiled(&mut harness, RESTORED),
        "the window is tiled once the app answers"
    );
}
