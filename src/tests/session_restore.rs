use std::time::Duration;

use bevy::ecs::query::Has;
use bevy::prelude::*;

use crate::commands::{Command, Operation};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::state::{
    PaneruState, SavedColumn, SavedDisplay, SavedRect, SavedStrip, SavedWindow, SavedWorkspace,
};
use crate::ecs::tile_overrides::TileOverrides;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{RestoreWindowState, SpawnWindowTrigger, Unmanaged};
use crate::events::Event;
use crate::manager::{Display, Origin, Size};
use crate::platform::{ProcessSerialNumber, WorkspaceId};
use crate::tests::{
    EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID, MockState,
    TEST_DISPLAY_HEIGHT, TEST_DISPLAY_ID, TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID,
    TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH, TEST_WORKSPACE_ID, TestHarness, find_window_entity,
};

#[test]
fn test_startup_focus_preserves_the_focused_window_when_restoring_a_session() {
    let harness = TestHarness::new()
        .with_windows(2)
        .with_state(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 0,
            columns: vec![
                SavedColumn::Single(saved_window(0)),
                SavedColumn::Single(saved_window(1)),
            ],
        }]));
    harness.mock_state.focus_window_unreported(1);

    harness
        .on_iteration(0, |world, state| {
            assert_eq!(
                state.focused_window_id(TEST_PROCESS_ID),
                Some(1),
                "restoring a session must keep the app's existing focused window"
            );
            crate::assert_focused!(world, 1);
            assert_eq!(state.raise_count(), 0, "restoring must not raise a window");
        })
        .run(vec![Event::MenuOpened { window_id: 1 }]);
}

#[test]
fn test_startup_focus_preserves_a_frontmost_floating_app_when_restoring_a_session() {
    let settings_pid = TEST_PROCESS_ID + 1;
    let settings_window_id = 2;
    let mut harness = TestHarness::new()
        .with_windows(2)
        .with_app(settings_pid, "miri.dev", "Miri Settings", |app| {
            app.focused_window_id = Some(settings_window_id);
        })
        .with_state(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 0,
            columns: vec![
                SavedColumn::Single(saved_window(0)),
                SavedColumn::Single(saved_window(1)),
            ],
        }]));
    harness
        .mock_state
        .update_app(TEST_PROCESS_ID, |app| app.is_frontmost = false);
    let settings_window = harness.mock_state.spawn_window(
        settings_pid,
        TEST_WORKSPACE_ID,
        settings_window_id,
        IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    harness
        .mock_state
        .update_window(settings_window_id, |window| window.fixed_size = true);
    harness.existed_at_launch([settings_window_id]);
    harness
        .world()
        .trigger(SpawnWindowTrigger(vec![settings_window]));

    harness
        .on_iteration(0, move |world, state| {
            assert_eq!(
                state.raise_count(),
                0,
                "restoring behind a floating app must not raise any tiled window"
            );
            assert_eq!(
                state.focused_window_id(settings_pid),
                Some(settings_window_id)
            );
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), None);
            crate::assert_focused!(world, settings_window_id);
            let entity = find_window_entity(settings_window_id, world);
            assert!(world.entity(entity).contains::<Unmanaged>());
        })
        .run(vec![Event::MenuOpened {
            window_id: settings_window_id,
        }]);
}

#[test]
fn test_startup_focus_selects_the_first_window_without_a_saved_session() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, state| {
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(0));
            crate::assert_focused!(world, 0);
            assert!(state.raise_count() > 0);
        })
        .run(vec![Event::MenuOpened { window_id: 0 }]);
}

/// A Space has one strip, so a saved virtual workspace row comes back as that
/// strip (Miri #364).
#[test]
fn test_startup_restore_rebuilds_a_saved_virtual_row_as_the_spaces_strip() {
    let state = PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![SavedDisplay {
            display_id: TEST_DISPLAY_ID,
            bounds: SavedRect {
                min_x: 0,
                min_y: TEST_MENUBAR_HEIGHT,
                max_x: TEST_DISPLAY_WIDTH,
                max_y: TEST_DISPLAY_HEIGHT,
            },
            active: true,
            workspace_ids: vec![TEST_WORKSPACE_ID],
        }],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(1),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 1,
                columns: vec![
                    SavedColumn::Single(saved_window(0)),
                    SavedColumn::Single(saved_window(1)),
                ],
            }],
        }],
    };

    let mut harness = TestHarness::new().with_windows(2).with_state(state);

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
    let strips = query
        .iter(world)
        .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
        .collect::<Vec<_>>();

    assert_eq!(strips.len(), 1, "the restored Space should have one strip");
    let (restored, active) = strips[0];
    assert!(active);
    assert_eq!(restored.virtual_index, 0);

    let columns = restored.columns().collect::<Vec<_>>();
    assert_eq!(columns.len(), 2);
    assert!(matches!(columns[0], Column::Single(_)));
    assert!(matches!(columns[1], Column::Single(_)));
}

#[test]
fn test_startup_restore_keeps_unmatched_windows_on_the_restored_row() {
    let mut harness = TestHarness::new().with_windows(2);

    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(0))],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let mut query = world.query::<&LayoutStrip>();
    let matching = query
        .iter(world)
        .filter(|strip| strip.id() == TEST_WORKSPACE_ID)
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "restore should keep unmatched windows on row 0 instead of creating VW2"
    );
    assert_eq!(matching[0].virtual_index, 0);

    let windows = matching[0].all_windows();
    assert_eq!(windows.len(), 2);
    assert!(
        windows.contains(&crate::tests::harness::find_window_entity(0, world)),
        "restored row should contain window 0"
    );
    assert!(
        windows.contains(&crate::tests::harness::find_window_entity(1, world)),
        "restored row should contain window 1"
    );
}

