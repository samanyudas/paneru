use super::*;

#[test]
fn ending_a_press_replays_layout_changes_for_every_native_tab() {
    let mut control = three_columns();
    add_native_tab(&mut control, 1, 3);
    control.mock_state.os_close_window(0);
    control.advance(SETTLE);
    let expected = frame(control.world(), 3);

    let mut failures = Vec::new();
    for ending in [
        "mouse-up",
        "watchdog",
        "mouse-move",
        "resize",
        "confirmation",
    ] {
        let mut harness = three_columns();
        add_native_tab(&mut harness, 1, 3);
        let start = frame(harness.world(), 3);
        let point = start.min + IVec2::new(100, 10);
        press_at(&mut harness, point);
        harness.advance(FRAME);
        assert!(is_pressed(&drag_state(harness.world())));
        harness.mock_state.os_close_window(0);
        harness.advance(SETTLE);
        assert_ne!(start, expected, "closing the neighbor reflows the strip");
        assert_eq!(
            frame(harness.world(), 3),
            start,
            "the press holds the window"
        );

        match ending {
            "watchdog" => harness.mock_state.set_left_button_down(false),
            "mouse-move" => {
                harness.mock_state.set_left_button_down(false);
                harness.world().write_message(Event::MouseMoved {
                    point: cg(point),
                    modifiers: Modifiers::empty(),
                });
            }
            // The resize ends the press, which keeps the window while the
            // button stays down, and lets it go at the release.
            "resize" => {
                harness
                    .world()
                    .write_message(Event::WindowResized { window_id: 3 });
                harness.advance(FRAME);
                assert_eq!(frame(harness.world(), 3), start, "held until release");
                release(&mut harness, point);
            }
            "confirmation" => {
                let moved = point + IVec2::new(20, 0);
                harness.world().write_message(mouse_dragged(moved));
                harness.advance(FRAME);
                release(&mut harness, moved);
                assert!(matches!(
                    drag_state(harness.world()),
                    DragState::Released { .. }
                ));
            }
            _ => release(&mut harness, point),
        }
        harness.advance(SETTLE);

        assert_eq!(drag_state(harness.world()), DragState::Idle);
        for id in [1, 3] {
            let actual = frame(harness.world(), id);
            if actual != expected {
                failures.push((ending, id, actual));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "expected {expected:?}, got {failures:?}"
    );
}

fn ext_bounds(height: i32) -> IRect {
    IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        height,
    )
}

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

/// macOS moves window `id` into the external display's space. The window
/// lands on that display, unless a drag already put it there.
fn macos_moves_to_ext(harness: &mut TestHarness, id: WinID) {
    harness.mock_state.update_window(id, |window| {
        window.workspace_id = EXT_WORKSPACE_ID;
        let shift = TEST_DISPLAY_WIDTH + 100 - window.frame.min.x;
        if window.frame.center().x < TEST_DISPLAY_WIDTH {
            window.frame.min.x += shift;
            window.frame.max.x += shift;
        }
    });
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
}

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

#[test]
fn a_held_drop_is_refused_when_a_display_resize_reduces_the_fit_limit() {
    let mut harness = still_drag_over_a_stack_boundary();
    let DragState::Dragging(drag) = drag_state(harness.world()) else {
        panic!("drag in progress");
    };
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, 500));
    harness.world().write_message(Event::DisplayConfigured {
        display_id: TEST_DISPLAY_ID,
    });
    harness.advance(ms(500));
    assert!(matches!(
        feedback(harness.world()),
        Some(OverlayShape::Refusal(_))
    ));
    release(&mut harness, drag.pointer);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2]]);
}

#[test]
fn a_cross_display_drop_survives_macos_activating_the_destination_before_release() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(ms(100));
    release(&mut harness, point);
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
fn tiling_restore_matches_the_native_frame_of_a_padded_window() {
    for resizing in [false, true] {
        let mut harness = three_columns();
        let padding = (14, 12);
        harness.mock_state.update_window(0, |window| {
            window.horizontal_padding = padding.0;
            window.vertical_padding = padding.1;
        });
        let before = frame(harness.world(), 0);
        if resizing {
            let edge = Origin::new(before.max.x - padding.0 - 1, before.center().y);
            press_at(&mut harness, edge);
            harness.advance(FRAME);
        }
        let tile = crate::ecs::systems::padded(IRect::new(0, 20, 512, 768), padding);
        macos_tile(&mut harness, 0, tile);
        harness.advance(SETTLE);
        if resizing {
            assert_eq!(frame(harness.world(), 0).width(), tile.width());
        } else {
            assert_eq!(frame(harness.world(), 0), before);
        }
    }
}

#[test]
fn a_lost_mouse_up_keeps_grace_for_a_delayed_manual_edge_resize() {
    for pointer_moved in [false, true] {
        let mut harness = three_columns();
        let start = frame(harness.world(), 0);
        let edge = Origin::new(start.max.x - 1, start.center().y);
        press_at(&mut harness, edge);
        harness.advance(FRAME);
        harness.mock_state.set_left_button_down(false);
        if pointer_moved {
            harness.world().write_message(Event::MouseMoved {
                point: cg(edge + IVec2::new(60, 0)),
                modifiers: Modifiers::empty(),
            });
            harness.advance(ms(150));
        }
        let tile = IRect::new(0, 20, 512, 768);
        macos_tile(&mut harness, 0, tile);
        harness.advance(SETTLE);
        assert_eq!(frame(harness.world(), 0), tile);
        macos_tile(&mut harness, 0, IRect::new(0, 20, 512, 394));
        harness.advance(SETTLE);
        assert_eq!(
            frame(harness.world(), 0),
            tile,
            "release grace must expire for subsequent macOS tiling"
        );
    }
}

#[test]
fn early_display_activation_keeps_a_refused_drag_in_its_source() {
    for native_tabs in [false, true] {
        let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
        let grabbed = if native_tabs {
            add_native_tab(&mut harness, 2, 3);
            3
        } else {
            2
        };
        let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
        let members = source[2].clone();
        let point = middle_of(harness.world(), 100, 900);
        press_and_drag(&mut harness, grabbed, point);
        harness.mock_state.set_left_button_down(false);
        for member in members {
            macos_moves_to_ext(&mut harness, member);
        }
        harness.advance(ms(100));
        assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
        release(&mut harness, point);
        harness.advance(SETTLE);
        assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
        assert_eq!(
            layout_on(harness.world(), EXT_WORKSPACE_ID),
            vec![vec![100]]
        );
    }
}

