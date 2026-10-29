//! Heights of the windows stacked in one column.

use std::collections::VecDeque;
use std::time::Duration;

use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::config::{Config, MainOptions};
use crate::ecs::{KeptHeight, KeptHeightReread, ResizeMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::{Size, Window};
use crate::platform::WinID;

use super::*;

const SETTLE: Duration = Duration::from_secs(2);

/// A display as tall as a 14 inch laptop's. On the default 768 px test
/// display a three-window stack falls below the minimum window height, and
/// `binpack_heights` already averages it there.
const TALL: i32 = 1290;
const COLUMN: i32 = TALL - TEST_MENUBAR_HEIGHT;

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

fn tall_harness() -> TestHarness {
    tall_harness_with_windows(3)
}

fn tall_harness_with_windows(count: i32) -> TestHarness {
    let harness = TestHarness::new();
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, TALL));
    harness.with_windows(count)
}

/// Stacks windows 1 and 2 under window 0, one at a time, so one column holds
/// all three and window 2 is focused.
fn stack_three(harness: &mut TestHarness) {
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// The heights of `ids` top to bottom, checking that they share one column
/// and neither overlap nor leave a gap.
fn column_heights(world: &mut World, ids: &[WinID]) -> Vec<i32> {
    let frames = ids.iter().map(|id| frame(world, *id)).collect::<Vec<_>>();
    for pair in frames.windows(2) {
        assert_eq!(pair[0].min.x, pair[1].min.x, "one column: {frames:?}");
        assert_eq!(
            pair[0].max.y, pair[1].min.y,
            "no overlap or gap: {frames:?}"
        );
    }
    frames.iter().map(IRect::height).collect()
}

fn assert_even(heights: &[i32], total: i32) {
    assert_eq!(heights.iter().sum::<i32>(), total, "fills: {heights:?}");
    let spread = heights.iter().max().unwrap() - heights.iter().min().unwrap();
    assert!(spread <= 1, "even split: {heights:?}");
}

/// Keeping the existing heights would leave window 0 half the column and
/// the other two a quarter each.
#[test]
fn a_window_joining_a_stack_splits_the_column_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// The window leaving is the top one, so keeping heights would hand all of
/// its share to the last window.
#[test]
fn a_window_leaving_a_stack_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
        command(Operation::Stack(false)),
    ]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[1, 2]);
    assert_even(&heights, COLUMN);
}

#[test]
fn closing_a_stacked_window_splits_the_rest_evenly() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.mock_state.os_close_window(0);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[1, 2]);
    assert_even(&heights, COLUMN);
}

/// Stacking the top window has no column to join, so the heights the user
/// set stay.
#[test]
fn a_stack_command_that_changes_nothing_keeps_set_heights() {
    let mut harness = tall_harness();
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::Focus(Direction::North)),
        command(Operation::ResizeVertical(ResizeDirection::Grow)),
    ]);
    harness.advance(SETTLE);
    let before = column_heights(harness.world(), &[0, 1, 2]);
    assert!(before[0] > before[1], "the resize took effect: {before:?}");

    harness.run(vec![command(Operation::Stack(true))]);
    harness.advance(SETTLE);

    assert_eq!(column_heights(harness.world(), &[0, 1, 2]), before);
}

/// Window 1's app accepts no less than 500 px. By the time window 3 joins,
/// window 1 already sits at 500 px, so the even split's smaller height
/// changes nothing and the app reports no resize.
#[test]
fn a_window_that_keeps_a_taller_height_does_not_cover_the_next() {
    let mut harness = tall_harness_with_windows(4);
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2, 3]);
    assert_eq!(heights[1], 500, "{heights:?}");
    assert_even(&[heights[0], heights[2], heights[3]], COLUMN - 500);
}

/// The last window gets what is left of the column, so when it is the one
/// that keeps a taller height, the windows above it make room.
#[test]
fn a_last_window_that_keeps_a_taller_height_stays_on_screen() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(2, |window| window.min_height = 500);
    stack_three(&mut harness);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_eq!(heights[2], 500, "{heights:?}");
    assert_eq!(frame(harness.world(), 2).max.y, TALL, "ends at the bottom");
    assert_even(&heights[..2], COLUMN - 500);
}

