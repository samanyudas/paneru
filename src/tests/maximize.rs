//! Maximizing a window by double-clicking its title bar, and restoring it.

use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::drag::{DragFeedback, DragState, OverlayShape};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{DockPosition, SelectedVirtualMarker, SpawnWindowTrigger};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::{Modifiers, WinID, WorkspaceId};

use super::*;

mod restore_first;

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
    let sizes = frames(&harness, &[0, 2])
        .iter()
        .map(IRect::size)
        .collect::<Vec<_>>();
    assert_eq!(
        sizes,
        vec![before[0].size(), before[2].size()],
        "neighbors keep their sizes"
    );
    assert_nothing_covers(&mut harness, 1, &[0, 2]);

    double_click_title_bar(&mut harness, 1);
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

/// Sends `window <verb>` as a key binding or the command line does, and
/// lets it settle.
fn run_window_verb(harness: &mut TestHarness, verb: &str) {
    let command = crate::config::parse_command(&["window", verb]).expect("a window command");
    harness.run(vec![Event::Command { command }]);
    harness.advance(SETTLE);
}

/// The command toggles the focused window, and a double-click on its title
/// bar toggles the same state. `fullwidth` is the command's older name, which
/// existing bindings still use.
#[test]
fn the_command_and_a_double_click_toggle_one_state() {
    for verb in ["fullwidth", "maximize"] {
        let mut harness = three_windows();
        harness.mock_state.focus_window(1);
        harness.advance(SETTLE);
        let before = frames(&harness, &[0, 1, 2]);

        run_window_verb(&mut harness, verb);
        assert_eq!(os_frame(&harness, 1), usable_area(&mut harness), "{verb}");
        assert_nothing_covers(&mut harness, 1, &[0, 2]);
        double_click_title_bar(&mut harness, 1);
        assert_eq!(
            frames(&harness, &[0, 1, 2]),
            before,
            "{verb}: the double-click restores"
        );

        double_click_title_bar(&mut harness, 1);
        assert_eq!(os_frame(&harness, 1), usable_area(&mut harness), "{verb}");
        run_window_verb(&mut harness, verb);
        assert_eq!(
            frames(&harness, &[0, 1, 2]),
            before,
            "{verb}: the command restores"
        );
    }
}

/// On a floating window the command fills the usable area over the layout,
/// as a double-click does, and the next toggle of either kind restores it.
#[test]
fn the_command_fills_a_floating_window_and_restores_it() {
    let mut harness = three_windows();
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Manage)]);
    harness.advance(SETTLE);
    let before = os_frame(&harness, 1);
    let tiled = frames(&harness, &[0, 2]);

    run_window_verb(&mut harness, "maximize");
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));
    assert_eq!(frames(&harness, &[0, 2]), tiled, "the layout stays");
    run_window_verb(&mut harness, "maximize");
    assert_eq!(os_frame(&harness, 1), before);

    run_window_verb(&mut harness, "maximize");
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));
    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), before);
}

/// An app can move a maximized floating window to another display, and the
/// engine stops reading its frame while it is maximized. The command still
/// restores it on the display it is on now, as a double-click does.
#[test]
fn the_command_restores_a_floating_window_on_the_display_it_was_moved_to() {
    let mut harness = TestHarness::new().with_windows(3).with_display(
        EXT_DISPLAY_ID,
        EXT_BOUNDS,
        vec![EXT_WORKSPACE_ID],
    );
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Manage)]);
    harness.advance(SETTLE);

    run_window_verb(&mut harness, "maximize");
    assert_eq!(
        os_frame(&harness, 1),
        usable_area_of(&mut harness, TEST_DISPLAY_ID)
    );
    harness.mock_state.update_window(1, |window| {
        let size = window.frame.size();
        window.frame = IRect::from_corners(Origin::new(100, -1000), Origin::new(100, -1000) + size);
    });
    harness.advance(SETTLE);

    run_window_verb(&mut harness, "maximize");
    let restored = os_frame(&harness, 1);
    assert!(
        EXT_BOUNDS.contains(restored.center()),
        "restored on the display it was moved to: {restored:?}"
    );
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
    stacked_with(Config::default())
}

