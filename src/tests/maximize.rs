//! Maximizing a window by double-clicking its title bar, and restoring it.

use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::drag::{DragFeedback, DragState, OverlayShape};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{DockPosition, Position, SelectedVirtualMarker};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::{Modifiers, WinID, WorkspaceId};

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

/// The windows of the shown strip on `workspace`, column by column.
fn layout_on(harness: &mut TestHarness, workspace: WorkspaceId) -> Vec<Vec<WinID>> {
    let world = harness.world();
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query_filtered::<&LayoutStrip, With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|strip| strip.id() == workspace)
        .expect("a strip for the workspace")
        .columns()
        .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
        .collect()
}

/// Moves the pointer with the left button held from `from` to `to` in three
/// steps, with window 1 following each one as macOS moves it. Returns what
/// the drag overlay shows after each step.
fn drag_steps(harness: &mut TestHarness, from: Origin, to: Origin) -> Vec<Option<OverlayShape>> {
    let origin = os_frame(harness, 1).min;
    (1..=3)
        .map(|step| {
            let point = from + (to - from) * step / 3;
            harness.world().write_message(Event::MouseDragged {
                point: CGPoint::new(point.x.into(), point.y.into()),
                modifiers: Modifiers::empty(),
                button: MouseButton::Left,
            });
            harness
                .mock_state
                .os_move_window(1, origin + (point - from));
            harness.advance(Duration::from_millis(16));
            harness.world().resource::<DragFeedback>().shape
        })
        .collect()
}

/// Presses the left button at `point` on window 1's title bar.
fn press_title_bar(harness: &mut TestHarness, point: Origin) {
    harness.mock_state.set_left_button_down(true);
    harness.world().write_message(Event::MouseDown {
        point: CGPoint::new(point.x.into(), point.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
        window_id: Some(1),
        clicks: 1,
    });
    harness.advance(Duration::from_millis(16));
}

/// Releases the left button at `point`, with macOS reporting window 1 in
/// `space`, and lets the layout settle.
fn release_at(harness: &mut TestHarness, point: Origin, space: WorkspaceId) {
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(Event::MouseUp {
        point: CGPoint::new(point.x.into(), point.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    });
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = space);
    harness.advance(SETTLE);
}

/// The position of the shown strip on `workspace`.
fn strip_position(harness: &mut TestHarness, workspace: WorkspaceId) -> IVec2 {
    let world = harness.world();
    world
        .query_filtered::<(&LayoutStrip, &Position), With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|(strip, _)| strip.id() == workspace)
        .map(|(_, position)| position.0)
        .expect("a strip for the workspace")
}

/// A maximized window dragged by its title bar shows no drop line over its
/// own display, and the usual line over another one. Dropped there, it
/// joins that display's layout at the line, fills the display, and keeps
/// the focus. Restored, it takes its new slot.
#[test]
fn a_maximized_window_dragged_to_another_display_joins_its_layout() {
    let mut harness = maximized_below_a_larger_display();
    let press = Origin::new(500, 30);
    let within = Origin::new(700, 200);
    let across = Origin::new(700, -600);

    press_title_bar(&mut harness, press);
    let own = drag_steps(&mut harness, press, within);
    assert_eq!(own, vec![None; 3], "no line over its own display");
    let other = drag_steps(&mut harness, within, across);
    assert!(
        matches!(other.last(), Some(Some(OverlayShape::Line(_)))),
        "a line over the other display: {other:?}"
    );

    release_at(&mut harness, across, EXT_WORKSPACE_ID);

    assert_eq!(
        layout_on(&mut harness, EXT_WORKSPACE_ID),
        vec![vec![100], vec![1]]
    );
    assert!(
        !layout_on(&mut harness, TEST_WORKSPACE_ID)
            .concat()
            .contains(&1),
        "gone from the old display's layout"
    );
    let area = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);
    assert_focused!(harness.world(), 1);

    double_click_title_bar(&mut harness, 1);
    let restored = os_frame(&harness, 1);
    assert!(EXT_BOUNDS.contains(restored.min) && EXT_BOUNDS.contains(restored.max));
    assert!(
        restored.width() < area.width(),
        "back in a slot: {restored:?}"
    );
}