/// Window 0's app refused the even split's height when the stack formed.
/// Shrinking or equalizing does not ask it for less again, so with animation
/// on, the app gets no size it refuses and no resize keeps running.
#[test]
fn shrinking_or_equalizing_sends_no_height_the_app_refused() {
    for operation in [
        Operation::ResizeVertical(ResizeDirection::Shrink),
        Operation::Equalize,
    ] {
        let config: Config = (
            MainOptions {
                animation_speed: Some(14.0),
                ..Default::default()
            },
            vec![],
        )
            .into();
        let mut harness = tall_harness().with_config(config);
        harness
            .mock_state
            .update_window(0, |window| window.min_height = 700);
        stack_three(&mut harness);
        harness.run(vec![
            command(Operation::Focus(Direction::North)),
            command(Operation::Focus(Direction::North)),
        ]);
        let writes = harness.mock_state.resize_writes(0);
        harness.run(vec![command(operation.clone())]);
        harness.advance(SETTLE);

        assert_eq!(
            harness.mock_state.resize_writes(0),
            writes,
            "{operation:?}: no size below the height the app kept"
        );
        let world = harness.world();
        let resizing = world
            .query_filtered::<(), With<ResizeMarker>>()
            .iter(world)
            .count();
        assert_eq!(resizing, 0, "{operation:?}: every resize has finished");
        let heights = column_heights(world, &[0, 1, 2]);
        assert_eq!(heights[0], 700, "{operation:?}: {heights:?}");
        assert_eq!(heights.iter().sum::<i32>(), COLUMN, "{operation:?}");
    }
}

/// Two windows that each refuse less than 400 px share a 748 px column. No
/// split fits both, so a vertical resize there leaves the heights alone.
#[test]
fn a_vertical_resize_in_a_column_too_short_for_kept_heights_changes_nothing() {
    let mut harness = TestHarness::new().with_windows(2);
    for id in [0, 1] {
        harness
            .mock_state
            .update_window(id, |window| window.min_height = 400);
    }
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::North)),
    ]);
    harness.advance(SETTLE);
    let before = [frame(harness.world(), 0), frame(harness.world(), 1)];

    for direction in [ResizeDirection::Shrink, ResizeDirection::Grow] {
        harness.run(vec![command(Operation::ResizeVertical(direction))]);
        harness.advance(SETTLE);
        let after = [frame(harness.world(), 0), frame(harness.world(), 1)];
        assert_eq!(after, before, "{direction:?}");
    }
}

fn kept_height(world: &mut World, id: WinID) -> Option<(i32, i32)> {
    world
        .query::<(&Window, &KeptHeight)>()
        .iter(world)
        .find(|(window, _)| window.id() == id)
        .map(|(_, kept)| (kept.asked, kept.kept))
}

fn all_writes(harness: &TestHarness, ids: &[WinID]) -> Vec<usize> {
    ids.iter()
        .map(|id| harness.mock_state.resize_writes(*id))
        .collect()
}

fn animated(speed: f64) -> Config {
    (
        MainOptions {
            animation_speed: Some(speed),
            preset_stack_heights: crate::config::default_preset_stack_heights(),
            ..Default::default()
        },
        vec![],
    )
        .into()
}

/// An app that applies each size write `latency` after it arrives, in
/// order, as Chromium-based apps do. `notify` is whether macOS reports each
/// one with a resize notice.
struct LateApp {
    id: WinID,
    latency: Duration,
    notify: bool,
    now: Duration,
    sent: VecDeque<Duration>,
}

impl LateApp {
    fn new(harness: &TestHarness, id: WinID, latency: Duration, notify: bool) -> Self {
        harness
            .mock_state
            .update_window(id, |window| window.late_resizes = Some(VecDeque::new()));
        Self {
            id,
            latency,
            notify,
            now: Duration::ZERO,
            sent: VecDeque::new(),
        }
    }

