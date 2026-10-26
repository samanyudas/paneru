use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, Direction, MouseMove, MoveFocus, Operation, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::{DockPosition, Timeout};
use crate::events::Event;
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::WinID;
use crate::{
    assert_focused, assert_not_on_workspace, assert_on_workspace, assert_window_at,
    assert_window_size,
};

use super::*;

#[test]
fn test_multi_display_lifecycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
        Event::DisplayAdded {
            display_id: TEST_DISPLAY_ID,
        },
    ];

    let mut harness = TestHarness::new().with_windows(1);
    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    harness
        .on_iteration(1, |world, state| {
            let mut query = world.query_filtered::<Entity, With<Display>>();
            query.single(world).expect("should have one display");
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(2, |world, mut state| {
            assert!(
                world
                    .query_filtered::<Entity, With<Display>>()
                    .single(world)
                    .is_err(),
                "display should be despawned"
            );

            let workspace_entity = {
                let mut query = world.query_filtered::<Entity, With<LayoutStrip>>();
                query.single(world).expect("should have one workspace")
            };
            let workspace = world.entity(workspace_entity);
            assert!(
                workspace.get::<Timeout>().is_some(),
                "orphaned workspace should have a timeout"
            );
            assert!(
                workspace.get::<ChildOf>().is_none(),
                "orphaned workspace should have no parent"
            );
            state.add_display(
                TEST_DISPLAY_ID,
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
                vec![TEST_WORKSPACE_ID],
            );
        })
        .on_iteration(3, |world, _state| {
            let new_display_entity = world
                .query_filtered::<Entity, With<Display>>()
                .single(world)
                .expect("display should be spawned again");

            let workspace_entity = {
                let mut query = world.query_filtered::<Entity, With<LayoutStrip>>();
                query.single(world).expect("should have one workspace")
            };
            let workspace = world.entity(workspace_entity);
            assert!(
                workspace.get::<Timeout>().is_none(),
                "re-parented workspace should no longer have a timeout"
            );
            let child_of: &ChildOf = workspace
                .get::<ChildOf>()
                .expect("re-parented workspace should have a parent");
            assert_eq!(
                child_of.parent(),
                new_display_entity,
                "workspace should be child of the new display"
            );
        })
        .run(commands);
}

#[test]
fn test_multi_workspace_orphaning() {
    let mut commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];
    commands.extend((0..6).map(|_| Event::Command {
        command: Command::PrintState,
    }));

    let workspaces = vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1];
    let harness = TestHarness::new().with_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        workspaces,
    );
    harness
        .on_iteration(1, |world, state| {
            let display_entity = world
                .query_filtered::<Entity, With<Display>>()
                .single(world)
                .expect("should have one display");

            let workspace_entities = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .iter(world)
                .collect::<Vec<_>>();
            assert_eq!(workspace_entities.len(), 2, "should have two workspaces");

            for &ws in &workspace_entities {
                let child_of: &ChildOf = world
                    .entity(ws)
                    .get::<ChildOf>()
                    .expect("workspace should have parent");
                assert_eq!(child_of.parent(), display_entity);
            }
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(8, |world, _state| {
            let workspace_entities = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .iter(world)
                .collect::<Vec<_>>();
            for &ws in &workspace_entities {
                let entity: EntityRef = world.entity(ws);
                assert!(
                    entity.get::<Timeout>().is_some(),
                    "each workspace should have a timeout"
                );
                assert!(
                    entity.get::<ChildOf>().is_none(),
                    "each workspace should have no parent"
                );
            }
        })
        .run(commands);
}

#[test]
fn test_multi_display_no_height_crosstalk() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, ext_frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 200, frame);

    let ext_usable_height = EXT_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayChanged,
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, _state| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .on_iteration(2, |world, _state| {
            use crate::ecs::ActiveWorkspaceMarker;
            let mut strip_query =
                world.query_filtered::<&mut LayoutStrip, Without<ActiveWorkspaceMarker>>();
            for mut strip in strip_query.iter_mut(world) {
                strip.set_changed();
            }
        })
        .on_iteration(4, move |world, _state| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .run(commands);
}

#[test]
fn test_next_display_inserts_into_target_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .on_iteration(1, move |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, _state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .run(commands);
}

#[test]
fn test_send_next_display_stays_on_source() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 101, frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 100, frame);

    let commands = vec![
        Event::MenuOpened { window_id: 101 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, _state| {
            assert_on_workspace!(world, 100, TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, state| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
        })
        .run(commands);
}

#[test]
fn test_mouse_to_next_display() {
    let commands = vec![
        Event::MenuOpened { window_id: 101 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let display_bounds = IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0);

    // harness
    //     .mock_state
    //     .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 101, frame);
    // harness
    //     .mock_state
    //     .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 100, frame);
    // Pointer-follows-focus places the pointer on window 100 first, so the
    // pointer starts inside one display.
    let config: Config = (
        MainOptions {
            mouse_follows_focus: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, display_bounds, vec![EXT_WORKSPACE_ID])
        .with_window(100, |data| {
            data.pid = TEST_PROCESS_ID;
            data.workspace_id = TEST_WORKSPACE_ID;
            data.frame = frame;
        })
        .on_iteration(1, move |world, state| {
            let entity = find_window_entity(100, world);
            let window = world.get::<Window>(entity).expect("need window");
            assert_eq!(state.cursor_position(), window.frame().center());
        })
        .on_iteration(3, move |world, state| {
            let mut query = world.query::<(&Display, Option<&DockPosition>)>();
            let (display, dock) = query
                .iter(world)
                .find(|display| display.0.id() == EXT_DISPLAY_ID)
                .expect("need display");
            let config = world.resource::<Config>();
            let bounds = display.actual_display_bounds(dock, config);
            assert_eq!(state.cursor_position(), bounds.center());
        })
        .run(commands);
}

/// Regression test: paneru's init pass must not drag windows that live on
/// inactive displays onto the active display. `apply_window_properties`
/// initially appends every observed window to the active strip; if the
/// layout writers run before `finish_setup` has reassigned them, they
/// cache active-display coordinates into `Position` and `commit_window_position`
/// later pushes those to macOS, moving the windows.
#[test]
fn test_init_keeps_windows_on_their_real_displays() {
    // Internal (test) display is active. Window 100 lives on the external
    // display's space, window 200 lives on the active display's space.

    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 200, ext_frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, frame);

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(0, move |world, _state| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 200, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 200, EXT_WORKSPACE_ID);
            // The OS frame for window 100 must stay within the external
            // display's vertical bounds (negative y); if init moved it
            // onto the active display the frame would land at y >= 0.
            assert_window_at!(world, 100, ext_origin.x, ext_origin.y);
        })
        .run(commands);
}

/// Waking from sleep (or a resolution/configuration change) with a monitor
/// gone should reconcile the ECS display set against the OS even though no
/// per-display `DisplayRemoved` flag arrives: the vanished display is removed
/// and its workspace is orphaned.
#[test]
fn test_wake_reconciles_unplugged_display() {
    let harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    // A window on the external display so its workspace strip actually exists.
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, ext_frame);

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::SystemWoke { msg: String::new() },
    ];

    harness
        .on_iteration(1, |world, state| {
            let displays = world
                .query_filtered::<Entity, With<Display>>()
                .iter(world)
                .count();
            assert_eq!(displays, 2, "should start with two displays");

            // Unplug the external display behind paneru's back — no
            // DisplayRemoved event is sent, mimicking a wake-from-sleep.
            state.remove_display(EXT_DISPLAY_ID);
        })
        .on_iteration(2, |world, _state| {
            let displays = world
                .query_filtered::<Entity, With<Display>>()
                .iter(world)
                .count();
            assert_eq!(displays, 1, "reconcile should despawn the vanished display");

            // The external display's workspace must be orphaned, not lost.
            let orphan = world
                .query::<(&LayoutStrip, Option<&ChildOf>, Has<Timeout>)>()
                .iter(world)
                .find(|(strip, _, _)| strip.id() == EXT_WORKSPACE_ID)
                .map(|(_, child, timeout)| (child.is_some(), timeout));
            let (has_parent, has_timeout) =
                orphan.expect("external workspace strip should still exist");
            assert!(!has_parent, "orphaned workspace should have no parent");
            assert!(has_timeout, "orphaned workspace should carry a timeout");
        })
        .run(commands);
}

#[test]
fn test_display_configuration_reapplies_layout_after_geometry_change() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let original = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();

    let new_height = TEST_DISPLAY_HEIGHT - 100;
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(120, 0, TEST_DISPLAY_WIDTH + 120, new_height),
    );
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });

    harness.advance(Duration::from_millis(200));
    let world = harness.world();
    let display = world.query::<&Display>().single(world).unwrap();
    assert_eq!(display.bounds().min.x, 0, "wait for the display to settle");

    harness.advance(Duration::from_secs(1));
    let updated = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();
    assert_eq!(updated.min.x, original.min.x + 120);
    assert_eq!(updated.height(), new_height - TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_display_configuration_catches_late_geometry_change() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let original_x = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame()
        .min
        .x;
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });
    // The first settled scan still sees the old display configuration.
    harness.advance(Duration::from_millis(600));

    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(90, 0, TEST_DISPLAY_WIDTH + 90, TEST_DISPLAY_HEIGHT),
    );
    harness.advance(Duration::from_millis(1300));

    let world = harness.world();
    let display = world.query::<&Display>().single(world).unwrap();
    assert_eq!(display.bounds().min.x, 90);
    assert_eq!(
        world.get::<Window>(window_entity).unwrap().frame().min.x,
        original_x + 90
    );
}

#[test]
fn test_transient_empty_display_list_keeps_layout_and_retries() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    harness.mock_state.remove_display(TEST_DISPLAY_ID);
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });
    harness.advance(Duration::from_millis(600));

    let world = harness.world();
    let display_entity = world
        .query_filtered::<Entity, With<Display>>()
        .single(world)
        .unwrap();
    let strip_entity = world
        .query_filtered::<Entity, With<LayoutStrip>>()
        .single(world)
        .unwrap();
    assert_eq!(
        world.get::<ChildOf>(strip_entity).unwrap().parent(),
        display_entity
    );

    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(100, 0, TEST_DISPLAY_WIDTH + 100, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID],
    );
    harness.advance(Duration::from_millis(1600));
    let world = harness.world();
    let display = world.query::<&Display>().single(world).unwrap();
    assert_eq!(
        display.bounds().min.x,
        100,
        "retry should use the recovered display"
    );
}

