//! Window writes reach each app on that app's own writer, so a slow app
//! holds up neither the engine frame nor any other app's windows.

use std::time::{Duration, Instant};

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::SpawnWindowTrigger;
use crate::events::Event;
use crate::manager::{Size, Window};
use crate::platform::{Pid, WinID};

use super::*;

/// Owns windows 0 and 2.
const SLOW_APP: Pid = TEST_PROCESS_ID;
/// Owns window 1.
const OTHER_APP: Pid = 2;

const WINDOW_WIDTH: i32 = 700;
/// How long each of the slow app's writes takes, like Helium mid-swipe.
const SLOW_WRITE: Duration = Duration::from_millis(200);
/// How long the other app's write may take to land while the slow app is
/// stuck, under the second a held app stays stuck.
const OTHER_APP_LANDING: Duration = Duration::from_millis(500);
/// Simulated time per frame, as the harness runs it.
const FRAME: Duration = Duration::from_millis(20);
const FLUSH: Duration = Duration::from_secs(5);

/// Three columns, wider together than the display, whose apps take their
/// writes through the engine's per-app writers, settled at the strip start.
fn harness() -> TestHarness {
    harness_with(|harness| harness)
}

/// [`harness`], with `setup` applied before any window opens.
fn harness_with(setup: impl FnOnce(TestHarness) -> TestHarness) -> TestHarness {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = setup(TestHarness::new().with_config(config).with_app(
        OTHER_APP,
        "other",
        "OtherApp",
        |_| {},
    ));
    harness.mock_state.queue_writes(SLOW_APP);
    harness.mock_state.queue_writes(OTHER_APP);
    for (id, pid) in [(0, SLOW_APP), (1, OTHER_APP), (2, SLOW_APP)] {
        let frame = IRect::new(0, 0, WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = harness
            .mock_state
            .spawn_window(pid, TEST_WORKSPACE_ID, id, frame);
        harness.existed_at_launch([id]);
        harness.world().trigger(SpawnWindowTrigger(vec![window]));
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ]);
    assert!(harness.mock_state.flush_writes(FLUSH));
    harness
}

fn swipe(harness: &mut TestHarness) {
    harness.world().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
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

fn position_writes(mock_state: &MockState, id: WinID) -> usize {
    let mut writes = 0;
    mock_state.update_window(id, |window| writes = window.position_writes);
    writes
}

fn run_frames(harness: &mut TestHarness, frames: usize) {
    for _ in 0..frames {
        harness.advance(FRAME);
    }
}

/// Waits up to `timeout` for `done`, polling.
fn eventually(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Every write to the slow app takes 200ms, yet no engine frame of the swipe
/// waits for one, and once the app catches up every window is where the
/// engine put it.
#[test]
fn a_slow_app_does_not_hold_up_the_engine_frame() {
    let mut harness = harness();
    harness.mock_state.set_write_delay(SLOW_APP, SLOW_WRITE);
    let before = engine_frame(harness.world(), 0);

    swipe(&mut harness);
    assert_no_frame_waited(&mut harness, 15);

    assert_ne!(
        engine_frame(harness.world(), 0),
        before,
        "the swipe moved the slow app's window"
    );
    assert!(harness.mock_state.flush_writes(FLUSH));
    for id in 0..3 {
        assert_eq!(
            harness.mock_state.ax_frame(id),
            engine_frame(harness.world(), id),
            "window {id} landed where the engine put it"
        );
    }
}

/// While the slow app is stuck inside a write, the other app's window still
/// gets its frame at once.
#[test]
fn other_apps_get_their_writes_while_one_app_is_stuck() {
    let mut harness = harness();
    let stuck_at = harness.mock_state.ax_frame(0);
    harness.mock_state.hold_writes(SLOW_APP);

    swipe(&mut harness);
    run_frames(&mut harness, 3);

    let other = engine_frame(harness.world(), 1);
    let landed = eventually(OTHER_APP_LANDING, || {
        harness.mock_state.ax_frame(1) == other
    });
    let slow_app_frame = harness.mock_state.ax_frame(0);
    harness.mock_state.release_writes(SLOW_APP);

    assert!(landed, "the other app's window reached {other:?}");
    assert_eq!(
        slow_app_frame, stuck_at,
        "the stuck app had not applied its write yet"
    );
    assert!(harness.mock_state.flush_writes(FLUSH));
    assert_eq!(
        harness.mock_state.ax_frame(0),
        engine_frame(harness.world(), 0)
    );
}

/// The engine asks for a new frame every swipe frame while the slow app is
/// stuck in its first write. Once it answers, it gets the newest frame only,
/// not every frame it missed.
#[test]
fn a_stuck_app_gets_only_the_newest_frame_once_it_answers() {
    let mut harness = harness();
    let before = position_writes(&harness.mock_state, 0);
    harness.mock_state.hold_writes(SLOW_APP);

    swipe(&mut harness);
    let mut asked = Vec::new();
    for _ in 0..10 {
        run_frames(&mut harness, 1);
        let frame = engine_frame(harness.world(), 0);
        if asked.last() != Some(&frame) {
            asked.push(frame);
        }
    }
    harness.mock_state.release_writes(SLOW_APP);
    assert!(harness.mock_state.flush_writes(FLUSH));

    assert!(
        asked.len() > 2,
        "the swipe asked for {} frames",
        asked.len()
    );
    assert_eq!(
        position_writes(&harness.mock_state, 0) - before,
        2,
        "the write in flight, then the newest frame"
    );
    assert_eq!(harness.mock_state.ax_frame(0), *asked.last().unwrap());
}

/// Runs the exit cleanup directly. Sending `AppExit` through the full
/// schedule also runs the state-saving exit systems, which write the real user
/// state file.
fn run_exit_cleanup(world: &mut World) {
    use bevy::app::AppExit;
    use bevy::ecs::system::RunSystemOnce as _;

    world.write_message(AppExit::Success);
    world
        .run_system_once(crate::ecs::systems::cleanup_on_exit)
        .expect("running cleanup_on_exit");
}

/// The engine process ends as soon as the exit cleanup returns, so every
/// window must have its exit frame by then, even from an app that answers
/// later than a frame read waits for.
#[test]
fn exit_cleanup_returns_only_once_a_slow_app_has_its_exit_frames() {
    let mut harness = harness();
    let parked = engine_frame(harness.world(), 2);
    harness.mock_state.hold_writes(SLOW_APP);
    // Past the time the cleanup takes to give up on both of the slow app's
    // windows when it reads instead of waiting for the writes to land, yet
    // inside its one second budget.
    let release = {
        let mock_state = harness.mock_state.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            mock_state.release_writes(SLOW_APP);
        })
    };

    run_exit_cleanup(harness.world());
    let landed = harness.mock_state.ax_frame(2);
    release.join().unwrap();

    let exit_frame = engine_frame(harness.world(), 2);
    assert_ne!(exit_frame, parked, "the exit moved the parked window");
    assert_eq!(landed, exit_frame);
}