    fn advance(&mut self, harness: &mut TestHarness, duration: Duration) {
        const STEP: Duration = Duration::from_millis(20);
        let end = self.now + duration;
        while self.now < end {
            harness.advance(STEP);
            self.now += STEP;
            let held = harness.mock_state.late_resize_count(self.id);
            while self.sent.len() < held {
                self.sent.push_back(self.now);
            }
            while self
                .sent
                .front()
                .is_some_and(|sent| self.now >= *sent + self.latency)
            {
                self.sent.pop_front();
                harness
                    .mock_state
                    .os_apply_late_resize(self.id, self.notify);
            }
        }
    }
}

/// Window 0 is asked to go from half the column to a third when window 2
/// joins. Its app applies each write late, so `Window::resize` reads back the
/// old height and the layout makes room for it at once. Once the app catches
/// up, the room goes back and the stack ends in an even split, whether macOS
/// reports the late resize or the 500 ms re-read finds it. The app gets one
/// write, and each other window three: its share, the room it makes, and its
/// share again.
#[test]
fn a_window_that_resizes_late_gets_an_even_share() {
    let cases = [
        (200, true),
        (450, true),
        (600, true),
        (700, true),
        (1200, true),
        (3000, true),
        (200, false),
        (450, false),
    ];
    for (latency, notify) in cases {
        let mut harness = tall_harness();
        harness.run(vec![
            Event::MenuOpened { window_id: 0 },
            command(Operation::Focus(Direction::First)),
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(true)),
            command(Operation::Focus(Direction::Last)),
        ]);
        harness.advance(SETTLE);
        let before = all_writes(&harness, &[0, 1, 2]);
        let mut app = LateApp::new(&harness, 0, Duration::from_millis(latency), notify);
        harness
            .world()
            .write_message(command(Operation::Stack(true)));
        app.advance(&mut harness, Duration::from_millis(100));
        let early = column_heights(harness.world(), &[0, 1, 2]);
        assert_eq!(
            early[0],
            COLUMN / 2,
            "{latency} ms: room for the old height"
        );
        app.advance(&mut harness, Duration::from_millis(latency) + SETTLE);

        let case = format!("{latency} ms, notice {notify}");
        let heights = column_heights(harness.world(), &[0, 1, 2]);
        assert_even(&heights, COLUMN);
        assert_eq!(kept_height(harness.world(), 0), None, "{case}");
        let writes = all_writes(&harness, &[0, 1, 2]);
        assert_eq!(
            writes[0] - before[0],
            1,
            "{case}: one write to the late app"
        );
        for id in [1, 2] {
            assert!(
                writes[id] - before[id] <= 3,
                "{case}: window {id} writes {writes:?} from {before:?}"
            );
        }
    }
}

/// The window that keeps a taller height gets it in any position, and the
/// others share the rest evenly.
#[test]
fn a_kept_height_holds_in_every_stack_position() {
    for (id, position) in [(0, 0), (1, 1), (2, 2)] {
        let mut harness = tall_harness();
        harness
            .mock_state
            .update_window(id, |window| window.min_height = 500);
        stack_three(&mut harness);

        let heights = column_heights(harness.world(), &[0, 1, 2]);
        assert_eq!(heights[position], 500, "window {id}: {heights:?}");
        let others = (0..3)
            .filter(|index| *index != position)
            .map(|index| heights[index])
            .collect::<Vec<_>>();
        assert_even(&others, COLUMN - 500);
        assert_eq!(frame(harness.world(), 2).max.y, TALL);
    }
}

#[test]
fn two_windows_that_keep_taller_heights_share_a_column() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(0, |window| window.min_height = 450);
    harness
        .mock_state
        .update_window(2, |window| window.min_height = 500);
    stack_three(&mut harness);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_eq!(heights, vec![450, COLUMN - 950, 500]);
}

