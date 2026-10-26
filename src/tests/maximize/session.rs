//! A maximized window across an engine restart. The engine saves its session
//! when a window is maximized or restored, and the next engine brings the
//! window back maximized, with the width and stack it returns to.

use std::path::{Path, PathBuf};

use crate::ecs::state::{PaneruState, SavedColumn, SavedMaximize, StateFile};

use super::*;

/// Has `harness` save its session to a file of its own, and returns it.
fn save_session_of(harness: &mut TestHarness, name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "paneru-maximize-{name}-{}.json",
        std::process::id()
    ));
    _ = std::fs::remove_file(&path);
    harness.world().insert_resource(StateFile(path.clone()));
    path
}

/// The maximize saved in the session at `path` for window `id`.
fn saved_maximize(path: &Path, id: WinID) -> Option<SavedMaximize> {
    let state = PaneruState::load_from_file(path)?;
    state
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.strips)
        .flat_map(|strip| &strip.columns)
        .find_map(|column| match column {
            SavedColumn::Single(window) if window.window_id == id => Some(window),
            _ => None,
        })
        .and_then(|window| window.maximized.clone())
}

/// Starts a new engine on windows `ids` at the frames `harness` left them
/// at, with the session saved at `path`, as after a crash. `displays` first
/// changes the displays the new engine finds.
fn restart(
    harness: &TestHarness,
    path: &Path,
    ids: &[WinID],
    displays: impl FnOnce(&mut MockState),
) -> TestHarness {
    let state = PaneruState::load_from_file(path).expect("a saved session");
    _ = std::fs::remove_file(path);
    let mut restarted = TestHarness::new();
    displays(&mut restarted.mock_state);
    for &id in ids {
        let frame = os_frame(harness, id);
        restarted = restarted.with_window(id, |window| window.frame = frame);
    }
    let mut restarted = restarted.with_state(state);
    restarted.advance(SETTLE);
    restarted
}

/// Focuses window `id`, as the user does to bring it into view. An engine
/// focuses the first window of the strip when it starts.
fn show(harness: &mut TestHarness, id: WinID) {
    harness.mock_state.focus_window(id);
    harness.advance(SETTLE);
}

fn is_maximized(harness: &mut TestHarness, id: WinID) -> bool {
    let entity = find_window_entity(id, harness.world());
    let world = harness.world();
    world
        .query::<&LayoutStrip>()
        .iter(world)
        .any(|strip| strip.maximized(entity).is_some())
}

#[test]
fn a_toggle_saves_the_session() {
    let mut harness = three_windows_one_focused();
    let path = save_session_of(&mut harness, "toggle-saves");

    double_click_title_bar(&mut harness, 1);
    assert!(
        saved_maximize(&path, 1).is_some(),
        "maximizing saves the session"
    );

    harness.run(vec![command(Operation::Maximize)]);
    harness.advance(SETTLE);
    let saved = PaneruState::load_from_file(&path);
    _ = std::fs::remove_file(&path);
    assert!(saved.is_some(), "the session is saved");
    assert!(
        saved_maximize(&path, 1).is_none(),
        "restoring saves the session"
    );
}

#[test]
fn restart_keeps_a_maximized_window_and_its_earlier_width() {
    let mut harness = three_windows_one_focused();
    let path = save_session_of(&mut harness, "earlier-width");
    harness.run(vec![command(Operation::SetWidth(0.6))]);
    harness.advance(SETTLE);
    let before = frames(&harness, &[0, 1, 2]);
    double_click_title_bar(&mut harness, 1);

    let mut restarted = restart(&harness, &path, &[0, 1, 2], |_| {});
    assert!(
        is_maximized(&mut restarted, 1),
        "maximized after the restart"
    );
    show(&mut restarted, 1);
    assert_eq!(os_frame(&restarted, 1), usable_area(&mut restarted));

    double_click_title_bar(&mut restarted, 1);
    assert!(!is_maximized(&mut restarted, 1));
    assert_eq!(
        frames(&restarted, &[0, 1, 2]),
        before,
        "restore returns the width it had before the restart"
    );
}

#[test]
fn restart_keeps_its_source_stack() {
    let mut harness = stacked();
    let path = save_session_of(&mut harness, "source-stack");
    let before = frames(&harness, &[0, 1, 2]);
    double_click_title_bar(&mut harness, 1);

    let mut restarted = restart(&harness, &path, &[0, 1, 2], |_| {});
    assert!(
        is_maximized(&mut restarted, 1),
        "maximized after the restart"
    );
    assert_eq!(
        layout_on(&mut restarted, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );

    double_click_title_bar(&mut restarted, 1);
    assert_eq!(
        layout_on(&mut restarted, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]],
        "restore rejoins the stack it left before the restart"
    );
    assert_eq!(frames(&restarted, &[0, 1, 2]), before);
}

/// The space comes back on a larger display, as when the laptop's lid is
/// closed. The maximized window fills that display.
#[test]
fn a_maximized_window_restored_on_another_display_fills_it() {
    let mut harness = three_windows_one_focused();
    let path = save_session_of(&mut harness, "other-display");
    double_click_title_bar(&mut harness, 1);

    let mut restarted = restart(&harness, &path, &[0, 1, 2], |mock| {
        mock.remove_display(TEST_DISPLAY_ID);
        mock.add_display(
            EXT_DISPLAY_ID,
            IRect::new(0, 0, EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID],
        );
    });
    assert!(
        is_maximized(&mut restarted, 1),
        "maximized after the restart"
    );
    show(&mut restarted, 1);
    let area = usable_area_of(&mut restarted, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&restarted, 1), area);
}
