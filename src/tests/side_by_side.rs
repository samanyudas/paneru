use std::time::Duration;

use bevy::ecs::entity::Entity;
use bevy::math::IRect;
use bevy::prelude::{With, World};

use super::*;
use crate::commands::{Command, Direction, Operation};
use crate::ecs::SelectedVirtualMarker;
use crate::ecs::layout::LayoutStrip;
use crate::platform::WinID;

const SETTLE: Duration = Duration::from_secs(2);
const OWN: IRect = IRect {
    min: bevy::math::IVec2::new(0, 0),
    max: bevy::math::IVec2::new(TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
};

fn layout_on(world: &mut World, workspace: WorkspaceId) -> Vec<Vec<WinID>> {
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query_filtered::<&LayoutStrip, With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|strip| strip.id() == workspace)
        .map(|strip| {
            strip
                .columns()
                .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
                .collect()
        })
        .unwrap_or_default()
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

fn area(rect: IRect) -> i64 {
    if rect.is_empty() {
        0
    } else {
        i64::from(rect.width()) * i64::from(rect.height())
    }
}

/// The test display's strip of windows 0 to 3 beside `ext`, which holds
/// window 100, scrolled to `focus`.
fn beside(ext: IRect, focus: Direction) -> TestHarness {
    let mut harness = TestHarness::new()
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(ext.min.x + 100, ext.min.y + 20, ext.min.x + 900, ext.max.y);
        });
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(focus)),
        },
    ]);
    harness.advance(SETTLE);
    harness
}

#[test]
fn a_column_hidden_toward_a_display_beside_stays_off_it() {
    let ext = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = beside(ext, Direction::First);
    for id in 0..4 {
        let shown = frame(harness.world(), id);
        assert!(
            area(shown.intersect(ext)) <= area(shown.intersect(OWN)),
            "window {id} lies mostly on the display beside: {shown:?}"
        );
    }
    let hidden = frame(harness.world(), 3);
    assert!(
        hidden.min.y >= TEST_DISPLAY_HEIGHT - 10 && !hidden.intersect(OWN).is_empty(),
        "window 3 shows its cue at the bottom of its display: {hidden:?}"
    );
    assert!(hidden.intersect(ext).is_empty(), "{hidden:?}");
}

#[test]
fn a_column_tucks_past_the_corner_when_the_display_beside_ends_above_it() {
    let ext = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT / 2,
    );
    let mut harness = beside(ext, Direction::First);
    let hidden = frame(harness.world(), 3);
    assert!(
        hidden.min.x >= TEST_DISPLAY_WIDTH - 10 && hidden.min.y >= TEST_DISPLAY_HEIGHT - 10,
        "window 3 shows only a corner of itself: {hidden:?}"
    );
    assert!(hidden.intersect(ext).is_empty(), "{hidden:?}");
}

#[test]
fn a_column_hidden_toward_a_free_side_keeps_its_sliver() {
    let ext = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = beside(ext, Direction::Last);
    let hidden = frame(harness.world(), 0);
    assert!(
        hidden.max.x <= 10 && hidden.min.y < TEST_DISPLAY_HEIGHT / 2,
        "window 0 keeps its sliver at the free left edge: {hidden:?}"
    );
}

#[test]
fn moving_a_display_beside_a_parked_column_keeps_the_column_in_its_strip() {
    let below = IRect::new(
        0,
        TEST_DISPLAY_HEIGHT,
        EXT_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
    );
    let mut harness = beside(below, Direction::Last);
    let source = layout_on(harness.world(), TEST_WORKSPACE_ID);
    let parked = frame(harness.world(), 0);
    assert!(
        parked.max.x <= 10,
        "window 0 parks past the left edge: {parked:?}"
    );

    let left = IRect::new(-EXT_DISPLAY_WIDTH, 0, 0, EXT_DISPLAY_HEIGHT);
    harness.mock_state.set_display_bounds(EXT_DISPLAY_ID, left);
    harness.mock_state.update_window(100, |window| {
        window.frame = IRect::new(left.min.x + 100, 20, left.min.x + 900, left.max.y);
    });
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: EXT_DISPLAY_ID,
        });
    harness.advance(SETTLE);

    let reparked = frame(harness.world(), 0);
    assert!(
        area(reparked.intersect(left)) <= area(reparked.intersect(OWN)),
        "window 0 lies mostly on the display moved beside it: {reparked:?}"
    );
    assert_eq!(layout_on(harness.world(), TEST_WORKSPACE_ID), source);
    assert_eq!(
        layout_on(harness.world(), EXT_WORKSPACE_ID),
        vec![vec![100]]
    );
}

#[test]
fn a_column_centered_past_a_shared_edge_tucks() {
    // Window 1 spans 724 to 1354: its center is on the shorter display beside,
    // while the area on each display is the same.
    let ext = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        700,
    );
    let mut harness = TestHarness::new()
        .with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID])
        .with_window(0, |window| {
            window.frame = IRect::new(0, 0, 724, TEST_WINDOW_HEIGHT);
        })
        .with_window(1, |window| {
            window.frame = IRect::new(0, 0, 630, TEST_WINDOW_HEIGHT);
        });
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    assert_eq!(
        frame(harness.world(), 0).min.x,
        0,
        "window 0 starts the strip"
    );
    let past = frame(harness.world(), 1);
    assert!(
        !ext.contains(past.center()) && area(past.intersect(ext)) <= area(past.intersect(OWN)),
        "window 1 lies on the display beside: {past:?}"
    );
}
