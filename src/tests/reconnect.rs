//! Unplugging and replugging a display, as macOS 27 does it with separate
//! Spaces per display and the external display as the main display.
//!
//! On unplug macOS moves the external display's Spaces to the built-in
//! display. On replug it hands the external display every Space that existed
//! before, the built-in display's Space included, and gives the built-in
//! display a new empty Space. Each window must still end up on the display it
//! was on before the unplug, in its old column and stack.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::Side;
use crate::ecs::SelectedVirtualMarker;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::{Display, Window};
use crate::platform::{WinID, WorkspaceId};

use super::*;

const EXT: u32 = 3;
const BUILTIN: u32 = TEST_DISPLAY_ID;
const EXT_SPACE: WorkspaceId = 515;
const BUILTIN_SPACE: WorkspaceId = 1214;
const BUILTIN_EMPTY_SPACE: WorkspaceId = 1249;
const NEW_BUILTIN_SPACE: WorkspaceId = 1382;
const SETTLE: Duration = Duration::from_secs(5);
const WIDTH: i32 = 600;
const HEIGHT: i32 = 900;

fn ext_bounds() -> IRect {
    IRect::new(0, 0, 2560, 1440)
}

fn builtin_bounds() -> IRect {
    IRect::new(302, 1440, 2350, 2772)
}

/// The built-in display alone, which macOS makes the main display at the origin.
fn builtin_alone_bounds() -> IRect {
    IRect::new(0, 0, 2048, 1332)
}

fn spawn_row(harness: &TestHarness, space: WorkspaceId, ids: &[WinID], bounds: IRect) {
    for (i, &id) in (0..).zip(ids) {
        let x = bounds.min.x + i * WIDTH;
        let y = bounds.min.y + TEST_MENUBAR_HEIGHT;
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            space,
            id,
            IRect::new(x, y, x + WIDTH, y + HEIGHT),
        );
    }
}

fn stack_onto_left(harness: &mut TestHarness, id: WinID) {
    let entity = find_window_entity(id, harness.world());
    let world = harness.world();
    let mut strips = world.query::<&mut LayoutStrip>();
    let mut strip = strips
        .iter_mut(world)
        .find(|strip| strip.contains(entity))
        .expect("a strip holding the window");
    strip
        .stack(entity, Side::West)
        .expect("stacking the window");
}

/// Moves windows by the offset between two display origins, as macOS does when
/// it moves their Space to another display.
fn carry(harness: &TestHarness, ids: &[WinID], from: IRect, to: IRect) {
    let shift = to.min - from.min;
    for &id in ids {
        harness.mock_state.update_window(id, |window| {
            window.frame.min += shift;
            window.frame.max += shift;
        });
    }
}

/// The columns of `space`'s shown strip as window IDs, and the display that
/// strip is on.
fn layout_on(world: &mut World, space: WorkspaceId) -> (u32, Vec<Vec<WinID>>) {
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    let displays = world
        .query::<(Entity, &Display)>()
        .iter(world)
        .map(|(entity, display)| (entity, display.id()))
        .collect::<std::collections::HashMap<_, _>>();
    let (strip, parent) = world
        .query_filtered::<(&LayoutStrip, &ChildOf), With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|(strip, _)| strip.id() == space)
        .unwrap_or_else(|| panic!("no shown strip for Space {space}"));
    let columns = strip
        .columns()
        .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
        .collect();
    (displays[&parent.parent()], columns)
}

/// The built-in display shows `[10] [11 12]` in its current Space, beside an
/// empty Space. The external main display shows `[50] [51 52] [53]`.
fn two_displays() -> TestHarness {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        BUILTIN,
        builtin_bounds(),
        vec![BUILTIN_SPACE, BUILTIN_EMPTY_SPACE],
    );
    harness
        .mock_state
        .add_display(EXT, ext_bounds(), vec![EXT_SPACE]);
    harness.mock_state.activate_display(EXT);
    harness.mock_state.set_spaces_follow_frames(true);
    spawn_row(&harness, BUILTIN_SPACE, &[10, 11, 12], builtin_bounds());
    spawn_row(&harness, EXT_SPACE, &[50, 51, 52, 53], ext_bounds());
    harness.advance(Duration::from_secs(2));
    stack_onto_left(&mut harness, 12);
    stack_onto_left(&mut harness, 52);
    harness.advance(Duration::from_secs(2));

    assert_eq!(
        layout_on(harness.world(), BUILTIN_SPACE),
        (BUILTIN, vec![vec![10], vec![11, 12]])
    );
    assert_eq!(
        layout_on(harness.world(), EXT_SPACE),
        (EXT, vec![vec![50], vec![51, 52], vec![53]])
    );
    harness
}

/// What macOS does with the built-in display's Space when the external main
/// display goes away.
#[derive(Clone, Copy)]
enum BuiltinSpace {
    /// It keeps the Space beside the external display's, as seen on 28 October
    /// with a software unplug.
    Survives,
    /// It destroys the Space and moves its windows into the external display's
    /// Space, as seen on Sam's Mac with a cable unplug.
    Destroyed,
}

