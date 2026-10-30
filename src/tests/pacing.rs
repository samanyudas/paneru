//! A row scrolls at the pace of its slowest window: the window ahead in the
//! direction of motion never leaves the desktop showing between itself and
//! a slower window behind it.

use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::{RepositionMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::Window;
use crate::platform::{Pid, WinID};

use super::*;

/// Owns window 1, which takes its moves late, like Helium.
const SLOW_APP: Pid = TEST_PROCESS_ID;
/// Owns windows 0 and 2, and takes each move before the call returns.
const FAST_APP: Pid = 2;
/// Owns no window the engine tracks, like Finder showing only the desktop.
const DESKTOP_APP: Pid = 3;

/// Three of them fill more than the display.
const WINDOW_WIDTH: i32 = 600;
/// How long each of the slow app's writes takes, so that it shows each move
/// a frame or more late.
const SLOW_WRITE: Duration = Duration::from_millis(40);
const FRAME: Duration = Duration::from_millis(20);
const FLUSH: Duration = Duration::from_secs(5);

/// Windows 0, 1 and 2 in a row, at rest with window 0 at the display's left
/// edge.
fn harness() -> TestHarness {
    harness_with_widths([WINDOW_WIDTH; 3])
}

/// [`harness`], with windows 0, 1 and 2 `widths` wide.
fn harness_with_widths(widths: [i32; 3]) -> TestHarness {
    let config: Config = (
        MainOptions {
            animation_speed: Some(14.0),
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

/// Runs frames at the speed the slow app takes its moves until the row
/// settles, asserting after each that no more desktop than `rest` shows
/// between windows `left` and `right`.
fn settle_beside(harness: &mut TestHarness, left: WinID, right: WinID, rest: i32) {
    for frame in 0..200 {
        harness.advance(FRAME);
        std::thread::sleep(FRAME);
        assert!(
            gap(harness, left, right) <= rest,
            "desktop shows between windows {left} and {right} {frame} frames after the release: {:?} {:?}",
            harness.mock_state.ax_frame(left),
            harness.mock_state.ax_frame(right)
        );
        if !moving(harness.world()) && harness.mock_state.flush_writes(Duration::ZERO) {
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

/// Focus on an empty display brings Finder to the front with only its
/// desktop window, which the engine does not track, so window 1 keeps focus.
/// Focus back on window 1 brings its app to the front again, where it can
/// turn slow: window 0 ahead waits for it from the first step.
#[test]
fn the_window_ahead_waits_for_a_prompt_window_whose_app_comes_back_to_the_front() {
    let mut harness = harness().with_app(DESKTOP_APP, "com.apple.finder", "Finder", |_| {});
    harness.mock_state.set_write_delay(SLOW_APP, Duration::ZERO);
    harness.world().write_message(focus(Direction::East));
    settle_beside(&mut harness, 0, 1, i32::MAX);
    settle(&mut harness);
    for app in [DESKTOP_APP, SLOW_APP] {
        harness.mock_state.activate_app(app);
        settle(&mut harness);
    }
    let rest = gap(&harness, 0, 1);
    let start = harness.mock_state.ax_frame(1);
    harness.mock_state.hold_writes(SLOW_APP);

    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Center),
    });
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
        harness.mock_state.ax_frame(1).min.x < start.min.x,
        "window 1 moved left to the center from {start:?}"
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
