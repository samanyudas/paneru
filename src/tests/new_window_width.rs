use std::time::Duration;

use bevy::math::IRect;

use crate::config::{Config, InitialWindowWidth, MainOptions, WindowParams};
use crate::ecs::SpawnWindowTrigger;
use crate::platform::WinID;

use super::*;

const HALF_DISPLAY: i32 = TEST_DISPLAY_WIDTH / 2;
const OPENED_WIDTH: i32 = 380;

fn half_width_config(rules: Vec<WindowParams>) -> Config {
    let options = MainOptions {
        new_window_width: Some(0.5),
        ..MainOptions::default()
    };
    (options, rules).into()
}

/// Starts the engine on windows 0 and 1, both `TEST_WINDOW_WIDTH` wide, and
/// lets the startup grace period pass.
fn started(config: Config) -> TestHarness {
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .with_focused_window(0);
    harness.advance(Duration::from_secs(3));
    harness
}

/// Shows window `id` at `OPENED_WIDTH` wide, focused as macOS focuses a
/// window that just opened, and lets it settle.
fn open_window(harness: &mut TestHarness, id: WinID) {
    let opened = IRect::new(100, TEST_MENUBAR_HEIGHT, 100 + OPENED_WIDTH, 700);
    let window = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, id, opened);
    harness.mock_state.focus_window(id);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(Duration::from_secs(2));
}

fn width(harness: &TestHarness, id: WinID) -> i32 {
    harness.mock_state.ax_frame(id).width()
}

#[test]
fn a_window_that_opens_while_the_engine_runs_starts_at_half_the_display() {
    let mut harness = started(half_width_config(vec![]));
    open_window(&mut harness, 2);

    assert_eq!(width(&harness, 2), HALF_DISPLAY);
    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn windows_found_at_startup_keep_their_widths() {
    let mut harness = started(half_width_config(vec![]));
    open_window(&mut harness, 2);

    assert_eq!(width(&harness, 0), TEST_WINDOW_WIDTH);
    assert_eq!(width(&harness, 1), TEST_WINDOW_WIDTH);
}

/// A slow scan can find a window that was open when the engine started
/// after startup has ended. It is not a new window, so it keeps its width.
#[test]
fn a_window_open_at_launch_that_is_found_late_keeps_its_width() {
    let mut harness = started(half_width_config(vec![]));
    harness.existed_at_launch([2]);
    open_window(&mut harness, 2);

    assert_eq!(width(&harness, 2), OPENED_WIDTH);
}

/// A window whose app shows it without a full-screen button waits for its
/// level check before it tiles, and then starts at the new-window width.
#[test]
fn a_window_that_waits_for_its_level_check_starts_at_half_the_display() {
    let mut harness = started(half_width_config(vec![]));
    let opened = IRect::new(100, TEST_MENUBAR_HEIGHT, 100 + OPENED_WIDTH, 700);
    let window = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, opened);
    harness
        .mock_state
        .update_window(2, |window| window.no_full_screen_button = true);
    harness.mock_state.focus_window(2);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(Duration::from_millis(500));
    assert_eq!(
        width(&harness, 2),
        OPENED_WIDTH,
        "the window keeps its width while its level check runs"
    );

    harness.advance(Duration::from_secs(2));
    assert_eq!(width(&harness, 2), HALF_DISPLAY);
}

#[test]
fn a_width_rule_wins_over_the_new_window_width() {
    let mut rule = WindowParams::new("^Window 2$", None);
    rule.width = Some(InitialWindowWidth::Ratio(0.25));
    let mut harness = started(half_width_config(vec![rule]));
    open_window(&mut harness, 2);

    assert_eq!(width(&harness, 2), TEST_DISPLAY_WIDTH / 4);
}