#[test]
fn test_startup_restore_keeps_fullscreen_separate_from_unmatched_windows() {
    let mut harness = TestHarness::new().with_windows(2);

    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![SavedColumn::Fullscreen(saved_window(0))],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let restored_window = find_window_entity(0, world);
    let unmatched_window = find_window_entity(1, world);
    let mut query = world.query::<&LayoutStrip>();
    let matching = query
        .iter(world)
        .filter(|strip| strip.id() == TEST_WORKSPACE_ID)
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 2);
    let fullscreen = matching
        .iter()
        .find(|strip| strip.is_fullscreen())
        .expect("fullscreen restore should rebuild a fullscreen strip");
    assert_eq!(fullscreen.all_windows(), vec![restored_window]);
    assert!(
        matching
            .iter()
            .any(|strip| !strip.is_fullscreen() && strip.contains(unmatched_window))
    );
}

#[test]
fn test_startup_restore_prefers_existing_workspace_parent_before_saved_or_active_display() {
    let mut harness = TestHarness::new();

    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        200,
        IRect::from_corners(origin, origin + size),
    );
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(EXT_DISPLAY_ID),
        displays: vec![
            saved_display(EXT_DISPLAY_ID, true),
            saved_display(TEST_DISPLAY_ID, false),
        ],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(200))],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let restored_window = crate::tests::harness::find_window_entity(200, world);
    let restored_entity = {
        let mut query = world.query::<(Entity, &LayoutStrip)>();
        query
            .iter(world)
            .find(|(_, strip)| {
                strip.id() == TEST_WORKSPACE_ID
                    && strip.virtual_index == 0
                    && strip.contains(restored_window)
            })
            .map(|(entity, _)| entity)
            .expect("restored strip should exist")
    };
    let parent = world
        .entity(restored_entity)
        .get::<ChildOf>()
        .expect("restored strip should have a display parent")
        .parent();
    let display = world
        .entity(parent)
        .get::<Display>()
        .expect("parent should be a display");

    assert_eq!(
        display.id(),
        TEST_DISPLAY_ID,
        "stale saved display id should fall back to the display that already owns the native workspace"
    );
}

#[test]
fn test_startup_restore_preserves_saved_display_when_present() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, -TEST_WINDOW_WIDTH);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(origin, origin + size),
    );

    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(EXT_DISPLAY_ID),
        displays: vec![
            saved_display(EXT_DISPLAY_ID, true),
            saved_display(TEST_DISPLAY_ID, false),
        ],
        workspaces: vec![SavedWorkspace {
            workspace_id: EXT_WORKSPACE_ID,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(300))],
            }],
        }],
    });

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, |world, _state| {
            let restored_window = crate::tests::harness::find_window_entity(300, world);
            let parent = restored_strip_display_parent(world, EXT_WORKSPACE_ID, 0, restored_window);
            let display = world
                .entity(parent)
                .get::<Display>()
                .expect("parent should be a display");

            assert_eq!(
                display.id(),
                EXT_DISPLAY_ID,
                "restore should keep the exact saved display when it is present"
            );
        })
        .run(commands);
}

/// After a restart with focus on another display, the restored visible row on
/// the external display shows its windows right away instead of waiting off
/// screen until that display gets focus.
#[test]
fn test_startup_restore_shows_visible_strip_on_an_unfocused_display() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, -TEST_WINDOW_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(origin, origin + size),
    );

    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(EXT_DISPLAY_ID),
        displays: vec![
            saved_display(EXT_DISPLAY_ID, true),
            saved_display(TEST_DISPLAY_ID, false),
        ],
        workspaces: vec![SavedWorkspace {
            workspace_id: EXT_WORKSPACE_ID,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(300))],
            }],
        }],
    });

    harness
        .on_iteration(0, |world, state| {
            let ext_bounds = world
                .query::<&Display>()
                .iter(world)
                .find(|display| display.id() == EXT_DISPLAY_ID)
                .expect("external display exists")
                .bounds();
            let frame = state.create_window(300).frame();
            assert!(
                ext_bounds.contains(frame.min) && ext_bounds.contains(frame.max),
                "window frame {frame:?} must be inside the external display {ext_bounds:?}"
            );
        })
        .run(vec![Event::MenuOpened { window_id: 100 }]);
}

/// The restored visible row on an unfocused display sits inside the display's
/// padding, where the layout puts every other row.
#[test]
fn test_startup_restore_pads_visible_strip_on_an_unfocused_display() {
    let mut harness = TestHarness::new().with_config(padded_config());
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, -TEST_WINDOW_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(origin, origin + size),
    );

    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(EXT_DISPLAY_ID),
        displays: vec![
            saved_display(EXT_DISPLAY_ID, true),
            saved_display(TEST_DISPLAY_ID, false),
        ],
        workspaces: vec![SavedWorkspace {
            workspace_id: EXT_WORKSPACE_ID,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(300))],
            }],
        }],
    });

    harness
        .on_iteration(0, |world, state| {
            let ext_bounds = world
                .query::<&Display>()
                .iter(world)
                .find(|display| display.id() == EXT_DISPLAY_ID)
                .expect("external display exists")
                .bounds();
            let frame = state.create_window(300).frame();
            assert_eq!(
                frame.min,
                ext_bounds.min + PAD,
                "window frame {frame:?} must start at the padded origin of {ext_bounds:?}"
            );
        })
        .run(vec![Event::MenuOpened { window_id: 100 }]);
}

