//! Maximizing a window by double-clicking its title bar, and restoring it.

use std::time::Duration;

use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::MainOptions;
use crate::ecs::DockPosition;
use crate::events::Event;
use crate::manager::{Display, Origin, Size};
use crate::platform::WinID;

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// The window's frame as the OS has it.
fn os_frame(harness: &TestHarness, id: WinID) -> IRect {
    let mut frame = None;
    harness
        .mock_state
        .update_window(id, |window| frame = Some(window.frame));
    frame.expect("window exists")
}

fn frames(harness: &TestHarness, ids: &[WinID]) -> Vec<IRect> {
    ids.iter().map(|id| os_frame(harness, *id)).collect()
}

fn usable_area(harness: &mut TestHarness) -> IRect {
    let world = harness.world();
    world
        .query::<(&Display, Option<&DockPosition>)>()
        .iter(world)
        .next()
        .map(|(display, dock)| display.visible_bounds(dock))
        .expect("a display")
}

/// A point in the window's title bar.
fn title_bar(harness: &TestHarness, id: WinID) -> Origin {
    let frame = os_frame(harness, id);
    Origin::new(frame.center().x, frame.min.y + 10)
}

fn double_click_title_bar(harness: &mut TestHarness, id: WinID) {
    let point = title_bar(harness, id);
    harness.mock_state.simulate_double_click(point);
    harness.advance(SETTLE);
}

/// Three tiled windows, with window 1 clicked once so the strip has
/// already moved to show it.
fn three_windows() -> TestHarness {
    let mut harness = TestHarness::new().with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_click(point);
    harness.advance(SETTLE);
    harness
}

#[test]
fn a_title_bar_double_click_maximizes_and_the_next_restores() {
    let mut harness = three_windows();
    let before = frames(&harness, &[0, 1, 2]);

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));
    let others = frames(&harness, &[0, 2]);
    assert_eq!(others, vec![before[0], before[2]], "neighbors stay put");

    double_click_title_bar(&mut harness, 1);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

/// Repeated toggles return the window to the same frame every time.
#[test]
fn repeated_toggles_do_not_drift() {
    let mut harness = three_windows();
    let before = frames(&harness, &[0, 1, 2]);
    for _ in 0..3 {
        double_click_title_bar(&mut harness, 1);
        double_click_title_bar(&mut harness, 1);
    }
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

#[test]
fn a_double_click_on_a_title_bar_control_is_left_to_the_control() {
    let mut harness = three_windows();
    let before = os_frame(&harness, 1);
    let point = title_bar(&harness, 1);
    harness.mock_state.update_window(1, |window| {
        window.controls = vec![IRect::from_center_size(point, IVec2::splat(20))];
    });

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), before);
}

#[test]
fn a_double_click_below_the_title_bar_is_left_to_the_window() {
    let mut harness = three_windows();
    let before = os_frame(&harness, 1);

    harness.mock_state.simulate_double_click(before.center());
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), before);
}

/// macOS answers the same double-click with the app's own zoom, which can
/// land after the engine's write. On this rig Finder zoomed to 868 points
/// wide, and the engine took that as the column's new width.
#[test]
fn the_apps_own_zoom_does_not_replace_the_toggle() {
    let mut harness = three_windows();
    let before = frames(&harness, &[0, 1, 2]);
    let zoom = Size::new(300, 500);

    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_double_click(point);
    harness.advance(Duration::from_millis(100));
    harness.mock_state.os_resize_window(1, zoom);
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));

    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_double_click(point);
    harness.advance(Duration::from_millis(100));
    harness.mock_state.os_resize_window(1, zoom);
    harness.advance(SETTLE);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

/// Stacks window 1 under window 0.
fn stacked() -> TestHarness {
    let mut harness = TestHarness::new().with_windows(3);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_click(point);
    harness.advance(SETTLE);
    harness
}

#[test]
fn a_stacked_window_returns_to_its_place_in_the_stack() {
    let mut harness = stacked();
    let before = frames(&harness, &[0, 1, 2]);
    assert_eq!(before[0].max.y, before[1].min.y, "1 is under 0");

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));
    assert_eq!(os_frame(&harness, 0), before[0], "its stack mate stays put");

    double_click_title_bar(&mut harness, 1);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

