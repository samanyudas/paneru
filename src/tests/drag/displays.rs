//! Title-bar drags that drop a window onto another display.

use super::*;
use crate::platform::WorkspaceId;

/// Window 2's title bar, where each drag of it starts. Its centre is under
/// the press, so the pointer alone picks the column.
const FROM: Origin = Origin::new(1000, 30);

/// The external display, right of the main one, `height` points tall.
fn ext(height: i32) -> IRect {
    IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        height,
    )
}

/// Windows 0, 1 and 2 on the main display, and window 100 alone on an
/// external display `height` points tall, in a column 800 points wide at the
/// display's left edge. The window server sees the left button held down.
fn two_displays(height: i32) -> TestHarness {
    let ext = ext(height);
    holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
            .with_windows(3)
            .with_workspace_window(100, EXT_WORKSPACE_ID, move |window| {
                window.frame = IRect::new(ext.min.x + 100, 20, ext.min.x + 900, ext.max.y);
            }),
    )
}

/// The window IDs of each column of the strip of `workspace`, left to right,
/// each column top to bottom.
fn columns_on(world: &mut World, workspace: WorkspaceId) -> Vec<Vec<WinID>> {
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

fn on_main(world: &mut World) -> Vec<Vec<WinID>> {
    columns_on(world, TEST_WORKSPACE_ID)
}

fn on_ext(world: &mut World) -> Vec<Vec<WinID>> {
    columns_on(world, EXT_WORKSPACE_ID)
}

/// macOS lists `windows` in the external display's space, as it does for a
/// window dragged onto that display, and makes that display the active one.
fn macos_moves_to_ext(state: &MockState, windows: &[WinID]) {
    for id in windows {
        state.update_window(*id, |window| window.workspace_id = EXT_WORKSPACE_ID);
    }
    state.activate_display(EXT_DISPLAY_ID);
}

/// A drag of window 2 to `to`, released there, after which macOS moves the
/// window into the external display's space.
fn drag_2_to_ext(harness: TestHarness, to: Origin) -> (TestHarness, Vec<Event>) {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        release(to),
        settle(),
        settle(),
    ];
    let harness = window_follows(harness, 1, 2, to - FROM)
        .on_iteration(3, |_, state| macos_moves_to_ext(&state, &[2]));
    (harness, commands)
}

/// Window 2 in its column on the main display, at x = 800, where it was
/// before the drag.
fn assert_back_on_main(world: &mut World) {
    assert_eq!(on_main(world), vec![vec![0], vec![1], vec![2]]);
    assert_eq!(on_ext(world), vec![vec![100]]);
    let frame = frame(world, 2);
    assert!(
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT).contains(frame.center()),
        "{frame:?}"
    );
}

