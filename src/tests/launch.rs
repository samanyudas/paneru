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

const PARTLY_BUSY_APP: Pid = 9;
const ANSWERED: WinID = 90;
const LATE: WinID = 91;

/// An app whose main thread is blocked for most of each second can answer
/// for one of its windows and time out on the next one while the engine lists
/// them at launch. The window that timed out is tiled once the app answers
/// for it, without the user focusing it. (#63)
#[test]
fn a_window_that_answers_late_at_launch_is_tiled() {
    let mut harness = TestHarness::new().with_windows(1);
    let state = harness.mock_state.clone();
    state.spawn_app(PARTLY_BUSY_APP, "com.example.partly-busy", "Partly busy");
    for (id, x) in [(ANSWERED, 200), (LATE, 400)] {
        state.spawn_window(
            PARTLY_BUSY_APP,
            TEST_WORKSPACE_ID,
            id,
            IRect::new(x, 100, x + 700, 600),
        );
    }
    state.update_window(LATE, |window| window.ax_unanswered = true);

    let world = harness.world();
    let mut commands = world.commands();
    let watch = launch_watch("Partly busy", &mut commands);
    commands.spawn((
        watch,
        BProcess(Box::new(state.create_process(PARTLY_BUSY_APP))),
    ));
    world.flush();

    harness.advance(Duration::from_secs(1));
    assert!(
        tiled(&mut harness, ANSWERED),
        "the answered window is tiled"
    );
    assert!(!tiled(&mut harness, LATE), "the unanswered window waits");

    state.update_window(LATE, |window| window.ax_unanswered = false);
    harness.advance(Duration::from_secs(1));
    assert!(
        tiled(&mut harness, LATE),
        "the window is tiled once the app answers for it"
    );
}

/// An app that was hidden before the engine started still lists its windows
/// over Accessibility, as an Electron app such as Beeper does. Its window takes
/// no column, so moving focus along the strip cannot reach it and show the app
/// again, and it joins the strip when the user shows the app. (#374)
#[test]
fn a_window_of_an_app_hidden_before_launch_takes_no_column_until_shown() {
    let harness = TestHarness::new();
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = true);
    let mut harness = harness.with_windows(2);

    harness.advance(Duration::from_secs(2));
    assert!(
        !tiled(&mut harness, 0),
        "a hidden app's window takes no column"
    );
    assert!(
        !tiled(&mut harness, 1),
        "a hidden app's window takes no column"
    );

    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = false);
    harness.run(vec![Event::ApplicationVisible {
        pid: TEST_PROCESS_ID,
    }]);
    assert!(
        tiled(&mut harness, 0),
        "the window joins the strip once the app shows"
    );
    assert!(
        tiled(&mut harness, 1),
        "the window joins the strip once the app shows"
    );
}

const HIDDEN_APP: Pid = 11;
const HIDDEN_WINDOW: WinID = 110;

fn focused_window(harness: &mut TestHarness) -> Option<WinID> {
    let world = harness.world();
    world
        .query_filtered::<&Window, With<crate::ecs::FocusedMarker>>()
        .iter(world)
        .next()
        .map(|window| window.id())
}

/// A hidden app that opens a window takes no column, so focus stays on the
/// window the user is in. (#374)
#[test]
fn a_hidden_app_opening_a_window_leaves_focus_where_it_is() {
    let focus = 0;
    let mut harness = TestHarness::new().with_windows(3);
    harness.mock_state.focus_window(focus);
    harness.run(vec![Event::WindowFocused { window_id: focus }]);
    assert_eq!(focused_window(&mut harness), Some(focus));

    let state = harness.mock_state.clone();
    state.spawn_app(HIDDEN_APP, "com.example.hidden", "Hidden");
    state.update_app(HIDDEN_APP, |app| app.is_hidden = true);
    state.spawn_window(
        HIDDEN_APP,
        TEST_WORKSPACE_ID,
        HIDDEN_WINDOW,
        IRect::new(200, 100, 900, 600),
    );
    let world = harness.world();
    let mut commands = world.commands();
    let watch = launch_watch("Hidden", &mut commands);
    commands.spawn((watch, BProcess(Box::new(state.create_process(HIDDEN_APP)))));
    world.flush();
    harness.advance(Duration::from_secs(2));

    assert!(
        harness
            .world()
            .query::<&Window>()
            .iter(harness.world())
            .any(|window| window.id() == HIDDEN_WINDOW),
        "the engine tracks the hidden app's window"
    );
    assert!(!tiled(&mut harness, HIDDEN_WINDOW), "it takes no column");
    assert_eq!(
        focused_window(&mut harness),
        Some(focus),
        "focus stays on window {focus}"
    );
}

fn unmanaged(harness: &mut TestHarness, id: WinID) -> Option<String> {
    let world = harness.world();
    world
        .query::<(&Window, Option<&crate::ecs::Unmanaged>)>()
        .iter(world)
        .find(|(window, _)| window.id() == id)
        .and_then(|(_, unmanaged)| unmanaged.map(|unmanaged| format!("{unmanaged:?}")))
}

