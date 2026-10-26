//! A maximized window that a width, stack, float, or drag action changes is
//! restored first, and the action then applies to the restored window.

use crate::commands::ResizeDirection;
use crate::util::round_px;

use super::*;

fn run(harness: &mut TestHarness, operation: Operation) {
    harness.run(vec![command(operation)]);
    harness.advance(SETTLE);
}

fn width_of(ratio: f64, area: IRect) -> i32 {
    round_px(ratio * f64::from(area.width()))
}

fn widths(harness: &TestHarness, ids: &[WinID]) -> Vec<i32> {
    frames(harness, ids).iter().map(IRect::width).collect()
}

/// [`three_windows_one_focused`] with window 1 maximized by the command.
fn one_maximized() -> TestHarness {
    let mut harness = three_windows_one_focused();
    assert_eq!(os_frame(&harness, 1).width(), TEST_WINDOW_WIDTH);
    run(&mut harness, Operation::Maximize);
    let area = usable_area(&mut harness);
    assert_eq!(os_frame(&harness, 1), area);
    harness
}

/// [`stacked`] with window 1, under window 0, focused and maximized. It
/// pops out to a column of its own.
fn stacked_one_maximized() -> TestHarness {
    let mut harness = stacked();
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    run(&mut harness, Operation::Maximize);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    harness
}

/// Grow, shrink, a resize step, and an exact width each start from the
/// width the window had before it was maximized: they end where they end
/// for a window that was never maximized.
#[test]
fn a_width_command_restores_first_then_starts_from_the_earlier_width() {
    for operation in [
        Operation::Resize(ResizeDirection::Grow),
        Operation::Resize(ResizeDirection::Shrink),
        Operation::ResizeStep(ResizeDirection::Grow),
        Operation::ResizeStep(ResizeDirection::Shrink),
        Operation::SetWidth(0.6),
    ] {
        let mut reference = three_windows_one_focused();
        run(&mut reference, operation.clone());
        let expected = os_frame(&reference, 1).width();
        assert_ne!(
            expected, TEST_WINDOW_WIDTH,
            "{operation:?} changes the width"
        );

        let mut harness = one_maximized();
        run(&mut harness, operation.clone());
        assert_eq!(os_frame(&harness, 1).width(), expected, "{operation:?}");
        assert_eq!(widths(&harness, &[0, 2]), vec![TEST_WINDOW_WIDTH; 2]);
    }
}

/// A width command on a maximized window from a stack returns it to its
/// place and height in the stack, then sizes the whole column.
#[test]
fn a_width_command_returns_a_maximized_stacked_window_to_its_stack_first() {
    let mut harness = stacked();
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    let before = frames(&harness, &[0, 1]);
    let area = usable_area(&mut harness);
    run(&mut harness, Operation::Maximize);
    assert_eq!(os_frame(&harness, 1), area);

    run(&mut harness, Operation::Resize(ResizeDirection::Grow));
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]]
    );
    let after = frames(&harness, &[0, 1]);
    assert_eq!(widths(&harness, &[0, 1]), vec![width_of(0.5, area); 2]);
    assert_eq!(after[1].height(), before[1].height(), "its earlier height");
    assert_eq!(after[0].max.y, after[1].min.y, "1 is under 0 again");
}

/// Stacking a maximized window into its neighbour's column restores it
/// first, so it joins at its earlier width rather than the viewport's.
#[test]
fn a_stack_command_on_a_maximized_window_restores_it_before_it_joins() {
    let mut harness = one_maximized();

    run(&mut harness, Operation::Stack(Side::West));
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0, 1], vec![2]]
    );
    let joined = frames(&harness, &[0, 1]);
    assert_eq!(widths(&harness, &[0, 1]), vec![TEST_WINDOW_WIDTH; 2]);
    assert_eq!(joined[0].max.y, joined[1].min.y, "1 is under 0");
}

/// Another window stacked onto a maximized window's column restores that
/// window first, and the two share its earlier width.
#[test]
fn stacking_onto_a_maximized_window_restores_it_first() {
    let mut harness = one_maximized();
    focus(&mut harness, Direction::East);
    assert_focused!(harness.world(), 2);

    run(&mut harness, Operation::Stack(Side::West));
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1, 2]]
    );
    let joined = frames(&harness, &[1, 2]);
    assert_eq!(widths(&harness, &[1, 2]), vec![TEST_WINDOW_WIDTH; 2]);
    assert_eq!(joined[0].max.y, joined[1].min.y, "2 is under 1");
}

/// Unstacking a maximized window that left a stack restores it to a column
/// of its own at its earlier width.
#[test]
fn unstacking_a_maximized_window_restores_it() {
    let mut harness = stacked_one_maximized();
    let area = usable_area(&mut harness);

    run(&mut harness, Operation::Unstack(Side::East));
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    let restored = os_frame(&harness, 1);
    assert_eq!(restored.width(), TEST_WINDOW_WIDTH);
    assert_eq!(restored.height(), area.height());
}