#[test]
fn test_unplug_rehomes_workspace_and_window_on_surviving_display() {
    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let frame = IRect::from_corners(
        ext_origin,
        ext_origin + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, frame);
    harness.advance(Duration::from_millis(500));

    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID, EXT_WORKSPACE_ID],
    );
    harness
        .world()
        .write_message::<Event>(Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        });
    harness.advance(Duration::from_secs(1));

    let world = harness.world();
    let main = world.query::<(&Display, Entity)>().single(world).unwrap().1;
    let external_parent = world
        .query::<(&LayoutStrip, &ChildOf)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .map(|(_, child)| child.parent())
        .expect("external workspace should remain managed");
    assert_eq!(external_parent, main);
    let window_entity = find_window_entity(100, world);
    let window = world.get::<Window>(window_entity).unwrap();
    assert!(window.frame().min.y >= TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_late_space_rehome_reapplies_window_frame() {
    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        100,
        IRect::from_corners(
            ext_origin,
            ext_origin + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
        ),
    );
    harness.advance(Duration::from_millis(500));
    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness
        .world()
        .write_message::<Event>(Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        });
    harness.advance(Duration::from_millis(650));

    let world = harness.world();
    let orphan = world
        .query::<(&LayoutStrip, Option<&ChildOf>)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .expect("external workspace should be retained");
    assert!(orphan.1.is_none(), "workspace should await its new display");

    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID, EXT_WORKSPACE_ID],
    );
    harness.advance(Duration::from_millis(1200));

    let world = harness.world();
    let external_parent = world
        .query::<(&LayoutStrip, &ChildOf)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .map(|(_, child)| child.parent())
        .expect("external workspace should be reparented");
    let main = world.query::<(&Display, Entity)>().single(world).unwrap().1;
    assert_eq!(external_parent, main);
    let window_entity = find_window_entity(100, world);
    assert!(world.get::<Window>(window_entity).unwrap().frame().min.y >= TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_wake_reapplies_window_frame_without_geometry_change() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let original = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();

    harness.mock_state.update_window(0, |window| {
        window.frame.min.x += 200;
        window.frame.max.x += 200;
    });
    harness
        .world()
        .write_message::<Event>(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(1));

    let restored = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();
    assert_eq!(restored, original);
}

#[test]
fn test_vertical_swap_within_stack_stays_on_display() {
    // Regression test: with a display arranged *below* the active one, a
    // `Swap(South)` inside a stack used to swap the two windows and then
    // immediately send the focused one to the display below, because the
    // "is there anything left to swap with?" check ran against the layout
    // after the swap had already happened.
    let mut harness = TestHarness::new().with_windows(2);
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::South)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(4, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
        })
        .on_iteration(6, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 1, EXT_WORKSPACE_ID);
            assert_eq!(
                state.active_display(),
                TEST_DISPLAY_ID,
                "swapping inside a stack must not move focus to another display"
            );
        })
        .run(commands);
}

#[test]
fn test_hidden_stack_stays_off_the_display_below() {
    // Regression test: hiding a virtual workspace parks its strip at the
    // display's bottom-right corner. Windows below the strip origin - the
    // lower members of a stack - used to land past the bottom edge entirely,
    // inside the display underneath, which macOS then adopts them onto. The
    // window came back on the wrong display once the workspace was shown
    // again.
    let mut harness = TestHarness::new().with_windows(2);
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(4, |world, _state| {
            // Hidden, but still parked on their own display: every window keeps
            // its origin above the top edge of the display below.
            let mut query = world.query::<&crate::manager::Window>();
            for window in query.iter(world) {
                let frame = window.frame();
                assert!(
                    frame.min.y < TEST_DISPLAY_HEIGHT,
                    "window {} parked at {:?}, inside the display below",
                    window.id(),
                    frame
                );
                assert_eq!(
                    frame.min.y,
                    TEST_DISPLAY_HEIGHT - PARKED_STRIP_SLIVER,
                    "window {} should park on the corner sliver",
                    window.id()
                );
            }
        })
        .on_iteration(6, |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 1, EXT_WORKSPACE_ID);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 0, 394);
        })
        .run(commands);
}

/// Focusing a window on another display and coming back must not re-derive the
/// strip offset on the display we left: the centering the user asked for there
/// is still what they want to see when they return.
#[test]
fn test_center_survives_display_round_trip() {
    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(false),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let window_x = |world: &mut World, id: WinID| -> i32 {
        let mut query = world.query::<&Window>();
        query
            .iter(world)
            .find(|window| window.id() == id)
            .expect("window not found")
            .frame()
            .min
            .x
    };

    let commands = vec![
        // 0: boot with focus on window 0.
        Event::MenuOpened { window_id: 0 },
        // 1: center it on the main display.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: focus moves to the window on the external display.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: and back to window 0.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(config)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
        })
        .on_iteration(1, move |world, state| {
            assert_eq!(window_x(world, 0), centered, "window 0 must be centered");
            state.focus_window(100);
        })
        .on_iteration(2, move |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(3, move |world, _state| {
            assert_eq!(
                window_x(world, 0),
                centered,
                "returning from another display must not undo the centering"
            );
        })
        .run(commands);
}

#[test]
fn focusing_another_display_without_switching_its_workspace_does_not_flash() {
    let mut harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    // The focus event can arrive before the active-display notification.
    harness.mock_state.focus_window(100);
    harness.advance(Duration::from_millis(60));
    let world = harness.world();
    let mut active =
        world.query_filtered::<&LayoutStrip, With<crate::ecs::ActiveWorkspaceMarker>>();
    assert_eq!(active.single(world).unwrap().id(), EXT_WORKSPACE_ID);
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).next().is_none(),
        "focus on another display must not flash on the original display"
    );

    harness.mock_state.activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(60));
    let world = harness.world();
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).next().is_none(),
        "recognizing the new display must not turn focus into a workspace popup"
    );

    harness.mock_state.focus_window(0);
    harness.advance(Duration::from_millis(60));
    let world = harness.world();
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).next().is_none(),
        "returning to an unchanged strip must also stay quiet"
    );

    harness.mock_state.activate_display(TEST_DISPLAY_ID);
    harness.advance(Duration::from_millis(60));
    let world = harness.world();
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(messages.iter(world).next().is_none());
}

#[test]
fn focus_display_south_does_not_flash() {
    let mut harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(
                0,
                TEST_DISPLAY_HEIGHT,
                EXT_DISPLAY_WIDTH,
                TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
            ),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
        });
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::FocusDisplay(Direction::South)),
        },
    ]);

    let world = harness.world();
    let mut active =
        world.query_filtered::<&LayoutStrip, With<crate::ecs::ActiveWorkspaceMarker>>();
    assert_eq!(active.single(world).unwrap().id(), EXT_WORKSPACE_ID);
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).next().is_none(),
        "focus_display south must not create a workspace popup"
    );
}

#[test]
fn popup_waits_until_the_destination_display_is_active() {
    let mut harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    let target = {
        let world = harness.world();
        let display = world
            .query::<(Entity, &Display)>()
            .iter(world)
            .find_map(|(entity, display)| (display.id() == EXT_DISPLAY_ID).then_some(entity))
            .expect("external display");
        world
            .spawn((
                LayoutStrip::new(EXT_WORKSPACE_ID, 1),
                crate::ecs::Position(Origin::new(0, -EXT_DISPLAY_HEIGHT)),
                ChildOf(display),
            ))
            .id()
    };

    // Workspace activation arrives while the original display is still marked active.
    harness
        .world()
        .entity_mut(target)
        .insert(crate::ecs::ActiveWorkspaceMarker);
    harness.advance(Duration::from_millis(20));
    let world = harness.world();
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).next().is_none(),
        "the popup must not render on the old display"
    );

    harness.mock_state.activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(60));
    let world = harness.world();
    let mut messages = world.query::<&crate::ecs::FlashMessage>();
    assert!(
        messages.iter(world).any(|message| message.0 == "2"),
        "the pending workspace change should flash after display recognition"
    );
}

/// An empty row 0 must survive its display going away. Despawning it left the
/// space renumbered from "2" — the menu bar lists only the rows that exist —
/// with no switch or reap path that recreates row 0.
#[test]
fn test_empty_baseline_row_survives_display_removal() {
    let mut commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];
    commands.extend((0..6).map(|_| Event::Command {
        command: Command::PrintState,
    }));
    commands.push(Event::DisplayAdded {
        display_id: TEST_DISPLAY_ID,
    });

    TestHarness::new()
        .on_iteration(0, |world, state| {
            let strips = world
                .query::<&LayoutStrip>()
                .iter(world)
                .map(|strip| strip.virtual_index)
                .collect::<Vec<_>>();
            assert_eq!(strips, vec![0], "the space starts with an empty row 0");
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(7, |world, mut state| {
            let entity = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .single(world)
                .expect("empty row 0 should be orphaned, not despawned");
            assert!(
                world.entity(entity).get::<Timeout>().is_some(),
                "orphaned row 0 should carry a timeout"
            );
            state.add_display(
                TEST_DISPLAY_ID,
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
                vec![TEST_WORKSPACE_ID],
            );
        })
        .on_iteration(8, |world, _state| {
            let entity = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .single(world)
                .expect("row 0 should still exist after the display returns");
            assert!(
                world.entity(entity).get::<ChildOf>().is_some(),
                "row 0 should be re-parented to the returning display"
            );
        })
        .run(commands);
}

fn window_command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

fn window_entity(world: &mut World, id: WinID) -> Option<Entity> {
    world
        .query::<(&Window, Entity)>()
        .iter(world)
        .find_map(|(window, entity)| (window.id() == id).then_some(entity))
}

/// The external display sits above the built-in one, with window 100 on it.
fn harness_with_display_above() -> TestHarness {
    let ext_bounds = IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0);
    TestHarness::new()
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, ext_bounds, vec![EXT_WORKSPACE_ID])
        .with_window(100, |data| {
            data.workspace_id = EXT_WORKSPACE_ID;
            data.frame = IRect::new(100, -EXT_DISPLAY_HEIGHT + 100, 900, -100);
        })
}