/// When the kept heights add up to more than the column, no split keeps every
/// window both whole and on screen. The column keeps the split it gets
/// without this change, and only the window that refuses its share reaches
/// past it. The app is asked once.
#[test]
fn kept_heights_too_tall_for_the_column_keep_the_split() {
    let mut harness = TestHarness::new().with_windows(3);
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);

    let frames = [0, 1, 2].map(|id| frame(harness.world(), id));
    assert_eq!(
        frames.map(|frame| (frame.min.y, frame.max.y)),
        [(20, 269), (269, 769), (518, 768)]
    );
    let writes = harness.mock_state.resize_writes(1);
    for height in [300, 250, 280] {
        harness
            .mock_state
            .os_resize_window(0, Size::new(TEST_WINDOW_WIDTH, height));
        harness.advance(SETTLE);
    }
    assert_eq!(harness.mock_state.resize_writes(1), writes, "asked once");
}

/// A tabbed display gives each window the whole column. A window whose app
/// keeps a taller height keeps it, and nothing asks it again and again.
#[test]
fn a_tabbed_display_with_a_kept_height_stops_resizing() {
    let mut harness = TestHarness::new().with_windows(3);
    harness.mock_state.update_window(0, |window| {
        window.min_height = TEST_DISPLAY_HEIGHT + 100;
    });
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::ToggleTabbedDisplay),
    ]);
    harness.advance(SETTLE);

    let before = harness.mock_state.resize_writes(0);
    harness.advance(SETTLE);
    assert_eq!(harness.mock_state.resize_writes(0), before);
}

/// Once a stack has settled around a kept height, or around an app that
/// caught up late, ten idle seconds send no size write.
#[test]
fn a_settled_stack_sends_no_writes_while_idle() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    let before = all_writes(&harness, &[0, 1, 2]);
    harness.advance(Duration::from_secs(10));
    assert_eq!(all_writes(&harness, &[0, 1, 2]), before, "kept height");

    let mut harness = tall_harness();
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::Last)),
    ]);
    harness.advance(SETTLE);
    let mut app = LateApp::new(&harness, 0, Duration::from_millis(600), true);
    harness
        .world()
        .write_message(command(Operation::Stack(true)));
    app.advance(&mut harness, SETTLE);
    let before = all_writes(&harness, &[0, 1, 2]);
    app.advance(&mut harness, Duration::from_secs(10));
    assert_eq!(all_writes(&harness, &[0, 1, 2]), before, "late app");
}

/// Echoes of the height an app kept are not the user: they leave its
/// record alone, so Equalize still asks the window for nothing it refused.
#[test]
fn echoes_of_a_kept_height_do_not_ask_again() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(2, |window| window.min_height = 500);
    stack_three(&mut harness);
    let record = kept_height(harness.world(), 2);
    assert!(record.is_some());
    let writes = harness.mock_state.resize_writes(2);

    for _ in 0..3 {
        harness
            .mock_state
            .os_resize_window(2, Size::new(TEST_WINDOW_WIDTH, 500));
    }
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 2), record);
    harness.run(vec![command(Operation::Equalize)]);
    harness.advance(SETTLE);

    assert_eq!(harness.mock_state.resize_writes(2), writes);
    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_eq!(heights[2], 500, "{heights:?}");
}

/// Window 1's app accepts any height again, for example once a sheet that
/// held it open has closed. A window leaving and joining the column is a new
/// resize intent, so window 1 is asked for its share again and gets it.
#[test]
fn a_stack_change_asks_for_a_kept_height_again() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    assert_eq!(frame(harness.world(), 1).height(), 500);

    harness
        .mock_state
        .update_window(1, |window| window.min_height = 0);
    harness.run(vec![command(Operation::Stack(false))]);
    harness.advance(SETTLE);
    harness.run(vec![command(Operation::Stack(true))]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// A display change is a new resize intent too: once window 0's app accepts
/// any height again, a shrink after the display change is not held at the
/// height it kept before.
#[test]
fn a_display_change_ends_a_kept_height() {
    let mut harness = tall_harness_with_windows(2);
    harness
        .mock_state
        .update_window(0, |window| window.min_height = 700);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 0), Some((635, 700)));

    harness
        .mock_state
        .update_window(0, |window| window.min_height = 0);
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TALL - 100),
    );
    harness.run(vec![Event::DisplayConfigured {
        display_id: TEST_DISPLAY_ID,
    }]);
    harness.advance(SETTLE);
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 0), None);

    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::ResizeVertical(ResizeDirection::Shrink)),
    ]);
    harness.advance(SETTLE);
    let heights = column_heights(harness.world(), &[0, 1]);
    assert!(heights[0] < 700, "{heights:?}");
}

