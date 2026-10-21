use super::*;

// --- Drops across displays ---

/// Where the external display sits: to the right of the main one, sharing its
/// top edge.
fn ext_bounds(height: i32) -> IRect {
    IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        height,
    )
}

/// The main display with three columns, and an external display of
/// `ext_height` to its right with window 100 alone on it.
fn two_displays(config: &str, ext_height: i32) -> TestHarness {
    let config = crate::config::Config::try_from(config).expect("config parses");
    let ext = ext_bounds(ext_height);
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_windows(3)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(ext.min.x + 100, 20, ext.min.x + 900, ext.max.y);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    harness
}

/// The window IDs of each column of the strip shown for `workspace`, left to
/// right, each column top to bottom.
fn layout_on(world: &mut World, workspace: crate::platform::WorkspaceId) -> Vec<Vec<WinID>> {
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query_filtered::<&LayoutStrip, With<crate::ecs::SelectedVirtualMarker>>()
        .iter(world)
        .find(|strip| strip.id() == workspace)
        .expect("a strip for the workspace")
        .columns()
        .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
        .collect()
}

/// macOS puts a window released over another display on that display's
/// space, and that display becomes the active one.
fn macos_moves_to_ext(harness: &mut TestHarness, id: WinID) {
    harness
        .mock_state
        .update_window(id, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
}

/// Drags window `id` by its title bar onto the external display, drops it at
/// `to`, lets macOS move it there, and lets the layout settle.
fn title_drag_to_ext(harness: &mut TestHarness, id: WinID, to: Origin) -> IRect {
    let start = press_and_drag(harness, id, to);
    release(harness, to);
    macos_moves_to_ext(harness, id);
    harness.advance(SETTLE);
    start
}

#[test]
fn a_window_dropped_on_another_display_joins_a_column_there() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );

    let below_the_middle = middle_of(harness.world(), 100, 900);
    title_drag_to_ext(&mut harness, 2, below_the_middle);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100, 2]]
    );
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );
    assert_one_column(harness.world(), &[100, 2]);
    assert!(ext_bounds(EXT_DISPLAY_HEIGHT).contains(frame(harness.world(), 2).center()));
    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn a_window_dropped_above_a_column_on_another_display_becomes_its_top_window() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);

    let above_the_middle = middle_of(harness.world(), 100, 100);
    title_drag_to_ext(&mut harness, 2, above_the_middle);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![2, 100]]
    );
    assert_one_column(harness.world(), &[2, 100]);
}

#[test]
fn a_new_column_on_another_display_keeps_its_width_relative_to_the_display() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let start = frame(harness.world(), 2);
    let target = frame(harness.world(), 100);

    let outer_quarter = Origin::new(target.max.x - 20, 600);
    title_drag_to_ext(&mut harness, 2, outer_quarter);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
    let moved = frame(harness.world(), 2);
    let ratio = f64::from(start.width()) / f64::from(TEST_DISPLAY_WIDTH);
    let expected = crate::util::round_px(ratio * f64::from(EXT_DISPLAY_WIDTH));
    assert_eq!(moved.width(), expected, "{moved:?}");
    assert_eq!(moved.min.x, frame(harness.world(), 100).max.x, "{moved:?}");
    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn a_drop_in_empty_space_on_another_display_opens_a_column_at_the_nearest_end() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let target = frame(harness.world(), 100);
    let empty_space = Origin::new(TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH - 30, 600);
    assert!(empty_space.x > target.max.x, "{target:?}");
    title_drag_to_ext(&mut harness, 2, empty_space);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn a_full_stack_on_another_display_refuses_the_drop_and_the_window_returns() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);

    let into_the_column = middle_of(harness.world(), 100, 900);
    let start = press_and_drag(&mut harness, 2, into_the_column);
    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(Drag {
            target: DropTarget::Refused {
                reason: Refusal::StackFull,
                display: EXT_DISPLAY_ID,
            },
            ..
        })
    ));
    assert!(matches!(
        feedback(harness.world()),
        Some(OverlayShape::Refusal(_))
    ));
    release(&mut harness, into_the_column);
    harness.advance(SETTLE);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_back_right_of(harness.world(), 1, 2, start);
}

#[test]
fn a_smaller_display_refuses_a_stack_it_cannot_fit_under_fit_display() {
    // At the minimum window height the main display fits three windows in a
    // column, and the short external display fits one.
    let mut harness = two_displays("[options]\nstack_limit = \"fit\"\n", 300);

    let into_the_column = middle_of(harness.world(), 100, 200);
    let start = title_drag(&mut harness, 2, into_the_column);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_back_right_of(harness.world(), 1, 2, start);

    // The same drop on the main display's own column fits.
    let into_a_main_column = middle_of(harness.world(), 0, 600);
    title_drag(&mut harness, 2, into_a_main_column);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0, 2], vec![1]]
    );
}