/// macOS starts reconfiguring, then moves the external display's Space to the
/// built-in display and makes it current there, and destroys the built-in
/// display's empty Space.
fn unplug_external(harness: &mut TestHarness, builtin_space: BuiltinSpace) {
    harness
        .world()
        .write_message(Event::DisplayConfigured { display_id: EXT });
    harness.advance(Duration::from_millis(20));
    harness.mock_state.remove_display(EXT);
    carry(
        harness,
        &[50, 51, 52, 53],
        ext_bounds(),
        builtin_alone_bounds(),
    );
    carry(
        harness,
        &[10, 11, 12],
        builtin_bounds(),
        builtin_alone_bounds(),
    );
    let spaces = match builtin_space {
        BuiltinSpace::Survives => vec![EXT_SPACE, BUILTIN_SPACE],
        BuiltinSpace::Destroyed => {
            for id in [10, 11, 12] {
                harness
                    .mock_state
                    .update_window(id, |window| window.workspace_id = EXT_SPACE);
            }
            harness.world().write_message(Event::SpaceDestroyed {
                space_id: BUILTIN_SPACE,
            });
            vec![EXT_SPACE]
        }
    };
    harness
        .mock_state
        .add_display(BUILTIN, builtin_alone_bounds(), spaces);
    harness.world().write_message(Event::SpaceDestroyed {
        space_id: BUILTIN_EMPTY_SPACE,
    });
    harness
        .world()
        .write_message(Event::DisplayRemoved { display_id: EXT });
    harness.advance(SETTLE);
}

/// macOS starts reconfiguring, then hands the external display every Space
/// that survived, with its windows, and gives the built-in display a new empty
/// Space.
fn replug_external(harness: &mut TestHarness, builtin_space: BuiltinSpace, carried: &[WinID]) {
    harness
        .world()
        .write_message(Event::DisplayConfigured { display_id: EXT });
    harness.advance(Duration::from_millis(20));
    let spaces = match builtin_space {
        BuiltinSpace::Survives => vec![EXT_SPACE, BUILTIN_SPACE],
        BuiltinSpace::Destroyed => vec![EXT_SPACE],
    };
    harness.mock_state.add_display(EXT, ext_bounds(), spaces);
    harness
        .mock_state
        .add_display(BUILTIN, builtin_bounds(), vec![NEW_BUILTIN_SPACE]);
    carry(harness, carried, builtin_alone_bounds(), ext_bounds());
    harness.world().write_message(Event::SpaceCreated {
        space_id: NEW_BUILTIN_SPACE,
    });
    harness
        .world()
        .write_message(Event::DisplayAdded { display_id: EXT });
    harness.advance(Duration::from_millis(100));
    harness.mock_state.os_activate_display(EXT);
    harness.advance(SETTLE);
}

fn assert_back_home(harness: &mut TestHarness, builtin: &[Vec<WinID>], ext: &[Vec<WinID>]) {
    assert_eq!(
        layout_on(harness.world(), NEW_BUILTIN_SPACE),
        (BUILTIN, builtin.to_vec())
    );
    assert_eq!(layout_on(harness.world(), EXT_SPACE), (EXT, ext.to_vec()));
    for id in builtin.iter().flatten() {
        assert_eq!(
            harness.mock_state.space_of(*id),
            NEW_BUILTIN_SPACE,
            "window {id}"
        );
    }
    for id in ext.iter().flatten() {
        assert_eq!(harness.mock_state.space_of(*id), EXT_SPACE, "window {id}");
    }
}

#[test]
fn replugging_the_external_display_returns_each_window_to_its_display() {
    let mut harness = two_displays();

    unplug_external(&mut harness, BuiltinSpace::Survives);
    replug_external(
        &mut harness,
        BuiltinSpace::Survives,
        &[10, 11, 12, 50, 51, 52, 53],
    );

    assert_back_home(
        &mut harness,
        &[vec![10], vec![11, 12]],
        &[vec![50], vec![51, 52], vec![53]],
    );
}

#[test]
fn windows_of_a_destroyed_builtin_space_return_with_their_stack() {
    let mut harness = two_displays();

    unplug_external(&mut harness, BuiltinSpace::Destroyed);
    replug_external(
        &mut harness,
        BuiltinSpace::Destroyed,
        &[10, 11, 12, 50, 51, 52, 53],
    );

    assert_back_home(
        &mut harness,
        &[vec![10], vec![11, 12]],
        &[vec![50], vec![51, 52], vec![53]],
    );
}

#[test]
fn a_window_opened_while_unplugged_stays_on_the_builtin_display() {
    let mut harness = two_displays();
    unplug_external(&mut harness, BuiltinSpace::Survives);

    let origin = builtin_alone_bounds().min + IVec2::new(100, TEST_MENUBAR_HEIGHT);
    let window = harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_SPACE,
        60,
        IRect::from_corners(origin, origin + IVec2::new(WIDTH, HEIGHT)),
    );
    harness
        .world()
        .trigger(crate::ecs::SpawnWindowTrigger(vec![window]));
    harness.mock_state.os_close_window(53);
    harness.advance(SETTLE);
    replug_external(
        &mut harness,
        BuiltinSpace::Survives,
        &[10, 11, 12, 50, 51, 52, 60],
    );

    assert_back_home(
        &mut harness,
        &[vec![10], vec![11, 12], vec![60]],
        &[vec![50], vec![51, 52]],
    );
}