/// The user makes window 1 shorter than the height its app once refused.
/// That ends the record, so Equalize splits the column evenly.
#[test]
fn a_kept_height_ends_when_the_window_is_resized_shorter() {
    let mut harness = tall_harness();
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 500);
    stack_three(&mut harness);
    assert_eq!(frame(harness.world(), 1).height(), 500);

    harness
        .mock_state
        .update_window(1, |window| window.min_height = 0);
    harness
        .mock_state
        .os_resize_window(1, Size::new(TEST_WINDOW_WIDTH, 300));
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 1), None);
    harness.run(vec![command(Operation::Equalize)]);
    harness.advance(SETTLE);

    let heights = column_heights(harness.world(), &[0, 1, 2]);
    assert_even(&heights, COLUMN);
}

/// The user resizes window 0 to 900 px after its app kept 700 px of the 635
/// px it was asked for. The drag is the new height, and shrinking the window
/// still takes it down to the 700 px its app accepts. The second shrink
/// sends nothing: the app refused the first.
#[test]
fn a_user_resize_replaces_a_kept_height() {
    let mut harness = tall_harness_with_windows(2);
    harness
        .mock_state
        .update_window(0, |window| window.min_height = 700);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    harness
        .world()
        .write_message(command(Operation::Stack(true)));
    harness.advance(Duration::from_millis(700));
    assert_eq!(kept_height(harness.world(), 0), Some((635, 700)));
    harness
        .mock_state
        .os_resize_window(0, Size::new(TEST_WINDOW_WIDTH, 900));
    harness.advance(SETTLE);
    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![900, COLUMN - 900]
    );
    assert_eq!(kept_height(harness.world(), 0), None);

    harness.run(vec![command(Operation::Focus(Direction::North))]);
    let mut writes = vec![harness.mock_state.resize_writes(0)];
    for _ in 0..2 {
        harness.run(vec![command(Operation::ResizeVertical(
            ResizeDirection::Shrink,
        ))]);
        harness.advance(SETTLE);
        writes.push(harness.mock_state.resize_writes(0));
    }

    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![700, COLUMN - 700]
    );
    assert_eq!(writes[2], writes[1], "{writes:?}");
}

