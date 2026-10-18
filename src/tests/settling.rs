//! Settling the strip after a swipe, and the minimal shift that keyboard focus
//! makes, with Miri's configuration: `auto_center` unset.

use std::time::Duration;

use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::Scrolling;
use crate::events::Event;
use crate::manager::Window;
use crate::platform::WinID;

use super::*;

/// Long enough for any settle or strip animation to finish.
const SETTLE: Duration = Duration::from_secs(3);

/// A frame after the input, before the fingers count as lifted.
const ONE_FRAME: Duration = Duration::from_millis(20);

fn miri_config(animation_speed: Option<f64>) -> Config {
    (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            animation_speed,
            ..Default::default()
        },
        vec![],
    )
        .into()
}

fn harness_with_widths(widths: &[i32], animation_speed: Option<f64>) -> TestHarness {
    let mut harness = TestHarness::new().with_config(miri_config(animation_speed));
    for (id, width) in (0..).zip(widths.iter().copied()) {
        harness = harness.with_window(id, move |window| {
            window.frame.max.x = window.frame.min.x + width;
        });
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus(Direction::First),
    ]);
    harness
}

fn focus(direction: Direction) -> Event {
    Event::Command {
        command: Command::Window(Operation::Focus(direction)),
    }
}

fn swipe(delta: f64) -> Event {
    Event::Swipe { delta, fingers: 3 }
}

fn send(harness: &mut TestHarness, event: Event) {
    harness.world().write_message::<Event>(event);
}

fn window_x(world: &mut World, id: WinID) -> i32 {
    let mut query = world.query::<&Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .frame()
        .min
        .x
}

fn is_scrolling(world: &mut World) -> bool {
    world.query::<&Scrolling>().iter(world).next().is_some()
}

/// A flick that stops with the viewport center in a partly hidden column
/// lands on that column: the strip moves only until the column's right edge
/// meets the viewport edge, and the column takes focus.
#[test]
fn swipe_settles_a_partly_hidden_column_flush_with_the_viewport_edge() {
    // Miri animates at 14; unset means instant. The focus change the settle
    // makes must not start a strip animation that fights it either way.
    for animation_speed in [None, Some(14.0)] {
        let mut harness = harness_with_widths(&[700, 700, 700, 700], animation_speed);
        assert_eq!(window_x(harness.world(), 0), 0);

        send(&mut harness, swipe(0.05));
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "the settle has finished");
        assert_focused!(harness.world(), 1);
        assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
        assert_eq!(window_x(harness.world(), 0), TEST_DISPLAY_WIDTH - 1400);
    }
}

/// A flick toward the end of the strip lands where the strip's own scroll
/// bound stops it, on the last column, which is fully visible and so does not
/// move again.
#[test]
fn swipe_past_the_end_settles_on_the_last_column_where_the_flick_stops() {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], None);
    // Above every window, so focus follows mouse never picks a window the
    // strip slides under the pointer.
    harness.mock_state.set_cursor_position(IVec2::new(1000, 5));

    assert_focused!(harness.world(), 0);
    send(&mut harness, swipe(0.3));
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 4);
    assert_eq!(window_x(harness.world(), 4), 0);
}

/// Landing on a stack focuses the window in it that had focus last, not the
/// stack's top window.
#[test]
fn swipe_onto_a_stack_focuses_its_last_focused_window() {
    let mut harness = harness_with_widths(&[700, 700, 400, 400], None);
    harness.run(vec![
        focus(Direction::East),
        // Column 0 becomes [0, 1], with window 1 focused at the bottom.
        Event::Command {
            command: Command::Window(Operation::Stack(Side::West)),
        },
        focus(Direction::Last),
    ]);
    assert_focused!(harness.world(), 3);
    assert_eq!(window_x(harness.world(), 3), TEST_DISPLAY_WIDTH - 400);

    send(&mut harness, swipe(-0.1));
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 1);
    let x = window_x(harness.world(), 1);
    assert!(
        x >= 0 && x + 700 <= TEST_DISPLAY_WIDTH,
        "the stack is fully visible, got x {x}"
    );
}

/// A strip that fits the display gives a little under the fingers, never
/// following them freely, then springs back to where it was. Focus stays.
#[test]
fn swipe_on_a_fitting_strip_resists_and_springs_back() {
    let mut harness = harness_with_widths(&[400, 400], None);
    assert_focused!(harness.world(), 0);

    send(&mut harness, swipe(0.3));
    harness.advance(ONE_FRAME * 2);
    let stretched = window_x(harness.world(), 0);
    assert!(
        stretched < 0 && stretched > -TEST_DISPLAY_WIDTH / 8,
        "the strip gives a little with resistance, got x {stretched}"
    );

    harness.advance(SETTLE);
    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_eq!(window_x(harness.world(), 0), 0);
    assert_eq!(window_x(harness.world(), 1), 400);
    assert_focused!(harness.world(), 0);
}