/// The engine stops reading a maximized window's moves. An app that moves
/// the window afterwards does not stop a drag from its title bar where it is
/// now.
#[test]
fn a_maximized_window_an_app_moved_still_drags_to_another_display() {
    let mut harness = maximized_below_a_larger_display();
    harness.mock_state.os_move_window(1, Origin::new(0, 200));
    harness.advance(SETTLE);
    let press = Origin::new(500, 210);
    let across = Origin::new(700, -600);

    press_title_bar(&mut harness, press);
    let shapes = drag_steps(&mut harness, press, across);
    assert!(
        matches!(shapes.last(), Some(Some(OverlayShape::Line(_)))),
        "a line over the other display: {shapes:?}"
    );
    release_at(&mut harness, across, EXT_WORKSPACE_ID);

    assert_eq!(
        layout_on(&mut harness, EXT_WORKSPACE_ID),
        vec![vec![100], vec![1]]
    );
    let area = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);
}

/// A drag of a maximized window within its display whose release never
/// reaches the engine ends when the button reads up. The window stays where
/// it was let go, and the strip behind it does not scroll to its slot.
#[test]
fn a_lost_release_of_a_maximized_window_on_its_display_keeps_the_strip() {
    let mut harness = maximized_below_a_larger_display();
    // Scroll the strip so window 1's slot is off the display.
    let world = harness.world();
    let strip = world
        .query_filtered::<(Entity, &LayoutStrip), With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|(_, strip)| strip.id() == TEST_WORKSPACE_ID)
        .map(|(entity, _)| entity)
        .expect("a strip");
    world
        .entity_mut(strip)
        .get_mut::<Position>()
        .expect("a position")
        .0
        .x -= 2000;
    harness.advance(SETTLE);
    let scrolled = strip_position(&mut harness, TEST_WORKSPACE_ID);
    let press = Origin::new(500, 30);
    let within = Origin::new(600, 200);

    press_title_bar(&mut harness, press);
    drag_steps(&mut harness, press, within);
    assert!(
        matches!(
            harness.world().resource::<DragState>(),
            DragState::Dragging(_)
        ),
        "a drag"
    );
    harness.mock_state.set_left_button_down(false);
    harness.advance(SETTLE);

    assert!(!harness.world().resource::<DragState>().holds_window());
    assert_eq!(strip_position(&mut harness, TEST_WORKSPACE_ID), scrolled);
    assert_eq!(os_frame(&harness, 1).min, Origin::new(100, 190));
}

/// The frame the engine has the window show, padding applied.
fn shown(harness: &mut TestHarness, id: WinID) -> IRect {
    let entity = find_window_entity(id, harness.world());
    harness
        .world()
        .get::<Window>(entity)
        .expect("a window")
        .shown_frame()
}

fn sliver(harness: &mut TestHarness) -> i32 {
    harness.world().resource::<Config>().sliver_width()
}

/// No window in `others` shows over the part of maximized window `id` that
/// is on the display, except a parked sliver at the display's edge.
fn assert_nothing_covers(harness: &mut TestHarness, id: WinID, others: &[WinID]) {
    let display = IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
    let visible = os_frame(harness, id).intersect(display);
    let sliver = sliver(harness);
    for other in others {
        let overlap = shown(harness, *other).intersect(visible);
        assert!(
            overlap.is_empty() || overlap.width() <= sliver,
            "window {other} at {:?} covers {overlap:?} of window {id} at {visible:?}",
            shown(harness, *other),
        );
    }
}

fn focus(harness: &mut TestHarness, direction: Direction) {
    harness.run(vec![command(Operation::Focus(direction))]);
    harness.advance(SETTLE);
}

/// [`three_windows`] with window 1 focused, as a click on it leaves it.
fn three_windows_one_focused() -> TestHarness {
    let mut harness = three_windows();
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    harness
}

