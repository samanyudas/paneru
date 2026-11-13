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
use crate::platform::{WinID, WorkspaceId};

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
    harness_with_config(widths, miri_config(animation_speed))
}

fn harness_with_config(widths: &[i32], config: Config) -> TestHarness {
    let mut harness = TestHarness::new().with_config(config);
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
    Event::Swipe {
        delta,
        fingers: 3,
        distance_cm: 0.0,
    }
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

/// Fingers that touch down mid-settle catch the strip where it is, and
/// lifting them again settles from there.
#[test]
fn touch_down_mid_settle_catches_the_strip() {
    // Only an animated settle can be caught partway.
    let mut harness = harness_with_widths(&[700, 700, 700, 700], Some(14.0));

    send(&mut harness, swipe(0.05));
    // Past the middle of the 0.5 s ease, so the strip is nearer column 1.
    harness.advance(ONE_FRAME * 16);
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
/// display, still settles: the settle owns the strip until it lands.
#[test]
fn a_strip_that_loses_activation_mid_settle_still_settles() {
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
        world.get::<Scrolling>(strip).is_some(),
        "the settle goes on"
    );

    harness.advance(SETTLE);
    let world = harness.world();
    assert!(
        world.get::<Scrolling>(strip).is_none(),
        "no settle is left behind"
    );
    assert_eq!(
        world.get::<crate::ecs::Position>(strip).unwrap().x,
        crate::util::round_px(target)
    );
}

/// The external display, right of the test display.
fn ext_bounds() -> IRect {
    IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    )
}

/// Above the windows on the test display, so focus follows mouse would never
/// pick a window a strip slides under the pointer.
const POINTER_ON_TEST: IVec2 = IVec2::new(500, 5);
/// Above the windows on the external display.
const POINTER_ON_EXT: IVec2 = IVec2::new(TEST_DISPLAY_WIDTH + 900, 5);

/// Five 400-point windows, 0 to 4, on the test display and five 600-point
/// windows, 100 to 104, on the external display, each strip at its first
/// column, with window 0 focused and the test display active. The external
/// display shows `ext_spaces[0]`.
fn two_displays(config: Config, ext_spaces: Vec<WorkspaceId>) -> TestHarness {
    let mut harness = TestHarness::new().with_config(config).with_display(
        EXT_DISPLAY_ID,
        ext_bounds(),
        ext_spaces,
    );
    for id in 0..5 {
        harness = harness.with_workspace_window(id, TEST_WORKSPACE_ID, |window| {
            let x = id * 400;
            window.frame = IRect::new(x, TEST_MENUBAR_HEIGHT, x + 400, TEST_DISPLAY_HEIGHT);
        });
    }
    for id in 100..105 {
        harness = harness.with_workspace_window(id, EXT_WORKSPACE_ID, |window| {
            let x = TEST_DISPLAY_WIDTH + (id - 100) * 600;
            window.frame = IRect::new(x, TEST_MENUBAR_HEIGHT, x + 600, EXT_DISPLAY_HEIGHT);
        });
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus(Direction::First),
    ]);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 0);
    assert_eq!(window_x(harness.world(), 0), 0);
    assert_eq!(window_x(harness.world(), 100), TEST_DISPLAY_WIDTH);
    harness
}

fn swipe_target_config(target: &str, animation_speed: Option<f64>) -> Config {
    let speed = animation_speed.map_or(String::new(), |speed| {
        format!("animation_speed = {speed}\n")
    });
    Config::try_from(
        format!(
            "[options]\n{speed}[swipe.gesture]\nfingers_count = 3\ntarget = \"{target}\"\n[bindings]\n"
        )
        .as_str(),
    )
    .expect("config should parse")
}

/// The right edge of the external display, where its strip ends once it
/// settles on its last column.
const EXT_LAST_COLUMN_X: i32 = TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH - 600;

/// A swipe moves the strip on the display under the pointer, not the strip
/// with focus, and the window it lands on takes focus.
#[test]
fn swipe_moves_the_strip_under_the_pointer() {
    for animation_speed in [None, Some(14.0)] {
        let mut harness = two_displays(
            swipe_target_config("pointer", animation_speed),
            vec![EXT_WORKSPACE_ID],
        );
        harness.mock_state.set_cursor_position(POINTER_ON_EXT);

        send(&mut harness, swipe(0.3));
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "the settle has finished");
        assert_eq!(window_x(harness.world(), 0), 0, "the focused strip stays");
        assert_eq!(window_x(harness.world(), 104), EXT_LAST_COLUMN_X);
        assert_focused!(harness.world(), 104);
    }
}

