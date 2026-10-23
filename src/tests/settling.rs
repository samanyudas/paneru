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

/// A flick past the end of the strip lands on the last column, and the strip
/// end comes back to the viewport edge. Continuous swipe lets the fingers
/// pull the last column to the far edge, but no settle leaves empty space
/// beside an overflowing strip.
#[test]
fn swipe_past_the_end_settles_with_the_strip_end_on_the_viewport_edge() {
    for animation_speed in [None, Some(14.0)] {
        let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], animation_speed);
        // Above every window, so focus follows mouse never picks a window the
        // strip slides under the pointer.
        harness.mock_state.set_cursor_position(IVec2::new(1000, 5));

        assert_focused!(harness.world(), 0);
        send(&mut harness, swipe(0.3));
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "the settle has finished");
        assert_focused!(harness.world(), 4);
        assert_eq!(window_x(harness.world(), 4), TEST_DISPLAY_WIDTH - 400);
    }
}

#[test]
fn application_termination_during_swipe_preserves_landing() {
    for animation_speed in [None, Some(14.0)] {
        let run = |terminate: bool| {
            let mut harness = TestHarness::new()
                .with_config(miri_config(animation_speed))
                .with_app(2, "background", "Background", |_| {});
            let state = harness.mock_state.clone();
            let windows = (0..5)
                .map(|id| {
                    state.spawn_window(
                        if id == 1 { 2 } else { TEST_PROCESS_ID },
                        TEST_WORKSPACE_ID,
                        id,
                        IRect::new(id * 400, 24, id * 400 + 400, 764),
                    )
                })
                .collect();
            harness.existed_at_launch(0..5);
            harness
                .world()
                .trigger(crate::ecs::SpawnWindowTrigger(windows));
            harness.run(vec![
                Event::MenuOpened { window_id: 0 },
                focus(Direction::First),
            ]);
            state.set_cursor_position(IVec2::new(1000, 5));

            send(&mut harness, Event::TouchpadDown);
            send(&mut harness, swipe(0.6));
            harness.advance(ONE_FRAME * 2);
            assert_focused!(harness.world(), 0);
            assert!(
                state.create_window(0).frame().min.x < -350,
                "the old focused window must be parked off-screen, got {:?}",
                state.create_window(0).frame()
            );
            if terminate {
                send(
                    &mut harness,
                    Event::ApplicationTerminated {
                        psn: crate::platform::ProcessSerialNumber { high: 0, low: 2 },
                    },
                );
            }
            harness.advance(ONE_FRAME);
            send(&mut harness, Event::TouchpadUp);
            harness.advance(SETTLE);

            let frames = [0, 2, 3, 4].map(|id| state.create_window(id).frame());
            (harness, frames)
        };
        let (mut control, expected) = run(false);
        assert_focused!(control.world(), 4);
        assert_eq!(expected[3].max.x, TEST_DISPLAY_WIDTH);
        let (mut terminated, actual) = run(true);
        assert_eq!(
            actual, expected,
            "termination changed the swipe landing; animation_speed={animation_speed:?}"
        );
        assert_focused!(terminated.world(), 4);
        assert_eq!(
            terminated.mock_state.focused_window_id(TEST_PROCESS_ID),
            Some(4)
        );
        let world = terminated.world();
        assert!(
            world
                .query::<&Window>()
                .iter(world)
                .all(|window| window.id() != 1)
        );
    }
}

fn terminate_predecessor_after_landing_focus() -> TestHarness {
    let mut harness = TestHarness::new()
        .with_config(miri_config(Some(14.0)))
        .with_app(2, "predecessor", "Predecessor", |_| {});
    let state = harness.mock_state.clone();
    let windows = (0..4)
        .map(|id| {
            state.spawn_window(
                if id == 0 { 2 } else { TEST_PROCESS_ID },
                TEST_WORKSPACE_ID,
                id,
                IRect::new(id * 700, 24, id * 700 + 700, 764),
            )
        })
        .collect();
    harness.existed_at_launch(0..4);
    harness
        .world()
        .trigger(crate::ecs::SpawnWindowTrigger(windows));
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus(Direction::First),
    ]);
    state.set_cursor_position(IVec2::new(1000, 5));

    send(&mut harness, swipe(0.05));
    for _ in 0..50 {
        harness.advance(ONE_FRAME);
        if state.focused_window_id(TEST_PROCESS_ID) == Some(1) {
            break;
        }
    }
    harness.advance(ONE_FRAME * 2);
    assert_focused!(harness.world(), 1);
    assert_eq!(state.focused_window_id(TEST_PROCESS_ID), Some(1));
    let world = harness.world();
    assert_eq!(
        world
            .query_filtered::<Entity, (
                With<crate::ecs::FocusedMarker>,
                Or<(
                    With<crate::ecs::EnsureVisibleMarker>,
                    With<crate::ecs::ReshuffleAroundMarker>,
                )>,
            )>()
            .iter(world)
            .count(),
        0,
        "landing focus has no pending layout request before removal"
    );
    assert!(
        is_scrolling(harness.world()),
        "landing focus selected during scrolling"
    );
    send(
        &mut harness,
        Event::ApplicationTerminated {
            psn: crate::platform::ProcessSerialNumber { high: 0, low: 2 },
        },
    );
    harness.advance(ONE_FRAME);
    assert!(
        is_scrolling(harness.world()),
        "removal occurred during settling"
    );
    harness
}

