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

fn swap_columns(harness: &mut TestHarness, space: WorkspaceId, left: usize, right: usize) {
    let world = harness.world();
    let mut strips = world.query_filtered::<&mut LayoutStrip, With<SelectedVirtualMarker>>();
    strips
        .iter_mut(world)
        .find(|strip| strip.id() == space)
        .expect("the shown strip of the Space")
        .swap(left, right);
}

fn widths(harness: &TestHarness, ids: &[WinID]) -> Vec<i32> {
    ids.iter()
        .map(|id| harness.mock_state.ax_frame(*id).width())
        .collect()
}

/// A strip as its Space, virtual row, display, and columns of window IDs.
type StripLayout = (WorkspaceId, u32, Option<u32>, Vec<Vec<WinID>>);

fn every_strip(world: &mut World) -> Vec<StripLayout> {
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
    let mut strips = world
        .query::<(&LayoutStrip, Option<&ChildOf>)>()
        .iter(world)
        .map(|(strip, parent)| {
            let columns = strip
                .columns()
                .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
                .collect();
            let display = parent.and_then(|parent| displays.get(&parent.parent()).copied());
            (strip.id(), strip.virtual_index, display, columns)
        })
        .collect::<Vec<_>>();
    strips.sort();
    strips
}

#[test]
fn three_unplug_and_replug_cycles_leave_the_same_layout_and_widths() {
    const ALL: [WinID; 7] = [10, 11, 12, 50, 51, 52, 53];
    let mut harness = two_displays();
    let before = widths(&harness, &ALL);
    let mut builtin_space = BUILTIN_SPACE;
    // The Spaces macOS keeps handing the external display after each replug.
    let mut ext_spaces = vec![EXT_SPACE];

    for new_space in [
        NEW_BUILTIN_SPACE,
        NEW_BUILTIN_SPACE + 1,
        NEW_BUILTIN_SPACE + 2,
    ] {
        ext_spaces.push(builtin_space);
        harness
            .world()
            .write_message(Event::DisplayConfigured { display_id: EXT });
        harness.advance(Duration::from_millis(20));
        harness.mock_state.remove_display(EXT);
        carry(&harness, &ALL[3..], ext_bounds(), builtin_alone_bounds());
        carry(
            &harness,
            &ALL[..3],
            builtin_bounds(),
            builtin_alone_bounds(),
        );
        harness
            .mock_state
            .add_display(BUILTIN, builtin_alone_bounds(), ext_spaces.clone());
        harness.world().write_message(Event::SpaceDestroyed {
            space_id: BUILTIN_EMPTY_SPACE,
        });
        harness
            .world()
            .write_message(Event::DisplayRemoved { display_id: EXT });
        harness.advance(SETTLE);

        harness
            .world()
            .write_message(Event::DisplayConfigured { display_id: EXT });
        harness.advance(Duration::from_millis(20));
        harness
            .mock_state
            .add_display(EXT, ext_bounds(), ext_spaces.clone());
        harness
            .mock_state
            .add_display(BUILTIN, builtin_bounds(), vec![new_space]);
        carry(&harness, &ALL, builtin_alone_bounds(), ext_bounds());
        harness.world().write_message(Event::SpaceCreated {
            space_id: new_space,
        });
        harness
            .world()
            .write_message(Event::DisplayAdded { display_id: EXT });
        harness.advance(Duration::from_millis(100));
        harness.mock_state.os_activate_display(EXT);
        harness.advance(SETTLE);
        builtin_space = new_space;

        assert_eq!(
            layout_on(harness.world(), builtin_space),
            (BUILTIN, vec![vec![10], vec![11, 12]]),
            "cycle into Space {new_space}"
        );
        assert_eq!(
            layout_on(harness.world(), EXT_SPACE),
            (EXT, vec![vec![50], vec![51, 52], vec![53]]),
            "cycle into Space {new_space}"
        );
        for id in &ALL[..3] {
            assert_eq!(
                harness.mock_state.space_of(*id),
                builtin_space,
                "window {id}"
            );
        }
        for (id, (now, was)) in ALL.iter().zip(widths(&harness, &ALL).iter().zip(&before)) {
            assert!(
                (now - was).abs() <= 2,
                "window {id} is {now} wide, was {was}, after the cycle into Space {new_space}"
            );
        }
    }
}

