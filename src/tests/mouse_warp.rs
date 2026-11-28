use std::sync::{Arc, Mutex};

use bevy::prelude::*;

use crate::commands::Command;
use crate::config::{Config, MainOptions};
use crate::events::Event;

use super::*;

const RIGHT_DISPLAY_ID: u32 = 3;
const RIGHT_WORKSPACE_ID: u64 = 30;

fn left_monitor() -> IRect {
    IRect::new(-1408, -EXT_DISPLAY_HEIGHT, 512, 0)
}

fn right_monitor() -> IRect {
    IRect::new(-1408, -2 * EXT_DISPLAY_HEIGHT, 512, -EXT_DISPLAY_HEIGHT)
}

fn laptop() -> IRect {
    IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT)
}

fn desk(lowest_below: bool) -> TestHarness {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(lowest_below),
            ..Default::default()
        },
        vec![],
    )
        .into();
    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, left_monitor(), vec![EXT_WORKSPACE_ID])
        .with_display(RIGHT_DISPLAY_ID, right_monitor(), vec![RIGHT_WORKSPACE_ID])
}

fn moves_with_delta(points: &[(i32, i32, i64)]) -> Vec<Event> {
    points
        .iter()
        .map(|&(x, y, delta_y)| Event::MouseMoved {
            point: objc2_core_foundation::CGPoint {
                x: f64::from(x),
                y: f64::from(y),
            },
            delta_y,
            modifiers: crate::platform::Modifiers::empty(),
        })
        .collect()
}

fn moves(points: &[(i32, i32)]) -> Vec<Event> {
    let with_delta = points.iter().map(|&(x, y)| (x, y, 0)).collect::<Vec<_>>();
    moves_with_delta(&with_delta)
}

fn pointer_events(harness: TestHarness, events: Vec<Event>) -> (IVec2, IVec2) {
    let seen = Arc::new(Mutex::new((IVec2::ZERO, IVec2::ZERO)));
    let before = Arc::clone(&seen);
    let after = Arc::clone(&seen);
    harness
        .on_iteration(1, move |world, state| {
            before.lock().expect("cursor record").0 = state.cursor_position();
            world.write_message_batch(events.iter().cloned());
        })
        .on_iteration(2, move |_world, state| {
            after.lock().expect("cursor record").1 = state.cursor_position();
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
    *seen.lock().expect("cursor record")
}

fn pointer_moves(harness: TestHarness, points: &[(i32, i32)]) -> (IVec2, IVec2) {
    pointer_events(harness, moves(points))
}

#[test]
fn moving_down_from_the_upper_monitor_lands_on_the_laptop_below_it() {
    let (_, cursor) = pointer_moves(desk(true), &[(-1308, -1210), (-1308, -1190)]);
    assert_eq!(
        cursor,
        IVec2::new(612, 6),
        "100 px into the right monitor is 100 px right of the laptop's centre"
    );
}

#[test]
fn moving_down_from_the_upper_monitor_past_the_laptop_stays_on_it() {
    let (_, cursor) = pointer_moves(desk(true), &[(100, -1210), (100, -1190)]);
    assert_eq!(
        cursor,
        IVec2::new(100, -EXT_DISPLAY_HEIGHT - 1),
        "no display is below that part of the right monitor"
    );
}

#[test]
fn moving_up_from_the_lower_monitor_stays_on_it() {
    let (_, cursor) = pointer_moves(desk(true), &[(-1308, -1190), (-1308, -1210)]);
    assert_eq!(
        cursor,
        IVec2::new(-1308, -EXT_DISPLAY_HEIGHT),
        "no display is above the left monitor"
    );
}

#[test]
fn a_fast_flick_down_from_the_upper_monitor_still_reaches_the_laptop() {
    let (_, cursor) = pointer_moves(desk(true), &[(-1308, -1500), (-1308, -900)]);
    assert_eq!(cursor, IVec2::new(612, 6));
}

#[test]
fn moving_up_from_the_laptop_where_macos_has_no_display_above_reaches_the_upper_monitor() {
    let (_, cursor) = pointer_moves(desk(true), &[(800, 40), (800, 1)]);
    assert_eq!(cursor, IVec2::new(800 - 1920, -EXT_DISPLAY_HEIGHT - 7));
}

#[test]
fn the_laptop_leaves_the_sideways_warp_but_the_monitors_keep_it() {
    let (before, cursor) = pointer_moves(desk(true), &[(-1380, -600), (-1407, -600)]);
    assert_eq!(
        cursor, before,
        "the left edge of the left monitor has no display to its left"
    );

    let (_, cursor) = pointer_moves(desk(true), &[(480, -600), (511, -600)]);
    assert!(
        right_monitor().contains(cursor),
        "the right edge of the left monitor still reaches the right monitor: {cursor:?}"
    );

    let (before, cursor) = pointer_moves(desk(true), &[(990, 400), (1023, 400)]);
    assert_eq!(cursor, before, "the laptop's right edge does not warp");
}

#[test]
fn without_the_option_moving_down_between_monitors_stays_native() {
    let (before, cursor) = pointer_moves(desk(false), &[(-1308, -1210), (-1308, -1190)]);
    assert_eq!(cursor, before);
}

#[test]
fn with_two_displays_the_option_keeps_the_laptop_in_the_row() {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let harness = TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, left_monitor(), vec![EXT_WORKSPACE_ID]);
    let (_, cursor) = pointer_moves(harness, &[(990, 400), (1023, 400)]);
    assert!(
        left_monitor().contains(cursor),
        "the laptop's right edge still reaches the display above: {cursor:?}"
    );
    assert!(!laptop().contains(cursor));
}

#[test]
fn a_stale_event_after_a_sideways_warp_is_not_a_crossing() {
    let (_, cursor) = pointer_moves(desk(true), &[(480, -600), (511, -600), (500, -600)]);
    assert!(
        right_monitor().contains(cursor),
        "the pointer stays where the sideways warp put it: {cursor:?}"
    );
}

#[test]
fn top_pixel_row_of_laptop_belongs_to_laptop_not_monitor_above() {
    let (before, cursor) = pointer_moves(desk(true), &[(-200, -1500), (200, 0)]);
    assert_eq!(
        cursor, before,
        "y = 0 is the top pixel row of the laptop, not the bottom edge of the left monitor"
    );
}

#[test]
fn moving_sideways_along_the_laptop_top_edge_does_not_warp_up() {
    let (before, cursor) = pointer_moves(desk(true), &[(800, 1), (810, 1)]);
    assert_eq!(
        cursor, before,
        "horizontal motion along the laptop top edge must not warp to an upper monitor"
    );
}

#[test]
fn moving_down_from_a_staggered_upper_monitor_overhang_reaches_the_laptop() {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    // Upper right monitor is staggered 300 px left of the lower left monitor in
    // macOS's virtual layout, so x = -1600 has no display natively beneath it
    // even though its row position (620) sits over the laptop (0..1024).
    let staggered_left = IRect::new(-1408, -EXT_DISPLAY_HEIGHT, 512, 0);
    let staggered_right = IRect::new(-1708, -2 * EXT_DISPLAY_HEIGHT, 212, -EXT_DISPLAY_HEIGHT);
    let harness = TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, staggered_left, vec![EXT_WORKSPACE_ID])
        .with_display(RIGHT_DISPLAY_ID, staggered_right, vec![RIGHT_WORKSPACE_ID]);
    let (_, cursor) = pointer_moves(
        harness,
        &[
            (-1608, -EXT_DISPLAY_HEIGHT - 30),
            (-1608, -EXT_DISPLAY_HEIGHT - 1),
        ],
    );
    assert_eq!(
        cursor,
        IVec2::new(612, 6),
        "downward motion into the clamped overhang edge must reach the laptop below it"
    );
}