/// A display with no window gets its desktop focused, which makes it the
/// active display without moving the pointer.
#[test]
fn test_focus_display_without_a_window_focuses_its_desktop() {
    let ext_bounds = IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0);
    TestHarness::new()
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, ext_bounds, vec![EXT_WORKSPACE_ID])
        .on_iteration(1, move |world, state| {
            let target = world
                .query::<&Display>()
                .iter(world)
                .find(|display| display.id() == EXT_DISPLAY_ID)
                .map(Display::bounds)
                .expect("the target display should exist");
            assert_eq!(state.focused_desktop(), Some(target));
            assert_eq!(state.cursor_position(), Origin::ZERO);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::FocusDisplay(Direction::North)),
        ]);
}

/// Centre column and moving a window to another display leave the pointer
/// where it is.
#[test]
fn test_center_and_move_to_display_keep_the_pointer() {
    harness_with_display_above()
        .on_iteration(2, |_world, state| {
            assert_eq!(state.cursor_position(), Origin::ZERO);
        })
        .on_iteration(3, |world, state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_eq!(state.cursor_position(), Origin::ZERO);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::Center),
            window_command(Operation::MoveToDisplay(Direction::North)),
        ]);
}

/// Focus moves to the display in the pressed direction and nowhere else,
/// and the pointer stays where it is.
#[test]
fn test_focus_display_follows_the_physical_arrangement() {
    harness_with_display_above()
        .on_iteration(2, |world, state| {
            assert_focused!(world, 0);
            assert_eq!(state.cursor_position(), Origin::ZERO);
        })
        .on_iteration(3, |world, state| {
            assert_focused!(world, 100);
            assert_eq!(state.cursor_position(), Origin::ZERO);
            assert_eq!(state.focused_desktop(), None);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::FocusDisplay(Direction::South)),
            window_command(Operation::FocusDisplay(Direction::North)),
        ]);
}

#[test]
fn test_focus_display_preserves_the_selected_virtual_workspace() {
    harness_with_display_above()
        .with_window(101, |data| {
            data.workspace_id = EXT_WORKSPACE_ID;
            data.frame = IRect::new(900, -EXT_DISPLAY_HEIGHT + 100, 1300, -100);
        })
        .on_iteration(2, |world, _state| {
            let target = world
                .query::<(Entity, &Display)>()
                .iter(world)
                .find(|(_, display)| display.id() == EXT_DISPLAY_ID)
                .map(|(entity, _)| entity)
                .expect("the target display should exist");
            world
                .entity_mut(target)
                .insert(crate::ecs::ActiveDisplayMarker);
        })
        .on_iteration(5, |world, state| {
            assert_focused!(world, 100);
            let selected = world
                .query_filtered::<&LayoutStrip, With<crate::ecs::ActiveWorkspaceMarker>>()
                .single(world)
                .expect("the target display should have an active workspace");
            assert_eq!(selected.virtual_index, 1);
            let source = world
                .query::<(Entity, &Display)>()
                .iter(world)
                .find(|(_, display)| display.id() == TEST_DISPLAY_ID)
                .map(|(entity, _)| entity)
                .expect("the source display should exist");
            world
                .entity_mut(source)
                .insert(crate::ecs::ActiveDisplayMarker);
            state.focus_window(0);
        })
        .on_iteration(7, |world, _state| {
            assert_focused!(world, 100);
            let selected = world
                .query_filtered::<&LayoutStrip, With<crate::ecs::ActiveWorkspaceMarker>>()
                .single(world)
                .expect("the target display should have an active workspace");
            assert_eq!(selected.virtual_index, 1);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::FocusDisplay(Direction::North)),
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
            window_command(Operation::Center),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            window_command(Operation::FocusDisplay(Direction::North)),
        ]);
}

#[test]
fn test_focus_display_keeps_the_foreground_native_tab() {
    for remember_background in [false, true] {
        harness_with_display_above()
            .with_window(101, |data| {
                data.workspace_id = EXT_WORKSPACE_ID;
                data.frame = IRect::new(900, -EXT_DISPLAY_HEIGHT + 100, 1300, -100);
            })
            .on_iteration(1, move |world, state| {
                let front = find_window_entity(100, world);
                let back = find_window_entity(101, world);
                for mut strip in world.query::<&mut LayoutStrip>().iter_mut(world) {
                    if strip.id() == EXT_WORKSPACE_ID {
                        strip.append_tab_group(&[front, back]);
                    }
                }
                let mut history = world.resource_mut::<crate::ecs::focus::FocusHistory>();
                history.forget(front);
                history.forget(back);
                if remember_background {
                    history.record(EXT_WORKSPACE_ID, back, None);
                }
                state.window_visible(101, false);
            })
            .on_iteration(3, |world, _state| {
                assert_focused!(world, 100);
            })
            .run(vec![
                Event::MenuOpened { window_id: 0 },
                window_command(Operation::Focus(Direction::First)),
                Event::Command {
                    command: Command::PrintState,
                },
                window_command(Operation::FocusDisplay(Direction::North)),
            ]);
    }
}

/// The focused window moves to the display in the pressed direction only.
#[test]
fn test_move_to_display_follows_the_physical_arrangement() {
    harness_with_display_above()
        .on_iteration(2, |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .on_iteration(3, |world, _state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::East)),
            window_command(Operation::MoveToDisplay(Direction::North)),
        ]);
}

#[test]
fn test_move_to_display_round_trip_refits_source_strip_with_continuous_swipe() {
    assert_display_round_trip_refits_source_strip(true);
}

#[test]
fn test_move_to_display_round_trip_refits_source_strip_without_continuous_swipe() {
    assert_display_round_trip_refits_source_strip(false);
}

fn display_round_trip_harness(
    options: MainOptions,
    origin_x: i32,
    middle_width: i32,
) -> TestHarness {
    let config: Config = (
        MainOptions {
            padding_left: Some(4),
            padding_right: Some(4),
            ..options
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(origin_x, 1440, origin_x + 2048, 2772),
            vec![TEST_WORKSPACE_ID],
        )
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, 0, 2560, 1440),
            vec![EXT_WORKSPACE_ID],
        );

    for (id, x, width) in [
        (1, 4, 508),
        (2, 512, middle_width),
        (3, 512 + middle_width, 508),
    ] {
        harness = harness.with_workspace_window(id, TEST_WORKSPACE_ID, |window| {
            window.frame = IRect::new(origin_x + x, 1460, origin_x + x + width, 2772);
        });
    }
    harness.with_workspace_window(0, EXT_WORKSPACE_ID, |window| {
        window.frame = IRect::new(996, 20, 1706, 1440);
    })
}

/// A window open at launch on one display and later moved to another must
/// stay on the display that now holds it when the engine exits, not jump back
/// to its frame from before launch.
#[test]
fn exit_cleanup_keeps_a_moved_window_on_its_current_display() {
    let main_bounds = IRect::new(0, 1440, 2048, 2772);
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    harness.advance(Duration::from_secs(3));

    let world = harness.world();
    super::interaction::run_exit_cleanup(world);

    let frame = harness.mock_state.create_window(0).frame();
    assert!(
        main_bounds.contains(frame.min) && main_bounds.contains(frame.max),
        "exit frame {frame:?} must lie inside the main display {main_bounds:?}"
    );
}