#[test]
fn a_reorder_while_unplugged_is_dropped_for_windows_that_return() {
    let mut harness = two_displays();
    unplug_external(&mut harness, BuiltinSpace::Survives);
    swap_columns(&mut harness, EXT_SPACE, 0, 2);
    harness.advance(Duration::from_secs(1));
    assert_eq!(
        layout_on(harness.world(), EXT_SPACE),
        (BUILTIN, vec![vec![53], vec![51, 52], vec![50]])
    );

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

/// This event order is modeled on the unplug, not observed: macOS moves the
/// closed built-in display's Space to the external display, and on opening
/// gives the built-in display a new empty Space.
#[test]
fn closing_and_opening_the_lid_returns_the_builtin_windows_and_keeps_the_external_reorder() {
    let mut harness = two_displays();
    harness.world().write_message(Event::DisplayConfigured {
        display_id: BUILTIN,
    });
    harness.advance(Duration::from_millis(20));
    harness.mock_state.remove_display(BUILTIN);
    harness
        .mock_state
        .add_display(EXT, ext_bounds(), vec![EXT_SPACE, BUILTIN_SPACE]);
    carry(&harness, &[10, 11, 12], builtin_bounds(), ext_bounds());
    harness.world().write_message(Event::SpaceDestroyed {
        space_id: BUILTIN_EMPTY_SPACE,
    });
    harness.world().write_message(Event::DisplayRemoved {
        display_id: BUILTIN,
    });
    harness.advance(SETTLE);

    swap_columns(&mut harness, EXT_SPACE, 0, 2);
    harness.advance(Duration::from_secs(1));

    harness.world().write_message(Event::DisplayConfigured {
        display_id: BUILTIN,
    });
    harness.advance(Duration::from_millis(20));
    harness
        .mock_state
        .add_display(BUILTIN, builtin_bounds(), vec![NEW_BUILTIN_SPACE]);
    harness.world().write_message(Event::SpaceCreated {
        space_id: NEW_BUILTIN_SPACE,
    });
    harness.world().write_message(Event::DisplayAdded {
        display_id: BUILTIN,
    });
    harness.advance(SETTLE);

    assert_back_home(
        &mut harness,
        &[vec![10], vec![11, 12]],
        &[vec![53], vec![51, 52], vec![50]],
    );
}

/// As seen on 28 October with the built-in display turned off in software:
/// macOS destroys the built-in display's Space and moves its windows into the
/// external display's Space, where a native read admits them one by one.
#[test]
fn closing_the_lid_returns_the_windows_of_its_destroyed_space_with_their_stack() {
    let mut harness = two_displays();
    harness.world().write_message(Event::DisplayConfigured {
        display_id: BUILTIN,
    });
    harness.advance(Duration::from_millis(20));
    harness.mock_state.remove_display(BUILTIN);
    harness
        .mock_state
        .add_display(EXT, ext_bounds(), vec![EXT_SPACE]);
    carry(&harness, &[10, 11, 12], builtin_bounds(), ext_bounds());
    for id in [10, 11, 12] {
        harness
            .mock_state
            .update_window(id, |window| window.workspace_id = EXT_SPACE);
    }
    for space_id in [BUILTIN_SPACE, BUILTIN_EMPTY_SPACE] {
        harness
            .world()
            .write_message(Event::SpaceDestroyed { space_id });
    }
    harness.world().write_message(Event::DisplayRemoved {
        display_id: BUILTIN,
    });
    harness.advance(SETTLE);

    harness.world().write_message(Event::DisplayConfigured {
        display_id: BUILTIN,
    });
    harness.advance(Duration::from_millis(20));
    harness
        .mock_state
        .add_display(BUILTIN, builtin_bounds(), vec![NEW_BUILTIN_SPACE]);
    harness.world().write_message(Event::SpaceCreated {
        space_id: NEW_BUILTIN_SPACE,
    });
    harness.world().write_message(Event::DisplayAdded {
        display_id: BUILTIN,
    });
    harness.advance(SETTLE);

    assert_back_home(
        &mut harness,
        &[vec![10], vec![11, 12]],
        &[vec![50], vec![51, 52], vec![53]],
    );
}

/// The lid closes: macOS removes the built-in display, destroys its Spaces,
/// and lists their windows in the external display's Space over one read per
/// slice of `reads`. The first slice moves before the Spaces are destroyed
/// when `destroy_after_first_read` is set.
fn close_lid_listing(
    harness: &mut TestHarness,
    reads: &[&[WinID]],
    destroy_after_first_read: bool,
) {
    harness.world().write_message(Event::DisplayConfigured {
        display_id: BUILTIN,
    });
    harness.advance(Duration::from_millis(20));
    harness.mock_state.remove_display(BUILTIN);
    harness
        .mock_state
        .add_display(EXT, ext_bounds(), vec![EXT_SPACE]);
    carry(harness, &[10, 11, 12], builtin_bounds(), ext_bounds());
    let destroy = |harness: &mut TestHarness| {
        for space_id in [BUILTIN_SPACE, BUILTIN_EMPTY_SPACE] {
            harness
                .world()
                .write_message(Event::SpaceDestroyed { space_id });
        }
        harness.world().write_message(Event::DisplayRemoved {
            display_id: BUILTIN,
        });
    };
    if !destroy_after_first_read {
        destroy(harness);
    }
    for (index, read) in reads.iter().enumerate() {
        for &id in *read {
            harness
                .mock_state
                .update_window(id, |window| window.workspace_id = EXT_SPACE);
        }
        if index == 0 && destroy_after_first_read {
            harness.advance(Duration::from_millis(600));
            destroy(harness);
        }
        harness.advance(Duration::from_millis(600));
    }
    harness.advance(SETTLE);
}

fn assert_lid_closed_layout(harness: &mut TestHarness) {
    assert_eq!(
        layout_on(harness.world(), EXT_SPACE),
        (
            EXT,
            vec![vec![50], vec![51, 52], vec![53], vec![10], vec![11, 12]]
        )
    );
}

#[test]
fn closing_the_lid_keeps_its_stack_on_the_external_display() {
    let mut harness = two_displays();
    close_lid_listing(&mut harness, &[&[10, 11, 12]], false);
    assert_lid_closed_layout(&mut harness);
}

#[test]
fn a_stack_macos_lists_top_window_first_stays_one_stack() {
    let mut harness = two_displays();
    close_lid_listing(&mut harness, &[&[10, 11], &[12]], false);
    assert_lid_closed_layout(&mut harness);
}

#[test]
fn a_stack_macos_lists_bottom_window_first_stays_one_stack() {
    let mut harness = two_displays();
    close_lid_listing(&mut harness, &[&[10, 12], &[11]], false);
    assert_lid_closed_layout(&mut harness);
}

#[test]
fn a_stack_split_by_a_read_before_its_space_is_destroyed_stays_one_stack() {
    let mut harness = two_displays();
    close_lid_listing(&mut harness, &[&[10, 11], &[12]], true);
    assert_lid_closed_layout(&mut harness);
}

#[test]
fn a_three_window_stack_split_before_its_space_is_destroyed_stays_one_stack() {
    let mut harness = two_displays();
    let entity = find_window_entity(10, harness.world());
    let world = harness.world();
    let mut strips = world.query::<&mut LayoutStrip>();
    strips
        .iter_mut(world)
        .find(|strip| strip.contains(entity))
        .expect("a strip holding the window")
        .stack(entity, Side::East)
        .expect("stacking the window");
    harness.advance(Duration::from_secs(2));
    let (_, columns) = layout_on(harness.world(), BUILTIN_SPACE);
    let [stack] = &columns[..] else {
        panic!("one stack on the built-in display, got {columns:?}");
    };
    let stack = stack.clone();

    close_lid_listing(&mut harness, &[&stack[..1], &stack[1..]], true);

    assert_eq!(
        layout_on(harness.world(), EXT_SPACE),
        (EXT, vec![vec![50], vec![51, 52], vec![53], stack])
    );
}

#[test]
fn a_burst_that_moves_no_space_changes_no_strip() {
    let mut harness = two_displays();
    let before = every_strip(harness.world());

    harness
        .world()
        .write_message(Event::DisplayConfigured { display_id: EXT });
    harness.advance(SETTLE);

    assert_eq!(every_strip(harness.world()), before);
}

/// The built-in display below the external display, each display with more
/// columns than it shows, so columns park past both side edges of each.
fn two_displays_with_parked_columns() -> TestHarness {
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
    spawn_row(
        &harness,
        BUILTIN_SPACE,
        &[10, 11, 12, 13, 14, 15],
        builtin_bounds(),
    );
    spawn_row(&harness, EXT_SPACE, &[50, 51, 52, 53, 54, 55], ext_bounds());
    harness.advance(Duration::from_secs(2));
    harness.mock_state.focus_window(12);
    harness.mock_state.focus_window(53);
    harness.advance(Duration::from_secs(2));
    harness
}

/// Moves the built-in display to `to`, carrying its windows, and files every
/// window in the current Space of the display that now holds its center, as
/// macOS does. Returns the windows macOS filed in another display's Space.
fn move_builtin(harness: &mut TestHarness, from: IRect, to: IRect) -> Vec<WinID> {
    let builtin = [10, 11, 12, 13, 14, 15];
    let ext = [50, 51, 52, 53, 54, 55];
    harness.world().write_message(Event::DisplayMoved {
        display_id: BUILTIN,
    });
    harness.advance(Duration::from_millis(20));
    harness.mock_state.set_display_bounds(BUILTIN, to);
    carry(harness, &builtin, from, to);
    let mut refiled = Vec::new();
    for id in builtin.into_iter().chain(ext) {
        let center = harness.mock_state.ax_frame(id).center();
        let space = if ext_bounds().contains(center) {
            EXT_SPACE
        } else if to.contains(center) {
            BUILTIN_SPACE
        } else {
            continue;
        };
        if harness.mock_state.space_of(id) != space {
            harness
                .mock_state
                .update_window(id, |window| window.workspace_id = space);
            refiled.push(id);
        }
    }
    harness.world().write_message(Event::DisplayMoved {
        display_id: BUILTIN,
    });
    harness.advance(SETTLE);
    refiled
}

#[test]
fn moving_a_display_beside_another_keeps_every_window_in_its_strip() {
    for beside in [
        IRect::new(-2048, 0, 0, 1332),
        IRect::new(2560, 0, 4608, 1332),
    ] {
        let mut harness = two_displays_with_parked_columns();
        let before = every_strip(harness.world());

        let refiled = move_builtin(&mut harness, builtin_bounds(), beside);
        assert!(
            !refiled.is_empty(),
            "macOS files a parked window in the other display's Space at {beside:?}"
        );
        assert_eq!(
            every_strip(harness.world()),
            before,
            "beside at {beside:?}, after macOS refiled {refiled:?}"
        );

        move_builtin(&mut harness, beside, builtin_bounds());
        assert_eq!(
            every_strip(harness.world()),
            before,
            "back below from {beside:?}"
        );
    }
}

/// After the built-in display moves beside the external display, one of its
/// shown windows moves itself back toward where it was, as System Settings
/// does after you click Done in Arrange. Its frame then overlaps the external
/// display by 44 pt with its center on no display, and macOS files it in the
/// external display's Space. With `notify`, the window's app reports the
/// move. Without it, the report is lost, as when it lands during an animation.
fn strand_beside(harness: &mut TestHarness, id: WinID, notify: bool) {
    harness.mock_state.update_window(id, |window| {
        let shift = IVec2::new(44 - window.frame.max.x, 0);
        window.frame.min += shift;
        window.frame.max += shift;
        window.workspace_id = EXT_SPACE;
    });
    if notify {
        harness
            .world()
            .write_message(Event::WindowMoved { window_id: id });
    }
    harness.advance(Duration::from_millis(500));
}

#[test]
fn a_window_its_app_strands_off_every_display_returns_to_its_strip() {
    let beside = IRect::new(2560, 0, 4608, 1332);
    for notify in [true, false] {
        let mut harness = two_displays_with_parked_columns();
        let before = every_strip(harness.world());
        move_builtin(&mut harness, builtin_bounds(), beside);
        assert_eq!(every_strip(harness.world()), before, "notify {notify}");

        strand_beside(&mut harness, 12, notify);
        if notify {
            assert!(
                beside.contains(harness.mock_state.ax_frame(12).center()),
                "the reported move is undone at once, not at {:?}",
                harness.mock_state.ax_frame(12)
            );
        }
        harness.world().write_message(Event::SpaceChanged);
        harness.advance(SETTLE);
        assert_eq!(
            every_strip(harness.world()),
            before,
            "notify {notify}: the stranded window keeps its strip"
        );
        assert!(
            beside.contains(harness.mock_state.ax_frame(12).center()),
            "notify {notify}: the stranded window is back on the built-in display at {:?}",
            harness.mock_state.ax_frame(12)
        );

        move_builtin(&mut harness, beside, builtin_bounds());
        assert_eq!(
            every_strip(harness.world()),
            before,
            "notify {notify}: back below"
        );
    }
}

/// macOS carries the built-in display's shown windows with it and reports
/// their moves before the engine has read the new arrangement. Each carried
/// window is on the moved display, so none goes back to where it was.
#[test]
fn a_window_macos_carries_with_its_display_stays_where_macos_put_it() {
    let beside = IRect::new(2560, 0, 4608, 1332);
    let mut harness = two_displays_with_parked_columns();
    let before = every_strip(harness.world());
    let shown: Vec<WinID> = [10, 11, 12, 13, 14, 15]
        .into_iter()
        .filter(|&id| builtin_bounds().contains(harness.mock_state.ax_frame(id).center()))
        .collect();
    assert!(!shown.is_empty(), "the built-in display shows a window");
    harness.world().write_message(Event::DisplayMoved {
        display_id: BUILTIN,
    });
    harness.mock_state.set_display_bounds(BUILTIN, beside);
    carry(&harness, &shown, builtin_bounds(), beside);
    for &id in &shown {
        harness
            .world()
            .write_message(Event::WindowMoved { window_id: id });
    }
    harness.advance(Duration::from_millis(100));
    for &id in &shown {
        assert!(
            beside.contains(harness.mock_state.ax_frame(id).center()),
            "window {id} stays on the moved display, not at {:?}",
            harness.mock_state.ax_frame(id)
        );
    }
    harness.world().write_message(Event::DisplayMoved {
        display_id: BUILTIN,
    });
    harness.advance(SETTLE);
    assert_eq!(every_strip(harness.world()), before);
}