/// With `target = "focused"`, a swipe moves the strip with focus, wherever
/// the pointer is.
#[test]
fn swipe_with_focused_target_moves_the_focused_strip() {
    let mut harness = two_displays(swipe_target_config("focused", None), vec![EXT_WORKSPACE_ID]);
    harness.mock_state.set_cursor_position(POINTER_ON_EXT);

    send(&mut harness, swipe(0.3));
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_eq!(
        window_x(harness.world(), 100),
        TEST_DISPLAY_WIDTH,
        "the strip under the pointer stays"
    );
    assert_eq!(window_x(harness.world(), 4), TEST_DISPLAY_WIDTH - 400);
    assert_focused!(harness.world(), 4);
}

/// miri#443: focus moves to the other display while the fingers still hold a
/// swipe past the end of the strip. The swiped strip still settles with its
/// end on the viewport edge, and no swipe is left on it.
#[test]
fn swipe_settles_when_the_active_display_changes_mid_swipe() {
    for animation_speed in [None, Some(14.0)] {
        let mut harness = two_displays(
            swipe_target_config("pointer", animation_speed),
            vec![EXT_WORKSPACE_ID],
        );
        harness.mock_state.set_cursor_position(POINTER_ON_TEST);

        send(&mut harness, Event::TouchpadDown);
        harness.advance(ONE_FRAME);
        // Far enough to pull the last column to the far edge.
        for frame in 0..15 {
            if frame == 10 {
                harness.mock_state.focus_window(100);
                harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
            }
            send(&mut harness, swipe(0.2));
            harness.advance(ONE_FRAME);
        }
        assert_focused!(harness.world(), 100);
        assert_eq!(
            harness.mock_state.active_display(),
            EXT_DISPLAY_ID,
            "the other display is active"
        );
        assert!(
            window_x(harness.world(), 4) < TEST_DISPLAY_WIDTH - 400,
            "the fingers pulled the strip past its end"
        );
        send(&mut harness, Event::TouchpadUp);
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "no swipe is left behind");
        assert_eq!(
            window_x(harness.world(), 4),
            TEST_DISPLAY_WIDTH - 400,
            "the strip end is on the viewport edge"
        );
        assert_eq!(window_x(harness.world(), 100), TEST_DISPLAY_WIDTH);
        // The switch to the other display during the swipe stands.
        assert_focused!(harness.world(), 100);
    }
}

/// miri#443, inside the pause before a lifted swipe settles: focus moves to
/// the other display before the swipe counts as ended.
#[test]
fn swipe_settles_when_the_active_display_changes_before_the_settle() {
    for animation_speed in [None, Some(14.0)] {
        let mut harness = two_displays(
            swipe_target_config("pointer", animation_speed),
            vec![EXT_WORKSPACE_ID],
        );
        harness.mock_state.set_cursor_position(POINTER_ON_TEST);

        send(&mut harness, swipe(0.3));
        harness.advance(ONE_FRAME);
        harness.mock_state.focus_window(100);
        harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "no swipe is left behind");
        assert_eq!(
            window_x(harness.world(), 4),
            TEST_DISPLAY_WIDTH - 400,
            "the strip end is on the viewport edge"
        );
        assert_eq!(window_x(harness.world(), 100), TEST_DISPLAY_WIDTH);
        assert_focused!(harness.world(), 100);
    }
}

/// With window landing, the window a swipe lands on takes focus only once the
/// strip has landed. A switch to the other display before then stands.
#[test]
fn window_landing_swipe_keeps_a_switch_to_the_other_display() {
    for switch in [false, true] {
        let config = Config::try_from(
            "[options]\nanimation_speed = 14\n[swipe.gesture]\nfingers_count = 3\nwindow_landing = true\nhaptics = false\ntarget = \"pointer\"\n[bindings]\n",
        )
        .expect("config should parse");
        let mut harness = two_displays(config, vec![EXT_WORKSPACE_ID]);
        harness.mock_state.set_cursor_position(POINTER_ON_TEST);

        send(&mut harness, Event::TouchpadDown);
        move_fingers(&mut harness, 0.008, 40);
        send(&mut harness, Event::TouchpadUp);
        harness.advance(ONE_FRAME);
        if switch {
            harness.mock_state.focus_window(100);
            harness.mock_state.os_activate_display(EXT_DISPLAY_ID);
        }
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "the settle has finished");
        if switch {
            assert_focused!(harness.world(), 100);
        } else {
            assert_focused!(harness.world(), 1);
        }
    }
}