fn assert_display_round_trip_refits_source_strip(continuous_swipe: bool) {
    let harness = display_round_trip_harness(
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(continuous_swipe),
            ..Default::default()
        },
        0,
        890,
    );
    harness
        .on_iteration(2, |world, state| {
            assert_focused!(world, 0);
            assert_window_at!(world, 1, 4, 1460);
            assert_window_at!(world, 2, 512, 1460);
            assert_window_at!(world, 3, 1402, 1460);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(4, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            state.update_window(0, |window| window.workspace_id = TEST_WORKSPACE_ID);
            state.os_activate_display(TEST_DISPLAY_ID);
        })
        .on_iteration(5, |world, state| {
            assert_focused!(world, 0);
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
            let first = find_window_entity(1, world);
            assert!(
                world.get::<Window>(first).unwrap().frame().min.x < 4,
                "inserting the fourth column must scroll the overflowing strip"
            );
        })
        .on_iteration(6, |world, state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            state.update_window(0, |window| window.workspace_id = EXT_WORKSPACE_ID);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(7, |world, state| {
            assert_focused!(world, 0);
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), EXT_DISPLAY_ID);

            let frames: Vec<_> = [1, 2, 3]
                .into_iter()
                .map(|id| {
                    assert_on_workspace!(world, id, TEST_WORKSPACE_ID);
                    let entity = find_window_entity(id, world);
                    world.get::<Window>(entity).unwrap().frame()
                })
                .collect();
            assert_eq!(
                frames.iter().map(IRect::width).collect::<Vec<_>>(),
                vec![508, 890, 508],
                "the surviving columns must retain their widths"
            );
            assert!(
                frames.iter().all(|frame| frame.min.x >= 4 && frame.max.x <= 2044),
                "all three surviving columns fit and must remain visible after the round trip: {frames:?}"
            );
            assert_eq!(frames[1].min.x - frames[0].min.x, 508);
            assert_eq!(frames[2].min.x - frames[1].min.x, 890);
        })
        .run(vec![
            Event::MenuOpened { window_id: 1 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::FocusDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            window_command(Operation::MoveToDisplay(Direction::South)),
            Event::Command {
                command: Command::PrintState,
            },
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

fn source_strip_after_arrival(
    options: MainOptions,
    origin_x: i32,
    middle_width: i32,
) -> TestHarness {
    let mut harness = display_round_trip_harness(options, origin_x, middle_width);
    harness.run(vec![
        Event::MenuOpened { window_id: 1 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::FocusDisplay(Direction::North)),
    ]);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.run(vec![
        Event::Command {
            command: Command::PrintState,
        },
        window_command(Operation::MoveToDisplay(Direction::South)),
    ]);
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
    harness.advance(Duration::from_millis(500));
    assert_focused!(harness.world(), 0);
    assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);
    harness
}

fn source_frames(harness: &TestHarness) -> [IRect; 3] {
    [1, 2, 3].map(|id| harness.mock_state.create_window(id).frame())
}

#[test]
fn test_move_to_display_right_aligns_overflowing_arrival_after_width_adaptation() {
    let mut harness = source_strip_after_arrival(
        MainOptions {
            auto_center: Some(false),
            ..Default::default()
        },
        0,
        890,
    );
    assert_focused!(harness.world(), 0);
    assert_eq!(harness.mock_state.active_display(), TEST_DISPLAY_ID);
    for id in [1, 2, 3, 0] {
        assert_on_workspace!(harness.world(), id, TEST_WORKSPACE_ID);
        assert_not_on_workspace!(harness.world(), id, EXT_WORKSPACE_ID);
    }

    let frames = [1, 2, 3, 0].map(|id| harness.mock_state.create_window(id).frame());
    assert_eq!(
        frames.map(|frame| frame.width()),
        [508, 890, 508, 568],
        "the arriving width must adapt from 710/2552 to the 2040-point viewport"
    );
    let world = harness.world();
    let expected_columns = [1, 2, 3, 0].map(|id| find_window_entity(id, world));
    let destination = world
        .query::<&LayoutStrip>()
        .iter(world)
        .find(|strip| strip.id() == TEST_WORKSPACE_ID)
        .unwrap();
    assert_eq!(
        destination.all_columns(),
        expected_columns,
        "arrival must append the column without changing the existing order"
    );
    assert_eq!(
        frames[3].max.x, 2044,
        "the focused last column must finish at the usable right edge after width adaptation"
    );
}

fn pending_display_arrival(options: MainOptions, origin_x: i32, middle_width: i32) -> TestHarness {
    begin_display_arrival(display_round_trip_harness(options, origin_x, middle_width))
}

fn begin_display_arrival(harness: TestHarness) -> TestHarness {
    send_display_arrival(prepare_display_arrival(harness))
}

fn prepare_display_arrival(mut harness: TestHarness) -> TestHarness {
    harness.run(vec![
        Event::MenuOpened { window_id: 1 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::FocusDisplay(Direction::North)),
    ]);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    harness
}

fn send_display_arrival(mut harness: TestHarness) -> TestHarness {
    harness
        .world()
        .write_message(window_command(Operation::MoveToDisplay(Direction::South)));
    harness.advance(Duration::from_millis(20));
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
    harness.advance(Duration::from_millis(60));
    harness
}

#[test]
fn test_overflowing_arrival_uses_pending_width_and_survives_delayed_focus() {
    for continuous_swipe in [false, true] {
        for hidden_ratio in [0.0, 0.5, 1.0] {
            let mut harness = pending_display_arrival(
                MainOptions {
                    animation_speed: Some(8.0),
                    continuous_swipe: Some(continuous_swipe),
                    window_hidden_ratio: Some(hidden_ratio),
                    ..Default::default()
                },
                -2048,
                890,
            );
            let world = harness.world();
            let entity = find_window_entity(0, world);
            let slot = world.get::<crate::ecs::LayoutPosition>(entity).unwrap().0;
            let target = world
                .query::<(&LayoutStrip, &crate::ecs::RepositionMarker)>()
                .iter(world)
                .find(|(strip, _)| strip.contains(entity))
                .unwrap()
                .1
                .0;
            assert_eq!(slot.x + target.x + 568, -4);
            harness.advance(Duration::from_millis(140));
            if continuous_swipe {
                harness
                    .world()
                    .write_message(Event::WindowFocused { window_id: 0 });
            }
            harness.advance(Duration::from_secs(3));
            assert_focused!(harness.world(), 0);
            let frame = harness.mock_state.create_window(0).frame();
            assert_eq!(frame.width(), 568);
            assert_eq!(
                frame.max.x, -4,
                "continuous={continuous_swipe}, hidden={hidden_ratio}"
            );
            assert_window_at!(harness.world(), 0, -572, 1460);
        }
    }
}

#[test]
fn test_arrival_keeps_all_fit_and_auto_center_placement() {
    for (auto_center, middle_width) in [(false, 200), (true, 890)] {
        let mut harness = source_strip_after_arrival(
            MainOptions {
                auto_center: Some(auto_center),
                ..Default::default()
            },
            0,
            middle_width,
        );
        assert_eq!(harness.mock_state.create_window(0).frame().min.x, 669);
        assert_window_at!(harness.world(), 0, 669, 1460);
    }
}

#[test]
fn test_arrival_keeps_a_later_center_or_snap() {
    for operation in [Operation::Center, Operation::Snap] {
        let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
        harness.world().write_message(window_command(operation));
        harness.advance(Duration::from_millis(40));
        let placed = harness.mock_state.create_window(0).frame().min.x;
        harness.advance(Duration::from_millis(500));
        let frame = harness.mock_state.create_window(0).frame();
        assert_eq!(frame.width(), 568);
        assert_eq!(
            frame.min.x, placed,
            "arrival must preserve the newer placement"
        );
    }
}

#[test]
fn test_arrival_allows_free_panning_after_placement() {
    let mut harness = source_strip_after_arrival(
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        0,
        890,
    );
    assert_eq!(harness.mock_state.create_window(0).frame().max.x, 2044);
    harness.run(vec![Event::Swipe {
        delta: -0.04,
        fingers: 3,
    }]);
    assert_ne!(harness.mock_state.create_window(0).frame().max.x, 2044);
    let frames = [1, 2, 3, 0].map(|id| harness.mock_state.create_window(id).frame());
    assert!(frames.windows(2).all(|pair| pair[0].max.x == pair[1].min.x));
}

#[test]
fn test_arrival_preserves_a_completed_modifier_scroll() {
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    harness.world().write_message(Event::Scroll { delta: -0.1 });
    // A wheel tick counts as finished after a 150 ms pause.
    harness.advance(Duration::from_millis(160));
    let before = harness.mock_state.create_window(0).frame().min.x;
    harness.advance(Duration::from_millis(40));
    let world = harness.world();
    let entity = find_window_entity(0, world);
    let strip = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .find(|(_, strip)| strip.contains(entity))
        .unwrap()
        .0;
    assert!(world.get::<crate::ecs::Scrolling>(strip).is_none());
    harness.advance(Duration::from_millis(500));
    let after = harness.mock_state.create_window(0).frame().min.x;
    assert_eq!(
        after, before,
        "arrival must preserve a completed modifier-scroll"
    );
}

#[test]
fn test_repeated_display_transfer_preserves_width_ratio() {
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    harness
        .world()
        .write_message(window_command(Operation::MoveToDisplay(Direction::North)));
    harness.advance(Duration::from_millis(60));
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(60));
    assert_on_workspace!(harness.world(), 0, EXT_WORKSPACE_ID);
    harness.advance(Duration::from_millis(500));
    assert_eq!(harness.mock_state.create_window(0).frame().width(), 711);
}

#[test]
fn test_arrival_keeps_a_later_reorder() {
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    harness
        .world()
        .write_message(window_command(Operation::Swap(Direction::West)));
    harness.advance(Duration::from_millis(40));
    let placed = harness.mock_state.create_window(0).frame().min.x;
    harness.advance(Duration::from_millis(500));
    let frame = harness.mock_state.create_window(0).frame();
    assert_eq!(frame.width(), 568);
    assert_eq!(frame.min.x, placed);
    let world = harness.world();
    let entity = find_window_entity(0, world);
    assert!(world.query::<&LayoutStrip>().iter(world).any(|strip| {
        strip.contains(entity) && !matches!(strip.last(), Ok(crate::ecs::layout::Column::Single(last)) if last == entity)
    }));
}

#[test]
fn test_arrival_consumes_intent_when_native_width_is_constrained() {
    let harness =
        prepare_display_arrival(display_round_trip_harness(MainOptions::default(), 0, 890));
    harness
        .mock_state
        .update_window(0, |window| window.maximum_size = Some(Size::new(500, 1312)));
    let mut harness = send_display_arrival(harness);
    harness.advance(Duration::from_secs(2));
    let frame = harness.mock_state.create_window(0).frame();
    assert_eq!(frame.width(), 500);
    let world = harness.world();
    let entity = find_window_entity(0, world);
    assert_eq!(world.get::<crate::ecs::Bounds>(entity).unwrap().x, 500);
    assert!(world.get::<crate::ecs::PlaceAfterArrival>(entity).is_none());
    assert!(
        world
            .get::<crate::ecs::ReshuffleAroundMarker>(entity)
            .is_none()
    );
}

#[test]
fn test_arrival_width_is_set_before_destination_workspace_switch() {
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    harness
        .world()
        .write_message(window_command(Operation::VirtualAdd));
    harness.advance(Duration::from_millis(40));
    harness
        .world()
        .write_message(window_command(Operation::VirtualNumber(2)));
    harness.advance(Duration::from_millis(500));
    assert_eq!(harness.mock_state.create_window(0).frame().width(), 568);
    assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);
    let world = harness.world();
    let entity = find_window_entity(0, world);
    assert!(world.get::<crate::ecs::PlaceAfterArrival>(entity).is_none());
    assert!(
        world
            .query_filtered::<&LayoutStrip, With<crate::ecs::SelectedVirtualMarker>>()
            .iter(world)
            .all(|strip| !strip.contains(entity))
    );
}

#[test]
fn test_arrival_window_close_clears_intent() {
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    harness.mock_state.os_close_window(0);
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    assert!(
        world
            .query::<&Window>()
            .iter(world)
            .all(|window| window.id() != 0)
    );
    assert!(
        world
            .query::<&crate::ecs::PlaceAfterArrival>()
            .iter(world)
            .next()
            .is_none()
    );
}

#[test]
fn test_arrival_uses_the_destination_width_before_layout() {
    let mut grows = begin_display_arrival(
        display_round_trip_harness(MainOptions::default(), 0, 700)
            .with_display(
                TEST_DISPLAY_ID,
                IRect::new(0, 1440, 2560, 2772),
                vec![TEST_WORKSPACE_ID],
            )
            .with_display(
                EXT_DISPLAY_ID,
                IRect::new(0, 0, 2048, 1440),
                vec![EXT_WORKSPACE_ID],
            ),
    );
    assert_eq!(grows.mock_state.create_window(0).frame().max.x, 2556);
    grows.advance(Duration::from_millis(500));
    let frame = grows.mock_state.create_window(0).frame();
    assert_eq!(frame.width(), 888);
    assert_eq!(frame.max.x, 2556);

    let mut shrinks = pending_display_arrival(MainOptions::default(), 0, 400);
    let initial = shrinks.mock_state.create_window(0).frame();
    assert_eq!(initial.min.x, 669);
    shrinks.advance(Duration::from_millis(500));
    let frame = shrinks.mock_state.create_window(0).frame();
    assert_eq!(frame.width(), 568);
    assert_eq!(
        frame.min.x, initial.min.x,
        "fitting content keeps generic placement"
    );
}