#[test]
fn a_drop_on_another_display_survives_the_app_rebuilding_its_window_element() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);

    let below_the_middle = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, below_the_middle);
    release(&mut harness, below_the_middle);
    harness.mock_state.os_transfer_window(2, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100, 2]]
    );
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );
}

#[test]
fn a_floating_window_dragged_to_another_display_joins_no_layout() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Manage),
    ]);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );

    let into_the_column = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, into_the_column);
    assert_eq!(drag_state(harness.world()), DragState::Idle);
    assert_eq!(feedback(harness.world()), None);
    release(&mut harness, into_the_column);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(SETTLE);

    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );
}

fn strip_x(world: &mut World, workspace: crate::platform::WorkspaceId) -> i32 {
    world.query_filtered::<(&LayoutStrip, &crate::ecs::Position), With<crate::ecs::SelectedVirtualMarker>>()
        .iter(world).find(|(strip, _)| strip.id() == workspace).expect("shown strip").1.0.x
}

#[test]
fn an_outer_edge_scrolls_after_half_a_second_and_repeats_every_350ms() {
    let mut harness = TestHarness::new().with_windows(6);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
    let edge = Origin::new(2, 500);
    press_and_drag(&mut harness, 5, edge);
    let dragged = frame(harness.world(), 5);
    let before = strip_x(harness.world(), TEST_WORKSPACE_ID);
    harness.advance(ms(440));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), before);
    harness.advance(ms(80));
    let first = strip_x(harness.world(), TEST_WORKSPACE_ID);
    assert_eq!(
        frame(harness.world(), 5),
        dragged,
        "scrolling must leave the grabbed window under the pointer"
    );
    assert!(
        first > before,
        "{first} must expose a column left of {before}"
    );
    harness.advance(ms(280));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), first);
    harness.advance(ms(80));
    assert!(strip_x(harness.world(), TEST_WORKSPACE_ID) > first);
    release(&mut harness, edge);
    harness.advance(SETTLE);
    let dropped = strip_x(harness.world(), TEST_WORKSPACE_ID);
    harness.advance(ms(1000));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), dropped);
}

#[test]
fn leaving_an_edge_cancels_the_hold_and_reentering_waits_again() {
    let mut harness = TestHarness::new().with_windows(6);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
    let edge = Origin::new(2, 500);
    press_and_drag(&mut harness, 5, edge);
    let before = strip_x(harness.world(), TEST_WORKSPACE_ID);
    harness.advance(ms(300));
    harness
        .world()
        .write_message(mouse_dragged(Origin::new(40, 500)));
    harness.advance(ms(400));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), before);
    harness.world().write_message(mouse_dragged(edge));
    harness.advance(ms(400));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), before);
    harness.advance(ms(120));
    assert!(strip_x(harness.world(), TEST_WORKSPACE_ID) > before);
}

#[test]
fn an_edge_with_a_neighboring_display_does_not_scroll() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let edge = Origin::new(TEST_DISPLAY_WIDTH - 2, 500);
    press_and_drag(&mut harness, 0, edge);
    let before = strip_x(harness.world(), TEST_WORKSPACE_ID);
    harness.advance(ms(1200));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), before);
}

#[test]
fn an_edge_scroll_retargets_the_feedback_and_the_drop() {
    let mut harness = TestHarness::new().with_windows(6);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
    let edge = Origin::new(2, 500);
    press_and_drag(&mut harness, 5, edge);
    let initial = drag_state(harness.world());
    harness.advance(ms(540));
    let DragState::Dragging(after) = drag_state(harness.world()) else {
        panic!("drag continues");
    };
    assert_ne!(initial, DragState::Dragging(after.clone()));
    assert_ne!(feedback(harness.world()), None);
    let snapshot_target = after.target;
    release(&mut harness, edge);
    harness.advance(SETTLE);
    let DropTarget::NewColumn { index, .. } = snapshot_target else {
        panic!("edge opens a column");
    };
    let result = layout(harness.world());
    assert_eq!(result[index], vec![5], "{result:?}");
}