/// Balance skips a maximized column, and a maximized focused window gives
/// the other columns the width it had before it was maximized.
#[test]
fn balance_from_a_maximized_window_gives_the_others_its_earlier_width() {
    let mut harness = three_windows_one_focused();
    run(&mut harness, Operation::SetWidth(0.6));
    let area = usable_area(&mut harness);
    let earlier = os_frame(&harness, 1).width();
    assert_eq!(earlier, width_of(0.6, area));
    run(&mut harness, Operation::Maximize);

    run(&mut harness, Operation::Balance);
    assert_eq!(os_frame(&harness, 1), area, "still maximized");
    assert_eq!(widths(&harness, &[0, 2]), vec![earlier; 2]);
}

/// Balance from another window leaves a maximized column maximized.
#[test]
fn balance_leaves_a_maximized_column_maximized() {
    let mut harness = one_maximized();
    let area = usable_area(&mut harness);
    focus(&mut harness, Direction::East);
    run(&mut harness, Operation::SetWidth(0.3));

    run(&mut harness, Operation::Balance);
    assert_eq!(os_frame(&harness, 0).width(), width_of(0.3, area));
    focus(&mut harness, Direction::West);
    assert_eq!(os_frame(&harness, 1), area, "still maximized");
}

/// Equalize evens a stack's heights. A maximized window is in no stack, so
/// it stays maximized.
#[test]
fn equalize_leaves_a_maximized_window_maximized() {
    let mut harness = stacked_one_maximized();
    let area = usable_area(&mut harness);

    run(&mut harness, Operation::Equalize);
    assert_eq!(os_frame(&harness, 1), area);
}

/// Floating a maximized window restores it first, so it floats at the size
/// it would have floated at before it was maximized.
#[test]
fn floating_a_maximized_window_floats_it_at_its_earlier_size() {
    let mut reference = three_windows_one_focused();
    run(&mut reference, Operation::Manage);
    let floated = os_frame(&reference, 1);

    let mut harness = one_maximized();
    run(&mut harness, Operation::Manage);
    let entity = find_window_entity(1, harness.world());
    assert!(
        harness
            .world()
            .get::<crate::ecs::Unmanaged>(entity)
            .is_some(),
        "window 1 floats"
    );
    assert_eq!(os_frame(&harness, 1).size(), floated.size());
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![2]]
    );
}

/// A press on a window's edge, the resize macOS then reports, and the
/// release, the way a user resizes a window with the pointer.
fn edge_resize(harness: &mut TestHarness, id: WinID, edge: Origin, size: Size) {
    harness.mock_state.set_left_button_down(true);
    harness.world().write_message(Event::MouseDown {
        point: CGPoint::new(edge.x.into(), edge.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
        window_id: Some(id),
        clicks: 1,
    });
    harness.advance(Duration::from_millis(16));
    harness.mock_state.os_resize_window(id, size);
    harness.advance(Duration::from_millis(100));
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(Event::MouseUp {
        point: CGPoint::new(edge.x.into(), edge.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    });
    harness.advance(SETTLE);
}

/// A user's edge resize of a maximized window ends maximize, and the window
/// keeps the width the user gave it.
#[test]
fn an_edge_resize_ends_maximize_and_keeps_the_new_width() {
    let mut harness = one_maximized();
    let area = usable_area(&mut harness);

    edge_resize(
        &mut harness,
        1,
        Origin::new(area.max.x - 1, area.center().y),
        Size::new(700, area.height()),
    );
    assert_eq!(os_frame(&harness, 1).width(), 700);

    run(&mut harness, Operation::Maximize);
    assert_eq!(os_frame(&harness, 1), area, "the next toggle maximizes");
}

/// A window that opens while a maximized window has the focus opens in a
/// column beside it, and the maximized window stays maximized.
#[test]
fn a_new_window_opens_beside_a_maximized_one_which_stays_maximized() {
    let mut harness = one_maximized();
    let area = usable_area(&mut harness);

    let window = harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        3,
        IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(SETTLE);

    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![3], vec![2]]
    );
    assert_eq!(os_frame(&harness, 1).size(), area.size(), "still maximized");
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    assert_eq!(os_frame(&harness, 1), area);
}

/// Drags window `id` by the title bar point `from` to `to` in three steps,
/// with the window following each one as macOS moves it, and returns what
/// the drag overlay shows at the end. The button stays down.
fn drag(harness: &mut TestHarness, id: WinID, from: Origin, to: Origin) -> Option<OverlayShape> {
    let origin = os_frame(harness, id).min;
    for step in 1..=3 {
        let point = from + (to - from) * step / 3;
        harness.world().write_message(Event::MouseDragged {
            point: CGPoint::new(point.x.into(), point.y.into()),
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        });
        harness
            .mock_state
            .os_move_window(id, origin + (point - from));
        harness.advance(Duration::from_millis(16));
    }
    harness.world().resource::<DragFeedback>().shape
}