#[test]
fn pushing_down_at_clamped_overhang_bottom_edge_after_sideways_slide_reaches_laptop() {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let staggered_left = IRect::new(-1408, -EXT_DISPLAY_HEIGHT, 512, 0);
    let staggered_right = IRect::new(-1708, -2 * EXT_DISPLAY_HEIGHT, 212, -EXT_DISPLAY_HEIGHT);
    let harness = TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, staggered_left, vec![EXT_WORKSPACE_ID])
        .with_display(RIGHT_DISPLAY_ID, staggered_right, vec![RIGHT_WORKSPACE_ID]);
    // Slide sideways along the clamped bottom pixel row into the overhang
    // (delta_y = 0 stays on the upper monitor), then push down (delta_y > 0)
    // while the clamped Y coordinate remains unchanged.
    let (_, cursor) = pointer_events(
        harness,
        moves_with_delta(&[
            (-1650, -EXT_DISPLAY_HEIGHT - 1, 0),
            (-1608, -EXT_DISPLAY_HEIGHT - 1, 0),
            (-1608, -EXT_DISPLAY_HEIGHT - 1, 4),
        ]),
    );
    assert_eq!(
        cursor,
        IVec2::new(612, 6),
        "positive vertical delta at the clamped overhang edge must warp to the laptop"
    );
}

#[test]
fn moving_up_from_the_laptop_after_a_pause_still_reaches_the_upper_monitor() {
    // A single event with negative delta_y (such as moving straight from
    // (800, 5) to (800, 0) after pausing >80 ms, where no recent sample exists)
    // must still warp up to the upper monitor.
    let (_, cursor) = pointer_events(desk(true), moves_with_delta(&[(800, 0, -5)]));
    assert_eq!(cursor, IVec2::new(800 - 1920, -EXT_DISPLAY_HEIGHT - 7));
}

#[test]
fn coalescing_downward_push_outside_laptop_before_sideways_slide_does_not_warp() {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    // With `staggered_left` at (-1000..920) and `staggered_right` at
    // (-1600..320), `staggered_right` overhangs left for x in -1600..-1000
    // with `row_offset_x = 2520`:
    // - x = -1480 maps to row_x = 1040 (outside laptop 0..1024)
    // - x = -1520 maps to row_x = 1000 (inside laptop 0..1024)
    let staggered_left = IRect::new(-1000, -EXT_DISPLAY_HEIGHT, 920, 0);
    let staggered_right = IRect::new(-1600, -2 * EXT_DISPLAY_HEIGHT, 320, -EXT_DISPLAY_HEIGHT);
    let harness = TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, staggered_left, vec![EXT_WORKSPACE_ID])
        .with_display(RIGHT_DISPLAY_ID, staggered_right, vec![RIGHT_WORKSPACE_ID]);

    // Simulate `pump_events` receiving a downward push outside the laptop's
    // mapped range followed by a sideways slide into it within the same frame.
    let raw = moves_with_delta(&[
        (-1480, -EXT_DISPLAY_HEIGHT - 1, 4),
        (-1520, -EXT_DISPLAY_HEIGHT - 1, 0),
    ]);
    let mut pumped = Vec::new();
    let mut pending_mouse = None;
    for event in raw {
        crate::ecs::systems::push_pumped_event(event, &mut pending_mouse, &mut pumped);
    }
    pumped.extend(pending_mouse.take());
    assert_eq!(
        pumped.len(),
        2,
        "non-zero delta_y must stay paired with its original point instead of coalescing onto the sideways move"
    );

    let (before, cursor) = pointer_events(harness, pumped);
    assert_eq!(
        cursor, before,
        "a downward push outside the laptop followed by sideways motion into its range must not warp"
    );
}
