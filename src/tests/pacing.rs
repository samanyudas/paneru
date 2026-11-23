//! A row scrolls at the pace of its slowest window: the window ahead in the
//! direction of motion never leaves the desktop showing between itself and
//! a slower window behind it.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, Direction, Operation, ResizeDirection, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::{RepositionMarker, SpawnWindowTrigger, WidthLimits};
use crate::events::Event;
use crate::manager::Window;
use crate::platform::{Pid, WinID};

use super::*;

/// Owns window 1, which takes its moves late, like Helium.
const SLOW_APP: Pid = TEST_PROCESS_ID;
/// Owns windows 0 and 2, and takes each move before the call returns.
const FAST_APP: Pid = 2;

/// Three of them fill more than the display.
const WINDOW_WIDTH: i32 = 600;
/// How long each of the slow app's writes takes once its writes are let
/// through. The tests hold its writes while frames run and land them between
/// frames, so this sets no lateness on the engine's clock. It only makes
/// each landing take real time, as a slow app's does.
const SLOW_WRITE: Duration = Duration::from_millis(40);
const FRAME: Duration = Duration::from_millis(20);
/// How soon after a pass an event can wake the engine for the next one.
const SHORT_PASS: Duration = Duration::from_millis(2);
const FLUSH: Duration = Duration::from_secs(5);

/// Windows 0, 1 and 2 in a row, at rest with window 0 at the display's left
/// edge.
fn harness() -> TestHarness {
    harness_with_widths([WINDOW_WIDTH; 3])
}

/// [`harness`], with windows 0, 1 and 2 `widths` wide.
fn harness_with_widths(widths: [i32; 3]) -> TestHarness {
    harness_with(widths, Some(14.0))
}

/// [`harness_with_widths`] at `animation_speed`, where `None` turns
/// animation off.
fn harness_with(widths: [i32; 3], animation_speed: Option<f64>) -> TestHarness {
    let config: Config = (
        MainOptions {
            animation_speed,
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness =
        TestHarness::new()
            .with_config(config)
            .with_app(FAST_APP, "fast", "FastApp", |_| {});
    harness.mock_state.queue_writes(SLOW_APP);
    harness.mock_state.set_write_delay(SLOW_APP, SLOW_WRITE);
    for ((id, pid), width) in [(0, FAST_APP), (1, SLOW_APP), (2, FAST_APP)]
        .into_iter()
        .zip(widths)
    {
        let frame = IRect::new(0, 0, width, TEST_WINDOW_HEIGHT);
        let window = harness
            .mock_state
            .spawn_window(pid, TEST_WORKSPACE_ID, id, frame);
        harness.existed_at_launch([id]);
        harness.world().trigger(SpawnWindowTrigger(vec![window]));
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus(Direction::First),
    ]);
    settle(&mut harness);
    harness
}

fn focus(direction: Direction) -> Event {
    Event::Command {
        command: Command::Window(Operation::Focus(direction)),
    }
}

/// The frame the engine asked window `id` to have.
fn engine_frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .frame()
}

fn moving(world: &mut World) -> bool {
    world
        .query_filtered::<(), With<RepositionMarker>>()
        .iter(world)
        .next()
        .is_some()
}

/// Runs frames until no window moves and every write has landed.
fn settle(harness: &mut TestHarness) {
    for _ in 0..200 {
        harness.advance(FRAME);
        if !moving(harness.world()) && harness.mock_state.flush_writes(FLUSH) {
            harness.advance(FRAME);
            if !moving(harness.world()) {
                return;
            }
        }
    }
    panic!("the row did not settle");
}

/// How much desktop shows between windows `left` and `right` where their
/// apps have them.
fn gap(harness: &TestHarness, left: WinID, right: WinID) -> i32 {
    harness.mock_state.ax_frame(right).min.x - harness.mock_state.ax_frame(left).max.x
}

/// Lets the slow app take every move it has been asked for, and waits until
/// it has.
fn land_slow_writes(harness: &mut TestHarness) {
    harness.mock_state.release_writes(SLOW_APP);
    assert!(
        harness.mock_state.flush_writes(FLUSH),
        "the slow app takes its moves"
    );
}