/// Modifier-scroll with a mouse wheel moves the strip under the pointer too.
#[test]
fn wheel_scroll_moves_the_strip_under_the_pointer() {
    let mut harness = two_displays(swipe_target_config("pointer", None), vec![EXT_WORKSPACE_ID]);
    harness.mock_state.set_cursor_position(POINTER_ON_EXT);

    for _ in 0..4 {
        send(&mut harness, Event::Scroll { delta: 30.0 });
        harness.advance(ONE_FRAME);
    }
    assert!(
        window_x(harness.world(), 100) < TEST_DISPLAY_WIDTH,
        "the strip under the pointer moves"
    );
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_eq!(window_x(harness.world(), 0), 0, "the focused strip stays");
    let focused = harness
        .world()
        .query_filtered::<&Window, With<crate::ecs::FocusedMarker>>()
        .single(harness.world())
        .expect("a window has focus")
        .id();
    assert!(
        (100..105).contains(&focused),
        "a window on the external display has focus, not {focused}"
    );
}

/// The pointer moving to the other display mid-swipe does not take the swipe
/// with it.
#[test]
fn swipe_keeps_its_strip_when_the_pointer_moves_to_another_display() {
    let mut harness = two_displays(swipe_target_config("pointer", None), vec![EXT_WORKSPACE_ID]);
    harness.mock_state.set_cursor_position(POINTER_ON_EXT);

    send(&mut harness, Event::TouchpadDown);
    harness.advance(ONE_FRAME);
    for frame in 0..10 {
        if frame == 3 {
            harness.mock_state.set_cursor_position(POINTER_ON_TEST);
        }
        send(&mut harness, swipe(0.05));
        harness.advance(ONE_FRAME);
    }
    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_eq!(
        window_x(harness.world(), 0),
        0,
        "the strip now under the pointer stays"
    );
    assert_eq!(window_x(harness.world(), 104), EXT_LAST_COLUMN_X);
}

/// A new swipe on the other display, while the last swipe's strip still
/// settles, leaves that settle to finish where it was headed.
#[test]
fn swipe_on_another_display_leaves_a_settle_to_finish() {
    let mut harness = two_displays(
        swipe_target_config("pointer", Some(14.0)),
        vec![EXT_WORKSPACE_ID],
    );
    harness.mock_state.set_cursor_position(POINTER_ON_TEST);
    send(&mut harness, swipe(0.3));
    harness.advance(ONE_FRAME * 6);
    assert!(is_scrolling(harness.world()), "still settling");

    harness.mock_state.set_cursor_position(POINTER_ON_EXT);
    send(&mut harness, Event::TouchpadDown);
    harness.advance(ONE_FRAME);
    send(&mut harness, swipe(0.05));
    harness.advance(ONE_FRAME);
    let world = harness.world();
    assert_eq!(
        world.query::<&Scrolling>().iter(world).count(),
        2,
        "one strip settles while the other follows the fingers"
    );
    for _ in 0..4 {
        send(&mut harness, swipe(0.05));
        harness.advance(ONE_FRAME);
    }
    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "both settles have finished");
    assert_eq!(window_x(harness.world(), 4), TEST_DISPLAY_WIDTH - 400);
    assert!(
        window_x(harness.world(), 100) < TEST_DISPLAY_WIDTH,
        "the second swipe moved the other strip"
    );
}

/// A click on another display, while a swiped strip still settles, leaves
/// that settle to finish, so the strip never stays stretched past its end.
#[test]
fn click_on_another_display_leaves_a_settle_to_finish() {
    let mut harness = two_displays(
        swipe_target_config("pointer", Some(14.0)),
        vec![EXT_WORKSPACE_ID],
    );
    harness.mock_state.set_cursor_position(POINTER_ON_TEST);
    send(&mut harness, swipe(0.3));
    harness.advance(ONE_FRAME * 6);
    assert!(is_scrolling(harness.world()), "still settling");

    let point = POINTER_ON_EXT + IVec2::new(0, 300);
    send(
        &mut harness,
        Event::MouseDown {
            point: objc2_core_foundation::CGPoint::new(point.x.into(), point.y.into()),
            modifiers: crate::platform::Modifiers::empty(),
            button: crate::events::MouseButton::Left,
            window_id: Some(101),
            clicks: 1,
        },
    );
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_eq!(window_x(harness.world(), 4), TEST_DISPLAY_WIDTH - 400);
}