/// A restored row wider than the display keeps its focused last column in
/// view, inside the display's padding, also when a window that turns up
/// during the grace period applies the restore again without a focus change.
#[test]
fn test_startup_restore_shows_focused_last_column_of_a_wide_strip() {
    let harness = TestHarness::new()
        .with_config(padded_config())
        .with_windows(3)
        .with_state(PaneruState {
            version: 2,
            timestamp: 123_456_789,
            active_display_id: Some(TEST_DISPLAY_ID),
            displays: vec![saved_display(TEST_DISPLAY_ID, true)],
            workspaces: vec![SavedWorkspace {
                workspace_id: TEST_WORKSPACE_ID,
                display_id: Some(TEST_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
                    view: None,
                    virtual_index: 0,
                    columns: [1, 2, 0]
                        .into_iter()
                        .map(|id| SavedColumn::Single(saved_window(id)))
                        .collect(),
                }],
            }],
        });
    harness.mock_state.focus_window_unreported(0);

    let viewport = IRect::new(
        PAD,
        TEST_MENUBAR_HEIGHT + PAD,
        TEST_DISPLAY_WIDTH - PAD,
        TEST_DISPLAY_HEIGHT - PAD,
    );
    let check = move |world: &mut World, state: MockState| {
        crate::assert_focused!(world, 0);
        let focused = state.create_window(0).frame();
        assert!(
            focused.min.cmpge(viewport.min).all() && focused.max.cmple(viewport.max).all(),
            "focused window frame {focused:?} must lie inside the padded viewport {viewport:?}"
        );
        for id in 1..3 {
            let frame = state.create_window(id).frame();
            assert_eq!(
                frame.min.y, viewport.min.y,
                "window {id} frame {frame:?} must start at the padded top {}",
                viewport.min.y
            );
        }
    };
    harness
        .on_iteration(0, move |world, state| {
            check(world, state);
            world.trigger(RestoreWindowState);
        })
        .on_iteration(1, check)
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::MenuOpened { window_id: 0 },
        ]);
}

#[test]
fn test_startup_restore_keeps_current_native_workspace_active_across_multiple_workspaces() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -TEST_WINDOW_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        100,
        IRect::from_corners(origin, origin + size),
    );
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(ext_origin, ext_origin + size),
    );

    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![
            saved_display(TEST_DISPLAY_ID, true),
            saved_display(EXT_DISPLAY_ID, false),
        ],
        workspaces: vec![
            SavedWorkspace {
                workspace_id: TEST_WORKSPACE_ID,
                display_id: Some(TEST_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
                    view: None,
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(100))],
                }],
            },
            SavedWorkspace {
                workspace_id: EXT_WORKSPACE_ID,
                display_id: Some(EXT_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
                    view: None,
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(300))],
                }],
            },
        ],
    });

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, |world, _state| {
            let mut query = world.query::<(
                &LayoutStrip,
                Has<crate::ecs::ActiveWorkspaceMarker>,
                Has<crate::ecs::SelectedVirtualMarker>,
            )>();
            let restored = query
                .iter(world)
                .filter(|(strip, _, _)| {
                    (strip.id() == TEST_WORKSPACE_ID || strip.id() == EXT_WORKSPACE_ID)
                        && strip.virtual_index == 0
                        && !strip.all_windows().is_empty()
                })
                .collect::<Vec<_>>();

            assert_eq!(
                restored.iter().filter(|(_, active, _)| *active).count(),
                1,
                "restore should keep one global active native workspace"
            );
            assert!(
                restored
                    .iter()
                    .any(|(strip, active, selected)| strip.id() == TEST_WORKSPACE_ID
                        && *active
                        && *selected)
            );
            assert!(
                restored
                    .iter()
                    .any(|(strip, active, selected)| strip.id() == EXT_WORKSPACE_ID
                        && !*active
                        && *selected)
            );
        })
        .run(commands);
}

#[test]
fn test_restore_resource_is_removed_after_grace_period() {
    let mut harness = TestHarness::new().with_windows(1);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 0,
            columns: vec![SavedColumn::Single(saved_window(0))],
        }]));

    // Well inside the two-second default grace period.
    harness.advance(Duration::from_millis(500));

    assert!(
        harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );

    // And comfortably past the far end of it.
    harness.advance(Duration::from_secs(3));

    assert!(
        !harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );
    assert!(!harness.app.world().contains_resource::<PaneruState>());
}

#[test]
fn test_startup_restore_disabled_by_config_keeps_normal_layout() {
    let config = Config::try_from(
        r"
[options]

[restore]
enabled = false

[bindings]
",
    )
    .expect("config should parse");
    let mut harness = TestHarness::new().with_config(config).with_windows(1);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 1,
            columns: vec![SavedColumn::Single(saved_window(0))],
        }]));

    for _ in 0..5 {
        harness.app.update();
    }

    assert!(
        !harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );
    assert!(!harness.app.world().contains_resource::<PaneruState>());

    let world = harness.world();
    let mut query = world.query::<&LayoutStrip>();
    assert!(
        query
            .iter(world)
            .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0)
    );
    assert!(
        !query
            .iter(world)
            .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 1)
    );
}

#[test]
fn test_startup_restore_uses_first_restored_row_when_active_metadata_is_missing() {
    let mut harness = TestHarness::new().with_workspace_popup().with_windows(1);
    let mut state = state_with_strips(vec![SavedStrip {
        view: None,
        virtual_index: 2,
        columns: vec![SavedColumn::Single(saved_window(0))],
    }]);
    state.workspaces[0].active_virtual_index = None;
    harness.world().insert_resource(state);

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
    let active_strips = query
        .iter(world)
        .filter(|(strip, active)| strip.id() == TEST_WORKSPACE_ID && *active)
        .map(|(strip, _)| strip)
        .collect::<Vec<_>>();

    assert_eq!(active_strips.len(), 1);
    assert_eq!(active_strips[0].virtual_index, 0);
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).next().is_none(),
        "startup session restoration should not show a workspace switch popup"
    );
}

#[test]
fn test_startup_floating_rule_wins_over_saved_tiled_placement() {
    let mut params = WindowParams::new("^Window 1$", Some("test".to_string()));
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = TestHarness::new().with_config(config).with_windows(2);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 0,
            columns: vec![
                SavedColumn::Single(saved_window(0)),
                SavedColumn::Single(saved_window(1)),
            ],
        }]));

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let tiled = find_window_entity(0, world);
    let floating = find_window_entity(1, world);
    let mut query = world.query::<&LayoutStrip>();
    let strip_windows = query
        .iter(world)
        .flat_map(LayoutStrip::all_windows)
        .collect::<Vec<_>>();

    assert!(
        strip_windows.contains(&tiled),
        "a saved window without a floating rule should be restored into a strip"
    );
    assert!(
        world.entity(tiled).get::<Unmanaged>().is_none(),
        "a saved window without a floating rule should stay managed"
    );
    assert!(
        !strip_windows.contains(&floating),
        "a window matching a floating rule must not be restored into a strip"
    );
    assert!(
        matches!(
            world.entity(floating).get::<Unmanaged>(),
            Some(Unmanaged::Floating)
        ),
        "a window matching a floating rule must float even when saved as tiled"
    );
}