#[test]
fn a_late_accessibility_element_keeps_a_cross_display_drop() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    release(&mut harness, point);
    harness
        .mock_state
        .os_transfer_window_late(2, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(ms(400));
    harness.mock_state.os_list_window(2);
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
fn a_batched_gesture_and_display_activation_keep_a_refused_drag_in_its_source() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let start = frame(harness.world(), 2);
    let press = start.min + IVec2::new(100, 10);
    let point = middle_of(harness.world(), 100, 900);
    press_at(&mut harness, press);
    for at in 1..=6 {
        let sample = press + (point - press) * at / 6;
        harness.world().write_message(mouse_dragged(sample));
        harness
            .mock_state
            .os_move_window(2, start.min + (sample - press));
        for event in harness.mock_state.drain_events() {
            harness.world().write_message(event);
        }
    }
    harness.mock_state.set_left_button_down(false);
    macos_moves_to_ext(&mut harness, 2);
    for event in harness.mock_state.drain_events() {
        harness.world().write_message(event);
    }
    harness.world().write_message(mouse_up(point));
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn early_display_activation_preserves_the_width_ratio_of_a_new_column_and_its_native_tabs() {
    for native_tabs in [false, true] {
        let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
        let grabbed = if native_tabs {
            add_native_tab(&mut harness, 2, 3);
            3
        } else {
            2
        };
        let members = layout_on(harness.world(), TEST_WORKSPACE_ID)[2].clone();
        let start = frame(harness.world(), grabbed);
        let target = frame(harness.world(), 100);
        let point = Origin::new(target.max.x - 20, 600);
        press_and_drag(&mut harness, grabbed, point);
        harness.mock_state.set_left_button_down(false);
        for &member in &members {
            macos_moves_to_ext(&mut harness, member);
        }
        harness.advance(ms(100));
        assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID)[2], members);
        release(&mut harness, point);
        harness.advance(SETTLE);
        assert_eq!(
            layout_on(harness.world(), EXT_WORKSPACE_ID),
            vec![vec![100], members.clone()]
        );
        let expected = crate::util::round_px(
            f64::from(start.width()) / f64::from(TEST_DISPLAY_WIDTH) * f64::from(EXT_DISPLAY_WIDTH),
        );
        for member in members {
            assert_eq!(frame(harness.world(), member).width(), expected);
        }
    }
}

#[test]
fn native_batch_migration_of_other_windows_keeps_the_held_window_in_its_source() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    harness.mock_state.set_left_button_down(false);
    for id in [0, 1, 2] {
        macos_moves_to_ext(&mut harness, id);
    }
    harness.advance(ms(100));
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), vec![vec![2]]);
    let moved = harness
        .world()
        .query::<&LayoutStrip>()
        .iter(harness.world())
        .filter(|strip| strip.id() == EXT_WORKSPACE_ID)
        .map(|strip| strip.all_windows().len())
        .sum::<usize>();
    assert_eq!(
        moved, 3,
        "the native batch still moves windows 0 and 1 beside external 100"
    );
    release(&mut harness, point);
    harness.advance(SETTLE);
    let count = harness
        .world()
        .query::<&LayoutStrip>()
        .iter(harness.world())
        .map(|strip| strip.all_windows().len())
        .sum::<usize>();
    assert_eq!(
        count, 4,
        "the drop keeps every managed window in the layout"
    );
}

#[test]
fn review_lost_resize_release_expires_without_followup_events() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 0);
    press_at(&mut harness, Origin::new(start.max.x - 1, start.center().y));
    harness.advance(FRAME);
    harness.mock_state.set_left_button_down(false);
    harness.advance(ms(2000));
    macos_tile(&mut harness, 0, IRect::new(0, 20, 512, 768));
    harness.advance(SETTLE);
    assert_eq!(
        frame(harness.world(), 0),
        start,
        "a later native tile must restore after the lost-release grace expires"
    );
}

#[test]
fn fresh_native_migration_after_a_drop_applies_without_a_deadline() {
    for before_expiry in [false, true] {
        let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
        let point = middle_of(harness.world(), 0, 600);
        press_and_drag(&mut harness, 2, point);
        release(&mut harness, point);
        harness.advance(if before_expiry { ms(100) } else { SETTLE });
        let body = frame(harness.world(), 2).center();
        press_at(&mut harness, body);
        harness.advance(FRAME);
        release(&mut harness, body);
        assert_eq!(drag_state(harness.world()), DragState::Idle);
        let entity = harness
            .world()
            .query::<(Entity, &Window)>()
            .iter(harness.world())
            .find_map(|(entity, window)| (window.id() == 2).then_some(entity))
            .unwrap();
        assert!(
            harness
                .world()
                .get::<crate::ecs::RepositionMarker>(entity)
                .is_none()
        );
        assert!(
            harness
                .world()
                .get::<crate::ecs::ResizeMarker>(entity)
                .is_none()
        );
        macos_moves_to_ext(&mut harness, 2);
        harness.advance(ms(2000));
        assert_eq!(
            layout_on(harness.world(), EXT_WORKSPACE_ID),
            vec![vec![100], vec![2]],
            "before_expiry={before_expiry}"
        );
        assert_eq!(
            layout_on(harness.world(), TEST_WORKSPACE_ID),
            vec![vec![0], vec![1]],
            "before_expiry={before_expiry}"
        );
    }
}

