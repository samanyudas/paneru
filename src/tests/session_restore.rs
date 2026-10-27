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
fn test_startup_restore_rebuilds_virtual_workspace_layout() {
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
    let active_strips = query
        .iter(world)
        .filter(|(strip, active)| strip.id() == TEST_WORKSPACE_ID && *active)
        .map(|(strip, _)| strip)
        .collect::<Vec<_>>();

    assert_eq!(
        active_strips.len(),
        1,
        "exactly one virtual row should be active for the restored workspace"
    );

    let restored = active_strips[0];
    assert_eq!(restored.virtual_index, 1);

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
                    virtual_index: 0,
                    columns: [1, 2, 0]
                        .into_iter()
                        .map(|id| SavedColumn::Single(saved_window(id)))
                        .collect(),
                }],
            }],
        });

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
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(100))],
                }],
            },
            SavedWorkspace {
                workspace_id: EXT_WORKSPACE_ID,
                display_id: Some(EXT_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
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
    assert_eq!(active_strips[0].virtual_index, 2);
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
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
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
                        && strip.virtual_index == 1
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
        .on_iteration(3, |world, _state| {
            let mut messages = world.query::<&crate::ecs::FlashMessage>();
            assert!(messages.iter(world).any(|message| message.0 == "1"));
        })
        .on_iteration(4, |world, _state| {
            let mut messages = world.query::<&crate::ecs::FlashMessage>();
            assert!(messages.iter(world).any(|message| message.0 == "2"));
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

#[test]
fn test_startup_restore_keeps_one_selected_row_and_hides_inactive_rows() {
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
            strips: vec![
                SavedStrip {
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(0))],
                },
                SavedStrip {
                    virtual_index: 1,
                    columns: vec![SavedColumn::Single(saved_window(1))],
                },
            ],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
    let mut query = world.query::<(
        &LayoutStrip,
        Has<crate::ecs::ActiveWorkspaceMarker>,
        Has<crate::ecs::SelectedVirtualMarker>,
        Option<&PreviousStripPosition>,
    )>();
    let restored = query
        .iter(world)
        .filter(|(strip, _, _, _)| strip.id() == TEST_WORKSPACE_ID)
        .collect::<Vec<_>>();
    let active = restored
        .iter()
        .filter(|(strip, active, _, _)| *active && strip.virtual_index == 1)
        .count();
    let selected = restored
        .iter()
        .filter(|(_, _, selected, _)| *selected)
        .count();
    let inactive = restored.iter().find(|(strip, active, _, previous)| {
        strip.virtual_index == 0 && !*active && previous.is_some()
    });

    assert_eq!(active, 1);
    assert_eq!(selected, 1);
    assert!(
        inactive.is_some(),
        "inactive restored rows should be hidden with previous position"
    );
}

/// Restore consumes row 0's windows onto the saved rows, which empties row 0.
/// Despawning it there left the restored space numbered from "2", so the
/// emptied baseline row is kept.
#[test]
fn test_startup_restore_keeps_emptied_baseline_row() {
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
        vec![(0, 0, false), (1, 2, true)],
        "restore should keep the emptied row 0 alongside the restored row 1"
    );
}

/// Restoring a saved stack rebuilds it without changing who is in it, so the
/// windows keep the heights they have instead of splitting evenly.
#[test]
fn test_startup_restore_keeps_stack_heights() {
    use crate::ecs::state::SavedStackItem;
    use crate::manager::Window;

    let state = state_with_strips(vec![SavedStrip {
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

/// When session restore keeps an emptied baseline row 0 on a workspace whose
/// active virtual row is > 0, row 0 must be parked off-screen with a
/// `PreviousStripPosition` (and without `SelectedVirtualMarker`), so that
/// moving windows to row 0 positions them properly on or off screen.
#[test]
fn test_emptied_baseline_row_is_parked_and_repositions_moved_windows() {
    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            auto_center: Some(false),
            continuous_swipe: Some(true),
            virtual_workspace_animations: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMove(
                crate::commands::Direction::South,
                crate::commands::MoveFocus::Follow,
            )),
        },
    ];

    let mut display = saved_display(TEST_DISPLAY_ID, true);
    display.workspace_ids = vec![TEST_WORKSPACE_ID, 200];

    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            bevy::math::IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, 200],
        )
        .with_workspace_window(0, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(1, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(2, 200, |_| {});
    harness.world().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![display],
        workspaces: vec![
            SavedWorkspace {
                workspace_id: TEST_WORKSPACE_ID,
                display_id: Some(TEST_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
                    virtual_index: 0,
                    columns: vec![
                        SavedColumn::Single(saved_window(0)),
                        SavedColumn::Single(saved_window(1)),
                    ],
                }],
            },
            SavedWorkspace {
                workspace_id: 200,
                display_id: Some(TEST_DISPLAY_ID),
                active_virtual_index: Some(1),
                strips: vec![SavedStrip {
                    virtual_index: 1,
                    columns: vec![SavedColumn::Single(saved_window(2))],
                }],
            },
        ],
    });

    let parked_origin = bevy::math::IVec2::new(
        TEST_DISPLAY_WIDTH - crate::ecs::layout::PARKED_STRIP_SLIVER,
        TEST_DISPLAY_HEIGHT - crate::ecs::layout::PARKED_STRIP_SLIVER,
    );
    let visible_origin = bevy::math::IVec2::new(0, TEST_MENUBAR_HEIGHT);

    harness
        .on_iteration(0, move |world, _state| {
            assert_eq!(
                workspace_rows_state(world, 200),
                vec![
                    (0, parked_origin, false, false, Some(visible_origin)),
                    (1, parked_origin, false, true, Some(visible_origin)),
                ],
                "emptied baseline row 0 must be parked with PreviousStripPosition and not selected"
            );
        })
        .on_iteration(1, |world, _state| {
            let win0 = find_window_entity(0, world);
            assert!(world.get::<crate::ecs::Position>(win0).unwrap().0.x > 0);
        })
        .on_iteration(2, move |world, _state| {
            let win0 = find_window_entity(0, world);
            assert_eq!(
                world.get::<crate::ecs::Position>(win0).unwrap().0,
                visible_origin,
                "moving window 0 to a new virtual row must reposition it even when relative LayoutPosition stays (0, 0)"
            );
        })
        .run(commands);
}

fn workspace_rows_state(
    world: &mut World,
    workspace_id: WorkspaceId,
) -> Vec<(
    u32,
    bevy::math::IVec2,
    bool,
    bool,
    Option<bevy::math::IVec2>,
)> {
    let mut query = world.query::<(
        &LayoutStrip,
        &crate::ecs::Position,
        Has<crate::ecs::ActiveWorkspaceMarker>,
        Has<crate::ecs::SelectedVirtualMarker>,
        Option<&PreviousStripPosition>,
    )>();
    let mut rows = query
        .iter(world)
        .filter(|(strip, _, _, _, _)| strip.id() == workspace_id)
        .map(|(strip, pos, active, selected, prev)| {
            (
                strip.virtual_index,
                pos.0,
                active,
                selected,
                prev.map(|p| p.origin),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|(idx, ..)| *idx);
    rows
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

#[test]
fn an_exit_after_startup_saves_the_session() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    let after = session_after_exit(&mut harness, "running", "previous session", |world| {
        world.write_message(AppExit::Success);
    });

    assert_ne!(after, "previous session");
}