#[test]
fn macos_tile_rectangles_include_halves_quarters_and_fill_with_rounding() {
    use crate::ecs::drag::is_macos_tile;
    let visible = IRect::new(100, 40, 1300, 840);
    for tile in [
        visible,
        IRect::new(100, 40, 700, 840),
        IRect::new(700, 40, 1300, 840),
        IRect::new(100, 40, 1300, 440),
        IRect::new(100, 440, 1300, 840),
        IRect::new(100, 40, 700, 440),
        IRect::new(700, 40, 1300, 440),
        IRect::new(100, 440, 700, 840),
        IRect::new(700, 440, 1300, 840),
    ] {
        assert!(is_macos_tile(tile, visible), "{tile:?}");
        let rounded = IRect::from_corners(tile.min + IVec2::ONE, tile.max - IVec2::ONE);
        assert!(is_macos_tile(rounded, visible), "{rounded:?}");
    }
    assert!(!is_macos_tile(IRect::new(100, 40, 800, 840), visible));
    assert!(!is_macos_tile(IRect::new(100, 60, 700, 840), visible));
}

fn macos_tile(harness: &mut TestHarness, id: WinID, tile: IRect) {
    harness
        .mock_state
        .update_window(id, |window| window.frame = tile);
    harness
        .world()
        .write_message(Event::WindowMoved { window_id: id });
    harness
        .world()
        .write_message(Event::WindowResized { window_id: id });
    harness.advance(FRAME);
}

#[test]
fn macos_half_quarter_and_fill_after_a_drop_return_to_the_dropped_slot() {
    for tile in [
        IRect::new(0, 20, 512, 768),
        IRect::new(512, 394, 1024, 768),
        IRect::new(0, 20, 1024, 768),
    ] {
        let mut harness = three_columns();
        let point = middle_of(harness.world(), 0, 600);
        press_and_drag(&mut harness, 2, point);
        release(&mut harness, point);
        macos_tile(&mut harness, 2, tile);
        harness.advance(SETTLE);
        assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
        assert_one_column(harness.world(), &[0, 2]);
        assert_ne!(frame(harness.world(), 2), tile);
        crate::assert_focused!(harness.world(), 2);
    }
}

#[test]
fn green_button_tiling_without_a_drag_restores_the_existing_frame() {
    for tile in [
        IRect::new(0, 20, 512, 768),
        IRect::new(512, 394, 1024, 768),
        IRect::new(0, 20, 1024, 768),
    ] {
        let mut harness = three_columns();
        let expected = frame(harness.world(), 1);
        macos_tile(&mut harness, 1, tile);
        harness.advance(SETTLE);
        assert_eq!(frame(harness.world(), 1), expected);
        assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
    }
}

#[test]
fn a_user_edge_resize_to_a_tile_rectangle_keeps_the_users_size() {
    for released_before_resize in [false, true] {
        let mut harness = three_columns();
        let start = frame(harness.world(), 0);
        press_at(&mut harness, Origin::new(start.max.x - 1, start.center().y));
        harness.advance(FRAME);
        if released_before_resize {
            release(&mut harness, Origin::new(512, start.center().y));
        }
        let tile = IRect::new(0, 20, 512, 768);
        macos_tile(&mut harness, 0, tile);
        harness.advance(ms(200));
        assert_eq!(frame(harness.world(), 0).width(), 512);
        assert_eq!(frame(harness.world(), 0).min, tile.min);
    }
}

#[test]
fn a_programmatic_resize_that_is_not_a_tile_keeps_its_width() {
    let mut harness = three_columns();
    let expected_width = 560;
    let mut resized = frame(harness.world(), 0);
    resized.max.x = resized.min.x + expected_width;
    macos_tile(&mut harness, 0, resized);
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0).width(), expected_width);
}

#[test]
fn macos_tiling_of_a_floating_window_is_left_unchanged() {
    let mut harness = three_columns();
    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Manage),
    ]);
    harness.advance(SETTLE);
    let tile = IRect::new(0, 20, 512, 768);
    macos_tile(&mut harness, 0, tile);
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), tile);
}

#[test]
fn macos_tiling_uses_the_dock_work_area_without_engine_edge_padding() {
    let mut harness = harness_with(
        "[padding]\ntop = 12\nleft = 12\nright = 12\nbottom = 12\n",
        TEST_DISPLAY_HEIGHT,
        3,
    );
    let display = harness
        .world()
        .query::<(Entity, &crate::manager::Display)>()
        .iter(harness.world())
        .find(|(_, display)| display.id() == TEST_DISPLAY_ID)
        .unwrap()
        .0;
    harness
        .world()
        .entity_mut(display)
        .insert(crate::ecs::DockPosition::Bottom(60));
    harness
        .world()
        .get_mut::<crate::manager::Display>(display)
        .unwrap()
        .set_changed();
    let world = harness.world();
    for mut strip in world.query::<&mut LayoutStrip>().iter_mut(world) {
        strip.set_changed();
    }
    harness.advance(SETTLE);
    let expected = frame(harness.world(), 1);
    macos_tile(&mut harness, 1, IRect::new(0, 20, 512, 708));
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 1), expected);
}