/// Focus moves scroll the strip past a maximized window. It slides away
/// beside its neighbours, stays maximized, and fills the usable area again
/// when it comes back.
#[test]
fn focus_east_and_west_scroll_past_a_maximized_window() {
    let mut harness = three_windows_one_focused();
    double_click_title_bar(&mut harness, 1);
    let area = usable_area(&mut harness);
    assert_eq!(os_frame(&harness, 1), area);

    focus(&mut harness, Direction::East);
    assert_focused!(harness.world(), 2);
    let shown_two = os_frame(&harness, 2);
    assert!(
        area.contains(shown_two.min) && area.contains(shown_two.max),
        "window 2 is in view: {shown_two:?}"
    );
    assert!(
        os_frame(&harness, 1).max.x <= shown_two.min.x,
        "window 1 slid away to the west: {:?}",
        os_frame(&harness, 1)
    );
    assert_eq!(os_frame(&harness, 1).size(), area.size(), "still maximized");

    focus(&mut harness, Direction::West);
    assert_focused!(harness.world(), 1);
    assert_eq!(os_frame(&harness, 1), area);

    focus(&mut harness, Direction::West);
    assert_focused!(harness.world(), 0);
    assert!(
        os_frame(&harness, 1).min.x >= os_frame(&harness, 0).max.x,
        "window 1 slid away to the east: {:?}",
        os_frame(&harness, 1)
    );

    focus(&mut harness, Direction::East);
    assert_eq!(os_frame(&harness, 1), area);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
}

/// A swipe scrolls the strip past a maximized window, and no neighbour is
/// drawn over it on the way there or back.
#[test]
fn a_swipe_scrolls_past_a_maximized_window() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    double_click_title_bar(&mut harness, 1);
    let area = usable_area(&mut harness);
    assert_eq!(os_frame(&harness, 1), area);

    let swipe = |harness: &mut TestHarness, delta: f64| {
        for _ in 0..10 {
            harness
                .world()
                .write_message(Event::Swipe { delta, fingers: 3 });
            harness.advance(Duration::from_millis(20));
            assert_eq!(os_frame(harness, 1).size(), area.size(), "still maximized");
            assert_nothing_covers(harness, 1, &[0, 2]);
        }
        harness.advance(SETTLE);
    };
    swipe(&mut harness, 0.1);
    assert_ne!(os_frame(&harness, 1), area, "the swipe moved window 1");
    assert_eq!(os_frame(&harness, 1).size(), area.size(), "still maximized");
    assert_nothing_covers(&mut harness, 1, &[0, 2]);

    swipe(&mut harness, -0.1);
    assert_eq!(os_frame(&harness, 1).size(), area.size(), "still maximized");
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
}

/// With Miri's default gaps (outer 8 and inner 8, so an edge padding of 4
/// and a window padding of 4), a maximized window fills the usable area
/// exactly, and no neighbour shows over it.
#[test]
fn no_neighbor_covers_a_maximized_window_with_gaps() {
    let options = MainOptions {
        padding_top: Some(4),
        padding_right: Some(4),
        padding_bottom: Some(4),
        padding_left: Some(4),
        ..MainOptions::default()
    };
    let mut harness = TestHarness::new()
        .with_config((options, vec![]).into())
        .with_windows(3);
    for id in 0..3 {
        harness.mock_state.update_window(id, |window| {
            window.horizontal_padding = 4;
            window.vertical_padding = 4;
        });
    }
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);

    double_click_title_bar(&mut harness, 1);
    let area = usable_area(&mut harness);
    assert_eq!(os_frame(&harness, 1), area, "no edge or window padding");
    assert_nothing_covers(&mut harness, 1, &[0, 2]);

    focus(&mut harness, Direction::East);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
    focus(&mut harness, Direction::West);
    assert_eq!(os_frame(&harness, 1), area);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
}

/// Restore gives the window the width it had before it was maximized, after
/// the strip scrolled past it, and its neighbours keep their sizes.
#[test]
fn restore_returns_the_earlier_width_and_neighbors_keep_theirs() {
    let mut harness = three_windows_one_focused();
    harness.run(vec![command(Operation::SetWidth(0.6))]);
    harness.advance(SETTLE);
    let before = frames(&harness, &[0, 1, 2]);
    assert_ne!(before[1].width(), before[0].width(), "a width of its own");

    double_click_title_bar(&mut harness, 1);
    focus(&mut harness, Direction::East);
    focus(&mut harness, Direction::West);
    double_click_title_bar(&mut harness, 1);

    let after = frames(&harness, &[0, 1, 2]);
    let sizes = |frames: &[IRect]| frames.iter().map(IRect::size).collect::<Vec<_>>();
    assert_eq!(sizes(&after), sizes(&before));
}