#[test]
fn test_late_startup_window_restores_during_grace_period() {
    let mut harness = TestHarness::new().with_workspace_popup();
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 1,
            columns: vec![SavedColumn::Single(saved_window(99))],
        }]));

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, state| {
            assert!(world.contains_resource::<crate::ecs::restore::SessionRestore>());

            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                99,
                IRect::from_corners(origin, origin + size),
            );
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(2, |world, _state| {
            let restored_window = find_window_entity(99, world);
            let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
            let restored = query
                .iter(world)
                .find(|(strip, active)| {
                    strip.id() == TEST_WORKSPACE_ID
                        && strip.virtual_index == 0
                        && *active
                        && strip.contains(restored_window)
                })
                .map(|(strip, _)| strip);

            assert!(
                restored.is_some(),
                "late startup window should restore into saved row"
            );
            let mut messages = world.query::<&crate::ecs::FlashMessage>();
            assert!(
                messages.iter(world).next().is_none(),
                "late startup restoration should not show a workspace switch popup"
            );
        })
        .run(commands);
}

#[test]
fn test_late_startup_window_with_floating_rule_is_not_restored() {
    let mut params = WindowParams::new("^Window 99$", Some("test".to_string()));
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = TestHarness::new().with_config(config);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            view: None,
            virtual_index: 1,
            columns: vec![SavedColumn::Single(saved_window(99))],
        }]));

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, state| {
            assert!(world.contains_resource::<crate::ecs::restore::SessionRestore>());

            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                99,
                IRect::from_corners(origin, origin + size),
            );
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(2, |world, _state| {
            let window = find_window_entity(99, world);
            let mut query = world.query::<&LayoutStrip>();
            assert!(
                !query.iter(world).any(|strip| strip.contains(window)),
                "a late window matching a floating rule must not be restored into a strip"
            );
            assert!(
                !query
                    .iter(world)
                    .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 1),
                "restore must not create a saved row that only held a floating window"
            );
            assert!(
                matches!(
                    world.entity(window).get::<Unmanaged>(),
                    Some(Unmanaged::Floating)
                ),
                "a late window matching a floating rule must float even when saved as tiled"
            );
        })
        .run(commands);
}

/// A saved session can hold a second strip for a Space, a virtual workspace
/// row from an older engine. Its windows join the Space's one strip after the
/// windows of the first, and none is parked off screen (Miri #364).
#[test]
fn test_startup_restore_merges_a_second_saved_row_into_the_first() {
    let mut harness = TestHarness::new().with_windows(3);
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(1),
            strips: vec![
                SavedStrip {
                    view: None,
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(2))],
                },
                SavedStrip {
                    view: None,
                    virtual_index: 1,
                    columns: vec![
                        SavedColumn::Single(saved_window(1)),
                        SavedColumn::Single(saved_window(0)),
                    ],
                },
            ],
        }],
    });
    harness.advance(Duration::from_secs(1));

    let world = harness.world();
    let order = [2, 1, 0].map(|id| find_window_entity(id, world));
    let mut query = world.query::<(
        &LayoutStrip,
        Has<crate::ecs::ActiveWorkspaceMarker>,
        Option<&PreviousStripPosition>,
    )>();
    let strips = query
        .iter(world)
        .filter(|(strip, _, _)| strip.id() == TEST_WORKSPACE_ID)
        .map(|(strip, active, previous)| {
            (
                strip.virtual_index,
                strip.all_windows(),
                active,
                previous.is_some(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(strips, vec![(0, order.to_vec(), true, false)]);

    let display = IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
    let parked = IVec2::new(
        TEST_DISPLAY_WIDTH - crate::ecs::layout::PARKED_STRIP_SLIVER,
        TEST_DISPLAY_HEIGHT - crate::ecs::layout::PARKED_STRIP_SLIVER,
    );
    for id in [2, 1, 0] {
        let frame = harness.mock_state.ax_frame(id);
        assert_ne!(frame.min, parked, "window {id} must not be parked");
        assert!(
            display.contains(frame.min),
            "window {id} must start on the display, not off screen: {frame:?}"
        );
    }
}

/// Restore consumes the windows of the strip the Space starts with, which
/// empties it. The saved row then fills that strip instead of standing beside
/// it.
#[test]
fn test_startup_restore_refills_the_emptied_baseline_row() {
    let mut harness = TestHarness::new().with_windows(2);
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(1),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 1,
                columns: vec![
                    SavedColumn::Single(saved_window(0)),
                    SavedColumn::Single(saved_window(1)),
                ],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
    let mut rows = query
        .iter(world)
        .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
        .map(|(strip, active)| (strip.virtual_index, strip.all_windows().len(), active))
        .collect::<Vec<_>>();
    rows.sort_unstable();

    assert_eq!(
        rows,
        vec![(0, 2, true)],
        "the saved row should fill the Space's one strip"
    );
}

/// Restoring a saved stack rebuilds it without changing who is in it, so the
/// windows keep the heights they have instead of splitting evenly.
#[test]
fn test_startup_restore_keeps_stack_heights() {
    use crate::ecs::state::SavedStackItem;
    use crate::manager::Window;

    let state = state_with_strips(vec![SavedStrip {
        view: None,
        virtual_index: 0,
        columns: vec![SavedColumn::Stack(vec![
            SavedStackItem::Single(saved_window(0)),
            SavedStackItem::Single(saved_window(1)),
        ])],
    }]);
    let top = TEST_MENUBAR_HEIGHT;
    let mut harness = TestHarness::new()
        .with_window(0, move |window| {
            window.frame = IRect::new(0, top, TEST_WINDOW_WIDTH, top + 450);
        })
        .with_window(1, move |window| {
            window.frame = IRect::new(0, top + 450, TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT);
        })
        .with_state(state);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    let heights = [0, 1].map(|id| {
        world
            .query::<&Window>()
            .iter(world)
            .find(|window| window.id() == id)
            .expect("window exists")
            .frame()
            .height()
    });
    assert_eq!(heights, [450, TEST_DISPLAY_HEIGHT - top - 450]);
}

/// Exit cleanup returns each window to its frame from before the engine
/// started, so the next engine finds the windows at those widths. The session
/// saved at exit gives each column its width back.
#[test]
fn a_restart_keeps_column_widths() {
    use bevy::app::AppExit;
    use bevy::ecs::system::RunSystemOnce;

    let os_width = |harness: &TestHarness, id| {
        let mut width = 0;
        harness
            .mock_state
            .update_window(id, |window| width = window.frame.width());
        width
    };

    let mut harness = TestHarness::new().with_windows(2);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    for _ in 0..2 {
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::SetWidth(0.25)),
        }]);
        harness.advance(Duration::from_secs(2));
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::Focus(crate::commands::Direction::East)),
        }]);
        harness.advance(Duration::from_secs(1));
    }
    let widths = [0, 1].map(|id| os_width(&harness, id));

    let path =
        std::env::temp_dir().join(format!("paneru-column-widths-{}.json", std::process::id()));
    harness
        .world()
        .insert_resource(crate::ecs::state::StateFile(path.clone()));
    harness.world().write_message(AppExit::Success);
    harness
        .world()
        .run_system_once(crate::ecs::state::cleanup_on_exit)
        .expect("running the exit save");
    let state = PaneruState::load_from_file(&path).expect("a saved session");
    _ = std::fs::remove_file(&path);

    let top = TEST_MENUBAR_HEIGHT;
    let full_width = IRect::new(0, top, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
    assert!(
        widths.iter().all(|width| *width < full_width.width()),
        "the windows were narrowed before the restart: {widths:?}"
    );
    let mut restarted = TestHarness::new()
        .with_window(0, move |window| window.frame = full_width)
        .with_window(1, move |window| window.frame = full_width)
        .with_state(state);
    restarted.advance(Duration::from_secs(2));

    assert_eq!([0, 1].map(|id| os_width(&restarted, id)), widths);
}