/// Window 100's column spans x = 1024 to 1824, and its middle half x = 1224
/// to 1624. A drop below the middle of window 100 puts window 2 under it,
/// at the column's width.
#[test]
fn a_window_dropped_on_another_display_joins_a_column_there() {
    let to = Origin::new(1424, 900);
    let mut harness = two_displays(EXT_DISPLAY_HEIGHT);
    let log = record_overlay(&mut harness);
    let (harness, commands) = drag_2_to_ext(harness, to);

    let shown = log.clone();
    harness
        .on_iteration(0, |world, _| {
            assert_eq!(on_main(world), vec![vec![0], vec![1], vec![2]]);
            assert_eq!(on_ext(world), vec![vec![100]]);
            assert_eq!(frame(world, 100), IRect::new(1024, 20, 1824, 1200));
        })
        .on_iteration(2, move |_, _| {
            assert_eq!(
                last_call(&shown),
                Some(OverlayCall::Line(IRect::new(1024, 1197, 1824, 1200), WHITE))
            );
        })
        .on_iteration(5, move |world, _| {
            assert_eq!(on_ext(world), vec![vec![100, 2]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
            assert_eq!(frame(world, 100), IRect::new(1024, 20, 1824, 610));
            assert_eq!(frame(world, 2), IRect::new(1024, 610, 1824, 1200));
            assert_focused!(world, 2);
            assert_eq!(last_call(&log), Some(OverlayCall::Hide));
        })
        .run(commands);
}

#[test]
fn a_window_dropped_above_a_column_on_another_display_becomes_its_top_window() {
    let (harness, commands) =
        drag_2_to_ext(two_displays(EXT_DISPLAY_HEIGHT), Origin::new(1424, 100));

    harness
        .on_iteration(5, |world, _| {
            assert_eq!(on_ext(world), vec![vec![2, 100]]);
            assert_eq!(frame(world, 2), IRect::new(1024, 20, 1824, 610));
            assert_eq!(frame(world, 100), IRect::new(1024, 610, 1824, 1200));
        })
        .run(commands);
}

/// Window 2 takes 400 of the main display's 1024 points, so in a new column
/// on the 1920 point external display it is 750 points wide, the same
/// share, and as tall as that display's viewport.
#[test]
fn a_new_column_on_another_display_keeps_its_width_relative_to_the_display() {
    let (harness, commands) =
        drag_2_to_ext(two_displays(EXT_DISPLAY_HEIGHT), Origin::new(1804, 600));

    harness
        .on_iteration(5, |world, _| {
            assert_eq!(on_ext(world), vec![vec![100], vec![2]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
            assert_eq!(frame(world, 2), IRect::new(1824, 20, 1824 + 750, 1200));
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn a_drop_in_empty_space_on_another_display_opens_a_column_at_the_nearest_end() {
    let (harness, commands) =
        drag_2_to_ext(two_displays(EXT_DISPLAY_HEIGHT), Origin::new(2914, 600));

    harness
        .on_iteration(5, |world, _| {
            assert_eq!(on_ext(world), vec![vec![100], vec![2]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
            assert_eq!(frame(world, 2), IRect::new(1824, 20, 1824 + 750, 1200));
        })
        .run(commands);
}

/// The external display is 300 points tall, so its 280 point viewport fits
/// one window at the 200 point minimum height, and window 100's column has
/// no room. The main display's viewport would fit three. The destination's
/// rule refuses the drop, and window 2 goes back to its slot.
#[test]
fn a_column_without_room_on_another_display_refuses_the_drop() {
    let to = Origin::new(1424, 200);
    let mut harness = two_displays(300);
    let log = record_overlay(&mut harness);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    let shown = log.clone();
    window_follows(harness, 1, 2, to - FROM)
        .on_iteration(2, move |_, _| {
            assert_eq!(
                last_call(&shown),
                Some(OverlayCall::Refusal(IRect::new(1438, 214, 1460, 236)))
            );
        })
        .on_iteration(4, move |world, _| {
            assert_back_on_main(world);
            assert_eq!(last_call(&log), Some(OverlayCall::Hide));
        })
        .run(commands);
}

/// A floating window is in no strip, so dragging it onto another display
/// starts no drag, and the move macOS reports adds it to no layout.
#[test]
fn a_floating_window_dragged_to_another_display_joins_no_layout() {
    let to = Origin::new(1424, 900);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::Last)),
        window_command(Operation::Manage),
        press(FROM, 2),
        drag_to(to),
        release(to),
        settle(),
    ];

    window_follows(two_displays(EXT_DISPLAY_HEIGHT), 3, 2, to - FROM)
        .on_iteration(2, |world, _| {
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
        })
        .on_iteration(4, |world, _| {
            assert_eq!(*world.resource::<DragState>(), DragState::Idle);
        })
        .on_iteration(5, |_, state| macos_moves_to_ext(&state, &[2]))
        .on_iteration(6, |world, _| {
            assert_eq!(on_ext(world), vec![vec![100]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
        })
        .run(commands);
}

/// Window 3 is a native tab of window 2. A drag of either moves both, as
/// one item of the stack they join, sharing one frame.
#[test]
fn a_native_tab_group_crosses_displays_as_one_stack_item() {
    let to = Origin::new(1424, 900);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        settle(),
        press(FROM, 3),
        drag_to(to),
        release(to),
        settle(),
        settle(),
    ];

    window_follows(two_displays(EXT_DISPLAY_HEIGHT), 2, 2, to - FROM)
        .on_iteration(0, |world, state| {
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame(world, 2));
            state.window_visible(2, false);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _| {
            let columns = on_main(world);
            assert_eq!(columns.len(), 3, "{columns:?}");
            assert_eq!(columns[2].len(), 2, "one tab group: {columns:?}");
        })
        .on_iteration(4, |_, state| macos_moves_to_ext(&state, &[2, 3]))
        .on_iteration(6, |world, _| {
            let columns = on_ext(world);
            assert_eq!(columns.len(), 1, "{columns:?}");
            assert_eq!(columns[0][0], 100, "{columns:?}");
            let mut tabs = columns[0][1..].to_vec();
            tabs.sort_unstable();
            assert_eq!(tabs, vec![2, 3], "{columns:?}");
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
            assert_eq!(frame(world, 2), frame(world, 3), "the tabs share a frame");
            assert_eq!(frame(world, 2), IRect::new(1024, 610, 1824, 1200));
        })
        .run(commands);
}

/// The drop rule reads the layout at the release. Window 100 closed during
/// the drag, so the drop opens the first column of the emptied strip.
#[test]
fn a_drop_rechecks_the_destination_after_its_last_window_closes() {
    let to = Origin::new(1424, 900);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        settle(),
        release(to),
        settle(),
        settle(),
    ];

    window_follows(two_displays(EXT_DISPLAY_HEIGHT), 1, 2, to - FROM)
        .on_iteration(2, |_, state| state.os_close_window(100))
        .on_iteration(3, |world, _| {
            assert_eq!(on_ext(world), Vec::<Vec<WinID>>::new());
        })
        .on_iteration(4, |_, state| macos_moves_to_ext(&state, &[2]))
        .on_iteration(6, |world, _| {
            assert_eq!(on_ext(world), vec![vec![2]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
            assert_eq!(frame(world, 2), IRect::new(1024, 20, 1024 + 750, 1200));
        })
        .run(commands);
}

/// The external display goes away during the drag, so the release is over
/// no display, and window 2 goes back to its slot on the main display.
#[test]
fn a_drop_rechecks_a_display_that_disappears_before_release() {
    let to = Origin::new(1424, 900);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        },
        release(to),
        settle(),
    ];

    window_follows(two_displays(EXT_DISPLAY_HEIGHT), 1, 2, to - FROM)
        .on_iteration(2, |_, state| state.remove_display(EXT_DISPLAY_ID))
        .on_iteration(5, |world, _| {
            let window = world
                .query::<(Entity, &Window)>()
                .iter(world)
                .find_map(|(entity, window)| (window.id() == 2).then_some(entity))
                .expect("window 2 exists");
            let all = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|strip| strip.contains(window))
                .count();
            assert_eq!(all, 1, "window 2 is in one strip");
            assert!(on_main(world).contains(&vec![2]), "{:?}", on_main(world));
            let frame = frame(world, 2);
            assert!(
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT).contains(frame.center()),
                "{frame:?}"
            );
        })
        .run(commands);
}

/// macOS can make the destination display active while the button is still
/// down. The dragged window stays in its strip until the drop moves it.
#[test]
fn a_drop_on_another_display_survives_macos_activating_it_before_the_release() {
    let to = Origin::new(1424, 900);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        settle(),
        release(to),
        settle(),
    ];

    window_follows(two_displays(EXT_DISPLAY_HEIGHT), 1, 2, to - FROM)
        .on_iteration(2, |_, state| macos_moves_to_ext(&state, &[2]))
        .on_iteration(3, |world, _| {
            assert_eq!(on_main(world), vec![vec![0], vec![1], vec![2]]);
            assert_eq!(on_ext(world), vec![vec![100]]);
        })
        .on_iteration(5, |world, _| {
            assert_eq!(on_ext(world), vec![vec![100, 2]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
        })
        .run(commands);
}

/// A refused drop with macOS making the destination display active, and
/// listing the window in its space, before the release. The window and its
/// native tab stay in their strip, during the drag and after the return.
#[test]
fn early_display_activation_keeps_a_refused_drag_in_its_source() {
    for tabs in [false, true] {
        let to = Origin::new(1424, 200);
        let grabbed = if tabs { 3 } else { 2 };
        let members: &'static [WinID] = if tabs { &[2, 3] } else { &[2] };
        // The harness lists a new native tab before the window it joins.
        let source = if tabs {
            vec![vec![0], vec![1], vec![3, 2]]
        } else {
            vec![vec![0], vec![1], vec![2]]
        };
        let commands = vec![
            Event::MenuOpened { window_id: 0 },
            settle(),
            press(FROM, grabbed),
            drag_to(to),
            settle(),
            release(to),
            settle(),
        ];

        let during = source.clone();
        window_follows(two_displays(300), 2, 2, to - FROM)
            .on_iteration(0, move |world, state| {
                if tabs {
                    let window =
                        state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame(world, 2));
                    state.window_visible(2, false);
                    world.trigger(SpawnWindowTrigger(vec![window]));
                }
            })
            .on_iteration(3, move |world, state| {
                macos_moves_to_ext(&state, members);
                assert_eq!(on_main(world), during, "tabs {tabs}");
            })
            .on_iteration(4, move |world, _| {
                assert_eq!(on_ext(world), vec![vec![100]], "tabs {tabs}");
            })
            .on_iteration(6, move |world, _| {
                assert_eq!(on_main(world), source, "tabs {tabs}");
                assert_eq!(on_ext(world), vec![vec![100]], "tabs {tabs}");
            })
            .run(commands);
    }
}

/// The tap hands over the whole refused gesture in one update, with macOS
/// making the destination display active in the same update.
#[test]
fn a_batched_gesture_and_display_activation_keep_a_refused_drag_in_its_source() {
    let to = Origin::new(1424, 200);
    let commands = vec![Event::MenuOpened { window_id: 0 }, settle(), settle()];

    two_displays(300)
        .on_iteration(0, move |world, state| {
            let start = frame(world, 2).min;
            world.write_message(press(FROM, 2));
            for step in 1..=6 {
                let sample = FROM + (to - FROM) * step / 6;
                world.write_message(drag_to(sample));
                state.os_move_window(2, start + (sample - FROM));
                for event in state.drain_events() {
                    world.write_message(event);
                }
            }
            state.set_left_button_down(false);
            macos_moves_to_ext(&state, &[2]);
            for event in state.drain_events() {
                world.write_message(event);
            }
            world.write_message(release(to));
        })
        .on_iteration(2, |world, _| assert_back_on_main(world))
        .run(commands);
}

/// macOS moves window 2 and its native tab into the external display's space
/// before the release. The drop opens a new column there, and both keep the
/// share of the display's width window 2 had on the main display.
#[test]
fn early_display_activation_keeps_the_width_share_of_a_new_column_and_its_tabs() {
    for tabs in [false, true] {
        let to = Origin::new(1804, 600);
        let members: &'static [WinID] = if tabs { &[2, 3] } else { &[2] };
        let commands = vec![
            Event::MenuOpened { window_id: 0 },
            settle(),
            press(FROM, 2),
            drag_to(to),
            settle(),
            release(to),
            settle(),
        ];

        window_follows(two_displays(EXT_DISPLAY_HEIGHT), 2, 2, to - FROM)
            .on_iteration(0, move |world, state| {
                if tabs {
                    let window =
                        state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame(world, 2));
                    state.window_visible(2, false);
                    world.trigger(SpawnWindowTrigger(vec![window]));
                }
            })
            .on_iteration(3, move |_, state| macos_moves_to_ext(&state, members))
            .on_iteration(4, move |world, _| {
                assert_eq!(on_main(world).len(), 3, "tabs {tabs}");
            })
            .on_iteration(6, move |world, _| {
                let columns = on_ext(world);
                assert_eq!(columns.len(), 2, "tabs {tabs}: {columns:?}");
                let mut moved = columns[1].clone();
                moved.sort_unstable();
                assert_eq!(moved, members, "tabs {tabs}");
                for id in members {
                    assert_eq!(frame(world, *id).width(), 750, "tabs {tabs}, window {id}");
                }
            })
            .run(commands);
    }
}

/// After a refused drop from the external display, macOS can keep listing
/// window 2 in that display's space. Neither a click on the window nor a
/// later return to that display moves it there.
#[test]
fn a_click_on_a_returned_window_keeps_it_on_its_display() {
    let to = Origin::new(1424, 200);
    let body = Origin::new(1000, 400);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        settle(),
        release(to),
        settle(),
        press(body, 2),
        release(body),
        settle(),
        settle(),
    ];

    window_follows(two_displays(300), 1, 2, to - FROM)
        .on_iteration(2, |_, state| macos_moves_to_ext(&state, &[2]))
        .on_iteration(5, |world, state| {
            assert_back_on_main(world);
            state.activate_display(TEST_DISPLAY_ID);
        })
        .on_iteration(7, |world, _| assert_back_on_main(world))
        .on_iteration(8, |_, state| state.activate_display(EXT_DISPLAY_ID))
        .on_iteration(9, |world, _| assert_back_on_main(world))
        .run(commands);
}

/// The display gets shorter while a drag waits over a stack. The drop rule
/// reads the display at the release, so the stack has no room any more.
#[test]
fn a_held_drop_is_refused_when_a_display_resize_leaves_no_room() {
    let from = Origin::new(600, 30);
    let to = Origin::new(200, 300);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::Focus(Direction::East)),
        window_command(Operation::Stack(true)),
        press(from, 2),
        drag_to(to),
        Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        },
        release(to),
        settle(),
    ];

    let mut harness = holding(
        TestHarness::new()
            .with_config(drag_enabled())
            .with_windows(3),
    );
    let log = record_overlay(&mut harness);
    let shown = log.clone();
    window_follows(harness, 4, 2, to - from)
        .on_iteration(3, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
        })
        .on_iteration(5, move |_, state| {
            assert!(
                matches!(last_call(&shown), Some(OverlayCall::Line(..))),
                "{:?}",
                last_call(&shown)
            );
            state.set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, 500));
        })
        .on_iteration(6, move |_, _| {
            assert!(
                matches!(last_call(&log), Some(OverlayCall::Refusal(_))),
                "{:?}",
                last_call(&log)
            );
        })
        .on_iteration(8, |world, _| {
            assert_eq!(columns(world), vec![vec![0, 1], vec![2]]);
        })
        .run(commands);
}