fn assert_native_focus_inside_viewport(harness: &mut TestHarness, id: WinID) {
    assert_focused!(harness.world(), id);
    assert_eq!(
        harness.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(id)
    );
    let frame = harness.mock_state.create_window(id).frame();
    let viewport = IRect::new(
        0,
        TEST_MENUBAR_HEIGHT,
        TEST_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT,
    );
    assert_eq!(frame.size(), IVec2::new(700, 748));
    assert!(
        frame.min.cmpge(viewport.min).all() && frame.max.cmple(viewport.max).all(),
        "focused survivor must be fully inside viewport, got {frame:?}"
    );
}

#[test]
fn application_termination_after_landing_focus_keeps_survivor_visible() {
    let mut harness = terminate_predecessor_after_landing_focus();
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_native_focus_inside_viewport(&mut harness, 1);
}

#[test]
fn removal_during_settle_uses_current_focus_after_keyboard_switch() {
    let mut harness = terminate_predecessor_after_landing_focus();
    send(&mut harness, focus(Direction::Last));
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_native_focus_inside_viewport(&mut harness, 3);
    assert_eq!(window_x(harness.world(), 3), TEST_DISPLAY_WIDTH - 700);
}

#[test]
fn removal_during_settle_preserves_workspace_restore_placement() {
    let mut harness = terminate_predecessor_after_landing_focus();
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::VirtualNumber(1)),
    }]);
    harness.advance(SETTLE);
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::VirtualNumber(0)),
    }]);
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 1);
    assert_eq!(
        harness.mock_state.focused_window_id(TEST_PROCESS_ID),
        Some(1)
    );
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 1400);
}

/// The mirror case: a flick back past the start lands on the first column
/// with the strip start on the left viewport edge.
#[test]
fn swipe_past_the_start_settles_with_the_strip_start_on_the_viewport_edge() {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400], None);
    harness.mock_state.set_cursor_position(IVec2::new(1000, 5));
    harness.run(vec![focus(Direction::Last)]);
    assert_eq!(window_x(harness.world(), 4), TEST_DISPLAY_WIDTH - 400);

    send(&mut harness, swipe(-0.3));
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 0);
    assert_eq!(window_x(harness.world(), 0), 0);
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

/// A strip that stops being active mid-settle, as when focus moves to another
/// display, lands at once instead of keeping a settle no system drives.
#[test]
fn a_strip_that_loses_activation_mid_settle_lands() {
    let mut harness = harness_with_widths(&[700, 700, 700, 700], Some(14.0));
    send(&mut harness, swipe(0.05));
    harness.advance(ONE_FRAME * 6);

    let world = harness.app.world_mut();
    let (strip, target) = world
        .query_filtered::<(Entity, &Scrolling), With<crate::ecs::ActiveWorkspaceMarker>>()
        .iter(world)
        .find_map(|(entity, scroll)| scroll.settle.map(|target| (entity, target)))
        .expect("still settling");
    world
        .entity_mut(strip)
        .remove::<crate::ecs::ActiveWorkspaceMarker>();
    world.flush();

    assert!(
        world.get::<Scrolling>(strip).is_none(),
        "no settle is left behind"
    );
    assert_eq!(
        world.get::<crate::ecs::Position>(strip).unwrap().x,
        crate::util::round_px(target)
    );
}

fn swipe_and_maybe_switch_away(switch: bool) -> i32 {
    let mut harness = harness_with_widths(&[400, 400, 400, 400, 400, 400], Some(14.0));
    harness.mock_state.set_cursor_position(IVec2::new(1000, 5));
    send(&mut harness, swipe(0.05));
    // Late enough that the strip already shows the landed column fully.
    harness.advance(ONE_FRAME * 10);
    assert!(is_scrolling(harness.world()), "still settling");
    if switch {
        harness.run(vec![
            Event::Command {
                command: Command::Window(Operation::VirtualNumber(1)),
            },
            Event::Command {
                command: Command::Window(Operation::VirtualNumber(0)),
            },
        ]);
    }
    harness.advance(SETTLE);
    window_x(harness.world(), 0)
}

/// Switching virtual workspaces mid-settle returns the strip to the landing,
/// even when the landed column is already fully visible where the settle was
/// interrupted.
#[test]
fn virtual_switch_mid_settle_keeps_a_fully_visible_landing() {
    assert_eq!(
        swipe_and_maybe_switch_away(true),
        swipe_and_maybe_switch_away(false)
    );
}