/// The stack changes while its member is maximized, and the restored
/// window takes its place in the stack as it is now.
#[test]
fn restoring_after_the_stack_changed_uses_the_stack_as_it_is() {
    let mut harness = stacked();
    let top = os_frame(&harness, 0);

    double_click_title_bar(&mut harness, 1);
    harness.mock_state.os_close_window(0);
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));

    double_click_title_bar(&mut harness, 1);
    let restored = os_frame(&harness, 1);
    assert_eq!(
        restored.min.y, top.min.y,
        "the only window left fills the column"
    );
    assert_eq!(restored.height(), usable_area(&mut harness).height());
}

#[test]
fn a_floating_window_returns_to_its_frame() {
    let mut harness = three_windows();
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Manage)]);
    harness.advance(SETTLE);
    let entity = find_window_entity(1, harness.world());
    assert!(
        harness
            .world()
            .get::<crate::ecs::Unmanaged>(entity)
            .is_some(),
        "window 1 floats"
    );
    let before = os_frame(&harness, 1);

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), before);
}

/// macOS ignores a small shrink that leaves a window's bottom edge near the
/// bottom of the screen. A slot that ends just above that edge is still
/// reached from the maximized frame, which ends on it.
#[test]
fn a_window_returns_to_a_slot_that_ends_near_the_screen_edge() {
    let options = MainOptions {
        padding_top: Some(8),
        padding_bottom: Some(8),
        ..MainOptions::default()
    };
    let mut harness = TestHarness::new()
        .with_config((options, vec![]).into())
        .with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_click(point);
    harness.advance(SETTLE);
    harness.mock_state.update_window(1, |window| {
        window.sticky_bottom = Some(TEST_DISPLAY_HEIGHT);
    });
    let before = frames(&harness, &[0, 1, 2]);
    assert_eq!(before[1].max.y, TEST_DISPLAY_HEIGHT - 8);

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));

    double_click_title_bar(&mut harness, 1);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);

    // The app's own zoom lands after the restore and fills the column to
    // the edge again, as Helium's did on the rig.
    double_click_title_bar(&mut harness, 1);
    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_double_click(point);
    harness.advance(Duration::from_millis(100));
    harness
        .mock_state
        .os_resize_window(1, Size::new(before[1].width(), before[1].height() + 8));
    harness.advance(SETTLE);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

/// A maximized window the user dragged away still restores from its top
/// bar where it is now.
#[test]
fn a_dragged_maximized_window_restores_from_where_it_is() {
    // Queued writes keep the frame the engine last wrote or read, as the
    // real window does.
    let harness = TestHarness::new();
    harness.mock_state.queue_writes(TEST_PROCESS_ID);
    let mut harness = harness.with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    let point = title_bar(&harness, 1);
    harness.mock_state.simulate_click(point);
    harness.advance(SETTLE);
    let before = frames(&harness, &[0, 1, 2]);
    double_click_title_bar(&mut harness, 1);
    harness.mock_state.os_move_window(1, Origin::new(200, 300));
    harness.advance(SETTLE);

    double_click_title_bar(&mut harness, 1);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

const EXT_BOUNDS: IRect = IRect {
    min: IVec2::new(0, -EXT_DISPLAY_HEIGHT),
    max: IVec2::new(EXT_DISPLAY_WIDTH, 0),
};

/// The usable area of display `id`.
fn usable_area_of(harness: &mut TestHarness, id: u32) -> IRect {
    let world = harness.world();
    world
        .query::<(&Display, Option<&DockPosition>)>()
        .iter(world)
        .find(|(display, _)| display.id() == id)
        .map(|(display, dock)| display.visible_bounds(dock))
        .expect("the display")
}

/// The larger display placed to the right of the built-in display.
const EAST_EXT_BOUNDS: IRect = IRect {
    min: IVec2::new(TEST_DISPLAY_WIDTH, 0),
    max: IVec2::new(TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
};

/// Three tiled windows on the built-in display, with a larger display above
/// it, and window 1 focused and maximized.
fn maximized_below_a_larger_display() -> TestHarness {
    maximized_beside(EXT_BOUNDS)
}

/// Three tiled windows on the built-in display, a larger display at
/// `bounds` with one window, and window 1 focused and maximized.
fn maximized_beside(bounds: IRect) -> TestHarness {
    let mut harness = TestHarness::new()
        .with_windows(3)
        .with_display(EXT_DISPLAY_ID, bounds, vec![EXT_WORKSPACE_ID])
        .with_window(100, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
            window.frame = IRect::from_corners(
                bounds.min + IVec2::new(100, 100),
                bounds.min + IVec2::new(900, 1100),
            );
        });
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    double_click_title_bar(&mut harness, 1);
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);
    harness
}