fn stacked_with(config: Config) -> TestHarness {
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
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
    assert_eq!(
        os_frame(&harness, 0).width(),
        before[0].width(),
        "its stack mate keeps its width"
    );

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

/// A maximized window moved away from its column still restores from its
/// top bar where it is now.
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

/// Drags window 1 by its title bar to `to`, where macOS reports it in
/// `space` when the button is released.
fn drag_title_bar(harness: &mut TestHarness, to: Origin, space: WorkspaceId) {
    let press = title_bar(harness, 1);
    press_title_bar(harness, press);
    drag_steps(harness, press, to);
    release_at(harness, to, space);
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
/// that display's usable area, in both directions. A drag within its own
/// display leaves it maximized in its row, filling the display.
#[test]
fn a_maximized_window_dragged_to_another_display_fills_it() {
    let mut harness = maximized_below_a_larger_display();

    drag_title_bar(&mut harness, Origin::new(700, -600), EXT_WORKSPACE_ID);
    let area = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);

    drag_title_bar(&mut harness, Origin::new(1000, -1000), EXT_WORKSPACE_ID);
    assert_eq!(os_frame(&harness, 1), area);

    drag_title_bar(&mut harness, Origin::new(900, 300), TEST_WORKSPACE_ID);
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), area);

    drag_title_bar(&mut harness, Origin::new(600, 400), TEST_WORKSPACE_ID);
    assert_eq!(os_frame(&harness, 1), area);
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

    drag_title_bar(&mut harness, Origin::new(2800, 300), EXT_WORKSPACE_ID);
    assert_eq!(os_frame(&harness, 1), ext);

    drag_title_bar(&mut harness, Origin::new(900, 300), TEST_WORKSPACE_ID);
    assert_eq!(os_frame(&harness, 1), test);
}

/// A maximized window moved to a display that shows no strip waits there
/// for macOS to report its space. It is still maximized once that strip
/// appears, and restores to its earlier share of the viewport.
#[test]
fn a_maximized_window_moved_to_a_display_without_a_strip_stays_maximized() {
    let mut harness = TestHarness::new().with_windows(3).with_display(
        EXT_DISPLAY_ID,
        EAST_EXT_BOUNDS,
        vec![EXT_WORKSPACE_ID],
    );
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    let before = os_frame(&harness, 1);
    let test = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    let ext = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), test);

    let world = harness.world();
    let strips: Vec<_> = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .filter_map(|(entity, strip)| (strip.id() == EXT_WORKSPACE_ID).then_some(entity))
        .collect();
    for strip in strips {
        harness.world().despawn(strip);
    }
    harness.run(vec![command(Operation::MoveToDisplay(Direction::East))]);
    harness.advance(SETTLE);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
    harness.world().write_message(Event::SpaceCreated {
        space_id: EXT_WORKSPACE_ID,
    });
    harness.advance(SETTLE);
    harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
    harness.advance(SETTLE);
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
    assert_eq!(layout_on(&mut harness, EXT_WORKSPACE_ID), vec![vec![1]]);
    assert_eq!(os_frame(&harness, 1), ext);

    double_click_title_bar(&mut harness, 1);
    let share = f64::from(before.width()) / f64::from(test.width());
    let restored = os_frame(&harness, 1);
    assert_eq!(
        restored.width(),
        crate::util::round_px(share * f64::from(ext.width())),
        "restored to its earlier share of the viewport: {restored:?}"
    );
    assert_eq!(restored.height(), ext.height());
}

/// A maximized native tab group moved to a display that shows no strip
/// waits there for macOS to report its space. Closing one tab while it waits
/// leaves the other tab maximized once the strip appears.
fn a_tab_closed_on_the_way_to_a_display_without_a_strip(settled: bool) {
    let mut harness = TestHarness::new().with_windows(3).with_display(
        EXT_DISPLAY_ID,
        EAST_EXT_BOUNDS,
        vec![EXT_WORKSPACE_ID],
    );
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    let frame = os_frame(&harness, 1);
    let tab = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame);
    harness.mock_state.window_visible(1, false);
    harness.world().trigger(SpawnWindowTrigger(vec![tab]));
    harness.advance(SETTLE);
    let before = os_frame(&harness, 3);
    let test = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    let ext = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    double_click_title_bar(&mut harness, 3);
    assert_eq!(os_frame(&harness, 3), test);

    let world = harness.world();
    let strips: Vec<_> = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .filter_map(|(entity, strip)| (strip.id() == EXT_WORKSPACE_ID).then_some(entity))
        .collect();
    for strip in strips {
        harness.world().despawn(strip);
    }
    harness
        .world()
        .write_message(command(Operation::MoveToDisplay(Direction::East)));
    if settled {
        harness.advance(SETTLE);
    }
    harness.mock_state.os_close_window(3);
    harness.mock_state.window_visible(1, true);
    harness.advance(SETTLE);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
    harness.world().write_message(Event::SpaceCreated {
        space_id: EXT_WORKSPACE_ID,
    });
    harness.advance(SETTLE);
    harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
    harness.advance(SETTLE);
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
    assert_eq!(layout_on(&mut harness, EXT_WORKSPACE_ID), vec![vec![1]]);
    assert_eq!(os_frame(&harness, 1), ext);

    double_click_title_bar(&mut harness, 1);
    let share = f64::from(before.width()) / f64::from(test.width());
    let restored = os_frame(&harness, 1);
    assert_eq!(
        restored.width(),
        crate::util::round_px(share * f64::from(ext.width())),
        "restored to its earlier share of the viewport: {restored:?}"
    );
}