#[test]
fn a_native_tab_group_crosses_displays_as_one_stack_item() {
    let mut harness = two_displays("[options]\nstack_limit = 2\n", EXT_DISPLAY_HEIGHT);
    add_native_tab(&mut harness, 2, 3);
    let tabs = layout_on(harness.world(), TEST_WORKSPACE_ID)[2].clone();
    assert_eq!(tabs.len(), 2);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 3, point);
    release(&mut harness, point);
    macos_moves_to_ext(&mut harness, 2);
    macos_moves_to_ext(&mut harness, 3);
    harness.advance(SETTLE);
    let destination = layout_on(harness.world(), EXT_WORKSPACE_ID);
    assert_eq!(destination.len(), 1);
    assert_eq!(destination[0][0], 100);
    assert_eq!(destination[0][1..], tabs);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );
    assert_eq!(frame(harness.world(), 2), frame(harness.world(), 3));
    assert_eq!(
        frame(harness.world(), 3).width(),
        frame(harness.world(), 100).width()
    );
}

#[test]
fn a_display_removed_during_an_edge_hold_cancels_the_hold() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let edge = Origin::new(TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH - 2, 500);
    press_and_drag(&mut harness, 0, edge);
    harness.advance(ms(200));
    assert_eq!(
        harness
            .world()
            .query::<&crate::ecs::drag::EdgeHold>()
            .iter(harness.world())
            .count(),
        1
    );
    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness.world().write_message(Event::DisplayRemoved {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(ms(800));
    assert_eq!(
        harness
            .world()
            .query::<&crate::ecs::drag::EdgeHold>()
            .iter(harness.world())
            .count(),
        0
    );
}

#[test]
fn tiling_restore_keeps_an_apps_minimum_size_and_stops_retrying() {
    let mut harness = three_columns();
    harness
        .mock_state
        .update_window(0, |window| window.minimum_size = Some(Size::new(512, 748)));
    macos_tile(&mut harness, 0, IRect::new(0, 20, 512, 768));
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0).size(), Size::new(512, 748));
    let mut resize_writes = 0;
    harness
        .mock_state
        .update_window(0, |window| resize_writes = window.resize_writes);
    harness.advance(SETTLE);
    harness
        .mock_state
        .update_window(0, |window| assert_eq!(window.resize_writes, resize_writes));
}

#[test]
fn a_lost_release_or_closed_window_clears_the_edge_hold() {
    for closed in [false, true] {
        let mut harness = TestHarness::new().with_windows(6);
        harness.run(vec![
            Event::MenuOpened { window_id: 0 },
            command(Operation::Focus(Direction::Last)),
        ]);
        harness.advance(SETTLE);
        let edge = Origin::new(2, 500);
        press_and_drag(&mut harness, 5, edge);
        harness.advance(ms(200));
        assert_eq!(
            harness
                .world()
                .query::<&crate::ecs::drag::EdgeHold>()
                .iter(harness.world())
                .count(),
            1
        );
        if closed {
            harness.mock_state.os_close_window(5);
        } else {
            harness.mock_state.set_left_button_down(false);
        }
        harness.advance(SETTLE);
        assert_eq!(drag_state(harness.world()), DragState::Idle);
        assert_eq!(
            harness
                .world()
                .query::<&crate::ecs::drag::EdgeHold>()
                .iter(harness.world())
                .count(),
            0
        );
        let position = strip_x(harness.world(), TEST_WORKSPACE_ID);
        harness.advance(ms(1000));
        assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), position);
    }
}

#[test]
fn a_drop_rechecks_the_destination_after_its_last_window_closes() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    harness.mock_state.os_close_window(100);
    harness.advance(ms(100));
    release(&mut harness, point);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), EXT_WORKSPACE_ID), vec![vec![2]]);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );
}

#[test]
fn a_drop_rechecks_a_display_that_disappears_before_release() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness.world().write_message(Event::DisplayRemoved {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(ms(100));
    release(&mut harness, point);
    harness.advance(SETTLE);
    let strips = layout_on(harness.world(), TEST_WORKSPACE_ID);
    assert_eq!(
        strips.iter().flatten().filter(|&&id| id == 2).count(),
        1,
        "{strips:?}"
    );
    assert!(
        IRect::new(0, 20, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT)
            .contains(frame(harness.world(), 2).center())
    );
}