#[test]
fn test_arrival_resize_survives_a_replaced_native_handle() {
    let mut harness = pending_display_arrival(MainOptions::default(), 0, 890);
    let original = find_window_entity(0, harness.world());
    harness.mock_state.os_transfer_window(0, TEST_WORKSPACE_ID);
    harness.advance(Duration::from_secs(2));
    assert_eq!(find_window_entity(0, harness.world()), original);
    assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);
    assert_focused!(harness.world(), 0);
    assert_eq!(harness.mock_state.create_window(0).frame().max.x, 2044);
    assert_eq!(harness.mock_state.create_window(0).frame().width(), 568);
}

#[test]
fn test_source_refit_preserves_overflow_and_auto_center_anchoring() {
    for (auto_center, middle_width) in [(false, 1400), (true, 890)] {
        let mut harness = source_strip_after_arrival(
            MainOptions {
                auto_center: Some(auto_center),
                ..Default::default()
            },
            0,
            middle_width,
        );
        let before = source_frames(&harness);
        assert!(before[2].min.x >= 4 && before[2].max.x <= 2044);
        harness.run(vec![window_command(Operation::MoveToDisplay(
            Direction::North,
        ))]);
        assert_eq!(
            source_frames(&harness),
            before,
            "source neighbour anchoring must remain unchanged"
        );
    }
}

#[test]
fn test_source_refit_preserves_fixed_edge_after_stale_center() {
    let mut harness = display_round_trip_harness(
        MainOptions {
            continuous_swipe: Some(false),
            ..Default::default()
        },
        0,
        890,
    );
    harness.run(vec![
        Event::MenuOpened { window_id: 1 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::Center),
    ]);
    assert_eq!(source_frames(&harness)[0].min.x, 770);
    harness.run(vec![window_command(Operation::MoveToDisplay(
        Direction::North,
    ))]);
    assert_eq!(
        source_frames(&harness)[1].min.x,
        4,
        "removing a centered column must restore the noncontinuous left edge"
    );
}

#[test]
fn test_source_refit_discards_stale_center_placement() {
    let mut harness = source_strip_after_arrival(MainOptions::default(), 0, 890);
    harness.run(vec![window_command(Operation::Center)]);
    assert!(source_frames(&harness)[0].min.x < 4);
    harness.run(vec![window_command(Operation::MoveToDisplay(
        Direction::North,
    ))]);
    assert!(
        source_frames(&harness)
            .iter()
            .all(|frame| frame.min.x >= 4 && frame.max.x <= 2044)
    );
}

#[test]
fn test_source_refit_uses_nonzero_viewport_and_ignores_hidden_ratio() {
    let mut harness = source_strip_after_arrival(
        MainOptions {
            window_hidden_ratio: Some(1.0),
            ..Default::default()
        },
        -2048,
        890,
    );
    harness.run(vec![window_command(Operation::MoveToDisplay(
        Direction::North,
    ))]);
    assert!(
        source_frames(&harness)
            .iter()
            .all(|frame| frame.min.x >= -2044 && frame.max.x <= -4)
    );
}

#[test]
fn test_source_refit_preserves_a_fitting_pending_target() {
    let mut harness = source_strip_after_arrival(MainOptions::default(), 0, 890);
    let world = harness.world();
    let strip = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .find(|(_, strip)| strip.id() == TEST_WORKSPACE_ID)
        .unwrap()
        .0;
    world
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(IVec2::new(100, 1460)));
    harness.run(vec![window_command(Operation::MoveToDisplay(
        Direction::North,
    ))]);
    assert_eq!(
        source_frames(&harness)[0].min.x,
        100,
        "an already fitting target needs no correction"
    );
}

#[test]
fn test_source_refit_survives_delayed_stay_focus_during_animation() {
    assert_source_refit_survives_delayed_stay_focus(0.0);
}

#[test]
fn test_source_refit_survives_delayed_stay_focus_with_hidden_ratio() {
    assert_source_refit_survives_delayed_stay_focus(0.5);
}

fn assert_source_refit_survives_delayed_stay_focus(hidden_ratio: f64) {
    let mut harness = source_strip_after_arrival(
        MainOptions {
            window_hidden_ratio: Some(hidden_ratio),
            ..Default::default()
        },
        0,
        890,
    );
    let config: Config = (
        MainOptions {
            animation_speed: Some(8.0),
            window_hidden_ratio: Some(hidden_ratio),
            padding_left: Some(4),
            padding_right: Some(4),
            ..Default::default()
        },
        vec![],
    )
        .into();
    harness.world().insert_resource(config);
    harness
        .world()
        .write_message(window_command(Operation::ToNextDisplay(MoveFocus::Stay)));
    harness.advance(Duration::from_millis(60));
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = EXT_WORKSPACE_ID);
    assert!(
        source_frames(&harness)[0].min.x < 4,
        "the focus event must arrive before the animation finishes"
    );
    harness
        .world()
        .write_message(Event::WindowFocused { window_id: 3 });
    harness.advance(Duration::from_millis(2000));
    assert_focused!(harness.world(), 3);
    assert_eq!(harness.mock_state.active_display(), TEST_DISPLAY_ID);
    assert!(
        source_frames(&harness)
            .iter()
            .all(|frame| frame.min.x >= 4 && frame.max.x <= 2044),
        "delayed source focus must not stop the fitting correction: {:?}",
        source_frames(&harness)
    );
}

#[test]
fn test_move_to_display_keeps_focus_when_the_app_rebuilds_the_ax_element() {
    let moved = Rc::new(Cell::new(None));
    let recorded = moved.clone();
    harness_with_display_above()
        .on_iteration(2, move |world, state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            recorded.set(Some(find_window_entity(0, world)));
            state.os_transfer_window(0, EXT_WORKSPACE_ID);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(3, move |world, state| {
            assert_eq!(
                state.focused_window_id(TEST_PROCESS_ID),
                Some(0),
                "the app should still report the moved window as focused"
            );
            assert_focused!(world, 0);
            assert_eq!(
                window_entity(world, 0),
                moved.get(),
                "the moved window should keep its entity"
            );
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
        })
        .on_iteration(4, |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            window_command(Operation::MoveToDisplay(Direction::South)),
        ]);
}