/// A display that switches to another Space mid-swipe lands its swiped strip
/// at once, with no empty space beside it, and leaves no swipe on it.
#[test]
fn swipe_lands_when_its_display_switches_space() {
    let other_space = EXT_WORKSPACE_ID + 1;
    let mut harness = two_displays(
        swipe_target_config("pointer", Some(14.0)),
        vec![EXT_WORKSPACE_ID, other_space],
    );
    harness.mock_state.set_cursor_position(POINTER_ON_EXT);

    send(&mut harness, Event::TouchpadDown);
    harness.advance(ONE_FRAME);
    // Far enough to pull the last column to the far edge.
    for _ in 0..15 {
        send(&mut harness, swipe(0.2));
        harness.advance(ONE_FRAME);
    }
    assert!(is_scrolling(harness.world()), "the fingers hold the swipe");
    assert!(
        window_x(harness.world(), 104) < EXT_LAST_COLUMN_X,
        "the fingers pulled the strip past its end"
    );
    harness
        .mock_state
        .activate_workspace(EXT_DISPLAY_ID, other_space, false);
    send(&mut harness, Event::SpaceChanged);
    harness.advance(ONE_FRAME);

    assert!(!is_scrolling(harness.world()), "no swipe is left behind");
    let world = harness.world();
    let strip_x = world
        .query::<(&crate::ecs::layout::LayoutStrip, &crate::ecs::Position)>()
        .iter(world)
        .find_map(|(strip, position)| (strip.id() == EXT_WORKSPACE_ID).then_some(position.x))
        .expect("the swiped strip");
    // Window 104 sits 2400 points into its strip.
    assert_eq!(
        strip_x + 2400,
        EXT_LAST_COLUMN_X,
        "the strip landed at once"
    );
    harness.advance(SETTLE);
    assert_eq!(window_x(harness.world(), 104), EXT_LAST_COLUMN_X);
    assert_eq!(window_x(harness.world(), 0), 0);
}

/// When each trackpad pulse was requested.
#[derive(Resource, Default)]
struct Pulses(Vec<Duration>);

fn record_pulses(
    mut requests: MessageReader<crate::ecs::focus::PageHapticRequest>,
    time: Res<Time>,
    mut pulses: ResMut<Pulses>,
) {
    for _ in requests.read() {
        pulses.0.push(time.elapsed());
    }
}

/// Miri's Window by window swipe on four 700-point columns, with column 0
/// focused at the left edge.
fn window_landing_harness(haptics: bool) -> TestHarness {
    window_landing_harness_with_widths(&[700, 700, 700, 700], haptics)
}

fn window_landing_harness_with_widths(widths: &[i32], haptics: bool) -> TestHarness {
    let config = Config::try_from(
        format!(
            "[options]\nanimation_speed = 14\n[swipe.gesture]\nfingers_count = 3\nwindow_landing = true\nhaptics = {haptics}\n"
        )
        .as_str(),
    )
    .unwrap();
    let mut harness = harness_with_config(widths, config);
    harness.app.init_resource::<Pulses>();
    harness.app.add_systems(PostUpdate, record_pulses);
    // Above every window, so focus follows mouse never picks a window the
    // strip slides under the pointer.
    harness.mock_state.set_cursor_position(IVec2::new(1000, 5));
    harness
}

fn pulses(harness: &mut TestHarness) -> usize {
    harness.world().resource::<Pulses>().0.len()
}

/// One swipe: the fingers touch down, move one small `delta` per frame,
/// `frames` times, and lift.
fn drag(harness: &mut TestHarness, delta: f64, frames: usize) {
    send(harness, Event::TouchpadDown);
    move_fingers(harness, delta, frames);
    send(harness, Event::TouchpadUp);
}