/// Modifier-scroll with a mouse wheel arrives as separate ticks. A short gap
/// between ticks does not settle the strip. After a longer pause the strip
/// settles on the column nearest the center, which is fully visible here, so
/// the strip stays where the ticks left it.
#[test]
fn wheel_scroll_settles_after_a_short_pause() {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], None);

    send(&mut harness, Event::Scroll { delta: 30.0 });
    harness.advance(Duration::from_millis(100));
    assert!(is_scrolling(harness.world()), "a 100 ms gap is not a pause");
    assert_focused!(harness.world(), 0);
    send(&mut harness, Event::Scroll { delta: 30.0 });
    harness.advance(ONE_FRAME);
    let scrolled = window_x(harness.world(), 2);
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 2);
    assert_eq!(scrolled, 146);
    assert_eq!(window_x(harness.world(), 2), 146, "fully visible: no move");
}

/// Keyboard focus moves the strip only as far as needed to show the newly
/// focused column fully, and not at all when it is already fully visible.
#[test]
fn keyboard_focus_moves_the_strip_only_to_show_the_column() {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], None);
    let step = |harness: &mut TestHarness, direction: Direction| {
        harness.run(vec![focus(direction)]);
        harness.advance(SETTLE);
    };

    step(&mut harness, Direction::East);
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), 400, "fully visible: no move");

    step(&mut harness, Direction::East);
    assert_focused!(harness.world(), 2);
    assert_eq!(window_x(harness.world(), 2), 624, "right edge on the edge");

    step(&mut harness, Direction::East);
    assert_eq!(window_x(harness.world(), 3), 624, "right edge on the edge");

    step(&mut harness, Direction::West);
    assert_focused!(harness.world(), 2);
    assert_eq!(window_x(harness.world(), 2), 224, "fully visible: no move");

    step(&mut harness, Direction::Last);
    assert_eq!(window_x(harness.world(), 4), 624, "right edge on the edge");

    step(&mut harness, Direction::Nth(2));
    assert_focused!(harness.world(), 2);
    assert_eq!(window_x(harness.world(), 2), 0, "left edge on the edge");

    step(&mut harness, Direction::First);
    assert_eq!(window_x(harness.world(), 0), 0, "left edge on the edge");
}

/// Rapid keyboard focus during a strip animation lands where slow presses
/// would: each press measures against where the strip is going.
#[test]
fn rapid_keyboard_focus_during_animation_matches_slow_presses() {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], Some(14.0));
    for _ in 0..3 {
        send(&mut harness, focus(Direction::East));
        harness.advance(ONE_FRAME * 3);
    }
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 3);
    assert_eq!(window_x(harness.world(), 3), 624);
}

/// Window centering stays an explicit command that centers on demand.
#[test]
fn center_command_still_centers() {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], None);
    harness.run(vec![
        focus(Direction::East),
        Event::Command {
            command: Command::Window(Operation::Center),
        },
    ]);
    harness.advance(SETTLE);

    assert_eq!(window_x(harness.world(), 1), (TEST_DISPLAY_WIDTH - 400) / 2);
}

/// Switching virtual workspaces mid-settle keeps the landing: the strip
/// comes back where the settle was headed, not where it was caught.
#[test]
fn virtual_switch_mid_settle_restores_the_landing() {
    // Only an animated settle can be caught partway.
    let mut harness = harness_with_widths(&[700, 700, 700, 700], Some(14.0));

    send(&mut harness, swipe(0.05));
    harness.advance(ONE_FRAME * 6);
    assert!(is_scrolling(harness.world()), "still settling");
    harness.run(vec![
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
    ]);
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
}

/// Fingers that touch down mid-settle catch the strip where it is, and
/// lifting them again settles from there.
#[test]
fn touch_down_mid_settle_catches_the_strip() {
    // Only an animated settle can be caught partway.
    let mut harness = harness_with_widths(&[700, 700, 700, 700], Some(14.0));

    send(&mut harness, swipe(0.05));
    harness.advance(ONE_FRAME * 12);
    assert!(is_scrolling(harness.world()), "still settling");
    send(&mut harness, Event::TouchpadDown);
    harness.advance(ONE_FRAME);
    let caught = window_x(harness.world(), 1);
    harness.advance(ONE_FRAME);
    assert_eq!(
        window_x(harness.world(), 1),
        caught,
        "the strip holds still"
    );

    harness.advance(SETTLE);
    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
}

/// Without an animation speed (Reduce Motion or Miri's disable-animation
/// setting), the settle is instant: the strip is on its landing as soon as the
/// swipe ends, not partway along a spring.
#[test]
fn swipe_settles_instantly_without_animation() {
    let mut harness = harness_with_widths(&[700, 700, 700, 700], None);
    send(&mut harness, swipe(0.05));
    harness.advance(std::time::Duration::from_millis(100));

    assert!(!is_scrolling(harness.world()), "the settle is instant");
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
}