#[test]
fn review_display_reconciliation_preserves_refused_drag_source() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    macos_moves_to_ext(&mut harness, 2);
    harness.world().write_message(Event::DisplayConfigured {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(ms(450));
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        source,
        "display reconciliation must not migrate the held window before a refused drop"
    );
    release(&mut harness, point);
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

fn window_entity(harness: &mut TestHarness, id: WinID) -> Entity {
    harness
        .world()
        .query::<(Entity, &Window)>()
        .iter(harness.world())
        .find_map(|(entity, window)| (window.id() == id).then_some(entity))
        .unwrap()
}

fn refused_return_with_native_echo() -> TestHarness {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(FRAME * 3);
    release(&mut harness, point);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    harness
}

#[test]
fn deferred_activation_requeries_after_a_press_is_cancelled_without_another_activation() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let start = frame(harness.world(), 2);
    press_at(&mut harness, start.min + IVec2::new(100, 10));
    harness.advance(FRAME);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(FRAME * 3);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    let reads = harness.mock_state.membership_reads(EXT_WORKSPACE_ID);
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 2 });
    harness.advance(FRAME);
    release(&mut harness, start.min + IVec2::new(100, 10));
    harness.advance(FRAME * 3);
    assert!(harness.mock_state.membership_reads(EXT_WORKSPACE_ID) > reads);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn deferred_activation_uses_the_latest_native_destination() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let start = frame(harness.world(), 2);
    press_at(&mut harness, start.min + IVec2::new(100, 10));
    harness.advance(FRAME);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(FRAME * 3);
    harness
        .mock_state
        .update_window(2, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 2 });
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn verified_return_suppresses_duplicate_old_membership_without_a_deadline() {
    let mut harness = refused_return_with_native_echo();
    for _ in 0..3 {
        harness.world().write_message(Event::SpaceChanged);
        harness.advance(ms(1000));
    }
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn verified_return_allows_a_changed_native_frame_without_new_pointer_input() {
    let mut harness = refused_return_with_native_echo();
    harness.mock_state.update_window(2, |window| {
        window.frame.min.x += 300;
        window.frame.max.x += 300;
    });
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn verified_return_survives_later_engine_scroll_and_resize() {
    let mut harness = refused_return_with_native_echo();
    let entity = window_entity(&mut harness, 2);
    let strip = harness
        .world()
        .query::<(Entity, &LayoutStrip)>()
        .iter(harness.world())
        .find_map(|(entity, strip)| (strip.id() == TEST_WORKSPACE_ID).then_some(entity))
        .unwrap();
    harness
        .world()
        .get_mut::<crate::ecs::Position>(strip)
        .unwrap()
        .0
        .x -= 100;
    harness.advance(SETTLE);
    let size = harness.world().get::<crate::ecs::Bounds>(entity).unwrap().0;
    harness
        .world()
        .entity_mut(entity)
        .insert(crate::ecs::ResizeMarker(size.with_x(size.x + 50)));
    harness.advance(SETTLE);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
}

#[test]
fn a_press_during_late_handle_recovery_supersedes_the_old_placement() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    release(&mut harness, point);
    harness
        .mock_state
        .os_transfer_window_late(2, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(ms(100));
    let entity = window_entity(&mut harness, 2);
    assert!(
        harness
            .world()
            .get::<crate::ecs::AwaitingHandle>(entity)
            .is_some()
    );
    let body = frame(harness.world(), 2).center();
    harness.world().write_message(Event::MouseDown {
        point: crate::manager::origin_to(body),
        button: MouseButton::Left,
        window_id: Some(2),
        modifiers: Modifiers::empty(),
    });
    harness.advance(FRAME);
    release(&mut harness, body);
    harness.mock_state.os_list_window(2);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn a_failed_return_releases_membership_after_native_verification_fails() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    macos_moves_to_ext(&mut harness, 2);
    harness
        .mock_state
        .update_window(2, |window| window.dropped_positions = 1000);
    release(&mut harness, point);
    harness.advance(ms(2000));
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn native_query_failure_backs_off_suspends_and_wakes_on_a_fresh_signal() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    harness
        .mock_state
        .fail_membership_reads(EXT_WORKSPACE_ID, 1000);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(ms(3000));
    let reads = harness.mock_state.membership_reads(EXT_WORKSPACE_ID);
    harness.advance(ms(3000));
    assert_eq!(harness.mock_state.membership_reads(EXT_WORKSPACE_ID), reads);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    harness
        .mock_state
        .fail_membership_reads(EXT_WORKSPACE_ID, 0);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn fresh_activation_migrates_unrelated_windows_while_another_group_remains_held() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(FRAME * 3);
    macos_moves_to_ext(&mut harness, 0);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(FRAME * 3);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![0]]
    );
}

#[test]
fn one_native_tab_report_cannot_extract_another_held_tab() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    add_native_tab(&mut harness, 2, 3);
    let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 3, point);
    macos_moves_to_ext(&mut harness, 2);
    harness.world().write_message(Event::DisplayConfigured {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(ms(450));
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
    release(&mut harness, point);
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
}

#[test]
fn removing_a_group_member_during_placement_does_not_keep_other_windows_owned() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    add_native_tab(&mut harness, 2, 3);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 3, point);
    release(&mut harness, point);
    harness
        .mock_state
        .os_transfer_window_late(3, EXT_WORKSPACE_ID);
    harness.advance(ms(100));
    harness.mock_state.os_close_window(3);
    let entity = window_entity(&mut harness, 3);
    harness.world().despawn(entity);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn a_deferred_activation_follows_a_different_live_space_on_replay() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let third_space = EXT_WORKSPACE_ID + 1;
    let display = harness
        .world()
        .query::<(Entity, &crate::manager::Display)>()
        .iter(harness.world())
        .find_map(|(entity, display)| (display.id() == EXT_DISPLAY_ID).then_some(entity))
        .unwrap();
    harness.world().spawn((
        LayoutStrip::new(third_space, 0),
        ChildOf(display),
        crate::ecs::Position(Origin::new(TEST_DISPLAY_WIDTH, 0)),
        crate::ecs::SelectedVirtualMarker,
    ));
    let start = frame(harness.world(), 2);
    press_at(&mut harness, start.min + IVec2::new(100, 10));
    harness.advance(FRAME);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(FRAME * 3);
    harness
        .mock_state
        .update_window(2, |window| window.workspace_id = third_space);
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 2 });
    harness.advance(FRAME);
    release(&mut harness, start.min + IVec2::new(100, 10));
    harness.advance(SETTLE);
    let entity = window_entity(&mut harness, 2);
    assert!(
        harness
            .world()
            .query::<&LayoutStrip>()
            .iter(harness.world())
            .any(|strip| strip.id() == third_space && strip.contains(entity))
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn a_new_resize_press_after_idle_grace_keeps_manual_resize_classification() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 0);
    press_at(&mut harness, Origin::new(start.max.x - 1, start.center().y));
    harness.advance(FRAME);
    harness.mock_state.set_left_button_down(false);
    harness.advance(ms(2000));
    press_at(&mut harness, Origin::new(start.max.x - 1, start.center().y));
    harness.advance(FRAME);
    macos_tile(&mut harness, 0, IRect::new(0, 20, 512, 768));
    release(&mut harness, Origin::new(511, start.center().y));
    harness.advance(ms(100));
    assert_eq!(frame(harness.world(), 0).width(), 512);
}