/// Moves fingers already down: one small `delta` per frame, `frames` times.
fn move_fingers(harness: &mut TestHarness, delta: f64, frames: usize) {
    for _ in 0..frames {
        send(harness, swipe(delta));
        harness.advance(ONE_FRAME);
    }
}

/// A slow drag that leaves the viewport center on the column it began on
/// lands back there, with no pulse.
#[test]
fn window_landing_short_drag_springs_back_to_its_column() {
    let mut harness = window_landing_harness(true);
    drag(&mut harness, 0.002, 10);
    let dragged = window_x(harness.world(), 0);
    assert!(
        dragged < 0,
        "the strip follows the fingers, got x {dragged}"
    );
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 0);
    assert_eq!(window_x(harness.world(), 0), 0);
    assert_eq!(pulses(&mut harness), 0);
}

/// A slow drag that moves the strip far enough to step pulses once as it
/// does, and lands on the next column when the fingers lift, flush with the
/// viewport edge.
#[test]
fn window_landing_drag_pulses_once_it_has_moved_far_enough() {
    let mut harness = window_landing_harness(true);
    drag(&mut harness, 0.008, 40);
    assert_eq!(pulses(&mut harness), 1, "one detent while the fingers move");
    harness.advance(SETTLE);

    assert!(!is_scrolling(harness.world()), "the settle has finished");
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
    assert_eq!(pulses(&mut harness), 1, "the landing adds no pulse");
}

/// A quick flick lands on the next column even though the fingers barely
/// moved, and a hard one never glides past it. The landing pulses, since no
/// detent named that column.
#[test]
fn window_landing_flick_lands_on_the_next_column_only() {
    for (delta, frames) in [(0.03, 3), (0.1, 2)] {
        let mut harness = window_landing_harness(true);
        drag(&mut harness, delta, frames);
        harness.advance(SETTLE);

        assert!(!is_scrolling(harness.world()), "the settle has finished");
        assert_focused!(harness.world(), 1);
        assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
        assert_eq!(pulses(&mut harness), 1, "flick of {delta}");
    }
}

/// A flick back toward the start lands on the previous column.
#[test]
fn window_landing_flick_back_lands_on_the_previous_column() {
    let mut harness = window_landing_harness(true);
    harness.run(vec![focus(Direction::Nth(2))]);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 2);
    let before = pulses(&mut harness);

    drag(&mut harness, -0.03, 3);
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), 0);
    assert_eq!(pulses(&mut harness), before + 1);
}

/// Turning the pulses off keeps the landing and drops every pulse.
#[test]
fn window_landing_without_haptics_lands_with_no_pulse() {
    let mut harness = window_landing_harness(false);
    drag(&mut harness, 0.008, 40);
    harness.advance(SETTLE);
    drag(&mut harness, 0.03, 3);
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 2);
    assert_eq!(pulses(&mut harness), 0);
}

/// Windows nearly as wide as the display step with the same short drag as
/// narrow ones, both ways. Waiting for the next one to reach the viewport
/// center would take most of a trackpad width.
#[test]
fn window_landing_steps_between_wide_windows_with_a_short_drag() {
    let mut harness = window_landing_harness_with_widths(&[820, 820, 820], true);
    drag(&mut harness, 0.004, 45);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 820);

    drag(&mut harness, -0.004, 45);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 0);
    assert_eq!(window_x(harness.world(), 0), 0);
    assert_eq!(pulses(&mut harness), 2);
}

/// A second swipe that catches the first one's settle steps on from where
/// the first was headed.
#[test]
fn window_landing_second_quick_swipe_steps_again() {
    let mut harness = window_landing_harness(true);
    drag(&mut harness, 0.03, 3);
    harness.advance(ONE_FRAME * 4);
    assert!(
        is_scrolling(harness.world()),
        "the first settle is still running"
    );
    drag(&mut harness, 0.03, 3);
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 2);
    assert_eq!(window_x(harness.world(), 2), TEST_DISPLAY_WIDTH - 700);
    assert_eq!(pulses(&mut harness), 2);
}