/// Runs frames until the row settles, with the slow app showing each move
/// one frame late, and asserts after each frame that no more desktop than
/// `rest` shows between windows `left` and `right`.
///
/// The slow app's writes land between frames, never during one. Pacing
/// stops waiting for a window shown late for `LANDING_TIMEOUT` of engine
/// time, and a real write delay let a loaded machine run the frames past it
/// before the app answered (miri#496).
fn settle_beside(harness: &mut TestHarness, left: WinID, right: WinID, rest: i32) {
    // The app answers the writes it held as it is released.
    land_slow_writes(harness);
    for frame in 0..200 {
        harness.mock_state.hold_writes(SLOW_APP);
        harness.advance(FRAME);
        assert!(
            gap(harness, left, right) <= rest,
            "desktop shows between windows {left} and {right} {frame} frames after the release: {:?} {:?}",
            harness.mock_state.ax_frame(left),
            harness.mock_state.ax_frame(right)
        );
        land_slow_writes(harness);
        if !moving(harness.world()) {
            return;
        }
    }
    panic!("the row did not settle");
}

fn assert_landed(harness: &mut TestHarness) {
    for id in 0..3 {
        assert_eq!(
            harness.mock_state.ax_frame(id),
            engine_frame(harness.world(), id),
            "window {id} landed where the engine put it"
        );
    }
}

