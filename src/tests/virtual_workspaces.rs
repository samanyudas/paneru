//! Each native Space has one strip, so the virtual workspace commands still
//! parse but do nothing (Miri #364).

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation, Side};
use crate::ecs::ActiveWorkspaceMarker;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::Origin;
use crate::platform::WorkspaceId;
use crate::types::argv::parse_command;

use super::*;

type Layout = (Vec<(WorkspaceId, u32, Vec<Entity>, bool)>, Vec<IRect>);

/// Every strip with its windows and whether it is active, then every window's
/// frame as macOS has it.
fn layout(harness: &mut TestHarness) -> Layout {
    let world = harness.world();
    let mut strips = world
        .query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>()
        .iter(world)
        .map(|(strip, active)| (strip.id(), strip.virtual_index, strip.all_windows(), active))
        .collect::<Vec<_>>();
    strips.sort();
    let frames = (0..3).map(|id| harness.mock_state.ax_frame(id)).collect();
    (strips, frames)
}

#[test]
fn virtual_workspace_commands_change_nothing() {
    let mut harness = TestHarness::new().with_windows(3);
    // Window 1 is under window 0 in a stack, so a `virtualfocus south` that
    // still focused stack neighbours would move focus.
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
    ]);
    harness.advance(Duration::from_secs(1));
    let before = layout(&mut harness);
    assert_eq!(before.0.len(), 1, "the test starts with one strip");
    assert_eq!(before.0[0].2.len(), 3);
    crate::assert_focused!(harness.world(), 0);

    for argv in [
        "window virtualnum 2",
        "window virtualnum 1",
        "window virtual south",
        "window virtual 2",
        "window virtualadd",
        "window virtualfocus south",
        "window virtualfocus north",
        "window virtualmove south",
        "window virtualmove 2",
        "window virtualmovenum 2",
        "window virtualsend south",
        "window virtualsend 2",
        "window virtualsendnum 2",
    ] {
        let words = argv.split(' ').collect::<Vec<_>>();
        let command = parse_command(&words).expect("a virtual workspace command still parses");
        harness.run(vec![Event::Command { command }]);
        harness.advance(Duration::from_secs(1));

        assert_eq!(layout(&mut harness), before, "`{argv}` must change nothing");
        crate::assert_focused!(harness.world(), 0);
    }
}

/// A second strip that appears for a Space joins the Space's strip, rather
/// than becoming a row parked off screen.
#[test]
fn a_second_strip_for_a_space_joins_the_first() {
    let mut harness = TestHarness::new().with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(Duration::from_secs(1));

    let world = harness.world();
    let windows = [0, 1, 2].map(|id| find_window_entity(id, world));
    let (display, mut first) = world
        .query::<(&ChildOf, &mut LayoutStrip)>()
        .iter_mut(world)
        .find(|(_, strip)| strip.id() == TEST_WORKSPACE_ID)
        .map(|(child, strip)| (child.parent(), strip))
        .expect("the Space's strip");
    first.remove(windows[2]);
    let mut second = LayoutStrip::new(TEST_WORKSPACE_ID, 1);
    second.append(windows[2]);
    world.spawn((
        second,
        crate::ecs::Position(Origin::new(TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT)),
        ChildOf(display),
    ));
    harness.advance(Duration::from_secs(1));

    let (strips, frames) = layout(&mut harness);
    assert_eq!(
        strips,
        vec![(TEST_WORKSPACE_ID, 0, windows.to_vec(), true)],
        "the Space keeps one strip with every window"
    );
    let parked = IVec2::new(
        TEST_DISPLAY_WIDTH - crate::ecs::layout::PARKED_STRIP_SLIVER,
        TEST_DISPLAY_HEIGHT - crate::ecs::layout::PARKED_STRIP_SLIVER,
    );
    assert!(
        frames
            .iter()
            .all(|frame| frame.min.y == TEST_MENUBAR_HEIGHT && frame.min != parked),
        "no window is parked: {frames:?}"
    );
}

/// Windows that arrive already parked, in a strip that merges into the Space's
/// existing empty strip, come back on screen, although neither that strip's
/// position nor their slot in it changes.
#[test]
fn parked_windows_merged_into_an_existing_empty_strip_come_back_on_screen() {
    const ELSEWHERE: WorkspaceId = 999;
    let mut harness = TestHarness::new().with_windows(1);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(Duration::from_secs(1));

    let parked = Origin::new(
        TEST_DISPLAY_WIDTH - crate::ecs::layout::PARKED_STRIP_SLIVER,
        TEST_DISPLAY_HEIGHT - crate::ecs::layout::PARKED_STRIP_SLIVER,
    );
    let world = harness.world();
    let window = find_window_entity(0, world);
    let display = {
        let (child, mut strip) = world
            .query::<(&ChildOf, &mut LayoutStrip)>()
            .iter_mut(world)
            .find(|(_, strip)| strip.id() == TEST_WORKSPACE_ID)
            .expect("the Space's strip");
        strip.remove(window);
        child.parent()
    };
    let mut elsewhere = LayoutStrip::new(ELSEWHERE, 0);
    elsewhere.append(window);
    let elsewhere = world
        .spawn((elsewhere, crate::ecs::Position(parked), ChildOf(display)))
        .id();
    harness.advance(Duration::from_secs(1));
    assert_eq!(
        harness.mock_state.ax_frame(0).min,
        parked,
        "test setup: the window is parked"
    );

    let world = harness.world();
    world
        .get_mut::<LayoutStrip>(elsewhere)
        .expect("the parked strip")
        .remove(window);
    let mut arriving = LayoutStrip::new(TEST_WORKSPACE_ID, 0);
    arriving.append(window);
    world.spawn((arriving, crate::ecs::Position(parked), ChildOf(display)));
    harness.advance(Duration::from_secs(1));

    let world = harness.world();
    let strips = world
        .query::<&LayoutStrip>()
        .iter(world)
        .filter(|strip| strip.id() == TEST_WORKSPACE_ID)
        .count();
    let frame = harness.mock_state.ax_frame(0);
    assert_eq!(strips, 1, "the Space keeps one strip");
    assert_eq!(
        frame.min.y, TEST_MENUBAR_HEIGHT,
        "the window is back on screen: {frame:?}"
    );
}