/// The strip follows the fingers one to one, and a long drag stretches with
/// resistance once the next column shows in full, so the landing never jumps
/// back across columns the fingers pulled past. On lift it eases back from
/// where it shows, never from where the fingers went.
#[test]
fn window_landing_resists_past_the_next_column() {
    let mut harness = window_landing_harness(true);
    send(&mut harness, Event::TouchpadDown);
    move_fingers(&mut harness, 0.002, 10);
    // Ten moves of 0.002 of the trackpad at the default 0.35 sensitivity.
    let fingers = 7;
    assert_eq!(window_x(harness.world(), 0), -fingers, "one to one");

    move_fingers(&mut harness, 0.05, 40);
    let stretched = window_x(harness.world(), 1);
    let landed = TEST_DISPLAY_WIDTH - 700;
    assert!(
        stretched < landed && stretched > landed - TEST_DISPLAY_WIDTH / 8,
        "stretched only a little past the landing, got x {stretched}"
    );
    send(&mut harness, Event::TouchpadUp);
    let mut shown = stretched;
    for _ in 0..50 {
        harness.advance(ONE_FRAME);
        let x = window_x(harness.world(), 1);
        assert!(
            (shown..=landed).contains(&x),
            "eases from {shown} toward {landed}, got x {x}"
        );
        shown = x;
    }
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), landed);
}

/// Fingers held still send no swipe events. The strip stays where they hold
/// it until they lift, and only then lands.
#[test]
fn window_landing_waits_for_held_fingers_to_lift() {
    let mut harness = window_landing_harness(true);
    send(&mut harness, Event::TouchpadDown);
    move_fingers(&mut harness, 0.008, 40);
    let held = window_x(harness.world(), 1);
    harness.advance(Duration::from_millis(500));
    assert_eq!(window_x(harness.world(), 1), held, "the strip stays held");
    assert_focused!(harness.world(), 0);
    assert_eq!(pulses(&mut harness), 1);

    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 700);
    assert_eq!(pulses(&mut harness), 1);
}

/// A quick move held still before the lift is no flick. Too short to step,
/// it springs back with no pulse.
#[test]
fn window_landing_hold_before_the_lift_is_no_flick() {
    let mut harness = window_landing_harness(true);
    send(&mut harness, Event::TouchpadDown);
    move_fingers(&mut harness, 0.03, 3);
    harness.advance(Duration::from_millis(500));
    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);

    assert_focused!(harness.world(), 0);
    assert_eq!(window_x(harness.world(), 0), 0);
    assert_eq!(pulses(&mut harness), 0);
}

/// Where the next column already shows in full, the strip resists from the
/// start, so it shows less than the fingers moved. The detent still clicks
/// at the same finger travel that steps at release.
#[test]
fn window_landing_detent_clicks_where_the_release_steps() {
    let mut harness = window_landing_harness_with_widths(&[400, 400, 400], true);
    send(&mut harness, Event::TouchpadDown);
    // About 61 points of finger travel, past the 51-point step, which the
    // resistance shows as about 41.
    move_fingers(&mut harness, 0.002, 85);
    assert_eq!(pulses(&mut harness), 1, "the detent clicks before the lift");

    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
    assert_eq!(pulses(&mut harness), 1, "the landing adds no pulse");
}

/// The landing focuses its window once the strip has landed, so an app slow
/// to take focus never stalls the strip partway.
#[test]
fn window_landing_focuses_once_the_strip_lands() {
    let mut harness = window_landing_harness(true);
    drag(&mut harness, 0.008, 40);
    harness.advance(ONE_FRAME * 4);
    assert!(is_scrolling(harness.world()), "the settle is still running");
    assert_focused!(harness.world(), 0);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
}

/// After landing on a narrow last column flush with the edge, the viewport
/// center still lies over the wide column beside it. A swipe on toward the
/// end starts from the landed column, so it springs back without a click
/// instead of clicking and stepping onto the column it already shows.
#[test]
fn window_landing_starts_from_the_column_it_landed_on() {
    let mut harness = window_landing_harness_with_widths(&[900, 400], true);
    drag(&mut harness, 0.008, 40);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 400);
    assert_eq!(pulses(&mut harness), 1);

    drag(&mut harness, 0.008, 40);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 1);
    assert_eq!(window_x(harness.world(), 1), TEST_DISPLAY_WIDTH - 400);
    assert_eq!(pulses(&mut harness), 1, "the strip end gives no click");

    drag(&mut harness, -0.008, 40);
    harness.advance(SETTLE);
    assert_focused!(harness.world(), 0);
    assert_eq!(window_x(harness.world(), 0), 0);
    assert_eq!(pulses(&mut harness), 2);
}