/// Sending a window away leaves focus on its source neighbour, and the sent
/// window stays managed on the target even when its AX element is rebuilt.
#[test]
fn test_send_to_display_keeps_the_window_when_the_app_rebuilds_the_ax_element() {
    let moved = Rc::new(Cell::new(None));
    let recorded = moved.clone();
    harness_with_display_above()
        .with_window(1, |_| {})
        .on_iteration(2, move |world, state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            recorded.set(Some(find_window_entity(0, world)));
            state.os_transfer_window(0, EXT_WORKSPACE_ID);
        })
        .on_iteration(3, move |world, state| {
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(1));
            assert_focused!(world, 1);
            assert_eq!(
                window_entity(world, 0),
                moved.get(),
                "the sent window should keep its entity"
            );
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::ToNextDisplay(MoveFocus::Stay)),
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// Closing a window after it moved displays still removes it and hands focus
/// to the window beside it.
#[test]
fn test_close_after_move_to_display_still_removes_the_window() {
    harness_with_display_above()
        .on_iteration(2, |_world, state| {
            state.os_transfer_window(0, EXT_WORKSPACE_ID);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(3, |_world, state| {
            state.os_close_window(0);
        })
        .on_iteration(4, |world, state| {
            assert_eq!(window_entity(world, 0), None);
            assert_focused!(world, 100);
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(100));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// The app tears the old AX element down after replacing it. That teardown
/// names an element the moved window no longer uses, so the window stays.
#[test]
fn test_move_to_display_ignores_the_teardown_of_the_replaced_ax_element() {
    let moved = Rc::new(Cell::new(None));
    let recorded = moved.clone();
    harness_with_display_above()
        .on_iteration(2, move |world, state| {
            recorded.set(Some(find_window_entity(0, world)));
            state.os_transfer_window(0, EXT_WORKSPACE_ID);
            state.os_tear_down_replaced_element(0);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(3, move |world, state| {
            assert_eq!(window_entity(world, 0), moved.get());
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(0));
            assert_focused!(world, 0);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// The app may list the fresh AX element a while after the old one died. The
/// moved window keeps its entity and focus while it waits, then takes the
/// fresh element, so the reverse move takes it back.
#[test]
fn test_move_to_display_waits_for_a_late_ax_element() {
    let moved = Rc::new(Cell::new(None));
    let (recorded, waiting) = (moved.clone(), moved.clone());
    harness_with_display_above()
        .on_iteration(2, move |world, state| {
            recorded.set(Some(find_window_entity(0, world)));
            state.os_transfer_window_late(0, EXT_WORKSPACE_ID);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(3, move |world, state| {
            assert_eq!(window_entity(world, 0), waiting.get());
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(0));
            assert_focused!(world, 0);
            state.os_list_window(0);
        })
        .on_iteration(4, move |world, state| {
            let entity = window_entity(world, 0);
            assert_eq!(entity, moved.get());
            let window = world.get::<Window>(entity.unwrap()).unwrap();
            assert!(
                window.role().is_ok(),
                "the window should use the fresh element"
            );
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(0));
            assert_focused!(world, 0);
        })
        .on_iteration(5, |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            window_command(Operation::MoveToDisplay(Direction::South)),
        ]);
}

#[test]
fn test_close_while_waiting_for_a_late_ax_element_removes_the_window() {
    harness_with_display_above()
        .on_iteration(2, |_world, state| {
            state.os_transfer_window_late(0, EXT_WORKSPACE_ID);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(3, |_world, state| {
            state.os_close_window(0);
        })
        .on_iteration(4, |world, state| {
            assert_eq!(window_entity(world, 0), None);
            assert_focused!(world, 100);
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(100));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// With no fresh AX element, the window is closed the usual way after a
/// bounded wait, and repeated notifications do not extend that wait.
#[test]
fn test_move_to_display_stops_waiting_for_an_ax_element_that_never_comes() {
    let moved = Rc::new(Cell::new(None));
    let (recorded, waiting) = (moved.clone(), moved.clone());
    harness_with_display_above()
        .on_iteration(2, move |world, state| {
            recorded.set(Some(find_window_entity(0, world)));
            state.os_transfer_window_late(0, EXT_WORKSPACE_ID);
            state.os_activate_display(EXT_DISPLAY_ID);
        })
        .on_iteration(3, move |world, state| {
            assert_eq!(window_entity(world, 0), waiting.get());
            state.os_repeat_space_destroy(0);
        })
        .on_iteration(4, |_world, state| {
            state.os_repeat_space_destroy(0);
        })
        .on_iteration(5, move |world, state| {
            assert_ne!(window_entity(world, 0), moved.get());
            assert_focused!(world, 100);
            assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(100));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::MoveToDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// Focus and swap within a column stay on the display at the column's edge.
/// Only the display commands cross to another display.
#[test]
fn test_vertical_focus_and_swap_stay_on_the_display_at_a_column_edge() {
    harness_with_display_above()
        .on_iteration(2, |world, state| {
            assert_focused!(world, 0);
            assert_eq!(state.cursor_position(), Origin::ZERO);
        })
        .on_iteration(3, |world, _state| {
            assert_focused!(world, 0);
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::Focus(Direction::North)),
            window_command(Operation::Swap(Direction::North)),
        ]);
}

#[test]
fn test_late_handle_replays_pending_geometry() {
    let mut harness = harness_with_display_above();
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
    ]);
    let entity = find_window_entity(0, harness.world());
    harness
        .world()
        .write_message(window_command(Operation::MoveToDisplay(Direction::North)));
    harness.advance(Duration::from_millis(60));
    harness
        .mock_state
        .os_transfer_window_late(0, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(600));
    assert!(
        harness
            .world()
            .get::<crate::ecs::AwaitingHandle>(entity)
            .is_some()
    );
    let expected = harness.world().get::<crate::ecs::Bounds>(entity).unwrap().0;
    let expected_origin = harness
        .world()
        .get::<crate::ecs::Position>(entity)
        .unwrap()
        .0;
    harness.mock_state.os_list_window(0);
    harness.advance(Duration::from_millis(2000));
    assert_eq!(window_entity(harness.world(), 0), Some(entity));
    assert!(
        harness
            .world()
            .get::<crate::ecs::AwaitingHandle>(entity)
            .is_none()
    );
    let actual = harness.mock_state.create_window(0).frame();
    assert_eq!(
        actual.size(),
        expected,
        "native size must catch up after handle recovery"
    );
    assert_eq!(
        actual.min, expected_origin,
        "native position must catch up after handle recovery"
    );
}

#[test]
fn test_late_handle_reconciles_native_size_constraints() {
    let mut harness = harness_with_display_above();
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
    ]);
    let entity = find_window_entity(0, harness.world());
    harness
        .world()
        .write_message(window_command(Operation::MoveToDisplay(Direction::North)));
    harness.advance(Duration::from_millis(60));
    harness
        .mock_state
        .os_transfer_window_late(0, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(600));
    harness.mock_state.update_window(0, |window| {
        window.maximum_size = Some(IVec2::new(600, 2000));
    });
    harness.mock_state.os_list_window(0);
    harness.advance(Duration::from_millis(2000));
    assert_eq!(window_entity(harness.world(), 0), Some(entity));
    assert_eq!(harness.mock_state.create_window(0).frame().width(), 600);
    assert_eq!(
        harness
            .world()
            .get::<crate::ecs::Bounds>(entity)
            .unwrap()
            .0
            .x,
        600
    );
}

#[test]
#[allow(clippy::float_cmp)]
fn test_late_handle_keeps_width_ratio_after_active_display_changes() {
    let mut harness = harness_with_display_above().with_window(1, |_| {});
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
    ]);
    let entity = find_window_entity(0, harness.world());
    harness
        .world()
        .write_message(window_command(Operation::MoveToDisplay(Direction::North)));
    harness.advance(Duration::from_millis(60));
    harness
        .mock_state
        .os_transfer_window_late(0, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(600));
    harness.mock_state.focus_window(1);
    harness.mock_state.os_activate_display(TEST_DISPLAY_ID);
    harness.advance(Duration::from_millis(100));
    let expected = harness
        .world()
        .get::<crate::ecs::WidthRatio>(entity)
        .unwrap()
        .0;
    harness.mock_state.os_list_window(0);
    harness.advance(Duration::from_millis(2000));
    assert_eq!(window_entity(harness.world(), 0), Some(entity));
    assert_eq!(
        harness
            .world()
            .get::<crate::ecs::WidthRatio>(entity)
            .unwrap()
            .0,
        expected
    );
}

#[test]
fn test_late_recovery_respects_new_user_focus() {
    let mut harness = harness_with_display_above();
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::MoveToDisplay(Direction::North)),
    ]);
    let entity = find_window_entity(0, harness.world());
    harness
        .mock_state
        .os_transfer_window_late(0, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(200));
    harness.mock_state.focus_window(100);
    harness.advance(Duration::from_millis(100));
    harness.mock_state.os_list_window(0);
    harness.advance(Duration::from_millis(200));
    assert_eq!(window_entity(harness.world(), 0), Some(entity));
    assert_eq!(
        harness.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(100)
    );
    assert_focused!(harness.world(), 100);
}

#[test]
fn test_current_close_with_warm_membership_is_authoritative() {
    let mut harness = harness_with_display_above();
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::MoveToDisplay(Direction::North)),
    ]);
    harness.mock_state.os_transfer_window(0, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(100));
    let entity = find_window_entity(0, harness.world());
    let element = harness.world().get::<Window>(entity).unwrap().element();
    harness.world().write_message(Event::WindowDestroyed {
        window_id: 0,
        source: crate::events::DestroySource::Accessibility { element },
    });
    harness.advance(Duration::from_millis(40));
    assert_eq!(window_entity(harness.world(), 0), None);
}

#[test]
fn test_close_new_generation_before_recovery_is_not_a_ghost() {
    let mut harness = harness_with_display_above();
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::MoveToDisplay(Direction::North)),
    ]);
    harness
        .mock_state
        .os_transfer_window_late(0, EXT_WORKSPACE_ID);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.advance(Duration::from_millis(200));
    harness.mock_state.os_close_window(0);
    harness.advance(Duration::from_millis(100));
    assert_eq!(window_entity(harness.world(), 0), None);
}

#[test]
fn test_fix_window_size_on_focus_only_after_display_change() {
    use crate::ecs::{Bounds, RefreshWindowSizeOnFocus};

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::DisplayChanged,
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |_world, state| {
            // Silently change window 1's OS width without emitting WindowResized.
            state.update_window(1, |window| {
                window.frame.max.x = window.frame.min.x + 600;
            });
        })
        .on_iteration(1, |world, _state| {
            let win1 = find_window_entity(1, world);
            assert_eq!(
                world.get::<Bounds>(win1).expect("win1 bounds").0.x,
                TEST_WINDOW_WIDTH,
                "ordinary focus changes must not poll AX to re-read window size"
            );
            assert!(world.get::<RefreshWindowSizeOnFocus>(win1).is_none());
        })
        .on_iteration(2, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            assert!(
                world.get::<RefreshWindowSizeOnFocus>(win0).is_some()
                    && world.get::<RefreshWindowSizeOnFocus>(win1).is_some(),
                "DisplayChanged only attaches RefreshWindowSizeOnFocus without immediately polling AX"
            );
            assert_eq!(
                world.get::<Bounds>(win1).expect("win1 bounds").0.x,
                TEST_WINDOW_WIDTH,
                "already-focused window is not re-read until it next gains focus"
            );
        })
        .on_iteration(3, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            assert!(
                world.get::<RefreshWindowSizeOnFocus>(win0).is_none(),
                "focusing window 0 consumes its RefreshWindowSizeOnFocus marker"
            );
            assert!(
                world.get::<RefreshWindowSizeOnFocus>(win1).is_some(),
                "window 1 keeps RefreshWindowSizeOnFocus until it gains focus"
            );
        })
        .on_iteration(4, |world, _state| {
            let win1 = find_window_entity(1, world);
            assert_eq!(
                world.get::<Bounds>(win1).expect("win1 bounds").0.x,
                600,
                "focusing window 1 with RefreshWindowSizeOnFocus refreshes its Bounds from AX"
            );
            assert!(
                world.get::<RefreshWindowSizeOnFocus>(win1).is_none(),
                "window 1's RefreshWindowSizeOnFocus marker is removed after checking"
            );
        })
        .run(commands);
}

#[test]
fn test_attach_display_moves_virtual_workspace_together() {
    attach_moves_virtual_workspace_together(IRect::new(
        0,
        -EXT_DISPLAY_HEIGHT,
        EXT_DISPLAY_WIDTH,
        0,
    ));
}

#[test]
fn test_attach_display_moves_virtual_workspace_together_below() {
    attach_moves_virtual_workspace_together(IRect::new(
        0,
        TEST_DISPLAY_HEIGHT,
        EXT_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
    ));
}

#[test]
fn test_attach_display_moves_virtual_workspace_together_to_the_right() {
    attach_moves_virtual_workspace_together(IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    ));
}

/// Attaches the external display at `ext`.
fn attach_moves_virtual_workspace_together(ext: IRect) {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::DisplayAdded {
            display_id: EXT_DISPLAY_ID,
        },
    ];

    let mut harness = TestHarness::new().with_windows(4);
    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    harness
        .on_iteration(5, move |world, mut state| {
            // Before attaching EXT_DISPLAY_ID: [0, 1] on v0, [2, 3] on v1.
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            let win2 = find_window_entity(2, world);
            let win3 = find_window_entity(3, world);

            let v0 = world
                .query::<&LayoutStrip>()
                .iter(world)
                .find(|s| s.id() == TEST_WORKSPACE_ID && s.virtual_index == 0)
                .expect("v0 on TEST_WORKSPACE_ID");
            assert_eq!(v0.all_windows(), vec![win0, win1]);

            let v1 = world
                .query::<&LayoutStrip>()
                .iter(world)
                .find(|s| s.id() == TEST_WORKSPACE_ID && s.virtual_index == 1)
                .expect("v1 on TEST_WORKSPACE_ID");
            assert_eq!(v1.all_windows(), vec![win2, win3]);

            // Attach EXT_DISPLAY_ID and report both windows 2 and 3 on EXT_WORKSPACE_ID.
            state.add_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID]);
            state.update_window(2, |w| w.workspace_id = EXT_WORKSPACE_ID);
            state.update_window(3, |w| w.workspace_id = EXT_WORKSPACE_ID);
        })
        .on_iteration(6, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            let win2 = find_window_entity(2, world);
            let win3 = find_window_entity(3, world);

            let main_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == TEST_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            assert_eq!(
                main_strips,
                vec![(0, vec![win0, win1])],
                "source display should only have v0 with [0, 1] after v1 moved away"
            );

            let ext_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == EXT_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            assert_eq!(
                ext_strips,
                vec![(0, vec![win2, win3])],
                "new display should receive [2, 3] together in its first virtual workspace v0"
            );
        })
        .run(commands);
}