const PADDING: u16 = 4;
const PAD: i32 = PADDING as i32;

fn padded_config() -> Config {
    let options = MainOptions {
        padding_top: Some(PADDING),
        padding_right: Some(PADDING),
        padding_bottom: Some(PADDING),
        padding_left: Some(PADDING),
        ..MainOptions::default()
    };
    (options, vec![]).into()
}

fn saved_display(display_id: u32, active: bool) -> SavedDisplay {
    SavedDisplay {
        display_id,
        bounds: SavedRect {
            min_x: 0,
            min_y: TEST_MENUBAR_HEIGHT,
            max_x: TEST_DISPLAY_WIDTH,
            max_y: TEST_DISPLAY_HEIGHT,
        },
        active,
        workspace_ids: vec![TEST_WORKSPACE_ID],
    }
}

fn state_with_strips(strips: Vec<SavedStrip>) -> PaneruState {
    PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips,
        }],
    }
}

fn restored_strip_display_parent(
    world: &mut World,
    workspace_id: WorkspaceId,
    virtual_index: u32,
    window: Entity,
) -> Entity {
    let mut query = world.query::<(Entity, &LayoutStrip)>();
    let restored_entity = query
        .iter(world)
        .find(|(_, strip)| {
            strip.id() == workspace_id
                && strip.virtual_index == virtual_index
                && strip.contains(window)
        })
        .map(|(entity, _)| entity)
        .expect("restored strip should exist");
    world
        .entity(restored_entity)
        .get::<ChildOf>()
        .expect("restored strip should have a display parent")
        .parent()
}

fn saved_window(window_id: i32) -> SavedWindow {
    SavedWindow {
        window_id,
        pid: TEST_PROCESS_ID,
        psn: ProcessSerialNumber { high: 1, low: 2 },
        bundle_id: "test".to_string(),
        title: String::new(),
        identifier: String::new(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
        maximized: None,
        width: None,
    }
}

/// A harness whose floating rule matches window 1, with both windows saved as
/// tiled, as an engine restart finds them.
fn restarted_with_floating_rule(overrides: TileOverrides) -> TestHarness {
    restarted_with_window_one(Some(true), false, overrides)
}

/// A harness whose window 1 cannot be resized, with both windows saved as
/// tiled. `rule` is what a rule for window 1 says about floating, if any.
fn restarted_with_fixed_size_window(rule: Option<bool>, overrides: TileOverrides) -> TestHarness {
    restarted_with_window_one(rule, true, overrides)
}

fn restarted_with_window_one(
    floating: Option<bool>,
    fixed_size: bool,
    overrides: TileOverrides,
) -> TestHarness {
    restarted_with(floating, fixed_size, false, overrides)
}

fn restarted_with(
    floating: Option<bool>,
    fixed_size: bool,
    minimized: bool,
    overrides: TileOverrides,
) -> TestHarness {
    let mut params = WindowParams::new("^Window 1$", Some("test".to_string()));
    params.floating = floating;
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_window(1, |window| {
            window.fixed_size = fixed_size;
            window.minimized = minimized;
        });
    let world = harness.app.world_mut();
    world.insert_resource(overrides);
    world.insert_resource(state_with_strips(vec![SavedStrip {
        view: None,
        virtual_index: 0,
        columns: vec![
            SavedColumn::Single(saved_window(0)),
            SavedColumn::Single(saved_window(1)),
        ],
    }]));
    for _ in 0..5 {
        harness.app.update();
    }
    harness
}

fn floats(harness: &mut TestHarness, id: crate::platform::WinID) -> bool {
    let world = harness.world();
    let entity = find_window_entity(id, world);
    matches!(
        world.entity(entity).get::<Unmanaged>(),
        Some(Unmanaged::Floating)
    )
}

/// Focuses window `id` and toggles it between floating and tiled.
fn toggle(harness: &mut TestHarness, id: crate::platform::WinID) {
    harness.mock_state.focus_window(id);
    harness.advance(Duration::from_millis(500));
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Manage),
    }]);
    harness.advance(Duration::from_millis(500));
}