/// While the slow app is stuck in its first write, the row scrolls left and
/// window 0, ahead of it, waits beside it. Once the app answers, the row
/// finishes where it would have without the wait.
#[test]
fn the_window_ahead_waits_for_a_slow_window_behind_it() {
    let mut harness = harness();
    let rest = gap(&harness, 0, 1);
    let start = harness.mock_state.ax_frame(0);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    for frame in 0..10 {
        harness.advance(FRAME);
        assert!(
            gap(&harness, 0, 1) <= rest,
            "desktop shows between windows 0 and 1 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(0),
            harness.mock_state.ax_frame(1)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(&mut harness, 0, 1, rest);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert!(
        harness.mock_state.ax_frame(0).min.x < start.min.x,
        "window 0 scrolled left from {start:?}"
    );
    assert_eq!(gap(&harness, 0, 1), rest, "the row closed up");
}

/// The same as the row scrolls right: window 2, ahead, waits beside the
/// slow window 1 behind it.
#[test]
fn the_window_ahead_waits_for_a_slow_window_behind_it_scrolling_right() {
    let mut harness = harness();
    harness.world().write_message(focus(Direction::Last));
    settle(&mut harness);
    let rest = gap(&harness, 1, 2);
    let start = harness.mock_state.ax_frame(2);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::First));
    for frame in 0..10 {
        harness.advance(FRAME);
        assert!(
            gap(&harness, 1, 2) <= rest,
            "desktop shows between windows 1 and 2 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(1),
            harness.mock_state.ax_frame(2)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(&mut harness, 1, 2, rest);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert!(
        harness.mock_state.ax_frame(2).min.x > start.min.x,
        "window 2 scrolled right from {start:?}"
    );
}

/// Focus moves to the slow window 1, and the row scrolls it into view. Its
/// app shows nothing for three frames, as an app that gains focus does. Once
/// it answers, window 1 shows its first step and slides on from there,
/// rather than showing far along at once while it covers window 0 (miri#500).
#[test]
fn a_slow_window_coming_into_view_starts_its_slide_where_its_app_first_shows_it() {
    let mut harness = harness();
    let start = harness.mock_state.ax_frame(1).min.x;
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    for _ in 0..3 {
        harness.advance(FRAME);
    }
    let mut shown = vec![start];
    land_slow_writes(&mut harness);
    shown.push(harness.mock_state.ax_frame(1).min.x);
    for _ in 0..200 {
        harness.mock_state.hold_writes(SLOW_APP);
        harness.advance(FRAME);
        land_slow_writes(&mut harness);
        shown.push(harness.mock_state.ax_frame(1).min.x);
        if !moving(harness.world()) {
            break;
        }
    }
    settle(&mut harness);
    assert_landed(&mut harness);

    let end = harness.mock_state.ax_frame(1).min.x;
    assert!(
        end < start,
        "window 1 scrolled into view from {start} to {end}"
    );
    let largest_step = (start - end) / 3;
    assert!(
        shown
            .windows(2)
            .all(|pair| (0..=largest_step).contains(&(pair[0] - pair[1]))),
        "window 1 slides from {start} to {end} in steps of at most {largest_step}: {shown:?}"
    );
}

/// The slow window 1 widens between windows 0 and 2. Its right edge moves
/// right as its left edge moves left, so window 2 ahead on the right waits
/// for its right edge, as window 0 on the left waits for its left edge.
#[test]
fn the_neighbours_wait_beside_a_slow_window_that_widens() {
    let mut harness = harness_with_widths([WINDOW_WIDTH, 400, WINDOW_WIDTH]);
    harness.world().write_message(focus(Direction::East));
    settle(&mut harness);
    let rest = gap(&harness, 1, 2);
    let start = harness.mock_state.ax_frame(1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness
        .world()
        .write_message(window(Operation::Resize(ResizeDirection::Grow)));
    for frame in 0..10 {
        harness.advance(FRAME);
        for (left, right) in [(0, 1), (1, 2)] {
            assert!(
                gap(&harness, left, right) <= rest,
                "desktop shows between windows {left} and {right} after frame {frame}: {:?} {:?}",
                harness.mock_state.ax_frame(left),
                harness.mock_state.ax_frame(right)
            );
        }
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(&mut harness, 1, 2, rest);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert!(
        harness.mock_state.ax_frame(1).width() > start.width(),
        "window 1 widened from {start:?}"
    );
}

/// Window 0 narrows beside the slow window 1. Its right edge moves left
/// ahead of window 1, so it keeps its width until window 1 follows.
#[test]
fn a_narrowing_window_waits_for_a_slow_window_on_its_right() {
    let mut harness = harness();
    let rest = gap(&harness, 0, 1);
    let start = harness.mock_state.ax_frame(0);
    harness.mock_state.hold_writes(SLOW_APP);

    harness
        .world()
        .write_message(window(Operation::Resize(ResizeDirection::Grow)));
    for frame in 0..10 {
        harness.advance(FRAME);
        assert!(
            gap(&harness, 0, 1) <= rest,
            "desktop shows between windows 0 and 1 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(0),
            harness.mock_state.ax_frame(1)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(&mut harness, 0, 1, rest);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert!(
        harness.mock_state.ax_frame(0).width() < start.width(),
        "window 0 narrowed from {start:?}"
    );
}

/// Window 0 narrows by only 6 points, from 518 to 512, beside the slow
/// window 1. Its first step comes close enough to 512 to snap there, and it
/// still keeps its right edge beside window 1 until window 1 follows.
#[test]
fn a_narrowing_window_that_snaps_still_waits_for_a_slow_window() {
    let mut harness = harness_with_widths([518, WINDOW_WIDTH, WINDOW_WIDTH]);
    let rest = gap(&harness, 0, 1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness
        .world()
        .write_message(window(Operation::Resize(ResizeDirection::Grow)));
    for frame in 0..5 {
        harness.advance(FRAME);
        assert!(
            gap(&harness, 0, 1) <= rest,
            "desktop shows between windows 0 and 1 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(0),
            harness.mock_state.ax_frame(1)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(&mut harness, 0, 1, rest);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert_eq!(
        harness.mock_state.ax_frame(0).width(),
        512,
        "window 0 narrowed"
    );
}

/// With animation off, window 0 narrows beside the slow window 1 at once,
/// in one write, rather than keeping its width until window 1 follows.
#[test]
fn with_animation_off_a_narrowing_window_takes_its_width_at_once() {
    let mut harness = harness_with([WINDOW_WIDTH; 3], None);
    let start = harness.mock_state.ax_frame(0);
    harness.mock_state.hold_writes(SLOW_APP);

    harness
        .world()
        .write_message(window(Operation::Resize(ResizeDirection::Grow)));
    harness.advance(FRAME);
    let first = harness.mock_state.ax_frame(0);
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);

    assert_landed(&mut harness);
    let landed = harness.mock_state.ax_frame(0);
    assert!(
        landed.width() < start.width(),
        "window 0 narrowed from {start:?}"
    );
    assert_eq!(
        first, landed,
        "window 0 took its narrowed frame in the first frame"
    );
}

/// An app that showed every move of one motion in time can turn slow when
/// one of its windows gains focus, as Helium does: window 2 ahead waits for
/// window 1 from the first step, rather than trusting it to keep up.
#[test]
fn the_window_ahead_waits_for_a_prompt_window_whose_app_gains_focus() {
    let mut harness = harness();
    harness.mock_state.set_write_delay(SLOW_APP, Duration::ZERO);
    harness.world().write_message(focus(Direction::Last));
    settle_beside(&mut harness, 1, 2, i32::MAX);
    settle(&mut harness);
    let rest = gap(&harness, 1, 2);
    let start = harness.mock_state.ax_frame(1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::West));
    for frame in 0..10 {
        harness.advance(FRAME);
        assert!(
            gap(&harness, 1, 2) <= rest,
            "desktop shows between windows 1 and 2 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(1),
            harness.mock_state.ax_frame(2)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(&mut harness, 1, 2, rest);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert!(
        harness.mock_state.ax_frame(1).min.x > start.min.x,
        "window 1 scrolled right from {start:?}"
    );
}

/// An app that stops answering holds the row only for a while: window 0
/// reaches its target while window 1's app is still stuck.
#[test]
fn a_stuck_window_does_not_hold_the_row_for_good() {
    let mut harness = harness();
    let stuck = harness.mock_state.ax_frame(1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    harness.advance(Duration::from_millis(600));

    let window = find_window_entity(0, harness.world());
    assert!(
        harness.world().get::<RepositionMarker>(window).is_none(),
        "window 0 finished its move"
    );
    assert_eq!(
        harness.mock_state.ax_frame(0),
        engine_frame(harness.world(), 0),
        "window 0 shows where the engine put it"
    );
    assert_eq!(
        harness.mock_state.ax_frame(1),
        stuck,
        "the slow app had not taken its move"
    );
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);
    assert_landed(&mut harness);
}

/// A move short enough to finish in one step of the ease: window 0 ahead
/// still waits for the slow window 1 behind it, rather than snapping to its
/// target while window 1 has yet to move.
#[test]
fn the_window_ahead_does_not_snap_past_a_slow_window_on_a_short_move() {
    let mut harness = harness_with_widths([600, TEST_DISPLAY_WIDTH - 600 + 6, 400]);
    let rest = gap(&harness, 0, 1);
    let start = harness.mock_state.ax_frame(0);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    for frame in 0..8 {
        harness.advance(FRAME);
        assert!(
            gap(&harness, 0, 1) <= rest,
            "desktop shows between windows 0 and 1 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(0),
            harness.mock_state.ax_frame(1)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);

    assert_landed(&mut harness);
    assert_eq!(
        harness.mock_state.ax_frame(0).min.x,
        start.min.x - 6,
        "the row scrolled by 6 points"
    );
}

/// A pass too short to move window 0 a whole point, near the end of a short
/// move: window 0 ahead still waits for the slow window 1 behind it, rather
/// than taking the rest of its slide at once while window 1 has yet to move.
#[test]
fn the_window_ahead_does_not_pass_a_slow_window_on_a_pass_too_short_to_move_it() {
    let mut harness = harness_with_widths([600, TEST_DISPLAY_WIDTH - 600 + 12, 400]);
    let rest = gap(&harness, 0, 1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    for _ in 0..5 {
        harness.advance(FRAME);
    }
    harness
        .world()
        .insert_resource(TimeUpdateStrategy::ManualDuration(SHORT_PASS));
    harness.advance(SHORT_PASS);
    harness
        .world()
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME));
    assert!(
        gap(&harness, 0, 1) <= rest,
        "desktop shows between windows 0 and 1 after a short pass: {:?} {:?}",
        harness.mock_state.ax_frame(0),
        harness.mock_state.ax_frame(1)
    );
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);
    assert_landed(&mut harness);
}

/// Owns no window the engine tracks, like Finder showing only the desktop.
const DESKTOP_APP: Pid = 3;

fn window(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// [`harness`] with window 1 focused, its app at the front and counted as
/// prompt after showing every move of one motion in time.
fn prompt_window_1() -> TestHarness {
    let mut harness = harness().with_app(DESKTOP_APP, "com.apple.finder", "Finder", |_| {});
    harness.mock_state.set_write_delay(SLOW_APP, Duration::ZERO);
    harness.world().write_message(focus(Direction::East));
    settle_beside(&mut harness, 0, 1, i32::MAX);
    settle(&mut harness);
    harness
}

/// Runs `operation`, which moves the focused window 1 left, while the slow
/// app holds its writes, asserting that window 0 ahead waits beside it.
fn assert_window_0_waits(harness: &mut TestHarness, operation: Operation) {
    let rest = gap(harness, 0, 1);
    let start = harness.mock_state.ax_frame(1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(window(operation));
    for frame in 0..10 {
        harness.advance(FRAME);
        assert!(
            gap(harness, 0, 1) <= rest,
            "desktop shows between windows 0 and 1 after frame {frame}: {:?} {:?}",
            harness.mock_state.ax_frame(0),
            harness.mock_state.ax_frame(1)
        );
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle_beside(harness, 0, 1, rest);
    settle(harness);

    assert_landed(harness);
    assert!(
        harness.mock_state.ax_frame(1).min.x < start.min.x,
        "window 1 moved left from {start:?}"
    );
}

/// Focus on an empty display brings Finder to the front with only its
/// desktop window, which the engine does not track, so window 1 keeps focus.
/// Its app left the front all the same and can turn slow: window 0 ahead
/// waits for it from the first step of the next move.
#[test]
fn the_window_ahead_waits_for_a_prompt_window_whose_app_leaves_the_front() {
    let mut harness = prompt_window_1();
    harness.mock_state.activate_app(DESKTOP_APP);
    settle(&mut harness);

    assert_window_0_waits(&mut harness, Operation::Center);
}

/// The slow app shows every move in time while Finder is at the front, then
/// comes back to it, as focus back from an empty display brings it, though
/// no tracked window gains or loses focus. Coming to the front can turn it
/// slow: window 0 ahead waits for it from the first step of the next move.
#[test]
fn the_window_ahead_waits_for_a_prompt_window_whose_app_comes_back_to_the_front() {
    let mut harness = prompt_window_1();
    harness.mock_state.activate_app(DESKTOP_APP);
    settle(&mut harness);
    harness.world().write_message(window(Operation::Center));
    settle_beside(&mut harness, 0, 1, i32::MAX);
    settle(&mut harness);
    harness.mock_state.activate_app(SLOW_APP);
    settle(&mut harness);

    assert_window_0_waits(&mut harness, Operation::Maximize);
}

/// Events wake the engine for passes a few milliseconds apart, each a small
/// share of a slide. Window 0 waits for the slow window 1 behind it, then in
/// one such pass moves as far as window 1 shows, rather than easing on from
/// where it waited while window 1 covers it (miri#500).
#[test]
fn a_window_held_for_a_slow_window_keeps_up_once_that_window_shows() {
    let mut harness = harness();
    let before = gap(&harness, 0, 1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    for _ in 0..3 {
        harness.advance(FRAME);
    }
    land_slow_writes(&mut harness);
    harness.mock_state.hold_writes(SLOW_APP);
    harness
        .world()
        .insert_resource(TimeUpdateStrategy::ManualDuration(SHORT_PASS));
    harness.advance(SHORT_PASS);
    harness
        .world()
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME));
    let met = gap(&harness, 0, 1);
    let frames = (
        harness.mock_state.ax_frame(0),
        harness.mock_state.ax_frame(1),
    );
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);
    assert_landed(&mut harness);

    assert_eq!(
        met, before,
        "windows 0 and 1 meet as at rest one pass after window 1 shows its first step: {frames:?}"
    );
}

/// Windows 1 and 2, of `widths`, share a stack right of window 0, and focus
/// moves to the slow window 1. Its app shows nothing for three frames, so
/// it gets only the first step of its slide. Asserts that window 2, which
/// takes each move at once, is asked for the same place as window 1 each
/// frame.
fn assert_a_stack_slides_together(widths: [i32; 3]) {
    let mut harness = harness_with_widths(widths);
    harness.run(vec![
        focus(Direction::Last),
        window(Operation::Stack(Side::West)),
        focus(Direction::North),
        focus(Direction::First),
    ]);
    settle(&mut harness);
    let asked = |harness: &mut TestHarness| {
        (
            engine_frame(harness.world(), 1).min.x,
            engine_frame(harness.world(), 2).min.x,
        )
    };
    let start = asked(&mut harness);
    assert_eq!(start.0, start.1, "windows 1 and 2 share a column");
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::East));
    let mut places = Vec::new();
    for _ in 0..3 {
        harness.advance(FRAME);
        places.push(asked(&mut harness));
    }
    land_slow_writes(&mut harness);
    for _ in 0..200 {
        harness.mock_state.hold_writes(SLOW_APP);
        harness.advance(FRAME);
        places.push(asked(&mut harness));
        land_slow_writes(&mut harness);
        if !moving(harness.world()) {
            break;
        }
    }
    settle(&mut harness);
    assert_landed(&mut harness);

    let end = asked(&mut harness);
    assert!(
        end.0 < start.0,
        "the stack scrolled into view from {start:?} to {end:?}"
    );
    assert!(
        places.iter().all(|(top, bottom)| top == bottom),
        "windows 1 and 2 are asked for the same place each frame: {places:?}"
    );
}

/// A window stacked with one whose app shows late slides with it, rather
/// than on ahead of it (miri#500).
#[test]
fn stacked_windows_slide_together_while_one_of_them_shows_late() {
    assert_a_stack_slides_together([TEST_DISPLAY_WIDTH - 100, WINDOW_WIDTH, WINDOW_WIDTH]);
}

/// The same on a scroll of a few points, which the window that takes its
/// moves at once would finish before the late one shows its first step.
#[test]
fn stacked_windows_stay_together_on_a_short_scroll_while_one_shows_late() {
    assert_a_stack_slides_together([WINDOW_WIDTH, 431, 431]);
}

/// The slow window 1 cannot grow past its width, so stacked with the wider
/// window 0 it parks a few points from it past the left edge. Focus comes
/// back to the stack while window 1's app shows nothing for three frames:
/// the two slide back in no further apart than they parked.
#[test]
fn stacked_windows_of_two_widths_slide_together_while_one_shows_late() {
    let narrow = WINDOW_WIDTH - 2;
    let mut harness = harness_with_widths([WINDOW_WIDTH, narrow, 2 * WINDOW_WIDTH]);
    let world = harness.world();
    let entity = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .find(|(_, window)| window.id() == 1)
        .expect("window 1")
        .0;
    world.entity_mut(entity).insert(WidthLimits {
        min: None,
        max: Some(narrow),
    });
    harness.run(vec![
        focus(Direction::East),
        window(Operation::Stack(Side::West)),
        focus(Direction::Last),
    ]);
    settle(&mut harness);
    let asked =
        |harness: &mut TestHarness| [0, 1].map(|id| engine_frame(harness.world(), id).min.x);
    let start = asked(&mut harness);
    let parked_apart = (start[0] - start[1]).abs();
    assert!(
        parked_apart > 0,
        "windows 0 and 1 park at different places: {start:?}"
    );
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::First));
    let mut places = Vec::new();
    for _ in 0..3 {
        harness.advance(FRAME);
        places.push(asked(&mut harness));
    }
    land_slow_writes(&mut harness);
    for _ in 0..200 {
        harness.mock_state.hold_writes(SLOW_APP);
        harness.advance(FRAME);
        places.push(asked(&mut harness));
        land_slow_writes(&mut harness);
        if !moving(harness.world()) {
            break;
        }
    }
    settle(&mut harness);
    assert_landed(&mut harness);

    let end = asked(&mut harness);
    assert!(
        start[1] < end[1],
        "the stack scrolled into view from {start:?} to {end:?}"
    );
    assert!(
        places
            .iter()
            .all(|[wide, narrow]| (wide - narrow).abs() <= parked_apart),
        "windows 0 and 1 stay within {parked_apart} points of each other: {places:?}"
    );
}

/// Window 2 joins window 1's column while the row scrolls, so the two head
/// for the same place from opposite sides. Each slides straight there, and
/// neither is put where the other is.
#[test]
fn windows_that_meet_in_a_stack_from_both_sides_each_slide_straight_there() {
    let mut harness = harness();
    harness.run(vec![focus(Direction::Last)]);
    settle(&mut harness);
    let asked =
        |harness: &mut TestHarness| [1, 2].map(|id| engine_frame(harness.world(), id).min.x);
    let start = asked(&mut harness);

    harness
        .world()
        .write_message(window(Operation::Stack(Side::West)));
    let mut places = vec![start];
    for _ in 0..200 {
        harness.advance(FRAME);
        places.push(asked(&mut harness));
        if !moving(harness.world()) {
            break;
        }
    }
    settle(&mut harness);
    assert_landed(&mut harness);

    let end = asked(&mut harness);
    assert_eq!(end[0], end[1], "windows 1 and 2 share a column");
    assert!(
        start[0] < end[0] && end[1] < start[1],
        "windows 1 and 2 meet from opposite sides: {start:?} to {end:?}"
    );
    assert!(
        places
            .windows(2)
            .all(|pair| pair[0][0] <= pair[1][0] && pair[1][1] <= pair[0][1]),
        "window 1 only moves right and window 2 only moves left: {places:?}"
    );
}

/// Windows 0 and 1 share a stack, and window 2 joins it from the right.
/// Window 1 only moves up to make room, so its app shows no move sideways.
/// Window 2 does not wait for one: it slides in from its first frame.
#[test]
fn a_window_joining_a_stack_does_not_wait_for_one_that_only_moves_up() {
    let mut harness = harness_with_widths([300; 3]);
    harness.run(vec![
        focus(Direction::East),
        window(Operation::Stack(Side::West)),
        focus(Direction::Last),
    ]);
    settle(&mut harness);
    let mut places = vec![engine_frame(harness.world(), 2).min.x];

    harness
        .world()
        .write_message(window(Operation::Stack(Side::West)));
    for _ in 0..4 {
        harness.advance(FRAME);
        places.push(engine_frame(harness.world(), 2).min.x);
    }

    assert!(
        places.windows(2).all(|pair| pair[1] < pair[0]),
        "window 2 moves left each frame: {places:?}"
    );
    settle(&mut harness);
    assert_landed(&mut harness);
}

/// At a slow animation speed, the first step of a six-point scroll rounds
/// to no move at all. The slow window 1 could never show such a step, so it
/// takes its slide instead and still scrolls in.
#[test]
fn a_slow_window_whose_first_step_rounds_to_no_move_still_scrolls_in() {
    let mut harness =
        harness_with_widths([WINDOW_WIDTH, TEST_DISPLAY_WIDTH - WINDOW_WIDTH + 6, 400]);
    let start = engine_frame(harness.world(), 1).min.x;
    // The row settles at the usual speed, then scrolls at a slow one.
    let slow: Config = (
        MainOptions {
            animation_speed: Some(5.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    harness.world().insert_resource(slow);

    harness.world().write_message(focus(Direction::East));
    let mut places = Vec::new();
    // A motion at speed 5 lasts 0.91 s, 46 frames.
    for _ in 0..60 {
        harness.advance(FRAME);
        land_slow_writes(&mut harness);
        places.push(engine_frame(harness.world(), 1).min.x);
        if !moving(harness.world()) {
            break;
        }
    }

    assert!(
        !moving(harness.world()),
        "window 1 finishes its scroll from {start}: {places:?}"
    );
    assert_eq!(engine_frame(harness.world(), 1).min.x, start - 6);
    assert_landed(&mut harness);
}

/// Focus moves from window 0 to window 2, whose app takes each move at once,
/// and the row scrolls left with the slow window 1 ahead of window 2. Window
/// 1 shows nothing for four frames. It keeps its slide beside window 2 all
/// the same, rather than only its first step: window 2 does not wait for
/// it, so it would stay under window 2 for the rest of the slide (miri#500).
#[test]
fn a_slow_window_keeps_its_slide_ahead_of_a_window_that_moves_on() {
    let mut harness = harness();
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::Last));
    let mut covers = Vec::new();
    let mut places = Vec::new();
    for _ in 0..4 {
        harness.advance(FRAME);
        let behind = engine_frame(harness.world(), 2).min.x;
        covers.push(engine_frame(harness.world(), 1).max.x - behind);
        places.push(behind);
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);
    assert_landed(&mut harness);

    assert!(
        places.windows(2).all(|pair| pair[1] < pair[0]),
        "window 2 moves on each frame: {places:?}"
    );
    assert!(
        covers.windows(2).all(|pair| pair[1] <= pair[0]),
        "window 2 is asked no further over window 1 from frame to frame: {covers:?}"
    );
}

/// Window 0 is tucked behind the slow window 1, and its app reports it 3
/// points from where the engine asked, as apps do. Focus moves to window 1,
/// which leads the row in, and the engine puts window 0 back as the row
/// moves. That shift is no slide for window 1 to follow: while its app
/// shows nothing, window 1 gets only its first step (miri#500).
#[test]
fn a_window_that_leads_the_row_in_keeps_its_first_step_while_one_behind_it_only_shifts() {
    let mut harness = harness_with_widths([1016, 508, 1016]);
    harness.world().write_message(focus(Direction::Last));
    settle_beside(&mut harness, 1, 2, i32::MAX);
    settle(&mut harness);
    let tucked = harness.mock_state.ax_frame(0).min;
    harness
        .mock_state
        .os_move_window(0, tucked + IVec2::new(3, 0));
    for _ in 0..5 {
        harness.advance(FRAME);
    }
    let start = engine_frame(harness.world(), 1).min.x;
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(focus(Direction::West));
    let mut places = Vec::new();
    for _ in 0..4 {
        harness.advance(FRAME);
        places.push(engine_frame(harness.world(), 1).min.x);
    }
    harness.mock_state.release_writes(SLOW_APP);
    settle(&mut harness);
    assert_landed(&mut harness);

    assert!(
        places[0] > start,
        "window 1 takes a first step from {start}: {places:?}"
    );
    assert!(
        places.iter().all(|&place| place == places[0]),
        "window 1 waits at its first step while its app shows nothing: {places:?}"
    );
}

/// The engine frame of each of windows 0, 1 and 2.
fn engine_frames(world: &mut World) -> Vec<IRect> {
    (0..3).map(|id| engine_frame(world, id)).collect()
}

/// A width command moves the row to its new layout without easing there:
/// once each app has shown the first frame, every window has its final
/// frame. The slow window 1 still sets when its neighbours may follow.
#[test]
fn a_resize_takes_its_new_layout_at_once() {
    let mut harness = harness();
    let start = engine_frames(harness.world());

    harness
        .world()
        .write_message(window(Operation::Resize(ResizeDirection::Grow)));
    harness.advance(FRAME);
    land_slow_writes(&mut harness);
    harness.advance(FRAME);
    let early = engine_frames(harness.world());
    settle(&mut harness);
    assert_landed(&mut harness);

    let end = engine_frames(harness.world());
    assert_ne!(start, end, "the resize changed the row");
    assert_eq!(early, end, "the row had its new layout after two frames");
}

/// A motion after a resize, such as a focus move, still eases.
#[test]
fn a_focus_move_after_a_resize_still_eases() {
    let mut harness = harness();
    harness
        .world()
        .write_message(window(Operation::Resize(ResizeDirection::Grow)));
    settle(&mut harness);

    harness.world().write_message(focus(Direction::Last));
    harness.advance(FRAME);
    land_slow_writes(&mut harness);
    harness.advance(FRAME);
    assert!(moving(harness.world()), "the focus move eases over frames");
    settle(&mut harness);
    assert_landed(&mut harness);
}