#[test]
fn test_attach_display_only_moves_windows_reported_by_os() {
    attach_moves_only_reported_windows(IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0));
}

#[test]
fn test_attach_display_only_moves_windows_reported_by_os_below() {
    attach_moves_only_reported_windows(IRect::new(
        0,
        TEST_DISPLAY_HEIGHT,
        EXT_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
    ));
}

#[test]
fn test_attach_display_only_moves_windows_reported_by_os_to_the_right() {
    attach_moves_only_reported_windows(IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    ));
}

/// Attaches the external display at `ext`.
fn attach_moves_only_reported_windows(ext: IRect) {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::DisplayAdded {
            display_id: EXT_DISPLAY_ID,
        },
    ];

    let mut harness = TestHarness::new().with_windows(4);
    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    harness
        .on_iteration(5, move |_world, mut state| {
            // Attach EXT_DISPLAY_ID, but macOS only reports window 2 (not 3) on EXT_WORKSPACE_ID.
            state.add_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID]);
            state.update_window(2, |w| w.workspace_id = EXT_WORKSPACE_ID);
        })
        .on_iteration(6, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            let win2 = find_window_entity(2, world);
            let win3 = find_window_entity(3, world);

            let mut main_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == TEST_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            main_strips.sort_by_key(|(idx, _)| *idx);
            assert_eq!(
                main_strips,
                vec![(0, vec![win0, win1]), (1, vec![win3])],
                "window 3 was not reported on the new display and must stay on v1 of main display"
            );

            let ext_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == EXT_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            assert_eq!(
                ext_strips,
                vec![(0, vec![win2])],
                "only window 2 should move to the new display"
            );
        })
        .run(commands);
}

#[test]
fn test_attach_display_compacts_emptied_first_virtual_workspace() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::DisplayAdded {
            display_id: EXT_DISPLAY_ID,
        },
    ];

    let mut harness = TestHarness::new().with_windows(4);
    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    harness
        .on_iteration(5, |_world, mut state| {
            // Attach EXT_DISPLAY_ID and move [0, 1] from v0 of TEST_WORKSPACE_ID to EXT_WORKSPACE_ID.
            state.add_display(
                EXT_DISPLAY_ID,
                IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
                vec![EXT_WORKSPACE_ID],
            );
            // macOS moves the windows onto the display whose space they join.
            for id in [0, 1] {
                state.update_window(id, |w| {
                    w.workspace_id = EXT_WORKSPACE_ID;
                    w.frame.min.y -= EXT_DISPLAY_HEIGHT;
                    w.frame.max.y -= EXT_DISPLAY_HEIGHT;
                });
            }
        })
        .on_iteration(6, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            let win2 = find_window_entity(2, world);
            let win3 = find_window_entity(3, world);

            let main_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == TEST_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            assert_eq!(
                main_strips,
                vec![(0, vec![win2, win3])],
                "emptied v0 on source display must be removed and v1 compacted to v0"
            );

            let ext_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == EXT_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            assert_eq!(ext_strips, vec![(0, vec![win0, win1])]);
        })
        .run(commands);
}

#[test]
fn test_remove_display_appends_strip_as_next_virtual_workspace() {
    let mut harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(2)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |_| {})
        .with_workspace_window(101, EXT_WORKSPACE_ID, |_| {});

    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        },
    ];

    harness
        .on_iteration(0, |_world, state| {
            // Unplug EXT_DISPLAY_ID; macOS moves windows 100 and 101 to TEST_WORKSPACE_ID.
            state.remove_display(EXT_DISPLAY_ID);
            state.update_window(100, |w| w.workspace_id = TEST_WORKSPACE_ID);
            state.update_window(101, |w| w.workspace_id = TEST_WORKSPACE_ID);
        })
        .on_iteration(1, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            let win100 = find_window_entity(100, world);
            let win101 = find_window_entity(101, world);

            let mut main_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == TEST_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            main_strips.sort_by_key(|(idx, _)| *idx);
            assert_eq!(
                main_strips,
                vec![(0, vec![win0, win1]), (1, vec![win100, win101])],
                "windows from removed display must become the next virtual workspace (v1) on surviving display, not merged into v0"
            );

            let leftover_ext = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == EXT_WORKSPACE_ID)
                .count();
            assert_eq!(
                leftover_ext, 0,
                "emptied orphan strip from removed display should be despawned"
            );
        })
        .run(commands);
}

#[test]
fn test_remove_display_populates_empty_first_workspace_on_surviving_display() {
    let mut harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |_| {})
        .with_workspace_window(101, EXT_WORKSPACE_ID, |_| {});

    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        },
    ];

    harness
        .on_iteration(0, |_world, state| {
            // TEST_WORKSPACE_ID has no windows (empty v0); EXT_DISPLAY_ID has [100, 101].
            state.remove_display(EXT_DISPLAY_ID);
            state.update_window(100, |w| w.workspace_id = TEST_WORKSPACE_ID);
            state.update_window(101, |w| w.workspace_id = TEST_WORKSPACE_ID);
        })
        .on_iteration(1, |world, _state| {
            let win100 = find_window_entity(100, world);
            let win101 = find_window_entity(101, world);

            let main_strips: Vec<(u32, Vec<Entity>)> = world
                .query::<&LayoutStrip>()
                .iter(world)
                .filter(|s| s.id() == TEST_WORKSPACE_ID)
                .map(|s| (s.virtual_index, s.all_windows()))
                .collect();
            assert_eq!(
                main_strips,
                vec![(0, vec![win100, win101])],
                "surviving display with empty v0 must have no empty first workspace after receiving strip from removed display"
            );
        })
        .run(commands);
}

#[test]
fn test_remove_focused_display_activates_migrated_strip() {
    assert_removed_display_follows_focus(true, 0);
}

#[test]
fn test_remove_focused_display_parks_selected_second_destination_strip() {
    assert_removed_display_follows_focus(true, 1);
}

#[test]
fn test_remove_inactive_display_keeps_migrated_strip_parked() {
    assert_removed_display_follows_focus(false, 0);
}

struct RemovedDisplayFixture {
    harness: TestHarness,
    destination_selected: Entity,
    destination_origin: IVec2,
    destination_frame: IRect,
}

fn removed_display_fixture(
    removed_display_focused: bool,
    destination_virtual_index: u32,
) -> RemovedDisplayFixture {
    use crate::ecs::ActiveWorkspaceMarker;

    let mut harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(2)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |_| {});
    let commands = vec![Event::MenuOpened { window_id: 0 }];
    harness = harness.on_iteration(0, |world, _state| {
        assert_focused!(world, 0);
    });
    harness.run(commands);
    if destination_virtual_index == 1 {
        let commands = vec![window_command(Operation::VirtualMoveNumber(
            1,
            MoveFocus::Follow,
        ))];
        harness = harness.on_iteration(0, |world, _state| {
            assert_focused!(world, 0);
            let mut strips = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
            assert_eq!(strips.single(world).unwrap().virtual_index, 1);
        });
        harness.run(commands);
    }
    let destination_selected = harness
        .world()
        .query_filtered::<Entity, (With<LayoutStrip>, With<ActiveWorkspaceMarker>)>()
        .single(harness.world())
        .unwrap();
    let destination_display = harness
        .world()
        .get::<ChildOf>(destination_selected)
        .unwrap()
        .parent();
    let destination_origin = harness
        .world()
        .get::<Display>(destination_display)
        .unwrap()
        .bounds()
        .min;
    let destination_frame = harness.mock_state.create_window(0).frame();

    if removed_display_focused {
        harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
        harness.mock_state.focus_window(100);
        let commands = vec![Event::WindowFocused { window_id: 100 }];
        harness = harness.on_iteration(0, |world, _state| {
            assert_focused!(world, 100);
        });
        harness.run(commands);
    }

    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness
        .mock_state
        .update_window(100, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness
        .app
        .world_mut()
        .write_message(Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        });

    RemovedDisplayFixture {
        harness,
        destination_selected,
        destination_origin,
        destination_frame,
    }
}

fn sample_removed_display_frames(harness: &mut TestHarness) -> Vec<(u32, IRect, IRect, Entity)> {
    let mut samples = Vec::new();
    for tick in 1..=175 {
        harness.advance(Duration::from_millis(20));
        let migrated_window = find_window_entity(100, harness.world());
        let migrated_strip = harness
            .world()
            .query::<(Entity, &LayoutStrip)>()
            .iter(harness.world())
            .find_map(|(entity, strip)| {
                (strip.id() == TEST_WORKSPACE_ID && strip.contains(migrated_window))
                    .then_some(entity)
            });
        if let Some(migrated_strip) = migrated_strip {
            samples.push((
                tick,
                harness.mock_state.create_window(0).frame(),
                harness.mock_state.create_window(100).frame(),
                migrated_strip,
            ));
        }
    }
    samples
}