// --- Readers of a window's frame while the engine's write to it is pending ---

/// Holds the slow app, then has the engine move window `id` right by `dx`, so
/// that write stays pending until the test releases the app. Returns the
/// frame the engine asked for.
fn pend_move(harness: &mut TestHarness, id: WinID, dx: i32) -> IRect {
    harness.mock_state.hold_writes(SLOW_APP);
    let entity = find_window_entity(id, harness.world());
    harness
        .world()
        .get_mut::<crate::ecs::Position>(entity)
        .expect("a tiled window has a position")
        .0
        .x += dx;
    harness.advance(FRAME);
    let asked = engine_frame(harness.world(), id);
    assert_ne!(
        harness.mock_state.ax_frame(id),
        asked,
        "the write is pending"
    );
    asked
}

/// Releases the slow app and waits until its writes land.
fn release_slow_app(harness: &mut TestHarness) {
    harness.mock_state.release_writes(SLOW_APP);
    assert!(harness.mock_state.flush_writes(FLUSH));
}

/// Counts the engine's waits on an app's writer instead of timing frames, as
/// a loaded machine can stall a frame that never waited.
fn assert_no_frame_waited(harness: &mut TestHarness, frames: usize) {
    let before = harness.mock_state.engine_waits();
    run_frames(harness, frames);
    assert_eq!(
        harness.mock_state.engine_waits() - before,
        0,
        "engine waits on an app's writer"
    );
}

fn has<C: Component>(world: &mut World, id: WinID) -> bool {
    let entity = find_window_entity(id, world);
    world.get::<C>(entity).is_some()
}

/// `window_frame_update`: an app resizes its own window while the engine's
/// move to it is still on its way. The frame read must wait for the move
/// to land, without holding up any engine frame, and then the app's width
/// is adopted. The column keeps its full height.
#[test]
fn a_frame_notification_during_a_pending_write_is_read_once_the_write_lands() {
    let mut harness = harness();
    pend_move(&mut harness, 0, 40);
    let native = Size::new(WINDOW_WIDTH - 120, TEST_WINDOW_HEIGHT - 300);
    harness.mock_state.os_resize_window(0, native);

    assert_no_frame_waited(&mut harness, 5);
    release_slow_app(&mut harness);
    harness.advance(Duration::from_secs(1));

    let entity = find_window_entity(0, harness.world());
    assert_eq!(
        harness
            .world()
            .get::<crate::ecs::Bounds>(entity)
            .unwrap()
            .0
            .x,
        native.x,
        "the engine adopted the width the app chose"
    );
}