#[test]
fn a_later_native_move_of_a_returned_window_applies() {
    let mut harness = refused_return_with_native_echo();
    harness
        .mock_state
        .update_window(2, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(FRAME * 3);
    macos_moves_to_ext(&mut harness, 2);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn failed_native_frame_readback_cannot_acknowledge_a_return() {
    let mut harness = two_displays("[options]\nstack_limit = 1\n", EXT_DISPLAY_HEIGHT);
    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 2, point);
    macos_moves_to_ext(&mut harness, 2);
    release(&mut harness, point);
    harness
        .mock_state
        .update_window(2, |window| window.frame_read_failures = 255);
    harness.advance(ms(1500));
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

#[test]
fn append_to_a_display_without_a_strip_finishes_its_pending_placement() {
    use bevy::ecs::system::RunSystemOnce;
    for native_tabs in [false, true] {
        let mut harness = TestHarness::new()
            .with_display(
                EXT_DISPLAY_ID,
                ext_bounds(EXT_DISPLAY_HEIGHT),
                vec![EXT_WORKSPACE_ID],
            )
            .with_windows(3);
        harness.run(vec![Event::MenuOpened { window_id: 0 }]);
        harness.advance(SETTLE);
        if native_tabs {
            add_native_tab(&mut harness, 0, 3);
        }
        let expected = layout_on(harness.world(), TEST_WORKSPACE_ID)[0].clone();
        let strips: Vec<_> = harness
            .world()
            .query::<(Entity, &LayoutStrip)>()
            .iter(harness.world())
            .filter_map(|(entity, strip)| (strip.id() == EXT_WORKSPACE_ID).then_some(entity))
            .collect();
        for strip in strips {
            harness.world().despawn(strip);
        }
        harness.world().write_message(Event::Command {
            command: Command::Window(Operation::ToNextDisplay(crate::commands::MoveFocus::Follow)),
        });
        harness.advance(SETTLE);
        assert!(
            !layout_on(harness.world(), TEST_WORKSPACE_ID)
                .iter()
                .flatten()
                .any(|id| *id == 0)
        );
        assert!(ext_bounds(EXT_DISPLAY_HEIGHT).contains(frame(harness.world(), 0).center()));
        let active = harness
            .world()
            .run_system_once(|activity: crate::ecs::params::FrameActivity| activity.mid_frame())
            .unwrap();
        assert!(
            !active,
            "the empty-display append must finish without a target strip"
        );
        harness.world().write_message(Event::SpaceChanged);
        harness.advance(ms(2000));
        assert!(
            !layout_on(harness.world(), TEST_WORKSPACE_ID)
                .iter()
                .flatten()
                .any(|id| expected.contains(id))
        );
        harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
        harness.advance(FRAME * 3);
        harness.world().write_message(Event::SpaceCreated {
            space_id: EXT_WORKSPACE_ID,
        });
        harness.advance(FRAME * 3);
        harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
        harness.advance(FRAME * 3);
        assert!(
            !layout_on(harness.world(), TEST_WORKSPACE_ID)
                .iter()
                .flatten()
                .any(|id| expected.contains(id))
        );
        for member in &expected {
            macos_moves_to_ext(&mut harness, *member);
        }
        harness.advance(SETTLE);
        assert_eq!(layout_on(harness.world(), EXT_WORKSPACE_ID), vec![expected]);
    }
}

#[test]
fn an_external_resize_keeps_a_returned_window_on_its_display() {
    let mut harness = refused_return_with_native_echo();
    let old = frame(harness.world(), 2);
    harness
        .mock_state
        .os_resize_window(2, old.size().with_x(old.width() + 73));
    harness.advance(FRAME * 3);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn a_closed_window_stays_removed_until_a_fresh_creation_reuses_its_id() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    let removed = window_entity(&mut harness, 2);
    let old_frame = frame(harness.world(), 2);
    let element = harness.world().get::<Window>(removed).unwrap().element();
    harness
        .mock_state
        .fail_membership_reads(TEST_WORKSPACE_ID, 1);
    harness.world().write_message(Event::WindowDestroyed {
        window_id: 2,
        source: crate::events::DestroySource::Accessibility { element },
    });
    harness.advance(SETTLE);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert!(
        harness
            .world()
            .query::<&Window>()
            .iter(harness.world())
            .all(|window| window.id() != 2),
        "warm native and AX lists cannot recreate an authoritative close"
    );

    let replacement =
        harness
            .mock_state
            .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, old_frame);
    harness
        .world()
        .trigger(crate::ecs::SpawnWindowTrigger(vec![replacement]));
    harness.advance(SETTLE);
    assert_ne!(window_entity(&mut harness, 2), removed);
    macos_moves_to_ext(&mut harness, 2);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![2]]
    );
}

fn click(harness: &mut TestHarness, at: Origin) {
    press_at(harness, at);
    harness.advance(FRAME);
    release(harness, at);
}

fn membership_reads(harness: &TestHarness) -> usize {
    harness.mock_state.membership_reads(TEST_WORKSPACE_ID)
        + harness.mock_state.membership_reads(EXT_WORKSPACE_ID)
}

#[test]
fn a_click_on_a_returned_window_keeps_it_on_its_display() {
    let mut harness = refused_return_with_native_echo();
    let body = frame(harness.world(), 2).center();
    click(&mut harness, body);
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn clicks_read_no_native_membership_and_leave_other_windows_in_place() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    harness
        .mock_state
        .update_window(1, |window| window.on_all_spaces = true);
    let reads = membership_reads(&harness);
    for _ in 0..5 {
        let body = frame(harness.world(), 0).center();
        click(&mut harness, body);
        harness.advance(ms(200));
    }
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
    assert_eq!(membership_reads(&harness), reads);
}

#[test]
fn an_edge_resize_from_a_shared_column_boundary_keeps_the_users_size() {
    // A press just inside the right column, named by the window server, and
    // a press on the boundary that names no window, as in a padded gap.
    for named in [true, false] {
        let mut harness = three_columns();
        let left = frame(harness.world(), 0);
        let right = frame(harness.world(), 1);
        let press = if named {
            Origin::new(right.min.x + 2, left.center().y)
        } else {
            Origin::new(left.max.x, left.center().y)
        };
        harness.mock_state.set_left_button_down(true);
        let window_id = named.then_some(1);
        if named {
            assert_eq!(harness.mock_state.window_at(press), window_id);
        }
        harness.world().write_message(Event::MouseDown {
            point: cg(press),
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
            window_id,
        });
        harness.advance(FRAME);
        let tile = IRect::new(0, 20, 512, 768);
        macos_tile(&mut harness, 0, tile);
        harness.advance(ms(200));
        assert_eq!(frame(harness.world(), 0), tile, "named={named}");
    }
}

#[test]
fn layout_changes_after_a_returned_drop_read_no_native_membership() {
    let mut harness = refused_return_with_native_echo();
    let reads = membership_reads(&harness);
    for swap in 0..10 {
        let direction = if swap % 2 == 0 {
            Direction::West
        } else {
            Direction::East
        };
        harness.world().write_message(Event::Command {
            command: Command::Window(Operation::Swap(direction)),
        });
        harness.advance(ms(100));
    }
    assert_eq!(membership_reads(&harness), reads);
}

fn has_window(harness: &mut TestHarness, id: WinID) -> bool {
    harness
        .world()
        .query::<&Window>()
        .iter(harness.world())
        .any(|window| window.id() == id)
}

#[test]
fn a_window_closed_while_its_handle_was_late_returns_once_the_app_lists_it() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    harness
        .mock_state
        .os_transfer_window_late(2, TEST_WORKSPACE_ID);
    harness.advance(SETTLE);
    assert!(
        !has_window(&mut harness, 2),
        "the handle recovery gave up and closed the window"
    );
    harness.mock_state.os_list_window(2);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert!(has_window(&mut harness, 2));
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![0], vec![1], vec![2]]
    );
}

#[test]
fn an_ignored_native_window_is_checked_again_after_a_while() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        7,
        IRect::new(100, 20, 500, 768),
    );
    harness
        .mock_state
        .update_window(7, |window| window.ax_unlisted = true);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert!(!has_window(&mut harness, 7));
    harness.mock_state.os_list_window(7);
    harness.advance(ms(60_000));
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(SETTLE);
    assert!(has_window(&mut harness, 7));
}

