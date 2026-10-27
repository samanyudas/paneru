//! Apps the engine finds before macOS reports them as regular apps.

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