/// The user's resize lands in the same frame as the 500 ms re-read of the
/// height window 0's app kept. The resize wins: the re-read does not turn
/// the user's height into a kept one, so a shrink still reaches 700 px.
#[test]
fn a_user_resize_in_the_frame_of_the_reread_wins() {
    let mut harness = tall_harness_with_windows(2);
    harness
        .mock_state
        .update_window(0, |window| window.min_height = 700);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    harness
        .world()
        .write_message(command(Operation::Stack(true)));
    let mut waited = Duration::ZERO;
    loop {
        harness.advance(Duration::from_millis(20));
        waited += Duration::from_millis(20);
        assert!(waited < SETTLE, "a re-read is waiting");
        let world = harness.world();
        let remaining = world
            .query::<&KeptHeightReread>()
            .iter(world)
            .next()
            .map(|reread| reread.0.remaining());
        if remaining.is_some_and(|remaining| remaining <= Duration::from_millis(20)) {
            break;
        }
    }
    harness.mock_state.update_window(0, |window| {
        window.frame.max.y = window.frame.min.y + 900;
    });
    harness
        .world()
        .write_message(Event::WindowResized { window_id: 0 });
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 0), None);
    assert_eq!(frame(harness.world(), 0).height(), 900);
    let world = harness.world();
    let rereads = world.query::<&KeptHeightReread>().iter(world).count();
    assert_eq!(rereads, 0, "the re-read ended with the record");

    harness.run(vec![
        command(Operation::Focus(Direction::North)),
        command(Operation::ResizeVertical(ResizeDirection::Shrink)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![700, COLUMN - 700]
    );
}

/// A native tab group, windows 1 and 2, sits under window 0. Its app keeps
/// 700 px of the 635 px it is asked for. The user then drags the group to 900
/// px. macOS reports a native tab's resize without marking its `Bounds`
/// changed, and the drag still ends the record: the group is 900 px, and a
/// later shrink takes it back down to the 700 px its app accepts.
#[test]
fn a_user_resize_of_a_native_tab_group_replaces_its_kept_height() {
    let mut harness = tall_harness_with_windows(2);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    {
        let state = harness.mock_state.clone();
        let world = harness.world();
        let leader = harness::find_window_entity(1, world);
        let frame = world
            .get::<Window>(leader)
            .map(|window| window.frame())
            .unwrap();
        let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, frame);
        state.window_visible(1, false);
        world.trigger(SpawnWindowTrigger(vec![window]));
    }
    harness.advance(SETTLE);
    for id in [1, 2] {
        harness
            .mock_state
            .update_window(id, |window| window.min_height = 700);
    }
    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
    ]);
    harness.advance(SETTLE);
    harness
        .world()
        .write_message(command(Operation::Stack(true)));
    harness.advance(Duration::from_millis(700));
    assert_eq!(column_heights(harness.world(), &[0, 1]), vec![570, 700]);
    assert_eq!(column_heights(harness.world(), &[0, 2]), vec![570, 700]);

    for id in [1, 2] {
        harness
            .mock_state
            .os_resize_window(id, Size::new(TEST_WINDOW_WIDTH, 900));
    }
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 1), None);
    assert_eq!(kept_height(harness.world(), 2), None);
    assert_eq!(frame(harness.world(), 1).height(), 900);

    for _ in 0..2 {
        harness.run(vec![command(Operation::ResizeVertical(
            ResizeDirection::Shrink,
        ))]);
        harness.advance(SETTLE);
    }
    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![COLUMN - 700, 700]
    );
    assert_eq!(frame(harness.world(), 2).height(), 700);
}

/// With a slow animation, a shrink below the height window 0's app accepts
/// steps the window down until the animation ends, then the stack settles
/// around the height the app kept. The kept height never holds the
/// animation back. Speed 8 is the slow end of the documented range.
#[test]
fn a_slow_resize_animation_finishes_before_the_stack_settles() {
    let mut harness = tall_harness_with_windows(2).with_config(animated(8.0));
    harness
        .mock_state
        .update_window(0, |window| window.min_height = 700);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
        command(Operation::Focus(Direction::North)),
    ]);
    harness.advance(SETTLE);
    harness
        .mock_state
        .os_resize_window(0, Size::new(TEST_WINDOW_WIDTH, 900));
    harness.advance(SETTLE);
    assert_eq!(kept_height(harness.world(), 0), None);

    harness.run(vec![command(Operation::ResizeVertical(
        ResizeDirection::Shrink,
    ))]);
    harness.advance(Duration::from_secs(10));

    let world = harness.world();
    let resizing = world
        .query_filtered::<(), With<ResizeMarker>>()
        .iter(world)
        .count();
    assert_eq!(resizing, 0, "the animation has finished");
    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![700, COLUMN - 700]
    );
    let writes = harness.mock_state.resize_writes(0);
    harness.advance(Duration::from_secs(10));
    assert_eq!(harness.mock_state.resize_writes(0), writes, "settled");
}