#[test]
fn a_stale_report_for_the_display_a_parked_column_overhangs_changes_nothing() {
    let ext = ext_bounds(EXT_DISPLAY_HEIGHT);
    let mut harness = TestHarness::new()
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(ext.min.x + 100, 20, ext.min.x + 900, ext.max.y);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    let parked = frame(harness.world(), 3);
    assert!(
        parked.min.x < TEST_DISPLAY_WIDTH && ext.contains(parked.center()),
        "window 3 parks as a sliver at the edge toward the external display: {parked:?}"
    );
    let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
    assert!(source.iter().flatten().any(|id| *id == 3));

    harness
        .mock_state
        .update_window(3, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn a_returned_window_parked_past_the_outer_edge_ignores_a_stale_report() {
    let config = crate::config::Config::try_from("[options]\nstack_limit = 1\n").unwrap();
    let ext = ext_bounds(EXT_DISPLAY_HEIGHT);
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_windows(5)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(ext.min.x + 100, 20, ext.min.x + 900, ext.max.y);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
    assert_eq!(source.len(), 5);

    let point = middle_of(harness.world(), 100, 900);
    press_and_drag(&mut harness, 0, point);
    macos_moves_to_ext(&mut harness, 0);
    harness.advance(FRAME * 3);
    release(&mut harness, point);
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);

    harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
    harness.advance(FRAME * 3);
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    harness.advance(SETTLE);
    let parked = frame(harness.world(), 0);
    assert!(
        parked.center().x < 0,
        "window 0 parks past the outer edge: {parked:?}"
    );

    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn a_warm_listed_closed_window_asks_only_its_own_app() {
    const OTHER_APP: i32 = TEST_PROCESS_ID + 1;
    let mut harness =
        two_displays("", EXT_DISPLAY_HEIGHT).with_app(OTHER_APP, "other", "Other", |_| {});
    harness.advance(SETTLE);
    let removed = window_entity(&mut harness, 2);
    let element = harness.world().get::<Window>(removed).unwrap().element();
    harness.world().write_message(Event::WindowDestroyed {
        window_id: 2,
        source: crate::events::DestroySource::Accessibility { element },
    });
    harness.advance(SETTLE);
    assert!(!has_window(&mut harness, 2));
    let reads = harness.mock_state.window_list_reads(OTHER_APP);
    for _ in 0..3 {
        harness.world().write_message(Event::SpaceChanged);
        harness.advance(ms(500));
    }
    assert!(!has_window(&mut harness, 2));
    assert_eq!(harness.mock_state.window_list_reads(OTHER_APP), reads);
}

#[test]
fn a_stale_report_during_an_engine_animation_changes_nothing() {
    let config = crate::config::Config::try_from("[options]\nanimation_speed = 12.0\n").unwrap();
    let ext = ext_bounds(EXT_DISPLAY_HEIGHT);
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(ext.min.x + 100, 20, ext.min.x + 900, ext.max.y);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
    assert!(ext.contains(frame(harness.world(), 3).center()));

    // macOS lists window 3 in the external space without moving it, and the
    // report arrives while a scroll animates the window back onto its display.
    harness
        .mock_state
        .update_window(3, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(FRAME);
    let entity = window_entity(&mut harness, 3);
    let presented = harness
        .world()
        .get::<crate::ecs::PresentedOrigin>(entity)
        .unwrap()
        .0;
    let moving = frame(harness.world(), 3);
    assert_ne!(moving.min, presented, "window 3 is still animating");
    assert!(ext.contains(moving.center()), "{moving:?}");
    harness.advance(SETTLE);
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn an_edge_resize_of_a_native_tab_keeps_the_users_size_for_every_tab() {
    let mut harness = three_columns();
    add_native_tab(&mut harness, 0, 3);
    assert_eq!(layout(harness.world())[0].len(), 2, "one tab group");
    // The background tab reports wider frame insets, so its own edge band
    // misses a press on the visible tab's edge.
    let padding = (14, 0);
    harness
        .mock_state
        .update_window(0, |window| window.horizontal_padding = padding.0);
    let start = frame(harness.world(), 3);
    let edge = Origin::new(start.max.x - 1, start.center().y);
    press_at(&mut harness, edge);
    harness.advance(FRAME);
    let tile = IRect::new(0, 20, 512, 768);
    let padded_tile = crate::ecs::systems::padded(tile, padding);
    // The user resizes the visible tab, and the app reports the shared
    // resize for the background tab too.
    macos_tile(&mut harness, 3, tile);
    macos_tile(&mut harness, 0, padded_tile);
    harness.advance(ms(200));
    assert_eq!(frame(harness.world(), 3), tile);
    assert_eq!(frame(harness.world(), 0), padded_tile);
}

#[test]
fn a_real_native_move_during_an_engine_animation_applies() {
    let config = crate::config::Config::try_from("[options]\nanimation_speed = 12.0\n").unwrap();
    let ext = ext_bounds(EXT_DISPLAY_HEIGHT);
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(ext.min.x + 100, 20, ext.min.x + 900, ext.max.y);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);

    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    harness.advance(FRAME);
    let entity = window_entity(&mut harness, 0);
    assert!(
        harness
            .world()
            .get::<crate::ecs::RepositionMarker>(entity)
            .is_some(),
        "the scroll is animating window 0"
    );
    // macOS moves window 0 onto the external display and into its space, and
    // its move and the display change arrive together.
    macos_moves_to_ext(&mut harness, 0);
    harness
        .world()
        .write_message(Event::WindowMoved { window_id: 0 });
    for event in harness.mock_state.drain_events() {
        harness.world().write_message(event);
    }
    harness.advance(SETTLE);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100], vec![0]]
    );
    assert_eq!(
        layout_on(harness.world(), TEST_WORKSPACE_ID),
        vec![vec![1], vec![2], vec![3]]
    );
}

#[test]
fn a_neighbor_display_showing_a_fullscreen_space_still_stops_the_edge_scroll() {
    let mut harness = two_displays("", EXT_DISPLAY_HEIGHT);
    // The external display shows a native fullscreen space, which has no
    // managed strip, so the drag snapshot leaves that display out.
    harness
        .mock_state
        .activate_workspace(EXT_DISPLAY_ID, EXT_WORKSPACE_ID + 1, true);
    let edge = Origin::new(TEST_DISPLAY_WIDTH - 2, 500);
    press_and_drag(&mut harness, 0, edge);
    let before = strip_x(harness.world(), TEST_WORKSPACE_ID);
    harness.advance(ms(1200));
    assert_eq!(strip_x(harness.world(), TEST_WORKSPACE_ID), before);
}