fn show_test_app(harness: &mut TestHarness) {
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = false);
    harness.run(vec![Event::ApplicationVisible {
        pid: TEST_PROCESS_ID,
    }]);
}

/// A window that a float rule matches and that opened while its app was
/// hidden floats once the app shows, instead of taking a column. (#380)
#[test]
fn a_window_of_a_hidden_app_follows_its_float_rule_when_shown() {
    let config = crate::config::Config::try_from(
        "[bindings]\n[windows.float_test]\ntitle = \".*\"\nbundle_id = \"test\"\nfloating = true\n",
    )
    .expect("config should parse");
    let harness = TestHarness::new().with_config(config);
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = true);
    let mut harness = harness.with_windows(1);
    harness.advance(Duration::from_secs(2));

    show_test_app(&mut harness);
    assert!(!tiled(&mut harness, 0), "a floating window takes no column");
    assert_eq!(unmanaged(&mut harness, 0).as_deref(), Some("Floating"));
}

/// A window that was minimized while its app was hidden stays out of the strip
/// once the app shows, because it is still minimized. (#380)
#[test]
fn a_minimized_window_of_a_hidden_app_takes_no_column_when_shown() {
    let harness = TestHarness::new();
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = true);
    let mut harness = harness
        .with_window(0, |window| window.minimized = true)
        .with_window(1, |_| {});
    harness.advance(Duration::from_secs(2));

    show_test_app(&mut harness);
    assert!(
        !tiled(&mut harness, 0),
        "a minimized window takes no column"
    );
    assert_eq!(unmanaged(&mut harness, 0).as_deref(), Some("Minimized"));
    assert!(
        tiled(&mut harness, 1),
        "the app's other window joins the strip"
    );
}

const LAUNCHED_HIDDEN_APP: Pid = 12;
const LAUNCHED_HIDDEN_WINDOW: WinID = 120;

/// Launches an app hidden, as a login item set to hide does. While the app is
/// hidden, Accessibility reports its window as `AXDialog`, so the engine does
/// not track it.
fn launch_hidden(harness: &mut TestHarness) {
    let state = harness.mock_state.clone();
    state.spawn_app(LAUNCHED_HIDDEN_APP, "com.example.login-item", "Login item");
    state.update_app(LAUNCHED_HIDDEN_APP, |app| app.is_hidden = true);
    state.spawn_window(
        LAUNCHED_HIDDEN_APP,
        TEST_WORKSPACE_ID,
        LAUNCHED_HIDDEN_WINDOW,
        IRect::new(200, 100, 900, 600),
    );
    state.update_window(LAUNCHED_HIDDEN_WINDOW, |window| {
        window.listed_as_dialog = true;
    });
    let world = harness.world();
    let mut commands = world.commands();
    let watch = launch_watch("Login item", &mut commands);
    commands.spawn((
        watch,
        BProcess(Box::new(state.create_process(LAUNCHED_HIDDEN_APP))),
    ));
    world.flush();
}

/// Shows the app, as a click on its Dock icon does. macOS sends the show
/// before the window stops reading as `AXDialog`, so the engine's first look
/// at the app can still miss the window.
fn show_launched_hidden_app(harness: &mut TestHarness) {
    let state = harness.mock_state.clone();
    state.update_app(LAUNCHED_HIDDEN_APP, |app| app.is_hidden = false);
    harness.world().write_message(Event::ApplicationVisible {
        pid: LAUNCHED_HIDDEN_APP,
    });
    harness.advance(Duration::from_millis(100));
    state.update_window(LAUNCHED_HIDDEN_WINDOW, |window| {
        window.listed_as_dialog = false;
    });
}

/// A window of an app that launched hidden joins the strip within 2 seconds
/// of the show, however long the app stayed hidden. (#431)
#[test]
fn a_window_of_an_app_launched_hidden_tiles_when_shown() {
    for hidden_for in [Duration::from_secs(20), Duration::from_secs(60)] {
        let mut harness = TestHarness::new().with_windows(1);
        launch_hidden(&mut harness);
        harness.advance(hidden_for);
        assert!(
            !tiled(&mut harness, LAUNCHED_HIDDEN_WINDOW),
            "a hidden app's window takes no column"
        );

        show_launched_hidden_app(&mut harness);
        harness.advance(Duration::from_millis(1900));
        assert!(
            tiled(&mut harness, LAUNCHED_HIDDEN_WINDOW),
            "after {hidden_for:?} hidden, the window joins the strip within 2 s of the show"
        );
    }
}

/// An app that launches hidden and is never shown takes no column. (#431)
#[test]
fn an_app_launched_hidden_and_never_shown_takes_no_column() {
    let mut harness = TestHarness::new().with_windows(1);
    launch_hidden(&mut harness);
    harness.advance(Duration::from_secs(60));
    assert!(
        !tiled(&mut harness, LAUNCHED_HIDDEN_WINDOW),
        "a hidden app's window takes no column"
    );
}