/// An app that applies each write 200 ms late gets every step of an
/// animated shrink and ends where a prompt app does, rather than stopping at
/// the first step it had not applied yet.
#[test]
fn a_late_app_ends_an_animated_shrink_where_a_prompt_app_does() {
    let shrink = |latency: Option<Duration>| {
        let mut harness = tall_harness_with_windows(2).with_config(animated(14.0));
        harness.run(vec![
            Event::MenuOpened { window_id: 0 },
            command(Operation::Focus(Direction::First)),
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(true)),
        ]);
        harness.advance(SETTLE);
        let shrink = command(Operation::ResizeVertical(ResizeDirection::Shrink));
        if let Some(latency) = latency {
            let mut app = LateApp::new(&harness, 1, latency, true);
            harness.world().write_message(shrink);
            app.advance(&mut harness, SETTLE + SETTLE);
        } else {
            harness.run(vec![shrink]);
            harness.advance(SETTLE);
        }
        column_heights(harness.world(), &[0, 1])
    };

    let prompt = shrink(None);
    assert!(prompt[1] < COLUMN / 2, "window 1 shrank: {prompt:?}");
    assert_eq!(shrink(Some(Duration::from_millis(200))), prompt);
}

/// Window 0's app applies each write late while window 1's app refuses less
/// than 500 px. Until window 0 catches up, its old height and window 1's do
/// not both fit, so the column keeps the split it was asked for. Once window
/// 0 catches up, window 1 keeps 500 px and the others share the rest.
#[test]
fn a_late_window_beside_one_that_keeps_a_taller_height() {
    for (latency, notify) in [(600, true), (200, false)] {
        let mut harness = tall_harness();
        harness
            .mock_state
            .update_window(1, |window| window.min_height = 500);
        harness.run(vec![
            Event::MenuOpened { window_id: 0 },
            command(Operation::Focus(Direction::First)),
            command(Operation::Focus(Direction::East)),
            command(Operation::Stack(true)),
            command(Operation::Focus(Direction::Last)),
        ]);
        harness.advance(SETTLE);
        let mut app = LateApp::new(&harness, 0, Duration::from_millis(latency), notify);
        harness
            .world()
            .write_message(command(Operation::Stack(true)));
        app.advance(&mut harness, SETTLE + SETTLE);

        let heights = column_heights(harness.world(), &[0, 1, 2]);
        assert_eq!(heights[1], 500, "{latency} ms: {heights:?}");
        assert_even(&[heights[0], heights[2]], COLUMN - 500);
    }
}

/// Shrinking or equalizing a stack held by window 0's kept height stops at
/// that height, so no other window is resized toward a split that the next
/// layout would undo.
#[test]
fn shrinking_or_equalizing_at_a_kept_height_moves_no_window() {
    for operation in [
        Operation::ResizeVertical(ResizeDirection::Shrink),
        Operation::Equalize,
    ] {
        let mut harness = tall_harness().with_config(animated(14.0));
        harness
            .mock_state
            .update_window(0, |window| window.min_height = 700);
        stack_three(&mut harness);
        harness.run(vec![
            command(Operation::Focus(Direction::North)),
            command(Operation::Focus(Direction::North)),
        ]);
        let before = all_writes(&harness, &[0, 1, 2]);
        harness.run(vec![command(operation.clone())]);
        harness.advance(SETTLE);

        assert_eq!(all_writes(&harness, &[0, 1, 2]), before, "{operation:?}");
        assert_eq!(
            column_heights(harness.world(), &[0, 1, 2]),
            vec![700, 285, 285]
        );
    }
}

/// The display gets 100 px shorter. The layout keeps window 0's height and
/// asks the last window for the rest, which its app refuses. Window 0 alone
/// changes nothing, so it is the height the last window kept that makes room
/// for it.
#[test]
fn a_last_window_that_keeps_a_taller_height_after_a_display_change_stays_on_screen() {
    const SHORTER: i32 = TALL - 100;
    let mut harness = tall_harness_with_windows(2);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(true)),
    ]);
    harness.advance(SETTLE);
    harness
        .mock_state
        .update_window(1, |window| window.min_height = 600);
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, SHORTER),
    );
    harness.run(vec![Event::DisplayConfigured {
        display_id: TEST_DISPLAY_ID,
    }]);
    harness.advance(SETTLE);
    harness.advance(SETTLE);

    let column = SHORTER - TEST_MENUBAR_HEIGHT;
    assert_eq!(
        column_heights(harness.world(), &[0, 1]),
        vec![column - 600, 600]
    );
    assert_eq!(frame(harness.world(), 1).max.y, SHORTER);
}