/// The main display above a smaller, empty display, as on the native rig:
/// 2560x1440 at the origin, and 2048x1332 below it.
fn stacked_displays(config: &str) -> (TestHarness, IRect) {
    let config = crate::config::Config::try_from(config).expect("config parses");
    let lower = IRect::new(182, 1440, 2230, 2772);
    let harness = TestHarness::new().with_config(config);
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, 2560, 1440));
    let mut harness = harness
        .with_display(EXT_DISPLAY_ID, lower, vec![EXT_WORKSPACE_ID])
        .with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    (harness, lower)
}

#[test]
fn a_window_stacked_into_a_new_column_on_a_lower_display_stays_on_that_display() {
    let (mut harness, lower) = stacked_displays("[options]\nstack_limit = 2\n");
    title_drag(
        &mut harness,
        0,
        Origin::new(lower.center().x, lower.min.y + 120),
    );
    assert_eq!(layout_on(harness.world(), EXT_WORKSPACE_ID), vec![vec![0]]);
    let column = frame(harness.world(), 0);
    assert!(lower.contains(column.center()), "{column:?}");

    let bottom = Origin::new(column.center().x, lower.max.y - 100);
    title_drag(&mut harness, 1, bottom);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![0, 1]]
    );
    let (top, below) = (frame(harness.world(), 0), frame(harness.world(), 1));
    for frame in [top, below] {
        assert!(
            frame.min.y >= lower.min.y && frame.max.y <= lower.max.y,
            "the stack stays inside the lower display: {top:?} {below:?}"
        );
    }
    assert_eq!(top.min.y, column.min.y, "the column top does not move");
}

#[test]
fn an_animated_macos_tile_returns_to_the_slot_it_had_before_the_animation() {
    let quarter = IRect::new(512, 20, 1024, 394);
    let half = IRect::new(0, 20, 512, 768);
    let fill = IRect::new(0, 20, 1024, 768);
    for tile in [half, quarter, fill] {
        let mut harness = three_columns();
        let start = frame(harness.world(), 1);
        // macOS animates the window toward the tile, and reports each step.
        for step in 1..=4 {
            let at = |from: i32, to: i32| from + (to - from) * step / 4;
            let frame = IRect::new(
                at(start.min.x, tile.min.x),
                at(start.min.y, tile.min.y),
                at(start.max.x, tile.max.x),
                at(start.max.y, tile.max.y),
            );
            macos_tile(&mut harness, 1, frame);
        }
        harness.advance(SETTLE);
        assert_eq!(frame(harness.world(), 1), start, "tile {tile:?}");
        assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
    }
}

#[test]
fn a_drag_whose_move_notification_arrives_late_still_drops() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 2);
    let press = start.min + IVec2::new(100, 10);
    let to = middle_of(harness.world(), 0, 600);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    // The window follows the pointer, but the app's move notifications reach
    // the engine only 300ms later, as a slow window ID lookup delays them.
    for at in 1..=6 {
        let point = press + (to - press) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        let origin = start.min + (point - press);
        harness.mock_state.update_window(2, |window| {
            window.frame = IRect::from_corners(origin, origin + start.size());
        });
        harness.advance(FRAME);
    }
    harness.advance(ms(300));
    harness
        .world()
        .write_message(Event::WindowMoved { window_id: 2 });
    harness.advance(FRAME);
    release(&mut harness, to);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
}

#[test]
fn a_toolbar_text_selection_never_becomes_a_window_drag() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 2);
    let press = start.min + IVec2::new(100, 10);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    for at in 1..=6 {
        harness
            .world()
            .write_message(mouse_dragged(press + IVec2::new(30 * at, 0)));
        harness.advance(FRAME);
    }
    harness.advance(ms(300));
    harness
        .world()
        .write_message(mouse_dragged(press + IVec2::new(190, 0)));
    harness.advance(FRAME);
    assert!(matches!(
        drag_state(harness.world()),
        DragState::Unconfirmed(_)
    ));
    release(&mut harness, press + IVec2::new(190, 0));
    assert_eq!(drag_state(harness.world()), DragState::Idle);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
}

#[test]
fn a_window_held_at_a_screen_edge_still_drags_and_drops_at_the_pointer() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 0);
    assert_eq!(start.min.x, 0, "window 0 sits on the screen's left edge");
    let press = start.min + IVec2::new(100, 10);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    // The pointer moves right and down, but the screen edge holds the
    // window's left edge, so macOS moves it only down.
    for at in 1..=6 {
        let pointer = press + IVec2::new(30 * at, 59 * at);
        harness.world().write_message(mouse_dragged(pointer));
        harness
            .mock_state
            .os_move_window(0, start.min.with_y(start.min.y + 59 * at));
        harness.advance(FRAME);
    }
    let to = middle_of(harness.world(), 1, 600);
    harness.world().write_message(mouse_dragged(to));
    harness.advance(FRAME);
    assert!(
        matches!(drag_state(harness.world()), DragState::Dragging(_)),
        "{:?}",
        drag_state(harness.world())
    );
    release(&mut harness, to);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![1, 0], vec![2]]);
}

#[test]
fn an_app_moving_its_window_across_the_pointer_is_not_a_drag() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 1);
    let press = start.min + IVec2::new(100, 10);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    // The pointer moves right while the app moves its own window down.
    for at in 1..=6 {
        harness
            .world()
            .write_message(mouse_dragged(press + IVec2::new(30 * at, 0)));
        harness
            .mock_state
            .os_move_window(1, start.min.with_y(start.min.y + 40 * at));
        harness.advance(FRAME);
    }
    assert!(!matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    release(&mut harness, press + IVec2::new(180, 0));
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
}

/// Presses window 1, away from every screen edge, and drags the pointer
/// right and down while macOS moves the window only down. Returns the
/// window's slot and the frame macOS left it at.
fn drag_window_one_on_one_axis(harness: &mut TestHarness) -> (IRect, IRect, Origin) {
    let start = frame(harness.world(), 1);
    let press = start.min + IVec2::new(100, 10);
    press_at(harness, press);
    harness.advance(FRAME);
    let mut pointer = press;
    for at in 1..=6 {
        pointer = press + IVec2::new(30 * at, 59 * at);
        harness.world().write_message(mouse_dragged(pointer));
        harness
            .mock_state
            .os_move_window(1, start.min.with_y(start.min.y + 59 * at));
        harness.advance(FRAME);
    }
    // One more pointer sample after the confirm window ends the press.
    harness.advance(ms(300));
    harness.world().write_message(mouse_dragged(pointer));
    harness.advance(FRAME);
    let origin = start.min.with_y(start.min.y + 59 * 6);
    (
        start,
        IRect::from_corners(origin, origin + start.size()),
        pointer,
    )
}

#[test]
fn a_window_off_the_screen_edges_moving_on_one_axis_does_not_confirm() {
    let mut harness = three_columns();
    let (_, moved, pointer) = drag_window_one_on_one_axis(&mut harness);
    assert_eq!(frame(harness.world(), 1), moved);
    assert!(
        matches!(drag_state(harness.world()), DragState::Unconfirmed(_)),
        "{:?}",
        drag_state(harness.world())
    );
    release(&mut harness, pointer);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
}