/// `verify_window_position`: a check of the engine's own pending move waits
/// for it to land instead of reading the frame the window is leaving, and
/// spends none of its attempts meanwhile.
#[test]
fn position_verification_waits_for_the_engines_pending_write() {
    let mut harness = harness();
    let asked = pend_move(&mut harness, 0, 40);
    let entity = find_window_entity(0, harness.world());
    harness
        .world()
        .entity_mut(entity)
        .insert(crate::ecs::VerifyWindowPosition::default());

    assert_no_frame_waited(&mut harness, 5);
    assert!(
        has::<crate::ecs::VerifyWindowPosition>(harness.world(), 0),
        "the check waits for the write"
    );

    release_slow_app(&mut harness);
    harness.advance(FRAME * 2);
    assert!(!has::<crate::ecs::VerifyWindowPosition>(harness.world(), 0));
    assert_eq!(harness.mock_state.ax_frame(0), asked);
}

/// `window_resize_verifier`: the app refuses the narrower size the engine is
/// still writing. The check waits for the write to land, then adopts the
/// width the app kept.
#[test]
fn size_verification_waits_for_the_engines_pending_write() {
    let mut harness = harness();
    let minimum = Size::new(WINDOW_WIDTH - 100, 100);
    harness
        .mock_state
        .update_window(0, |window| window.minimum_size = Some(minimum));
    harness.mock_state.hold_writes(SLOW_APP);
    let entity = find_window_entity(0, harness.world());
    harness
        .world()
        .get_mut::<crate::ecs::Bounds>(entity)
        .unwrap()
        .0
        .x = WINDOW_WIDTH - 300;
    harness.advance(FRAME);
    harness
        .world()
        .entity_mut(entity)
        .insert(crate::ecs::VerifyWindowSize::default());

    assert_no_frame_waited(&mut harness, 5);
    release_slow_app(&mut harness);
    harness.advance(Duration::from_secs(1));

    assert_eq!(
        harness
            .world()
            .get::<crate::ecs::Bounds>(entity)
            .unwrap()
            .0
            .x,
        minimum.x,
        "the engine adopted the width the app kept"
    );
}

/// `fix_window_size_on_focus`: focus arrives while the engine's write is
/// pending. The refresh does not wait, and stays due for a later focus.
#[test]
fn a_focus_size_refresh_waits_for_a_later_focus_while_a_write_is_pending() {
    use crate::ecs::{FocusedMarker, RefreshWindowSizeOnFocus};

    let mut harness = harness();
    pend_move(&mut harness, 0, 40);
    let entity = find_window_entity(0, harness.world());
    harness.world().entity_mut(entity).remove::<FocusedMarker>();
    harness.advance(FRAME);
    harness
        .world()
        .entity_mut(entity)
        .insert((RefreshWindowSizeOnFocus, FocusedMarker));

    assert_no_frame_waited(&mut harness, 3);
    assert!(has::<RefreshWindowSizeOnFocus>(harness.world(), 0));
    release_slow_app(&mut harness);
}

/// `poll_pressed_window`: a press on a window whose engine move is pending
/// is polled every frame. The poll must neither wait for the move nor
/// report it as the user dragging the window.
#[test]
fn a_pressed_window_is_not_polled_while_the_engines_write_is_pending() {
    use crate::ecs::drag::DragState;
    use crate::events::MouseButton;
    use crate::platform::Modifiers;
    use objc2_core_foundation::CGPoint;

    let mut harness = harness();
    let start = harness.mock_state.ax_frame(0);
    pend_move(&mut harness, 0, 40);
    let press = start.min + IVec2::new(100, 10);
    let cg = |point: IVec2| CGPoint::new(point.x.into(), point.y.into());
    harness.mock_state.set_left_button_down(true);
    harness.world().write_message(Event::MouseDown {
        point: cg(press),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
        window_id: Some(0),
        clicks: 1,
    });
    harness.advance(FRAME);
    harness.world().write_message(Event::MouseDragged {
        point: cg(press + IVec2::new(60, 0)),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    });

    assert_no_frame_waited(&mut harness, 5);
    assert!(
        !matches!(
            harness.world().resource::<DragState>(),
            DragState::Dragging(_)
        ),
        "the engine's own move is no drag"
    );
    harness.mock_state.set_left_button_down(false);
    release_slow_app(&mut harness);
}