fn assert_removed_display_follows_focus(
    removed_display_focused: bool,
    destination_virtual_index: u32,
) {
    use crate::ecs::workspace::PreviousStripPosition;
    use crate::ecs::{ActiveWorkspaceMarker, FocusedMarker, SelectedVirtualMarker};

    let RemovedDisplayFixture {
        mut harness,
        destination_selected,
        destination_origin,
        destination_frame,
    } = removed_display_fixture(removed_display_focused, destination_virtual_index);
    let samples = sample_removed_display_frames(&mut harness);
    assert!(
        !samples.is_empty(),
        "the removed display's window must migrate"
    );
    let (_, _, _, migrated_strip) = *samples.last().unwrap();
    let migrated_window = find_window_entity(100, harness.world());
    let parked_origin = IVec2::new(
        TEST_DISPLAY_WIDTH - PARKED_STRIP_SLIVER,
        TEST_DISPLAY_HEIGHT - PARKED_STRIP_SLIVER,
    );
    for (tick, destination_frame, migrated_frame, _) in &samples {
        let parked_frame = if removed_display_focused {
            destination_frame
        } else {
            migrated_frame
        };
        assert_eq!(
            parked_frame.min, parked_origin,
            "only the selected row may be visible after migration at {tick} frames; destination: {destination_frame:?}, migrated: {migrated_frame:?}"
        );
    }

    let (visible_strip, hidden_strip) = if removed_display_focused {
        (migrated_strip, destination_selected)
    } else {
        (destination_selected, migrated_strip)
    };
    let world = harness.world();
    assert!(world.get::<SelectedVirtualMarker>(visible_strip).is_some());
    assert!(world.get::<ActiveWorkspaceMarker>(visible_strip).is_some());
    assert!(world.get::<PreviousStripPosition>(visible_strip).is_none());
    assert!(world.get::<SelectedVirtualMarker>(hidden_strip).is_none());
    assert!(world.get::<ActiveWorkspaceMarker>(hidden_strip).is_none());
    let previous = world.get::<PreviousStripPosition>(hidden_strip).unwrap();
    assert_eq!(previous.origin, destination_origin);
    if removed_display_focused {
        assert_focused!(world, 100);
        assert!(
            world
                .get::<LayoutStrip>(visible_strip)
                .unwrap()
                .contains(migrated_window)
        );
        assert_eq!(
            harness.mock_state.create_window(100).frame().min,
            destination_origin
        );
        let commands = vec![window_command(Operation::VirtualNumber(
            destination_virtual_index,
        ))];
        harness = harness.on_iteration(0, move |world, state| {
            let focused = world
                .query_filtered::<Entity, With<FocusedMarker>>()
                .single(world)
                .unwrap();
            assert!(
                world
                    .get::<LayoutStrip>(destination_selected)
                    .unwrap()
                    .contains(focused),
                "switching back must focus a window in the destination's restored row"
            );
            assert_eq!(state.create_window(0).frame(), destination_frame);
            assert_eq!(state.create_window(100).frame().min, parked_origin);
            assert!(
                world
                    .get::<SelectedVirtualMarker>(destination_selected)
                    .is_some()
            );
            assert!(
                world
                    .get::<ActiveWorkspaceMarker>(destination_selected)
                    .is_some()
            );
            assert!(world.get::<PreviousStripPosition>(migrated_strip).is_some());
        });
        harness.run(commands);
    } else {
        assert_eq!(previous.focus, Some(migrated_window));
        assert_eq!(
            harness.mock_state.create_window(0).frame(),
            destination_frame
        );
    }
}

/// How much of window `id` shows on the built-in display, in points across.
fn shown_width(world: &mut World, id: WinID) -> i32 {
    let display = IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .map_or(0, |window| window.frame().intersect(display).width())
}

/// Closing every window shown on a display whose strip overflows, while
/// focus is on another display, must not leave that display empty with its
/// other windows parked off screen (#73).
#[test]
fn test_closing_the_shown_windows_of_an_unfocused_display_shows_another() {
    let closed = Rc::new(Cell::new(Vec::new()));
    let recorded = closed.clone();
    harness_with_display_above()
        .with_windows(5)
        .on_iteration(4, move |world, state| {
            assert_focused!(world, 100);
            let shown = (0..5)
                .filter(|&id| shown_width(world, id) > PARKED_STRIP_SLIVER)
                .collect::<Vec<_>>();
            assert!(
                !shown.is_empty() && shown.len() < 5,
                "the strip overflows: {shown:?}"
            );
            for &id in &shown {
                state.os_close_window(id);
            }
            recorded.set(shown);
        })
        .on_iteration(6, move |world, _state| {
            let closed = closed.take();
            assert!(!closed.is_empty(), "the shown windows were closed");
            assert_focused!(world, 100);
            let left = (0..5).filter(|id| !closed.contains(id)).collect::<Vec<_>>();
            let widths = left
                .iter()
                .map(|&id| (id, shown_width(world, id)))
                .collect::<Vec<_>>();
            assert!(
                widths.iter().any(|&(_, width)| width > PARKED_STRIP_SLIVER),
                "a remaining window shows on the display: {widths:?}"
            );
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::Last)),
            window_command(Operation::FocusDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// The same closes on a display that has since switched to another native
/// Space leave that hidden Space's strip where it was.
#[test]
fn test_closing_windows_of_a_hidden_space_does_not_scroll_its_strip() {
    let before = Rc::new(Cell::new(Vec::new()));
    let recorded = before.clone();
    harness_with_display_above()
        .with_windows(5)
        .on_iteration(4, move |world, state| {
            assert_focused!(world, 100);
            let shown = (0..5)
                .filter(|&id| shown_width(world, id) > PARKED_STRIP_SLIVER)
                .collect::<Vec<_>>();
            assert!(!shown.is_empty() && shown.len() < 5, "{shown:?}");
            state.show_space(TEST_DISPLAY_ID, TEST_WORKSPACE_ID + 1000);
            for &id in &shown {
                state.os_close_window(id);
            }
            recorded.set(
                (0..5)
                    .filter(|id| !shown.contains(id))
                    .map(|id| (id, window_x(world, id)))
                    .collect(),
            );
        })
        .on_iteration(6, move |world, _state| {
            let before = before.take();
            assert!(!before.is_empty(), "the shown windows were closed");
            let after = before
                .iter()
                .map(|&(id, _)| (id, window_x(world, id)))
                .collect::<Vec<_>>();
            assert_eq!(after, before, "the hidden Space's strip stayed put");
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            window_command(Operation::Focus(Direction::Last)),
            window_command(Operation::FocusDisplay(Direction::North)),
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

fn window_x(world: &mut World, id: WinID) -> i32 {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .map_or(i32::MIN, |window| window.frame().min.x)
}

/// Moves window `id` from the display it is on to the display in `direction`
/// and lets the arrival settle.
fn move_and_settle(harness: &mut TestHarness, id: WinID, direction: Direction) -> IRect {
    let (from, to, workspace) = match direction {
        Direction::South => (EXT_DISPLAY_ID, TEST_DISPLAY_ID, TEST_WORKSPACE_ID),
        _ => (TEST_DISPLAY_ID, EXT_DISPLAY_ID, EXT_WORKSPACE_ID),
    };
    harness.mock_state.os_activate_display(from);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    harness.run(vec![window_command(Operation::MoveToDisplay(direction))]);
    harness
        .mock_state
        .update_window(id, |window| window.workspace_id = workspace);
    harness.mock_state.os_activate_display(to);
    harness.advance(Duration::from_secs(3));
    harness.mock_state.create_window(id).frame()
}

/// A window that fills its display, as macOS Fill or Zoom leaves it, is wider
/// than the tiled viewport. Moved to another display, it fills that display's
/// viewport instead of overflowing it, in both directions.
#[test]
fn test_move_to_display_fits_a_filled_window_to_the_destination() {
    let mut harness = display_round_trip_harness(MainOptions::default(), 0, 890);
    harness
        .mock_state
        .update_window(0, |window| window.frame = IRect::new(0, 20, 2560, 1440));
    harness.run(vec![
        Event::MenuOpened { window_id: 1 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::FocusDisplay(Direction::North)),
    ]);
    let frame = move_and_settle(&mut harness, 0, Direction::South);
    assert_eq!(frame, IRect::new(4, 1460, 2044, 2772));
    assert_focused!(harness.world(), 0);

    harness
        .mock_state
        .update_window(0, |window| window.frame = IRect::new(0, 1460, 2048, 2772));
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 0 });
    harness.advance(Duration::from_secs(1));
    let frame = move_and_settle(&mut harness, 0, Direction::North);
    assert_eq!((frame.min.x, frame.max.x), (4, 2556));
    assert_focused!(harness.world(), 0);
}

/// Make column full width already fits the viewport, so it moves unchanged.
#[test]
fn test_move_to_display_keeps_a_full_width_column_full_width() {
    let mut harness = display_round_trip_harness(MainOptions::default(), 0, 890);
    harness.run(vec![
        Event::MenuOpened { window_id: 1 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::FocusDisplay(Direction::North)),
    ]);
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
    harness.run(vec![
        Event::Command {
            command: Command::PrintState,
        },
        window_command(Operation::FullWidth),
    ]);
    harness.advance(Duration::from_secs(1));
    assert_eq!(harness.mock_state.create_window(0).frame().width(), 2552);
    let frame = move_and_settle(&mut harness, 0, Direction::South);
    assert_eq!(frame, IRect::new(4, 1460, 2044, 2772));
}

/// A window whose minimum width exceeds the destination stays at its minimum
/// and overflows only where the strip can pan to it.
#[test]
fn test_move_to_display_leaves_a_window_too_wide_to_fit_reachable() {
    let mut harness = display_round_trip_harness(MainOptions::default(), 0, 890);
    harness.mock_state.update_window(0, |window| {
        window.frame = IRect::new(0, 20, 2560, 1440);
        window.minimum_size = Some(Size::new(2300, 400));
    });
    harness.run(vec![
        Event::MenuOpened { window_id: 1 },
        window_command(Operation::Focus(Direction::First)),
        window_command(Operation::FocusDisplay(Direction::North)),
    ]);
    let frame = move_and_settle(&mut harness, 0, Direction::South);
    assert_eq!(frame.width(), 2300);
    assert!(frame.min.x <= 4 && frame.max.x >= 2044, "{frame:?}");
    assert_focused!(harness.world(), 0);
}

/// A window that refuses the narrower width its move to a smaller display asks
/// for keeps its own width, and the strip still shows all of it.
#[test]
fn test_move_to_display_keeps_a_window_that_refused_its_width_visible() {
    let viewport = 4..=2044;
    for (fixed, width) in [(false, 1100), (true, 600)] {
        let mut harness = display_round_trip_harness(MainOptions::default(), 0, 890);
        harness.mock_state.update_window(0, |window| {
            window.frame = IRect::new(996, 20, 996 + width, 1440);
            window.minimum_size = Some(Size::new(width, 0));
        });
        harness.run(vec![
            Event::MenuOpened { window_id: 1 },
            window_command(Operation::Focus(Direction::First)),
            window_command(Operation::FocusDisplay(Direction::North)),
        ]);
        // Set once tiled, as a window the user tiled although it cannot be
        // resized: such a window floats when it appears.
        if fixed {
            harness.mock_state.update_window(0, |window| {
                window.fixed_size = true;
                window.maximum_size = Some(Size::new(width, i32::MAX));
            });
        }
        let frame = move_and_settle(&mut harness, 0, Direction::South);
        assert_eq!(frame.width(), width, "fixed: {fixed}");
        assert!(
            viewport.contains(&frame.min.x) && viewport.contains(&frame.max.x),
            "fixed: {fixed}, the focused window {frame:?} must lie inside the viewport {viewport:?}"
        );
        assert_focused!(harness.world(), 0);
    }
}