/// Toggles window 1 between floating and tiled, then returns the overrides an
/// engine restart would load.
fn toggle_window_one(harness: &mut TestHarness, times: usize) -> TileOverrides {
    for _ in 0..times {
        toggle(harness, 1);
    }
    harness
        .world()
        .remove_resource::<TileOverrides>()
        .expect("overrides")
}

/// Tiling a window against its floating rule lasts across an engine restart
/// while the window stays open.
#[test]
fn a_window_tiled_against_its_floating_rule_stays_tiled_after_a_restart() {
    let mut harness = restarted_with_floating_rule(TileOverrides::default());
    assert!(floats(&mut harness, 1));
    let overrides = toggle_window_one(&mut harness, 1);
    assert!(!floats(&mut harness, 1), "the toggle tiles the window");

    let mut restarted = restarted_with_floating_rule(overrides);
    assert!(!floats(&mut restarted, 1));
    let world = restarted.world();
    let entity = find_window_entity(1, world);
    let mut strips = world.query::<&LayoutStrip>();
    assert!(strips.iter(world).any(|strip| strip.contains(entity)));
}

/// Floating the window again ends the override, so the rule applies at the
/// next restart.
#[test]
fn floating_a_window_again_ends_its_override() {
    let mut harness = restarted_with_floating_rule(TileOverrides::default());
    let overrides = toggle_window_one(&mut harness, 2);
    assert!(floats(&mut harness, 1), "the second toggle floats it again");

    let mut restarted = restarted_with_floating_rule(overrides);
    assert!(floats(&mut restarted, 1));
}

/// Closing the window ends the override, so a window that later gets the
/// same number does not inherit it.
#[test]
fn closing_a_window_ends_its_override() {
    let mut harness = restarted_with_floating_rule(TileOverrides::default());
    toggle(&mut harness, 1);
    assert!(!floats(&mut harness, 1));
    harness.mock_state.os_close_window(1);
    harness.advance(Duration::from_secs(1));
    let overrides = harness
        .world()
        .remove_resource::<TileOverrides>()
        .expect("overrides");

    let mut restarted = restarted_with_floating_rule(overrides);
    assert!(floats(&mut restarted, 1));
}

/// A window that cannot be resized floats, even where a saved session placed
/// it in the strip.
#[test]
fn a_fixed_size_window_floats() {
    let mut harness = restarted_with_fixed_size_window(None, TileOverrides::default());
    assert!(floats(&mut harness, 1));
    assert!(!floats(&mut harness, 0));
}

/// A rule that says a window does not float wins over its fixed size.
#[test]
fn a_rule_can_tile_a_fixed_size_window() {
    let mut harness = restarted_with_fixed_size_window(Some(false), TileOverrides::default());
    assert!(!floats(&mut harness, 1));
}

/// Tiling a fixed-size window with the toggle lasts across an engine restart
/// while the window stays open.
#[test]
fn a_fixed_size_window_tiled_by_the_user_stays_tiled_after_a_restart() {
    let mut harness = restarted_with_fixed_size_window(None, TileOverrides::default());
    let overrides = toggle_window_one(&mut harness, 1);
    assert!(!floats(&mut harness, 1), "the toggle tiles the window");

    let mut restarted = restarted_with_fixed_size_window(None, overrides);
    assert!(!floats(&mut restarted, 1));
}

/// Minimizing and restoring a floating window keeps it floating.
#[test]
fn a_fixed_size_window_floats_after_it_is_minimized_and_restored() {
    let mut harness = restarted_with_fixed_size_window(None, TileOverrides::default());
    assert!(floats(&mut harness, 1));
    harness.mock_state.os_minimize_window(1, true);
    harness.advance(Duration::from_millis(500));
    let world = harness.world();
    let entity = find_window_entity(1, world);
    assert!(matches!(
        world.entity(entity).get::<Unmanaged>(),
        Some(Unmanaged::Minimized)
    ));
    harness.mock_state.os_minimize_window(1, false);
    harness.advance(Duration::from_millis(500));

    assert!(floats(&mut harness, 1));
    let world = harness.world();
    let entity = find_window_entity(1, world);
    let mut strips = world.query::<&LayoutStrip>();
    assert!(!strips.iter(world).any(|strip| strip.contains(entity)));
}

/// A floating window that was minimized when the engine started counts as
/// minimized, and floats once it is restored.
#[test]
fn a_fixed_size_window_minimized_at_start_floats_when_restored() {
    let mut harness = restarted_with(None, true, true, TileOverrides::default());
    let world = harness.world();
    let entity = find_window_entity(1, world);
    assert!(matches!(
        world.entity(entity).get::<Unmanaged>(),
        Some(Unmanaged::Minimized)
    ));
    harness.mock_state.os_minimize_window(1, false);
    harness.advance(Duration::from_millis(500));

    assert!(floats(&mut harness, 1));
    let world = harness.world();
    let mut strips = world.query::<&LayoutStrip>();
    assert!(!strips.iter(world).any(|strip| strip.contains(entity)));
}

/// Quitting the app ends the override of its windows, as closing them does.
#[test]
fn quitting_the_app_ends_its_overrides() {
    let mut harness = restarted_with_floating_rule(TileOverrides::default());
    toggle(&mut harness, 1);
    assert!(!floats(&mut harness, 1));
    harness.run(vec![Event::ApplicationTerminated {
        psn: ProcessSerialNumber {
            high: 0,
            low: TEST_PROCESS_ID.cast_unsigned(),
        },
    }]);
    let overrides = harness
        .world()
        .remove_resource::<TileOverrides>()
        .expect("overrides");
    assert!(overrides.is_empty());
}

/// A window that opened floating gets the padding of a tiled window once the
/// user tiles it.
#[test]
fn a_window_tiled_from_floating_gets_its_padding() {
    let mut floating = WindowParams::new("^Window 1$", Some("test".to_string()));
    floating.floating = Some(true);
    let mut padded = WindowParams::new(".*", None);
    padded.horizontal_padding = Some(4);
    padded.vertical_padding = Some(4);
    let config: Config = (MainOptions::default(), vec![floating, padded]).into();
    let mut harness = TestHarness::new().with_config(config).with_windows(2);
    harness.advance(Duration::from_millis(500));
    assert!(floats(&mut harness, 1));

    toggle(&mut harness, 1);
    assert!(!floats(&mut harness, 1));
    let mut set = (None, None);
    harness
        .mock_state
        .update_window(1, |window| set = window.padding_set);
    assert_eq!(set, (Some(4), Some(4)));
}