/// `recover_window_handles`: a window awaiting a handle whose engine write
/// is pending is not read until the write lands, then gets its frame.
#[test]
fn handle_recovery_waits_for_the_engines_pending_write() {
    use crate::ecs::AwaitingHandle;
    use bevy::time::{Timer, TimerMode};

    let mut harness = harness();
    let asked = pend_move(&mut harness, 0, 40);
    let entity = find_window_entity(0, harness.world());
    harness
        .world()
        .entity_mut(entity)
        .insert(AwaitingHandle(Timer::new(
            Duration::from_secs(5),
            TimerMode::Once,
        )));

    assert_no_frame_waited(&mut harness, 5);
    assert!(has::<AwaitingHandle>(harness.world(), 0));
    release_slow_app(&mut harness);
    harness.advance(FRAME * 2);
    assert!(!has::<AwaitingHandle>(harness.world(), 0));
    assert_eq!(harness.mock_state.ax_frame(0), asked);
}

/// `resize_window`: a width command on a window whose engine write is
/// pending steps from the frame the engine asked for, without waiting.
#[test]
fn a_width_command_does_not_wait_for_a_pending_write() {
    let mut harness = harness();
    pend_move(&mut harness, 0, 40);
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Resize(crate::commands::ResizeDirection::Grow)),
    });

    assert_no_frame_waited(&mut harness, 3);
    release_slow_app(&mut harness);
}

/// Drag tiling: macOS tiles a window, and the engine restores its slot
/// while the app is too busy to take the restore. No engine frame waits
/// for the restore, and once the app answers the window is back in its
/// slot.
#[test]
fn a_tile_restore_to_a_busy_app_holds_up_no_frame() {
    let mut harness = harness();
    let slot = engine_frame(harness.world(), 0);
    let tile = IRect::new(0, TEST_MENUBAR_HEIGHT, 512, TEST_DISPLAY_HEIGHT);
    harness.mock_state.hold_writes(SLOW_APP);
    harness
        .mock_state
        .update_window(0, |window| window.frame = tile);
    harness
        .world()
        .write_message(Event::WindowMoved { window_id: 0 });
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 0 });
    harness.advance(FRAME);
    assert_eq!(
        engine_frame(harness.world(), 0),
        slot,
        "the engine restores the slot"
    );
    assert_eq!(
        harness.mock_state.ax_frame(0),
        tile,
        "the restore is pending"
    );

    // Past the tile hold, which reads the frame back when it ends.
    assert_no_frame_waited(&mut harness, 60);
    release_slow_app(&mut harness);
    harness.advance(Duration::from_secs(2));
    assert!(harness.mock_state.flush_writes(FLUSH));

    assert_eq!(engine_frame(harness.world(), 0), slot);
    assert_eq!(harness.mock_state.ax_frame(0), slot);
}

/// Native membership: macOS reports a window in the space of another
/// display while the engine's write that brings it back to its own strip is
/// still pending. The frame the engine wrote is the evidence, so the stale
/// report moves nothing, and reading it waits on no app.
#[test]
fn a_stale_space_report_during_a_pending_write_keeps_the_window_in_its_strip() {
    let ext = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness =
        harness_with(|harness| harness.with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID]));
    crate::assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);

    pend_move(&mut harness, 0, 40);
    // Still on the other display, where a refused drop left it, as far as
    // macOS knows.
    harness.mock_state.update_window(0, |window| {
        window.workspace_id = EXT_WORKSPACE_ID;
        window.frame = IRect::from_corners(
            ext.min + IVec2::new(100, 20),
            ext.min + IVec2::new(100 + WINDOW_WIDTH, 20 + 700),
        );
    });
    harness.mock_state.os_activate_display(EXT_DISPLAY_ID);

    assert_no_frame_waited(&mut harness, 10);
    crate::assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);
    release_slow_app(&mut harness);
}

/// `apply_window_defaults`: a window opens with a configured width while
/// its app is too busy to take the resize in time. The engine keeps the
/// configured width instead of the width the window opened with.
#[test]
fn a_configured_width_survives_a_busy_app() {
    use crate::config::{InitialWindowWidth, WindowParams};

    let mut harness = harness_with(|harness| {
        let mut params = WindowParams::new(".*", None);
        params.width = Some(InitialWindowWidth::Ratio(0.5));
        harness.with_config(
            (
                MainOptions {
                    swipe_gesture_fingers: Some(3),
                    ..Default::default()
                },
                vec![params],
            )
                .into(),
        )
    });
    harness.mock_state.hold_writes(SLOW_APP);
    let frame = IRect::new(0, 0, WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let window = harness
        .mock_state
        .spawn_window(SLOW_APP, TEST_WORKSPACE_ID, 3, frame);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(FRAME);
    release_slow_app(&mut harness);
    harness.advance(Duration::from_secs(1));
    assert!(harness.mock_state.flush_writes(FLUSH));

    let entity = find_window_entity(3, harness.world());
    let bounds = harness.world().get::<crate::ecs::Bounds>(entity).unwrap().0;
    assert_eq!(bounds.x, TEST_DISPLAY_WIDTH / 2);
    assert_eq!(
        harness.mock_state.ax_frame(3).width(),
        TEST_DISPLAY_WIDTH / 2
    );
}