#[test]
fn an_unconfirmed_press_leaves_the_window_where_macos_put_it_until_the_release() {
    let mut harness = three_columns();
    let (start, moved, pointer) = drag_window_one_on_one_axis(&mut harness);
    harness.advance(SETTLE);
    assert_eq!(
        frame(harness.world(), 1),
        moved,
        "the engine does not pull back a window macOS may still be dragging"
    );
    release(&mut harness, pointer);
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 1), start);
}

#[test]
fn a_lost_release_with_the_button_up_returns_the_window_to_its_slot() {
    let mut harness = three_columns();
    let (start, moved, _) = drag_window_one_on_one_axis(&mut harness);
    assert_eq!(frame(harness.world(), 1), moved);
    // The release never reaches the tap, but the button reads up.
    harness.mock_state.set_left_button_down(false);
    harness.advance(ms(600));
    assert_eq!(drag_state(harness.world()), DragState::Idle);
    assert_eq!(frame(harness.world(), 1), start);
}

#[test]
fn a_button_stuck_down_ends_an_unconfirmed_press_at_the_press_limit() {
    for moved in [false, true] {
        let mut harness = three_columns();
        let start = frame(harness.world(), 1);
        if moved {
            drag_window_one_on_one_axis(&mut harness);
        } else {
            press_at(&mut harness, start.min + IVec2::new(100, 10));
        }
        // No more events, and the button reads down forever.
        harness.advance(Duration::from_secs(9));
        assert_ne!(
            drag_state(harness.world()),
            DragState::Idle,
            "moved={moved}"
        );
        harness.advance(Duration::from_secs(2));
        assert_eq!(
            drag_state(harness.world()),
            DragState::Idle,
            "moved={moved}"
        );
        harness.advance(SETTLE);
        assert_eq!(frame(harness.world(), 1), start, "moved={moved}");
    }
}

#[test]
fn explicit_resize_width_commands_keep_requested_frames_after_an_app_resize() {
    use crate::commands::ResizeDirection::{Grow, Shrink};

    let mut failures = Vec::new();
    for (operation, initial_width, app_width, width) in [
        (Operation::SetWidth(0.5), 400, 560, 512),
        (Operation::Resize(Grow), 256, 384, 512),
        (Operation::ResizeStep(Shrink), 400, 768, 512),
        (Operation::FullWidth, 400, 560, 1024),
    ] {
        let mut harness = harness_with(
            "[options]\nauto_center = false\npreset_column_widths = [0.25, 0.5, 0.75]\nwindow_resize_step = 0.25\n",
            TEST_DISPLAY_HEIGHT,
            1,
        );
        harness.run(vec![command(Operation::SetWidth(
            f64::from(initial_width) / f64::from(TEST_DISPLAY_WIDTH),
        ))]);
        harness.advance(SETTLE);
        let start = frame(harness.world(), 0);
        assert_eq!(start.width(), initial_width);
        let app_frame =
            IRect::from_corners(start.min, start.min + IVec2::new(app_width, start.height()));
        macos_tile(&mut harness, 0, app_frame);
        assert_eq!(frame(harness.world(), 0), app_frame);

        harness.world().write_message(command(operation.clone()));
        harness.advance(FRAME);
        let expected =
            IRect::from_corners(start.min, start.min + IVec2::new(width, start.height()));
        assert_eq!(frame(harness.world(), 0), expected, "{operation:?}");
        harness
            .world()
            .write_message(Event::WindowResized { window_id: 0 });
        harness.advance(SETTLE);
        let actual = frame(harness.world(), 0);
        if actual != expected {
            failures.push((operation, expected, actual));
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn explicit_resize_width_replaces_slots_for_stacked_windows_and_native_tabs() {
    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 2);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
    add_native_tab(&mut harness, 0, 2);
    harness.mock_state.focus_window(1);
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), IRect::new(0, 20, 400, 394));
    assert_eq!(frame(harness.world(), 1), IRect::new(0, 394, 400, 768));
    assert_eq!(frame(harness.world(), 2), frame(harness.world(), 0));

    let expected = [0, 1, 2].map(|id| {
        let start = frame(harness.world(), id);
        let app_frame = IRect::from_corners(start.min, start.min + IVec2::new(560, start.height()));
        harness
            .mock_state
            .update_window(id, |window| window.frame = app_frame);
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
        (
            id,
            IRect::from_corners(start.min, start.min + IVec2::new(512, start.height())),
        )
    });
    harness.advance(FRAME);
    harness
        .world()
        .write_message(command(Operation::SetWidth(0.5)));
    harness.advance(FRAME);
    for (id, expected) in expected {
        assert_eq!(frame(harness.world(), id), expected, "window {id}");
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(SETTLE);
    let actual = [0, 1, 2].map(|id| (id, frame(harness.world(), id)));
    assert_eq!(actual, expected);
}

#[test]
fn explicit_resize_fullwidth_replaces_the_slot_for_every_native_tab() {
    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 1);
    add_native_tab(&mut harness, 0, 1);
    let start = frame(harness.world(), 1);
    let app_frame = IRect::from_corners(start.min, start.min + IVec2::new(560, start.height()));
    for id in [0, 1] {
        harness
            .mock_state
            .update_window(id, |window| window.frame = app_frame);
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(FRAME);
    harness.world().write_message(command(Operation::FullWidth));
    harness.advance(FRAME);
    let expected = IRect::new(
        0,
        TEST_MENUBAR_HEIGHT,
        TEST_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT,
    );
    assert_eq!(
        [frame(harness.world(), 0), frame(harness.world(), 1)],
        [expected; 2]
    );
    for id in [0, 1] {
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(SETTLE);
    assert_eq!(
        [frame(harness.world(), 0), frame(harness.world(), 1)],
        [expected; 2]
    );
}

#[cfg(feature = "lua")]
#[test]
fn explicit_resize_declarative_width_replaces_the_focused_windows_old_slot() {
    use crate::types::windowset::LayoutOp;

    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 1);
    let start = frame(harness.world(), 0);
    macos_tile(
        &mut harness,
        0,
        IRect::new(0, start.min.y, 560, start.max.y),
    );
    harness.world().write_message(Event::Command {
        command: Command::Layout(vec![LayoutOp::SetWidth {
            window: 0,
            ratio: 0.5,
        }]),
    });
    harness.advance(FRAME * 2);
    let expected = IRect::new(0, start.min.y, 512, start.max.y);
    assert_eq!(frame(harness.world(), 0), expected);
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 0 });
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), expected);
}