/// A stacked window leaves its stack for a column of its own to the right of
/// the stack, and goes back to its place in the stack when restored.
#[test]
fn a_stacked_window_pops_out_right_and_returns_to_its_slot() {
    let mut harness = stacked();
    let before = frames(&harness, &[0, 1, 2]);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]]
    );

    double_click_title_bar(&mut harness, 1);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));

    double_click_title_bar(&mut harness, 1);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]]
    );
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

/// A maximized window moved to another display stays a maximized column of
/// that display's strip: focus scrolls past it there too.
#[test]
fn a_display_move_keeps_a_maximized_window_in_the_new_row() {
    let mut harness = maximized_below_a_larger_display();
    move_to_display(
        &mut harness,
        Direction::North,
        EXT_DISPLAY_ID,
        EXT_WORKSPACE_ID,
    );
    let area = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);

    focus(&mut harness, Direction::West);
    assert_focused!(harness.world(), 100);
    assert!(
        os_frame(&harness, 1).min.x >= os_frame(&harness, 100).max.x,
        "window 1 slid away to the east: {:?}",
        os_frame(&harness, 1)
    );
    focus(&mut harness, Direction::East);
    assert_eq!(os_frame(&harness, 1), area);
}

/// A maximized window moved to another workspace and back stays maximized,
/// fills the usable area, and focus still scrolls past it.
#[test]
fn a_workspace_move_keeps_a_window_maximized() {
    let mut harness = three_windows_one_focused();
    double_click_title_bar(&mut harness, 1);
    let area = usable_area(&mut harness);

    harness.run(vec![command(Operation::VirtualMove(
        Direction::East,
        crate::commands::MoveFocus::Follow,
    ))]);
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), area);

    harness.run(vec![command(Operation::VirtualMove(
        Direction::West,
        crate::commands::MoveFocus::Follow,
    ))]);
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), area);

    focus(&mut harness, Direction::West);
    assert_focused!(harness.world(), 2);
    assert!(
        os_frame(&harness, 1).min.x >= os_frame(&harness, 2).max.x,
        "window 1 slid away to the east: {:?}",
        os_frame(&harness, 1)
    );
    focus(&mut harness, Direction::East);
    assert_eq!(os_frame(&harness, 1), area);
}

/// A config reload that changes the gaps fits a maximized window to the
/// usable area again.
#[test]
fn a_padding_reload_refits_a_maximized_window() {
    let mut harness = three_windows_one_focused();
    double_click_title_bar(&mut harness, 1);
    let area = usable_area(&mut harness);

    // Outer and inner gaps of 16: an edge padding of 8 and a window padding
    // of 8.
    for id in 0..3 {
        harness.mock_state.update_window(id, |window| {
            window.horizontal_padding = 8;
            window.vertical_padding = 8;
        });
    }
    let path = std::env::temp_dir().join(format!(
        "paneru-maximize-reload-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        "[options]\npadding_top = 8\npadding_right = 8\npadding_bottom = 8\npadding_left = 8\n",
    )
    .expect("writing the new config");
    let reloaded = harness
        .world()
        .resource_mut::<Config>()
        .reload_config(&path);
    std::fs::remove_file(&path).expect("removing the new config");
    reloaded.expect("reloading the config");
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), area);

    focus(&mut harness, Direction::East);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
    focus(&mut harness, Direction::West);
    assert_eq!(os_frame(&harness, 1), area);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
}

/// Repeated toggles with scrolling in between end where they started.
#[test]
fn repeated_toggles_with_scrolling_do_not_drift() {
    let mut harness = three_windows_one_focused();
    let before = frames(&harness, &[0, 1, 2]);
    for _ in 0..3 {
        double_click_title_bar(&mut harness, 1);
        focus(&mut harness, Direction::East);
        focus(&mut harness, Direction::West);
        double_click_title_bar(&mut harness, 1);
    }
    let sizes = |frames: &[IRect]| frames.iter().map(IRect::size).collect::<Vec<_>>();
    assert_eq!(sizes(&frames(&harness, &[0, 1, 2])), sizes(&before));
}