#[test]
fn a_tab_closed_while_its_group_moves_leaves_the_other_maximized() {
    a_tab_closed_on_the_way_to_a_display_without_a_strip(false);
}

#[test]
fn a_tab_closed_while_its_group_waits_leaves_the_other_maximized() {
    a_tab_closed_on_the_way_to_a_display_without_a_strip(true);
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

/// An app that nudges a maximized window does not stop a drag from its
/// title bar, and the window fills the display it is dropped on.
#[test]
fn a_maximized_window_an_app_moved_still_drags_to_another_display() {
    let mut harness = maximized_below_a_larger_display();
    harness.mock_state.os_move_window(1, Origin::new(0, 30));
    harness.advance(SETTLE);
    let press = Origin::new(500, 40);
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
/// reaches the engine ends when the button reads up. The window stays
/// maximized in its row and fills the display again.
#[test]
fn a_lost_release_of_a_maximized_window_on_its_display_keeps_it_maximized() {
    let mut harness = maximized_below_a_larger_display();
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
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
    assert_eq!(os_frame(&harness, 1), area);
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

/// A stacked window leaves its stack for a column of its own on the side
/// `config` chooses, `popped` the layout then, and goes back to its place in
/// the stack when restored.
fn pops_out_and_returns_to_its_slot(config: &str, popped: &[&[WinID]]) {
    let config = Config::try_from(config).expect("config parses");
    let mut harness = stacked_with(config);
    let before = frames(&harness, &[0, 1, 2]);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]]
    );

    double_click_title_bar(&mut harness, 1);
    assert_eq!(layout_on(&mut harness, TEST_WORKSPACE_ID), popped);
    assert_eq!(os_frame(&harness, 1), usable_area(&mut harness));

    double_click_title_bar(&mut harness, 1);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]]
    );
    assert_eq!(frames(&harness, &[0, 1, 2]), before);
}

#[test]
fn a_stacked_window_pops_out_right_by_default() {
    pops_out_and_returns_to_its_slot("", &[&[0], &[1], &[2]]);
}

#[test]
fn a_stacked_window_pops_out_right_when_configured() {
    pops_out_and_returns_to_its_slot(
        "[options]\nmaximized_window_side = \"right\"\n",
        &[&[0], &[1], &[2]],
    );
}

#[test]
fn a_stacked_window_pops_out_left_when_configured() {
    pops_out_and_returns_to_its_slot(
        "[options]\nmaximized_window_side = \"left\"\n",
        &[&[1], &[0], &[2]],
    );
}

#[test]
fn maximized_window_side_parses_left_or_right() {
    let parse = |value: &str| {
        Config::try_from(format!("[options]\nmaximized_window_side = {value}\n").as_str())
    };
    assert!(parse("\"left\"").is_ok());
    assert!(parse("\"right\"").is_ok());
    let error = parse("\"up\"").expect_err("up is not a side").to_string();
    assert!(
        error.contains("unknown variant `up`, expected `left` or `right`"),
        "{error}"
    );
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

/// Reloads the config with outer and inner gaps of 16: an edge padding of 8
/// and a window padding of 8 for windows `ids`.
fn reload_with_gaps(harness: &mut TestHarness, ids: &[WinID]) {
    for id in ids {
        harness.mock_state.update_window(*id, |window| {
            window.horizontal_padding = 8;
            window.vertical_padding = 8;
        });
    }
    reload_options(harness, EDGE_PADDING_8);
}

const EDGE_PADDING_8: &str =
    "padding_top = 8\npadding_right = 8\npadding_bottom = 8\npadding_left = 8\n";

/// Reloads the config with `options` as its `[options]` table.
fn reload_options(harness: &mut TestHarness, options: &str) {
    let test = std::thread::current()
        .name()
        .unwrap_or_default()
        .replace("::", "-");
    let path = std::env::temp_dir().join(format!(
        "paneru-maximize-reload-{}-{test}.toml",
        std::process::id(),
    ));
    std::fs::write(&path, format!("[options]\n{options}")).expect("writing the new config");
    let reloaded = harness
        .world()
        .resource_mut::<Config>()
        .reload_config(&path);
    std::fs::remove_file(&path).expect("removing the new config");
    reloaded.expect("reloading the config");
    harness.advance(SETTLE);
}

/// A config reload that changes the gaps fits a maximized window to the
/// usable area again.
#[test]
fn a_padding_reload_refits_a_maximized_window() {
    let mut harness = three_windows_one_focused();
    double_click_title_bar(&mut harness, 1);
    let area = usable_area(&mut harness);

    reload_with_gaps(&mut harness, &[0, 1, 2]);
    assert_eq!(os_frame(&harness, 1), area);

    focus(&mut harness, Direction::East);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
    focus(&mut harness, Direction::West);
    assert_eq!(os_frame(&harness, 1), area);
    assert_nothing_covers(&mut harness, 1, &[0, 2]);
}

/// A config reload that changes the gaps also fits a maximized window on a
/// display without the focus to that display's usable area.
#[test]
fn a_padding_reload_refits_a_maximized_window_on_another_display() {
    let mut harness = maximized_beside(EAST_EXT_BOUNDS);
    move_to_display(
        &mut harness,
        Direction::East,
        EXT_DISPLAY_ID,
        EXT_WORKSPACE_ID,
    );
    let ext = usable_area_of(&mut harness, EXT_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 1), ext);
    harness.mock_state.focus_window(0);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 0);

    reload_with_gaps(&mut harness, &[0, 1, 2, 100]);
    assert_eq!(os_frame(&harness, 1), ext);
}