/// Gives the harness a session file holding `saved`, runs `exit`, then the
/// exit save, and returns what the file holds afterwards.
fn session_after_exit(
    harness: &mut TestHarness,
    name: &str,
    saved: &str,
    exit: impl FnOnce(&mut World),
) -> String {
    use bevy::ecs::system::RunSystemOnce;

    let path = std::env::temp_dir().join(format!("paneru-exit-{name}-{}.json", std::process::id()));
    std::fs::write(&path, saved).expect("writing the saved session");
    harness
        .world()
        .insert_resource(crate::ecs::state::StateFile(path.clone()));
    exit(harness.world());
    harness
        .world()
        .run_system_once(crate::ecs::state::cleanup_on_exit)
        .expect("running the exit save");
    let after = std::fs::read_to_string(&path).expect("reading the session");
    _ = std::fs::remove_file(&path);
    after
}

/// An exit while startup gathers processes leaves an empty or partial
/// layout. Saving it would replace the session that the next start restores.
#[test]
fn an_exit_during_startup_keeps_the_saved_session() {
    use bevy::ecs::system::RunSystemOnce;

    let mut harness = TestHarness::new().with_windows(1);
    let after = session_after_exit(&mut harness, "startup", "previous session", |world| {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(Event::Exit).expect("send exit");
        world.insert_non_send(receiver);
        world
            .run_system_once(crate::ecs::systems::gather_initial_processes)
            .expect("gathering the initial processes");
    });

    assert_eq!(after, "previous session");
}

/// Startup can still be discovering windows after it has gathered every
/// process. An exit then must not save the half-built layout either.
#[test]
fn an_exit_before_startup_finishes_keeps_the_saved_session() {
    let mut harness = TestHarness::new().with_windows(1);
    let after = session_after_exit(&mut harness, "discovering", "previous session", |world| {
        world.write_message(AppExit::Success);
    });

    assert_eq!(after, "previous session");
}

#[test]
fn an_exit_after_startup_saves_the_session() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    let after = session_after_exit(&mut harness, "running", "previous session", |world| {
        world.write_message(AppExit::Success);
    });

    assert_ne!(after, "previous session");
}

/// macOS switches Spaces with Control-Arrow on the display under the pointer,
/// which need not be the display with focus. A restored Space that appears there
/// must show its windows on that display, not leave them parked in its corner,
/// where Mission Control lists them but the desktop does not show them (#85).
#[test]
fn test_space_switch_on_display_without_focus_shows_restored_strip() {
    let hidden_space = EXT_WORKSPACE_ID + 1;
    let ext_bounds = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        ext_bounds,
        vec![EXT_WORKSPACE_ID, hidden_space],
    );
    let frame = IRect::from_corners(
        ext_bounds.min,
        ext_bounds.min + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, hidden_space, 200, frame);
    let saved_workspace = |workspace_id, window_id| SavedWorkspace {
        workspace_id,
        display_id: Some(EXT_DISPLAY_ID),
        active_virtual_index: Some(0),
        strips: vec![SavedStrip {
            view: None,
            virtual_index: 0,
            columns: vec![SavedColumn::Single(saved_window(window_id))],
        }],
    };
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![
            saved_display(TEST_DISPLAY_ID, true),
            saved_display(EXT_DISPLAY_ID, false),
        ],
        workspaces: vec![
            saved_workspace(EXT_WORKSPACE_ID, 100),
            saved_workspace(hidden_space, 200),
        ],
    });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(Duration::from_secs(1));
    assert_eq!(harness.mock_state.active_display(), TEST_DISPLAY_ID);

    harness
        .mock_state
        .activate_workspace(EXT_DISPLAY_ID, hidden_space, false);
    harness.world().write_message::<Event>(Event::SpaceChanged);
    harness.advance(Duration::from_secs(1));

    let frame = harness.mock_state.ax_frame(200);
    assert_eq!(
        frame.intersect(ext_bounds),
        frame,
        "the window of the Space now shown on its display must lie inside that display"
    );
}

/// macOS can give a Space a new ID while every window stays where it is. A
/// restored strip must follow its windows to the Space macOS now reports, not
/// rebuild the Space that no longer exists, where its windows are parked out of
/// sight (#85).
#[test]
fn test_startup_restore_follows_windows_to_their_renumbered_space() {
    let gone_space = EXT_WORKSPACE_ID + 997;
    let ext_bounds = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = TestHarness::new();
    harness
        .mock_state
        .add_display(EXT_DISPLAY_ID, ext_bounds, vec![EXT_WORKSPACE_ID]);
    let frame = IRect::from_corners(
        ext_bounds.min,
        ext_bounds.min + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    for id in [300, 301] {
        harness
            .mock_state
            .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, id, frame);
    }
    let mut ext_display = saved_display(EXT_DISPLAY_ID, false);
    ext_display.workspace_ids = vec![gone_space];
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true), ext_display],
        workspaces: vec![SavedWorkspace {
            workspace_id: gone_space,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                view: None,
                virtual_index: 0,
                columns: vec![
                    SavedColumn::Single(saved_window(301)),
                    SavedColumn::Single(saved_window(300)),
                ],
            }],
        }],
    });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(Duration::from_secs(1));
    assert_eq!(harness.mock_state.active_display(), TEST_DISPLAY_ID);

    let world = harness.world();
    let order = vec![
        find_window_entity(301, world),
        find_window_entity(300, world),
    ];
    let restored: Vec<(WorkspaceId, Vec<Entity>)> = world
        .query::<&LayoutStrip>()
        .iter(world)
        .filter(|strip| strip.all_windows().contains(&order[1]))
        .map(|strip| (strip.id(), strip.all_windows()))
        .collect();
    assert_eq!(
        restored,
        vec![(EXT_WORKSPACE_ID, order)],
        "the saved order must be rebuilt on the Space that holds the windows"
    );
    let shown = harness.mock_state.ax_frame(301);
    assert_eq!(
        shown.intersect(ext_bounds),
        shown,
        "the first restored window must show on its display, not sit parked"
    );
}