/// Presses window `id`'s title bar at `point`.
fn press(harness: &mut TestHarness, id: WinID, point: Origin) {
    harness.mock_state.set_left_button_down(true);
    harness.world().write_message(Event::MouseDown {
        point: CGPoint::new(point.x.into(), point.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
        window_id: Some(id),
        clicks: 1,
    });
    harness.advance(Duration::from_millis(16));
}

/// Window 1 maximized and held by its title bar at the right edge of the
/// display until the strip scrolls window 2's column into view. Returns
/// where the pointer is.
fn maximized_held_at_the_right_edge(harness: &mut TestHarness) -> Origin {
    let area = usable_area(harness);
    let grab = Origin::new(area.center().x, area.min.y + 10);
    let edge = Origin::new(area.max.x - 2, area.center().y);
    press(harness, 1, grab);
    drag(harness, 1, grab, edge);
    harness.advance(Duration::from_millis(600));
    edge
}

/// A maximized window dragged to a gap on its own display shows the drop
/// line, moves to the gap, and stays maximized.
#[test]
fn a_maximized_window_dragged_to_a_gap_on_its_display_stays_maximized() {
    let mut harness = one_maximized();
    let area = usable_area(&mut harness);
    let edge = maximized_held_at_the_right_edge(&mut harness);
    let shape = drag(&mut harness, 1, edge, edge);
    assert!(
        matches!(shape, Some(OverlayShape::Line(_))),
        "a drop line: {shape:?}"
    );

    release_at(&mut harness, edge, TEST_WORKSPACE_ID);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![2], vec![1]]
    );
    assert_eq!(os_frame(&harness, 1), area);
}

/// A maximized window dropped into another column on its own display is
/// restored first and joins that column at its width.
#[test]
fn a_maximized_window_dropped_into_a_column_joins_it_restored() {
    let mut harness = one_maximized();
    let edge = maximized_held_at_the_right_edge(&mut harness);
    let target = os_frame(&harness, 2);
    let into = Origin::new(target.center().x, target.center().y + 50);
    let shape = drag(&mut harness, 1, edge, into);
    assert!(
        matches!(shape, Some(OverlayShape::Line(_))),
        "a drop line: {shape:?}"
    );

    release_at(&mut harness, into, TEST_WORKSPACE_ID);
    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![2, 1]]
    );
    let joined = frames(&harness, &[2, 1]);
    assert_eq!(widths(&harness, &[2, 1]), vec![TEST_WINDOW_WIDTH; 2]);
    assert_eq!(joined[0].max.y, joined[1].min.y, "1 is under 2");
}

/// A window dropped into a maximized window's column restores that window
/// first, then joins it: the drop shows a line, not a refusal.
#[test]
fn a_window_dropped_into_a_maximized_column_restores_it_and_joins() {
    let mut harness = one_maximized();
    focus(&mut harness, Direction::East);
    let area = usable_area(&mut harness);
    let dragged = os_frame(&harness, 2);
    let grab = Origin::new(dragged.center().x, dragged.min.y + 10);
    let shown = os_frame(&harness, 1).intersect(area);
    assert!(shown.width() > 200, "window 1 shows: {shown:?}");
    let into = Origin::new(area.min.x + 100, area.center().y + 50);

    press(&mut harness, 2, grab);
    let shape = drag(&mut harness, 2, grab, into);
    assert!(
        matches!(shape, Some(OverlayShape::Line(_))),
        "a drop line: {shape:?}"
    );
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(Event::MouseUp {
        point: CGPoint::new(into.x.into(), into.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    });
    harness.advance(SETTLE);

    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1, 2]]
    );
    let joined = frames(&harness, &[1, 2]);
    assert_eq!(widths(&harness, &[1, 2]), vec![TEST_WINDOW_WIDTH; 2]);
    assert_eq!(joined[0].max.y, joined[1].min.y, "2 is under 1");
}

/// A window dragged from another display into a maximized window's column
/// restores that window first, then joins it at its earlier width.
#[test]
fn a_window_dropped_from_another_display_into_a_maximized_column_joins_it_restored() {
    let mut harness = maximized_below_a_larger_display();
    let area = usable_area_of(&mut harness, TEST_DISPLAY_ID);
    let dragged = os_frame(&harness, 100);
    let grab = Origin::new(dragged.center().x, dragged.min.y + 10);
    let into = Origin::new(area.center().x, area.center().y + 50);

    press(&mut harness, 100, grab);
    let shape = drag(&mut harness, 100, grab, into);
    assert!(
        matches!(shape, Some(OverlayShape::Line(_))),
        "a drop line: {shape:?}"
    );
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(Event::MouseUp {
        point: CGPoint::new(into.x.into(), into.y.into()),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    });
    harness
        .mock_state
        .update_window(100, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness.advance(SETTLE);

    assert_eq!(
        layout_on(&mut harness, TEST_WORKSPACE_ID),
        vec![vec![0], vec![1, 100], vec![2]]
    );
    assert_eq!(widths(&harness, &[1, 100]), vec![TEST_WINDOW_WIDTH; 2]);
}
