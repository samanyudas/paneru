//! Dragging a window in Mission Control onto another desktop. macOS moves the
//! window into that desktop's Space and reports the move. The window must
//! join the strip of that Space, on whichever display shows it, and stay
//! there.

use std::time::Duration;

use bevy::prelude::*;

use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::{Display, Window};
use crate::platform::{WinID, WorkspaceId};

use super::*;

const EXT: u32 = 3;
const BUILTIN: u32 = TEST_DISPLAY_ID;
const EXT_SPACE: WorkspaceId = 515;
const EXT_OTHER_SPACE: WorkspaceId = 516;
const BUILTIN_SPACE: WorkspaceId = 1214;
const SETTLE: Duration = Duration::from_secs(5);
const WIDTH: i32 = 600;
const HEIGHT: i32 = 900;

fn ext_bounds() -> IRect {
    IRect::new(0, 0, 2560, 1440)
}

fn builtin_bounds() -> IRect {
    IRect::new(-2048, 108, 0, 1440)
}

fn spawn_row(harness: &TestHarness, space: WorkspaceId, ids: &[WinID], bounds: IRect) {
    for (i, &id) in (0..).zip(ids) {
        let x = bounds.min.x + i * WIDTH;
        let y = bounds.min.y + TEST_MENUBAR_HEIGHT;
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            space,
            id,
            IRect::new(x, y, x + WIDTH, y + HEIGHT),
        );
    }
}

/// Each strip that holds a window, as its Space, its display, and its window
/// IDs in order.
fn strips(world: &mut World) -> Vec<(WorkspaceId, u32, Vec<WinID>)> {
    let ids: std::collections::HashMap<_, _> = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect();
    let displays: std::collections::HashMap<_, _> = world
        .query::<(Entity, &Display)>()
        .iter(world)
        .map(|(entity, display)| (entity, display.id()))
        .collect();
    let mut strips: Vec<_> = world
        .query::<(&LayoutStrip, &ChildOf)>()
        .iter(world)
        .map(|(strip, child)| {
            (
                strip.id(),
                displays[&child.parent()],
                strip
                    .all_windows()
                    .iter()
                    .map(|entity| ids[entity])
                    .collect(),
            )
        })
        .filter(|(_, _, windows): &(_, _, Vec<WinID>)| !windows.is_empty())
        .collect();
    strips.sort_unstable();
    strips
}

fn strip_of(world: &mut World, id: WinID) -> Option<(WorkspaceId, u32)> {
    strips(world)
        .into_iter()
        .find(|(_, _, windows)| windows.contains(&id))
        .map(|(space, display, _)| (space, display))
}

/// The built-in display, left of the external main display, shows `[10]
/// [11]`. The external display shows `[50] [51] [52]` beside an empty Space.
/// `focused` has the focus.
fn two_displays(focused: WinID) -> TestHarness {
    let mut harness = TestHarness::new();
    harness
        .mock_state
        .add_display(BUILTIN, builtin_bounds(), vec![BUILTIN_SPACE]);
    harness
        .mock_state
        .add_display(EXT, ext_bounds(), vec![EXT_SPACE, EXT_OTHER_SPACE]);
    harness.mock_state.activate_display(EXT);
    harness.mock_state.set_spaces_follow_frames(true);
    spawn_row(&harness, BUILTIN_SPACE, &[10, 11], builtin_bounds());
    spawn_row(&harness, EXT_SPACE, &[50, 51, 52], ext_bounds());
    harness.advance(Duration::from_secs(2));
    let display = if builtin_bounds().contains(harness.mock_state.ax_frame(focused).center()) {
        BUILTIN
    } else {
        EXT
    };
    harness.mock_state.focus_window(focused);
    harness.mock_state.os_activate_display(display);
    harness.advance(Duration::from_secs(2));
    let mut expected = vec![
        (EXT_SPACE, EXT, vec![50, 51, 52]),
        (BUILTIN_SPACE, BUILTIN, vec![10, 11]),
    ];
    expected.sort_unstable();
    assert_eq!(strips(harness.world()), expected);
    harness
}

/// Mission Control opens, the person drops `id` on another desktop, and
/// Mission Control closes. macOS moves the window by `shift` into `space`,
/// and reports the move while Mission Control still shows.
fn drag_in_mission_control(harness: &mut TestHarness, id: WinID, shift: IVec2, space: WorkspaceId) {
    harness
        .world()
        .write_message(Event::MissionControlShowAllWindows);
    harness.advance(Duration::from_millis(500));
    harness.mock_state.update_window(id, |window| {
        window.frame.min += shift;
        window.frame.max += shift;
        window.workspace_id = space;
    });
    harness
        .world()
        .write_message(Event::WindowMoved { window_id: id });
    harness.advance(Duration::from_millis(500));
    harness.world().write_message(Event::MissionControlExit);
    harness.advance(SETTLE);
}

fn to_builtin() -> IVec2 {
    IVec2::new(builtin_bounds().min.x + 200, builtin_bounds().min.y + 100)
}

#[test]
fn a_window_dragged_to_the_other_display_joins_its_strip_whichever_display_has_focus() {
    for focused in [10, 50] {
        let mut harness = two_displays(focused);
        let shift = to_builtin() - harness.mock_state.ax_frame(51).min;
        drag_in_mission_control(&mut harness, 51, shift, BUILTIN_SPACE);
        assert_eq!(
            strip_of(harness.world(), 51),
            Some((BUILTIN_SPACE, BUILTIN)),
            "focus on {focused}: the window joins the built-in strip, strips {:?}",
            strips(harness.world())
        );

        // Later layouts of either display keep it there.
        harness.mock_state.focus_window(50);
        harness.mock_state.os_activate_display(EXT);
        harness.advance(SETTLE);
        harness.mock_state.focus_window(10);
        harness.mock_state.os_activate_display(BUILTIN);
        harness.advance(SETTLE);
        assert_eq!(
            strip_of(harness.world(), 51),
            Some((BUILTIN_SPACE, BUILTIN)),
            "focus on {focused}: the window stays in the built-in strip, strips {:?}",
            strips(harness.world())
        );
        assert!(
            builtin_bounds().contains(harness.mock_state.ax_frame(51).center()),
            "focus on {focused}: the window is on the built-in display at {:?}",
            harness.mock_state.ax_frame(51)
        );
    }
}

#[test]
fn a_window_dragged_to_another_desktop_of_its_display_joins_that_desktops_strip() {
    let mut harness = two_displays(50);
    drag_in_mission_control(&mut harness, 51, IVec2::ZERO, EXT_OTHER_SPACE);
    assert_eq!(
        strip_of(harness.world(), 51),
        Some((EXT_OTHER_SPACE, EXT)),
        "strips {:?}",
        strips(harness.world())
    );
}