/// Moves the focused window 1 to the display in `direction`, as the
/// display commands do.
fn move_to_display(harness: &mut TestHarness, direction: Direction, display: u32, space: u64) {
    harness.run(vec![command(Operation::MoveToDisplay(direction))]);
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = space);
    harness.mock_state.os_activate_display(display);
    harness.advance(SETTLE);
}

/// Drags window 1 by its title bar to `origin`, with the pointer released
/// at `release`.
fn drag_to(harness: &mut TestHarness, origin: Origin, release: Origin, space: u64) {
    let start = title_bar(harness, 1);
    harness.mock_state.simulate_drag(start, release);
    harness.mock_state.os_move_window(1, origin);
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = space);
    harness.advance(SETTLE);
}

/// A maximized window moved by the display commands stays maximized and
/// fills the usable area of each display it reaches, and keeps the focus.
#[test]
fn a_maximized_window_moved_to_another_display_fills_it() {
    let mut harness = maximized_below_a_larger_display();

    move_to_display(
        &mut harness,
        Direction::North,
        EXT_DISPLAY_ID,
        EXT_WORKSPACE_ID,
    );
    let area = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);
    assert_focused!(harness.world(), 1);

    move_to_display(
        &mut harness,
        Direction::South,
        TEST_DISPLAY_ID,
        TEST_WORKSPACE_ID,
    );
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);
    assert_focused!(harness.world(), 1);
}

/// A maximized window dragged by its title bar to another display fills
/// that display's usable area, in both directions. A drag within one
/// display leaves it where the user dropped it.
#[test]
fn a_maximized_window_dragged_to_another_display_fills_it() {
    let mut harness = maximized_below_a_larger_display();

    // macOS can drop the window below the pointer: on a native rig Finder
    // ended 18 points lower.
    drag_to(
        &mut harness,
        Origin::new(0, -1082),
        Origin::new(500, -1100),
        EXT_WORKSPACE_ID,
    );
    let area = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);

    // Dropped low on the large display, the window's centre is on the
    // display below, but the pointer and macOS keep it on the large one.
    drag_to(
        &mut harness,
        Origin::new(0, -500),
        Origin::new(500, -490),
        EXT_WORKSPACE_ID,
    );
    assert_eq!(os_frame(&harness, 1).min, Origin::new(0, -500));
    assert_eq!(os_frame(&harness, 1).size(), area.size());

    drag_to(
        &mut harness,
        Origin::new(-200, 100),
        Origin::new(500, 110),
        TEST_WORKSPACE_ID,
    );
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);

    drag_to(
        &mut harness,
        Origin::new(50, 60),
        Origin::new(500, 70),
        TEST_WORKSPACE_ID,
    );
    assert_eq!(os_frame(&harness, 1).min, Origin::new(50, 60));
}

/// With the larger display to the right, a maximized window moved by the
/// display commands or dragged across fills each display it reaches.
#[test]
fn a_maximized_window_moved_between_side_by_side_displays_fills_them() {
    let mut harness = maximized_beside(EAST_EXT_BOUNDS);
    let ext = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    let test = usable_area_of(&mut harness, TEST_DISPLAY_ID);

    move_to_display(
        &mut harness,
        Direction::East,
        EXT_DISPLAY_ID,
        EXT_WORKSPACE_ID,
    );
    assert_eq!(os_frame(&harness, 1), ext);
    assert_focused!(harness.world(), 1);

    move_to_display(
        &mut harness,
        Direction::West,
        TEST_DISPLAY_ID,
        TEST_WORKSPACE_ID,
    );
    assert_eq!(os_frame(&harness, 1), test);
    assert_focused!(harness.world(), 1);

    drag_to(
        &mut harness,
        Origin::new(1200, 40),
        Origin::new(1700, 50),
        EXT_WORKSPACE_ID,
    );
    assert_eq!(os_frame(&harness, 1), ext);

    drag_to(
        &mut harness,
        Origin::new(-300, 40),
        Origin::new(200, 50),
        TEST_WORKSPACE_ID,
    );
    assert_eq!(os_frame(&harness, 1), test);
}