/// A saved Space can follow its window to a Space that another saved Space
/// also restores, which remaps it to that Space's display. Both saved Spaces
/// then fill one strip, in saved order, and no window is parked in a second
/// strip (Miri #364).
#[test]
fn test_startup_restore_remapped_onto_a_restored_space_keeps_one_strip() {
    let ext_bounds = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = TestHarness::new();
    harness
        .mock_state
        .add_display(EXT_DISPLAY_ID, ext_bounds, vec![EXT_WORKSPACE_ID]);
    let frame = IRect::from_corners(
        ext_bounds.min,
        ext_bounds.min + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    for id in [300, 301, 400] {
        harness
            .mock_state
            .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, id, frame);
    }
    let mut ext_display = saved_display(EXT_DISPLAY_ID, false);
    ext_display.workspace_ids = vec![EXT_WORKSPACE_ID];
    let saved_workspace = |workspace_id, display_id, window_ids: &[i32]| SavedWorkspace {
        workspace_id,
        display_id: Some(display_id),
        active_virtual_index: Some(0),
        strips: vec![SavedStrip {
            view: None,
            virtual_index: 0,
            columns: window_ids
                .iter()
                .map(|id| SavedColumn::Single(saved_window(*id)))
                .collect(),
        }],
    };
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true), ext_display],
        workspaces: vec![
            saved_workspace(EXT_WORKSPACE_ID, EXT_DISPLAY_ID, &[300, 301]),
            saved_workspace(TEST_WORKSPACE_ID, TEST_DISPLAY_ID, &[400]),
        ],
    });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(Duration::from_secs(1));

    let world = harness.world();
    let order = [300, 301, 400].map(|id| find_window_entity(id, world));
    let strips: Vec<(u32, Vec<Entity>)> = world
        .query::<&LayoutStrip>()
        .iter(world)
        .filter(|strip| strip.id() == EXT_WORKSPACE_ID)
        .map(|strip| (strip.virtual_index, strip.all_windows()))
        .collect();
    assert_eq!(
        strips,
        vec![(0, order.to_vec())],
        "both saved Spaces must restore into the one strip of the Space that holds their windows"
    );
    for id in [300, 301, 400] {
        let shown = harness.mock_state.ax_frame(id);
        assert_eq!(
            shown.intersect(ext_bounds),
            shown,
            "window {id} must show on its display, not sit parked"
        );
    }
}

/// The main display holds a row of three windows `width` wide, wider than the
/// display, and the external display one window, 300, as macOS lists them to
/// an engine that starts.
fn row_beside_a_focused_display(width: i32) -> TestHarness {
    let ext_bounds = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = TestHarness::new();
    harness
        .mock_state
        .add_display(EXT_DISPLAY_ID, ext_bounds, vec![EXT_WORKSPACE_ID]);
    let size = Size::new(width, TEST_WINDOW_HEIGHT);
    for id in 0..3 {
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            TEST_WORKSPACE_ID,
            id,
            IRect::from_corners(Origin::ZERO, size),
        );
    }
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(ext_bounds.min, ext_bounds.min + size),
    );
    harness
}

/// After a restart, a row comes back scrolled where the user left it, also
/// when focus is on another display and so cannot scroll the row (Miri #69).
#[test]
fn a_restart_keeps_where_a_row_was_scrolled() {
    restart_keeps_where_a_row_was_scrolled(TEST_WINDOW_WIDTH);
}

/// Exit cleanup gives each window back its width from before the engine
/// started, and the restore then resizes the columns to their saved widths.
/// The row must be scrolled for the saved widths, not the returned ones.
#[test]
fn a_restart_keeps_where_a_row_was_scrolled_when_widths_come_back() {
    restart_keeps_where_a_row_was_scrolled(TEST_WINDOW_WIDTH + 200);
}

/// Scrolls a row of `TEST_WINDOW_WIDTH` columns to its last column, restarts
/// with the windows `returned_width` wide, and checks the last column.
fn restart_keeps_where_a_row_was_scrolled(returned_width: i32) {
    let mut before = row_beside_a_focused_display(TEST_WINDOW_WIDTH);
    before.run(vec![Event::MenuOpened { window_id: 0 }]);
    before.advance(Duration::from_secs(1));
    let world = before.world();
    let row = world
        .query::<&LayoutStrip>()
        .iter(world)
        .find(|strip| strip.id() == TEST_WORKSPACE_ID && !strip.all_windows().is_empty())
        .expect("the main display has a row");
    let last = row
        .columns()
        .filter_map(Column::top)
        .last()
        .expect("the row has columns");
    let last = world
        .entity(last)
        .get::<crate::manager::Window>()
        .expect("the last column holds a window")
        .id();
    before.mock_state.focus_window(last);
    before.advance(Duration::from_secs(1));
    before.mock_state.focus_window(300);
    before.advance(Duration::from_secs(1));
    let scrolled = before.mock_state.ax_frame(last);
    assert!(
        scrolled.max.x <= TEST_DISPLAY_WIDTH && scrolled.min.x > 0,
        "the row must be scrolled to its last column {scrolled:?} before the restart"
    );
    let name = format!("scrolled-{returned_width}");
    let saved = session_after_exit(&mut before, &name, "", |world| {
        world.write_message(AppExit::Success);
    });

    let mut after = row_beside_a_focused_display(returned_width)
        .with_state(serde_json::from_str(&saved).expect("the saved session parses"));
    after.mock_state.activate_display(EXT_DISPLAY_ID);
    after.mock_state.focus_window(300);
    after.run(vec![Event::MenuOpened { window_id: 300 }]);
    after.advance(Duration::from_secs(1));

    assert_eq!(
        after.mock_state.ax_frame(last),
        scrolled,
        "the last column must be where it was before the restart"
    );
}