#[cfg(feature = "lua")]
#[test]
fn explicit_resize_declarative_width_replaces_a_nonfocused_windows_old_slot() {
    use crate::types::windowset::LayoutOp;

    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 2);
    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
    let start = frame(harness.world(), 0);
    let expected = IRect::new(0, start.min.y, 560, start.max.y);
    macos_tile(&mut harness, 0, expected);
    // Request the current width so this checks slot replacement independently
    // of whether nonfocused replay changes the live width.
    harness.world().write_message(Event::Command {
        command: Command::Layout(vec![LayoutOp::SetWidth {
            window: 0,
            ratio: 560.0 / f64::from(TEST_DISPLAY_WIDTH),
        }]),
    });
    harness.advance(FRAME * 2);
    macos_tile(
        &mut harness,
        0,
        IRect::new(0, start.min.y, 512, start.max.y),
    );
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), expected);
}

#[test]
fn explicit_resize_invalid_or_ineligible_command_keeps_a_pending_native_animation_slot() {
    for operation in [
        Operation::SetWidth(0.0),
        Operation::SetWidth(f64::NAN),
        Operation::ResizeVertical(crate::commands::ResizeDirection::Grow),
    ] {
        let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 1);
        let start = frame(harness.world(), 0);
        macos_tile(
            &mut harness,
            0,
            IRect::new(0, start.min.y, 560, start.max.y),
        );
        harness.world().write_message(command(operation.clone()));
        harness.advance(FRAME);
        macos_tile(
            &mut harness,
            0,
            IRect::new(0, start.min.y, 512, start.max.y),
        );
        harness.advance(SETTLE);
        assert_eq!(frame(harness.world(), 0), start, "{operation:?}");
    }
}

#[test]
fn explicit_resize_of_another_window_keeps_a_pending_native_animation_slot() {
    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 2);
    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
    let start = frame(harness.world(), 0);
    macos_tile(
        &mut harness,
        0,
        IRect::new(0, start.min.y, 560, start.max.y),
    );
    harness
        .world()
        .write_message(command(Operation::SetWidth(0.25)));
    harness.advance(FRAME);
    macos_tile(
        &mut harness,
        0,
        IRect::new(0, start.min.y, 512, start.max.y),
    );
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), start);
    assert_eq!(
        frame(harness.world(), 1),
        IRect::new(400, start.min.y, 656, start.max.y)
    );
}

#[test]
fn explicit_resize_vertical_replaces_slots_for_both_stack_members() {
    use crate::commands::ResizeDirection::{Grow, Shrink};

    let mut harness = harness_with(
        "[options]\nauto_center = false\npreset_stack_heights = [0.333333, 0.5, 0.666667]\n",
        TEST_DISPLAY_HEIGHT,
        2,
    );
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(Side::West)),
        command(Operation::Focus(Direction::North)),
        command(Operation::SetWidth(0.5)),
        command(Operation::ResizeVertical(Shrink)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), IRect::new(0, 20, 512, 269));
    assert_eq!(frame(harness.world(), 1), IRect::new(0, 269, 512, 768));
    // The lower member reports first. Its top edge moving down grows the
    // member above by the same amount, which then matches the app's frame.
    for (id, app_frame) in [
        (1, IRect::new(0, 320, 512, 768)),
        (0, IRect::new(0, 20, 512, 320)),
    ] {
        harness
            .mock_state
            .update_window(id, |window| window.frame = app_frame);
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(FRAME);
    harness
        .world()
        .write_message(command(Operation::ResizeVertical(Grow)));
    // The resize lands this frame, and the lower member moves to its new top
    // on the next.
    harness.advance(FRAME * 2);
    let expected = [IRect::new(0, 20, 512, 394), IRect::new(0, 394, 512, 768)];
    assert_eq!(
        [frame(harness.world(), 0), frame(harness.world(), 1)],
        expected
    );
    for id in [0, 1] {
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(SETTLE);
    assert_eq!(
        [frame(harness.world(), 0), frame(harness.world(), 1)],
        expected,
    );
}

#[test]
fn explicit_resize_equalize_replaces_slots_for_both_stack_members() {
    use crate::commands::ResizeDirection::Shrink;

    let mut harness = harness_with(
        "[options]\nauto_center = false\npreset_stack_heights = [0.333333, 0.5, 0.666667]\n",
        TEST_DISPLAY_HEIGHT,
        2,
    );
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(Side::West)),
        command(Operation::Focus(Direction::North)),
        command(Operation::SetWidth(0.5)),
        command(Operation::ResizeVertical(Shrink)),
    ]);
    harness.advance(SETTLE);
    // The app resizes both members. The lower one reports first, so its top
    // edge grows the member above to the app's frame.
    for (id, app_frame) in [
        (1, IRect::new(0, 320, 512, 768)),
        (0, IRect::new(0, 20, 512, 320)),
    ] {
        harness
            .mock_state
            .update_window(id, |window| window.frame = app_frame);
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(FRAME);
    harness.world().write_message(command(Operation::Equalize));
    harness.advance(FRAME * 2);
    let expected = [IRect::new(0, 20, 512, 394), IRect::new(0, 394, 512, 768)];
    assert_eq!(
        [frame(harness.world(), 0), frame(harness.world(), 1)],
        expected
    );
    for id in [0, 1] {
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
    }
    harness.advance(SETTLE);
    assert_eq!(
        [frame(harness.world(), 0), frame(harness.world(), 1)],
        expected
    );
}

#[test]
fn explicit_resize_balance_replaces_the_slot_of_a_resized_column() {
    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 2);
    harness.run(vec![command(Operation::SetWidth(0.5))]);
    harness.advance(SETTLE);
    let start = frame(harness.world(), 1);
    assert_eq!(start.min.x, 512, "window 1 starts beside the focused half");
    // The app widens the other column's window on its own.
    macos_tile(
        &mut harness,
        1,
        IRect::new(512, start.min.y, 1072, start.max.y),
    );
    harness.world().write_message(command(Operation::Balance));
    harness.advance(FRAME * 2);
    let expected = IRect::new(512, start.min.y, 1024, start.max.y);
    assert_eq!(frame(harness.world(), 1), expected);
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 1 });
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 1), expected);
}

#[cfg(feature = "lua")]
#[test]
fn explicit_resize_declarative_frame_replaces_the_windows_old_slot() {
    use crate::types::state::Frame;
    use crate::types::windowset::LayoutOp;

    let mut harness = harness_with("[options]\nauto_center = false\n", TEST_DISPLAY_HEIGHT, 1);
    let start = frame(harness.world(), 0);
    macos_tile(
        &mut harness,
        0,
        IRect::new(0, start.min.y, 560, start.max.y),
    );
    let expected = IRect::new(0, start.min.y, 512, start.max.y);
    harness.world().write_message(Event::Command {
        command: Command::Layout(vec![LayoutOp::SetFrame {
            window: 0,
            frame: Frame {
                x: expected.min.x,
                y: expected.min.y,
                width: expected.width(),
                height: expected.height(),
            },
        }]),
    });
    harness.advance(FRAME * 2);
    assert_eq!(frame(harness.world(), 0), expected);
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 0 });
    harness.advance(SETTLE);
    assert_eq!(frame(harness.world(), 0), expected);
}