/// The new-window width never makes a window wider than the tiling area.
#[test]
fn the_new_window_width_stays_inside_the_tiling_area() {
    let options = MainOptions {
        new_window_width: Some(1.0),
        padding_left: Some(10),
        padding_right: Some(10),
        ..MainOptions::default()
    };
    let mut harness = started((options, vec![]).into());
    open_window(&mut harness, 2);

    assert_eq!(width(&harness, 2), TEST_DISPLAY_WIDTH - 20);
}

#[test]
fn without_the_option_a_new_window_keeps_its_width() {
    let mut harness = started(Config::default());
    open_window(&mut harness, 2);

    assert_eq!(width(&harness, 2), OPENED_WIDTH);
}

/// A window that is minimized when the engine first sees it keeps its width,
/// in the Dock and once it is restored.
#[test]
fn a_window_found_minimized_keeps_its_width() {
    let mut harness = started(half_width_config(vec![]));
    let opened = IRect::new(100, TEST_MENUBAR_HEIGHT, 100 + OPENED_WIDTH, 700);
    let window = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, opened);
    harness
        .mock_state
        .update_window(2, |window| window.minimized = true);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(Duration::from_secs(2));

    assert_eq!(width(&harness, 2), OPENED_WIDTH);
}

/// Hides the app of window 2 or shows it again, as Cmd-H and a click on the
/// app's Dock icon do, and lets the change settle.
fn set_hidden(harness: &mut TestHarness, hidden: bool) {
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = hidden);
    let event = if hidden {
        crate::events::Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        }
    } else {
        crate::events::Event::ApplicationVisible {
            pid: TEST_PROCESS_ID,
        }
    };
    harness.run(vec![event]);
    harness.advance(Duration::from_secs(2));
}

/// A window that its app opens while hidden is new when the app shows, so
/// it starts at the new-window width then.
#[test]
fn a_window_that_a_hidden_app_opens_starts_at_half_the_display_when_shown() {
    let mut harness = started(half_width_config(vec![]));
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_hidden = true);
    let opened = IRect::new(100, TEST_MENUBAR_HEIGHT, 100 + OPENED_WIDTH, 700);
    let window = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, opened);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(Duration::from_secs(2));
    assert_eq!(
        width(&harness, 2),
        OPENED_WIDTH,
        "a hidden window keeps its width"
    );

    set_hidden(&mut harness, false);
    assert_eq!(width(&harness, 2), HALF_DISPLAY);
    assert_eq!(
        width(&harness, 0),
        TEST_WINDOW_WIDTH,
        "a startup window keeps its width"
    );
}

/// A window that tiled before its app hid is not new when the app shows
/// again, so it keeps the width it had.
#[test]
fn a_window_keeps_its_width_when_its_app_hides_and_shows() {
    let mut harness = started(half_width_config(vec![]));
    open_window(&mut harness, 2);
    harness.run(vec![crate::events::Event::Command {
        command: crate::commands::Command::Window(crate::commands::Operation::Resize(
            crate::commands::ResizeDirection::Shrink,
        )),
    }]);
    harness.advance(Duration::from_secs(2));
    let resized = width(&harness, 2);

    set_hidden(&mut harness, true);
    set_hidden(&mut harness, false);
    assert_eq!(width(&harness, 2), resized);
}

/// A window whose creation macOS never reported joins the strip when the
/// engine reads its space, and starts at the new-window width like any new
/// window.
#[test]
fn a_window_found_by_a_space_read_starts_at_half_the_display() {
    let mut harness = started(half_width_config(vec![]));
    let opened = IRect::new(100, TEST_MENUBAR_HEIGHT, 100 + OPENED_WIDTH, 700);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, opened);
    harness.run(vec![crate::events::Event::MissionControlExit]);
    harness.advance(Duration::from_secs(2));

    let entity = find_window_entity(2, harness.world());
    let mut strips = harness.world().query::<&crate::ecs::layout::LayoutStrip>();
    assert!(
        strips
            .iter(harness.world())
            .any(|strip| strip.contains(entity)),
        "the space read tiles the window"
    );
    assert_eq!(width(&harness, 2), HALF_DISPLAY);
}