/// The main display, 2560 by 1440, above an empty smaller display.
const LOWER: IRect = IRect {
    min: IVec2::new(182, 1440),
    max: IVec2::new(2230, 2772),
};

/// A drop onto an empty display below opens its first column there, and the
/// insertion line runs down the display's left edge. A second drop stacks
/// another window into that column, and the stack stays on that display.
#[test]
fn a_window_stacked_into_a_new_column_on_a_lower_display_stays_on_that_display() {
    let first_from = Origin::new(200, 30);
    let first_to = Origin::new(LOWER.center().x, LOWER.min.y + 120);
    // Window 0's new column is 320 points wide, 400 of 2560 points scaled
    // to 2048, at the lower display's left edge.
    let column = 182..502;
    let second_from = Origin::new(200, 30);
    let second_to = Origin::new(342, LOWER.max.y - 100);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(first_from, 0),
        drag_to(first_to),
        release(first_to),
        settle(),
        press(second_from, 1),
        drag_to(second_to),
        release(second_to),
        settle(),
    ];

    let harness = TestHarness::new().with_config(drag_enabled());
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, 2560, 1440));
    let mut harness = holding(
        harness
            .with_display(EXT_DISPLAY_ID, LOWER, vec![EXT_WORKSPACE_ID])
            .with_windows(3),
    );
    let log = record_overlay(&mut harness);
    let harness = window_follows(harness, 1, 0, first_to - first_from);
    let top = Rc::new(Cell::new(0));
    let first_top = top.clone();
    window_follows(harness, 5, 1, second_to - second_from)
        .on_iteration(0, |world, _| {
            assert_eq!(frame(world, 0), IRect::new(0, 20, 400, 1440));
            assert_eq!(on_ext(world), Vec::<Vec<WinID>>::new());
        })
        .on_iteration(2, move |_, _| {
            assert_eq!(
                last_call(&log),
                Some(OverlayCall::Line(IRect::new(182, 1460, 185, 2772), WHITE))
            );
        })
        .on_iteration(4, move |world, state| {
            macos_moves_to_ext(&state, &[0]);
            assert_eq!(on_ext(world), vec![vec![0]]);
            let frame = frame(world, 0);
            assert_eq!(frame.min.x..frame.max.x, column, "{frame:?}");
            assert!(LOWER.contains(frame.center()), "{frame:?}");
            first_top.set(frame.min.y);
        })
        .on_iteration(8, move |world, _| {
            assert_eq!(on_ext(world), vec![vec![0, 1]]);
            let (upper, below) = (frame(world, 0), frame(world, 1));
            for frame in [upper, below] {
                assert!(
                    frame.min.y >= LOWER.min.y && frame.max.y <= LOWER.max.y,
                    "the stack stays on the lower display: {upper:?} {below:?}"
                );
            }
            assert_eq!(upper.min.y, top.get(), "the column top does not move");
        })
        .run(commands);
}

/// With the option off, a window macOS lists in another display's space
/// joins that display's strip when its space becomes active, as before.
#[test]
fn without_the_option_a_window_macos_moves_to_another_display_joins_its_strip() {
    let to = Origin::new(1424, 900);
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        press(FROM, 2),
        drag_to(to),
        release(to),
        settle(),
        settle(),
    ];

    let harness = holding(
        TestHarness::new()
            .with_display(
                EXT_DISPLAY_ID,
                ext(EXT_DISPLAY_HEIGHT),
                vec![EXT_WORKSPACE_ID],
            )
            .with_windows(3)
            .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
                window.frame = IRect::new(1124, 20, 1924, EXT_DISPLAY_HEIGHT);
            }),
    );
    window_follows(harness, 1, 2, to - FROM)
        .on_iteration(3, |world, state| {
            assert_eq!(on_main(world), vec![vec![0], vec![1], vec![2]]);
            macos_moves_to_ext(&state, &[2]);
        })
        .on_iteration(5, |world, _| {
            assert_eq!(on_ext(world), vec![vec![100], vec![2]]);
            assert_eq!(on_main(world), vec![vec![0], vec![1]]);
        })
        .run(commands);
}
