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

fn moves_with_delta_and_time(points: &[(i32, i32, i64, u64)]) -> Vec<Event> {
    let base = std::time::Instant::now();
    points
        .iter()
        .map(|&(x, y, delta_y, offset_ms)| Event::MouseMoved {
            point: objc2_core_foundation::CGPoint {
                x: f64::from(x),
                y: f64::from(y),
            },
            delta_y,
            timestamp: base + std::time::Duration::from_millis(offset_ms),
            modifiers: crate::platform::Modifiers::empty(),
        })
        .collect()
}

fn moves_with_delta(points: &[(i32, i32, i64)]) -> Vec<Event> {
    let timed = points
        .iter()
        .enumerate()
        .map(|(i, &(x, y, delta_y))| (x, y, delta_y, (i as u64) * 10))
        .collect::<Vec<_>>();
    moves_with_delta_and_time(&timed)
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

fn pointer_pumped_events(harness: TestHarness, raw: Vec<Event>) -> (IVec2, IVec2) {
    let mut pumped = Vec::new();
    let mut motion = Vec::new();
    let mut pending_mouse = None;
    for event in raw {
        crate::ecs::systems::push_pumped_event(event, &mut pending_mouse, &mut pumped, &mut motion);
    }
    pumped.extend(pending_mouse.take());

    let seen = Arc::new(Mutex::new((IVec2::ZERO, IVec2::ZERO)));
    let before = Arc::clone(&seen);
    let after = Arc::clone(&seen);
    harness
        .on_iteration(1, move |world, state| {
            before.lock().expect("cursor record").0 = state.cursor_position();
            world.write_message_batch(pumped.iter().cloned());
            world.write_message_batch(motion.iter().copied());
            world
                .resource_mut::<crate::ecs::systems::PumpedPointerMotion>()
                .0 = true;
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

    // Push down outside the laptop's mapped range, then slide sideways into it
    // in the same pump drain: warp sees both samples in order and does not jump.
    let (before, cursor) = pointer_pumped_events(
        harness,
        moves_with_delta(&[
            (-1480, -EXT_DISPLAY_HEIGHT - 1, 4),
            (-1520, -EXT_DISPLAY_HEIGHT - 1, 0),
        ]),
    );
    assert_eq!(
        cursor, before,
        "a downward push outside the laptop followed by sideways motion into its range must not warp"
    );
}

#[test]
fn downward_push_then_sideways_sample_in_same_drain_still_warps_from_overhang() {
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

    let (_, cursor) = pointer_pumped_events(
        harness,
        moves_with_delta(&[
            (-1608, -EXT_DISPLAY_HEIGHT - 1, 4),
            (-1600, -EXT_DISPLAY_HEIGHT - 1, 0),
        ]),
    );
    assert_eq!(
        cursor,
        IVec2::new(612, 6),
        "a downward overhang push followed by a sideways sample in the same drain must still warp"
    );
}

#[test]
fn upward_move_then_sideways_sample_in_same_drain_still_warps_from_laptop() {
    let (_, cursor) =
        pointer_pumped_events(desk(true), moves_with_delta(&[(800, 0, -5), (810, 0, 0)]));
    assert_eq!(
        cursor,
        IVec2::new(800 - 1920, -EXT_DISPLAY_HEIGHT - 7),
        "an upward sample after a pause followed by a sideways sample in the same drain must still warp"
    );
}

#[test]
fn fast_downward_flick_across_native_seam_in_one_drain_still_warps_to_laptop() {
    let (_, cursor) = pointer_pumped_events(
        desk(true),
        moves_with_delta(&[(-1308, -1500, 0), (-1308, -900, 600)]),
    );
    assert_eq!(
        cursor,
        IVec2::new(612, 6),
        "both sides of a native display seam in one drain must stay ordered for row_crossing"
    );
}

#[test]
fn duplicate_edge_samples_in_one_drain_warp_only_once_without_spurious_carry() {
    let (_, cursor) = pointer_pumped_events(
        desk(true),
        moves_with_delta_and_time(&[(511, -600, 0, 0), (511, -600, 0, 10)]),
    );
    // Landing inset is 6 px from right_monitor.min.x (-1408) -> -1402 with zero
    // velocity carry (a second warp from the first landing would add 80 px carry).
    assert_eq!(
        cursor,
        IVec2::new(-1402, -1800),
        "remaining pre-warp samples in the same drain must be discarded after a warp succeeds"
    );
}

#[test]
fn velocity_carry_uses_event_timestamps_within_a_single_drain() {
    // Physical HID devices report Mach ticks (24 MHz on Apple silicon:
    // numer = 125, denom = 3 -> 240_000 ticks == 10 ms), while software-posted
    // events report nanoseconds above `mach_absolute_time()`. Even if a
    // main-run-loop stall dispatches both events 50 us apart (25 ms after the
    // first event), mapping directly against `uptime()` preserves the true
    // 10 ms interval: 1 px rightward over 10 ms = 100 px/s -> 30 ms carry =
    // 3 px -> landing at -1408 + 6 + 3 = -1399.
    let stalled_dispatch = std::time::Instant::now();
    let burst_second = stalled_dispatch + std::time::Duration::from_micros(50);

    // 1. Mach-tick timestamps (Apple silicon 125/3 timebase: 240_000 ticks = 10 ms).
    let now_ticks = 10_000_000_000;
    let t1_uptime =
        crate::platform::slice_tap::made_at_with_timebase(now_ticks - 240_000, now_ticks, 125, 3);
    let t2_uptime = crate::platform::slice_tap::made_at_with_timebase(now_ticks, now_ticks, 125, 3);
    let stall = std::time::Duration::from_millis(25);
    let t1_mach = crate::platform::input::event_timestamp_from_uptime(
        t1_uptime,
        t2_uptime + stall,
        stalled_dispatch,
    );
    let t2_mach = crate::platform::input::event_timestamp_from_uptime(
        t2_uptime,
        t2_uptime + stall + std::time::Duration::from_micros(50),
        burst_second,
    );
    assert_eq!(
        t2_mach.saturating_duration_since(t1_mach),
        std::time::Duration::from_millis(10)
    );
    assert!(
        t1_mach < stalled_dispatch && t2_mach < burst_second,
        "delayed events must map to their creation instants prior to dispatch"
    );

    // 2. Software-posted nanosecond timestamps through `event_timestamp_from_uptime`,
    //    plus zero-timestamp fallback through `cg_event_timestamp_to_instant`.
    let base_ns = std::time::Duration::from_secs(500);
    let t1_ns = crate::platform::input::event_timestamp_from_uptime(
        base_ns,
        base_ns + std::time::Duration::from_millis(35),
        stalled_dispatch,
    );
    let t2_ns = crate::platform::input::event_timestamp_from_uptime(
        base_ns + std::time::Duration::from_millis(10),
        base_ns + std::time::Duration::from_millis(35) + std::time::Duration::from_micros(50),
        burst_second,
    );
    assert_eq!(
        crate::platform::input::cg_event_timestamp_to_instant(0, burst_second),
        burst_second
    );

    for (label, t1, t2) in [
        ("mach ticks", t1_mach, t2_mach),
        ("nanoseconds", t1_ns, t2_ns),
    ] {
        let events = vec![
            Event::MouseMoved {
                point: objc2_core_foundation::CGPoint {
                    x: 509.0,
                    y: -600.0,
                },
                delta_y: 0,
                timestamp: t1,
                modifiers: crate::platform::Modifiers::empty(),
            },
            Event::MouseMoved {
                point: objc2_core_foundation::CGPoint {
                    x: 510.0,
                    y: -600.0,
                },
                delta_y: 0,
                timestamp: t2,
                modifiers: crate::platform::Modifiers::empty(),
            },
        ];
        let (_, cursor) = pointer_pumped_events(desk(true), events);
        assert_eq!(
            cursor,
            IVec2::new(-1399, -1800),
            "carry-over ({label}) must use normalized CGEvent timestamps even when run-loop dispatch stalls into a burst"
        );
    }
}

#[test]
fn pre_warp_edge_sample_arriving_in_next_drain_does_not_re_warp() {
    // Drain 1 warps from the right edge of `left_monitor` onto `right_monitor`.
    // Drain 2 (next frame) delivers a stale HID event created BEFORE Drain 1's
    // warp still carrying the source display's edge coordinate `(511, -600)`.
    // Comparing `motion.timestamp < warped_at` must drop it without re-warping.
    let pre_warp = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(20))
        .unwrap_or_else(std::time::Instant::now);
    let drain_1 = [Event::MouseMoved {
        point: objc2_core_foundation::CGPoint {
            x: 511.0,
            y: -600.0,
        },
        delta_y: 0,
        timestamp: pre_warp,
        modifiers: crate::platform::Modifiers::empty(),
    }];
    let drain_2_stale = [Event::MouseMoved {
        point: objc2_core_foundation::CGPoint {
            x: 511.0,
            y: -600.0,
        },
        delta_y: 0,
        timestamp: pre_warp + std::time::Duration::from_millis(5),
        modifiers: crate::platform::Modifiers::empty(),
    }];

    let seen = Arc::new(Mutex::new((IVec2::ZERO, IVec2::ZERO)));
    let after_1 = Arc::clone(&seen);
    let after_2 = Arc::clone(&seen);
    desk(true)
        .on_iteration(1, move |world, _state| {
            world.write_message_batch(drain_1.iter().cloned());
        })
        .on_iteration(2, move |world, state| {
            after_1.lock().expect("cursor record").0 = state.cursor_position();
            world.write_message_batch(drain_2_stale.iter().cloned());
        })
        .on_iteration(3, move |_world, state| {
            after_2.lock().expect("cursor record").1 = state.cursor_position();
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
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
    let (landing_1, landing_2) = *seen.lock().expect("cursor record");
    assert_eq!(landing_1, IVec2::new(-1402, -1800));
    assert_eq!(
        landing_2, landing_1,
        "a stale pre-warp sample arriving in the next drain must not trigger a second warp"
    );
}