/// Windows 1 and 2 maximized on the built-in display, window 2 in view.
fn two_maximized(mut harness: TestHarness) -> TestHarness {
    focus(&mut harness, Direction::East);
    assert_focused!(harness.world(), 2);
    double_click_title_bar(&mut harness, 2);
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    assert_eq!(os_frame(&harness, 2), area);
    harness
}

/// A padding reload that changes only the right padding narrows the first
/// of two maximized columns, which moves the second. The second still fills
/// the usable area.
#[test]
fn a_padding_reload_refits_a_maximized_window_after_another() {
    let mut harness = three_windows_one_focused();
    double_click_title_bar(&mut harness, 1);
    let mut harness = two_maximized(harness);
    let area = usable_area(&mut harness);

    reload_options(&mut harness, "padding_right = 16\n");
    assert_eq!(os_frame(&harness, 2), area);
}

/// The same holds on a display without the focus, where the strip is
/// aligned on its most visible column.
#[test]
fn a_padding_reload_refits_a_maximized_window_after_another_on_another_display() {
    let mut harness = two_maximized(maximized_beside(EAST_EXT_BOUNDS));
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    harness.mock_state.focus_window(100);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 100);
    assert_eq!(os_frame(&harness, 2), area);

    reload_options(&mut harness, "padding_left = 4\npadding_right = 16\n");
    assert_eq!(os_frame(&harness, 2), area);
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

/// A window already as wide as the viewport keeps the same column frame when
/// maximized, yet still fills the usable area: only its gaps change. Its app
/// gets the writes through the engine's per-app writers, as with `WindowOS`.
#[test]
fn a_full_width_window_fills_the_usable_area_when_maximized() {
    let options = MainOptions {
        padding_top: Some(4),
        padding_right: Some(4),
        padding_bottom: Some(4),
        padding_left: Some(4),
        ..MainOptions::default()
    };
    let harness = TestHarness::new().with_config((options, vec![]).into());
    harness.mock_state.queue_writes(TEST_PROCESS_ID);
    let mut harness = harness.with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::SetWidth(1.0))]);
    harness.advance(SETTLE);
    let area = usable_area(&mut harness);
    assert_eq!(os_frame(&harness, 1).width(), area.width() - 8);

    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1), area);
    double_click_title_bar(&mut harness, 1);
    assert_eq!(os_frame(&harness, 1).width(), area.width() - 8);
}

/// With `auto_center`, a padding reload keeps a strip with no maximized
/// window where the user swiped it, moved only as far as its viewport moved,
/// instead of centering its widest window again.
#[test]
fn a_padding_reload_keeps_a_swiped_strip_where_it_was() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
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
    let centered = os_frame(&harness, 1);
    for _ in 0..5 {
        harness.world().write_message(Event::Swipe {
            delta: 0.05,
            fingers: 3,
        });
        harness.advance(Duration::from_millis(20));
    }
    harness.advance(SETTLE);
    let swiped = frames(&harness, &[1, 2]);
    assert_ne!(swiped[0], centered, "the swipe moved window 1");

    reload_options(
        &mut harness,
        &format!("auto_center = true\nswipe_gesture_fingers = 3\n{EDGE_PADDING_8}"),
    );
    let after = frames(&harness, &[1, 2]);
    let lefts = |frames: &[IRect]| frames.iter().map(|frame| frame.min.x).collect::<Vec<_>>();
    let moved: Vec<_> = lefts(&swiped).iter().map(|x| x + 8).collect();
    assert_eq!(lefts(&after), moved, "moved by the left padding of 8 only");
}
