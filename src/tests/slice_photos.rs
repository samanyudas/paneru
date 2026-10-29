//! The panels on edges shared with another display: which show, when the
//! engine photographs the windows behind them, and when it repaints.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use bevy::math::{IRect, IVec2};
use bevy::prelude::World;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGImage, CGImageAlphaInfo,
};

use super::*;
use crate::commands::{Command, Direction, Operation, Side};
use crate::config::{Config, MainOptions};
use crate::ecs::shared_edge::{EdgeSlice, showing_edge_slices};
use crate::ecs::slice_photos::{
    Digest, Lookup, Photo, PhotoError, REFRESH, SliceScreen, SliceScreenHandle, Ticket,
};
use crate::platform::WinID;

const SETTLE: Duration = Duration::from_secs(2);
const RIGHT: IRect = IRect {
    min: IVec2::new(TEST_DISPLAY_WIDTH, 0),
    max: IVec2::new(TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH, EXT_DISPLAY_HEIGHT),
};
const LEFT: IRect = IRect {
    min: IVec2::new(-EXT_DISPLAY_WIDTH, 0),
    max: IVec2::new(0, EXT_DISPLAY_HEIGHT),
};

/// What the engine asked of the slice screen, and what the screen answers.
#[derive(Default)]
pub(super) struct Screen {
    denied: bool,
    unseen: HashSet<WinID>,
    pub(super) placed: Vec<(WinID, EdgeSlice)>,
    asked: Vec<(WinID, IRect, u32)>,
    tickets: BTreeMap<u32, Ticket>,
    lookups: Vec<WinID>,
    painted: Vec<(WinID, Digest)>,
}

pub(super) type ScreenLog = Rc<RefCell<Screen>>;

struct RecordingScreen(ScreenLog);

impl SliceScreen for RecordingScreen {
    fn may_photograph(&self) -> bool {
        !self.0.borrow().denied
    }

    fn place(&mut self, slices: &[(WinID, EdgeSlice)]) {
        self.0.borrow_mut().placed = slices.to_vec();
    }

    fn photograph(
        &mut self,
        window: WinID,
        slice: &EdgeSlice,
        generation: u32,
        ticket: Ticket,
        lookup: Lookup,
    ) {
        let mut screen = self.0.borrow_mut();
        screen.asked.push((window, slice.in_window, generation));
        screen.tickets.insert(generation, ticket);
        if lookup == Lookup::Fresh {
            screen.lookups.push(window);
        }
    }

    fn paint(&mut self, window: WinID, photo: &Photo) {
        self.0.borrow_mut().painted.push((window, photo.digest));
    }

    fn seen(&self, window: WinID) -> bool {
        !self.0.borrow().unseen.contains(&window)
    }
}

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// Slices on, with windows moving as fast as Sam has them.
pub(super) fn animated() -> Config {
    (
        MainOptions {
            shared_edge_slices: Some(true),
            animation_speed: Some(14.0),
            ..MainOptions::default()
        },
        vec![],
    )
        .into()
}

fn switch(on: bool) -> Config {
    (
        MainOptions {
            shared_edge_slices: Some(on),
            ..MainOptions::default()
        },
        vec![],
    )
        .into()
}

/// The test display beside the empty displays `others`, holding `count`
/// windows with focus moved by `focus` from the first, drawing on a
/// recording screen that starts as `screen`.
fn laid_out(
    config: Config,
    screen: Screen,
    others: &[IRect],
    count: i32,
    focus: Option<Direction>,
) -> (TestHarness, ScreenLog) {
    let log = Rc::new(RefCell::new(screen));
    let mut harness = TestHarness::new().with_config(config);
    for (offset, bounds) in (0..).zip(others) {
        harness = harness.with_display(
            EXT_DISPLAY_ID + offset,
            *bounds,
            vec![EXT_WORKSPACE_ID + u64::from(offset)],
        );
    }
    let mut harness = harness.with_windows(count);
    harness
        .world()
        .insert_non_send(SliceScreenHandle(Box::new(RecordingScreen(log.clone()))));
    let mut events = vec![
        Event::MenuOpened { window_id: 0 },
        command(Operation::Focus(Direction::First)),
    ];
    events.extend(focus.map(|direction| command(Operation::Focus(direction))));
    harness.run(events);
    harness.advance(SETTLE);
    (harness, log)
}

/// Window 3 shows a slice on the edge shared with the display to the right.
fn beside(config: Config, screen: Screen) -> (TestHarness, ScreenLog) {
    laid_out(config, screen, &[RIGHT], 4, None)
}

/// Window 1 shows a slice on the left edge and window 5 on the right one.
fn between() -> (TestHarness, ScreenLog) {
    laid_out(
        switch(true),
        Screen::default(),
        &[LEFT, RIGHT],
        7,
        Some(Direction::Nth(4)),
    )
}

fn showing(world: &mut World) -> Vec<(WinID, EdgeSlice)> {
    world
        .run_system_cached(showing_edge_slices)
        .expect("the showing slices")
}

fn placed_ids(log: &ScreenLog) -> Vec<WinID> {
    log.borrow().placed.iter().map(|(id, _)| *id).collect()
}

fn asks_for(log: &ScreenLog, window: WinID) -> Vec<u32> {
    log.borrow()
        .asked
        .iter()
        .filter(|(id, _, _)| *id == window)
        .map(|(_, _, generation)| *generation)
        .collect()
}

fn last_ask(log: &ScreenLog, window: WinID) -> u32 {
    *asks_for(log, window)
        .last()
        .unwrap_or_else(|| panic!("window {window} was photographed"))
}

fn lookups_for(log: &ScreenLog, window: WinID) -> usize {
    log.borrow()
        .lookups
        .iter()
        .filter(|id| **id == window)
        .count()
}

/// Whether the engine called off the photo of ask `generation`, so its
/// capture never starts.
fn called_off(log: &ScreenLog, generation: u32) -> bool {
    log.borrow().tickets[&generation].called_off()
}

/// Windows 3 and 4 stacked in the column on the edge shared with the
/// display to the right, each showing half a slice.
fn stacked_beside() -> (TestHarness, ScreenLog) {
    let (mut harness, log) = laid_out(switch(true), Screen::default(), &[RIGHT], 5, None);
    harness.run(vec![
        command(Operation::Focus(Direction::Last)),
        command(Operation::Stack(Side::West)),
        command(Operation::Focus(Direction::First)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(placed_ids(&log), vec![3, 4]);
    (harness, log)
}

/// Closes window 4, which grows the slice of window 3 without hiding it.
fn close_four(harness: &mut TestHarness) {
    let before = showing(harness.world());
    harness.mock_state.os_close_window(4);
    harness.run(vec![]);
    harness.advance(SETTLE);
    let after = showing(harness.world());
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].0, 3);
    assert_ne!(after[0], before[0], "window 3's slice grew");
}

/// A one-pixel image, standing in for a photo's pixels.
fn pixel() -> CFRetained<CGImage> {
    let space = CGColorSpace::new_device_rgb();
    // SAFETY: a null buffer asks Core Graphics to allocate its own.
    let context = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            1,
            1,
            8,
            4,
            space.as_deref(),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .expect("a bitmap context");
    CGBitmapContextCreateImage(Some(&context)).expect("an image")
}

fn photo(digest: u64) -> Photo {
    Photo {
        digest: Digest(digest),
        image: pixel(),
    }
}

/// Delivers the answer to the ask `generation` for `window`, as the
/// capture callback would.
fn arrive(
    harness: &mut TestHarness,
    window: WinID,
    generation: u32,
    result: Result<Photo, PhotoError>,
) {
    harness.world().write_message(Event::SlicePhoto {
        window_id: window,
        generation,
        result,
    });
    harness.app.update();
}

/// `beside` with the first photo of window 3 painted.
fn painted_beside() -> (TestHarness, ScreenLog) {
    painted_beside_with(switch(true))
}

/// `painted_beside` with `config`, which turns slices on.
pub(super) fn painted_beside_with(config: Config) -> (TestHarness, ScreenLog) {
    painted(beside(config, Screen::default()), 3)
}

/// Window 0 shows a slice on the edge shared with the display to the left,
/// with its first photo painted, under `config`, which turns slices on.
pub(super) fn painted_west_with(config: Config) -> (TestHarness, ScreenLog) {
    let laid_out = laid_out(config, Screen::default(), &[LEFT], 4, Some(Direction::Last));
    painted(laid_out, 0)
}

fn painted(
    (mut harness, log): (TestHarness, ScreenLog),
    window: WinID,
) -> (TestHarness, ScreenLog) {
    let first = last_ask(&log, window);
    arrive(&mut harness, window, first, Ok(photo(7)));
    assert_eq!(log.borrow().painted, vec![(window, Digest(7))]);
    (harness, log)
}

#[test]
fn a_showing_slice_gets_a_panel_at_its_place_and_a_photo_of_its_window() {
    let (mut harness, log) = beside(switch(true), Screen::default());

    let showing = showing(harness.world());
    assert_eq!(showing.len(), 1, "window 3 shows a slice: {showing:?}");
    let (id, slice) = showing[0];
    assert_eq!(log.borrow().placed, showing, "one panel at the slice");
    assert_eq!(
        log.borrow().asked.last().map(|(id, crop, _)| (*id, *crop)),
        Some((id, slice.in_window)),
        "a photo of window 3 for its slice"
    );
    assert!(log.borrow().painted.is_empty(), "nothing to paint yet");

    arrive(&mut harness, 3, last_ask(&log, 3), Ok(photo(7)));
    assert_eq!(log.borrow().painted, vec![(3, Digest(7))]);
}

#[test]
fn a_shown_slice_is_photographed_again_after_the_refresh_period() {
    let (mut harness, log) = painted_beside();
    let asks = asks_for(&log, 3).len();

    harness.advance(REFRESH);

    assert_eq!(asks_for(&log, 3).len(), asks + 1);
}

#[test]
fn an_unchanged_photo_does_not_repaint_and_a_changed_one_does() {
    let (mut harness, log) = painted_beside();

    harness.advance(REFRESH);
    arrive(&mut harness, 3, last_ask(&log, 3), Ok(photo(7)));
    assert_eq!(log.borrow().painted, vec![(3, Digest(7))], "unchanged");

    harness.advance(REFRESH);
    arrive(&mut harness, 3, last_ask(&log, 3), Ok(photo(8)));
    assert_eq!(
        log.borrow().painted,
        vec![(3, Digest(7)), (3, Digest(8))],
        "changed"
    );
}

#[test]
fn a_photo_from_an_earlier_ask_is_dropped() {
    let (mut harness, log) = painted_beside();
    let earlier = last_ask(&log, 3);
    harness.advance(REFRESH);
    let latest = last_ask(&log, 3);
    assert_ne!(earlier, latest);

    arrive(&mut harness, 3, earlier, Ok(photo(9)));
    assert_eq!(log.borrow().painted, vec![(3, Digest(7))], "stale photo");

    arrive(&mut harness, 3, latest, Ok(photo(8)));
    assert_eq!(log.borrow().painted, vec![(3, Digest(7)), (3, Digest(8))]);
}

#[test]
fn no_photo_is_taken_while_no_one_can_see_the_panel() {
    let (mut harness, log) = painted_beside();
    let asks = asks_for(&log, 3).len();

    log.borrow_mut().unseen.insert(3);
    harness.advance(REFRESH * 2);
    assert_eq!(asks_for(&log, 3).len(), asks, "unseen");

    log.borrow_mut().unseen.clear();
    harness.advance(REFRESH);
    assert_eq!(asks_for(&log, 3).len(), asks + 1, "seen again");
}

#[test]
fn with_the_switch_off_no_panel_shows_and_no_photo_is_taken() {
    let (mut harness, log) = beside(switch(false), Screen::default());

    assert_eq!(showing(harness.world()).len(), 1, "window 3 still hides");
    assert!(log.borrow().placed.is_empty());
    assert!(log.borrow().asked.is_empty());
}

#[test]
fn turning_the_switch_off_closes_the_panels() {
    let (mut harness, log) = painted_beside();
    let asks = log.borrow().asked.len();

    harness.world().insert_resource(switch(false));
    harness.advance(REFRESH * 2);

    assert!(log.borrow().placed.is_empty());
    assert_eq!(log.borrow().asked.len(), asks, "no photo after the switch");
}

#[test]
fn without_permission_no_panel_shows_until_it_is_granted() {
    let denied = Screen {
        denied: true,
        ..Screen::default()
    };
    let (mut harness, log) = beside(switch(true), denied);
    harness.advance(REFRESH * 2);
    assert!(log.borrow().placed.is_empty());
    assert!(log.borrow().asked.is_empty());

    log.borrow_mut().denied = false;
    harness.advance(REFRESH);

    assert_eq!(placed_ids(&log), vec![3]);
    assert_eq!(asks_for(&log, 3).len(), 1, "a photo once granted");
}

#[test]
fn a_slice_that_stops_showing_closes_its_panel() {
    let (mut harness, log) = painted_beside();
    let asked = last_ask(&log, 3);

    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);
    assert!(showing(harness.world()).is_empty());
    assert!(log.borrow().placed.is_empty());

    arrive(&mut harness, 3, asked, Ok(photo(8)));
    assert_eq!(log.borrow().painted, vec![(3, Digest(7))], "a late photo");
}

#[test]
fn a_window_gone_closes_only_its_panel() {
    let (mut harness, log) = between();
    assert_eq!(placed_ids(&log), vec![1, 5]);
    let asks = asks_for(&log, 1).len();

    arrive(
        &mut harness,
        1,
        last_ask(&log, 1),
        Err(PhotoError::WindowGone),
    );
    harness.advance(REFRESH * 2);

    assert_eq!(placed_ids(&log), vec![5]);
    assert_eq!(asks_for(&log, 1).len(), asks, "no photo of window 1 again");
}

#[test]
fn a_refused_photo_closes_every_panel_until_the_config_changes() {
    let (mut harness, log) = between();
    let (one, five) = (last_ask(&log, 1), last_ask(&log, 5));
    let asks = log.borrow().asked.len();

    arrive(&mut harness, 1, one, Err(PhotoError::Refused));
    assert!(log.borrow().placed.is_empty());
    arrive(&mut harness, 5, five, Ok(photo(7)));
    harness.advance(REFRESH * 2);
    assert!(log.borrow().painted.is_empty(), "no photo after a refusal");
    assert_eq!(
        log.borrow().asked.len(),
        asks,
        "no photo asked after a refusal"
    );

    harness.world().insert_resource(switch(true));
    harness.app.update();
    assert_eq!(placed_ids(&log), vec![1, 5]);
    assert_eq!(
        log.borrow().asked.len(),
        asks + 2,
        "both photographed again"
    );
}

#[test]
fn turning_the_switch_off_calls_off_a_photo_on_its_way() {
    let (mut harness, log) = beside(switch(true), Screen::default());
    let asked = last_ask(&log, 3);
    assert!(!called_off(&log, asked), "wanted while the switch is on");

    harness.world().insert_resource(switch(false));
    harness.app.update();

    assert!(called_off(&log, asked));
}

#[test]
fn a_slice_that_stops_showing_calls_off_its_photo_on_its_way() {
    let (mut harness, log) = beside(switch(true), Screen::default());
    let asked = last_ask(&log, 3);

    harness.run(vec![command(Operation::Focus(Direction::Last))]);
    harness.advance(SETTLE);

    assert!(showing(harness.world()).is_empty());
    assert!(called_off(&log, asked));
}

#[test]
fn a_newer_ask_calls_off_the_one_before() {
    let (mut harness, log) = stacked_beside();
    let earlier = last_ask(&log, 3);

    close_four(&mut harness);
    let latest = last_ask(&log, 3);

    assert_ne!(earlier, latest);
    assert!(called_off(&log, earlier));
    assert!(!called_off(&log, latest), "the newer ask stays wanted");
}

#[test]
fn a_slice_that_moves_while_no_one_can_see_its_panel_waits_to_be_seen() {
    let (mut harness, log) = stacked_beside();
    arrive(&mut harness, 3, last_ask(&log, 3), Ok(photo(7)));
    let asks = asks_for(&log, 3).len();

    log.borrow_mut().unseen.insert(3);
    close_four(&mut harness);
    harness.advance(REFRESH * 2);
    assert_eq!(asks_for(&log, 3).len(), asks, "unseen");

    log.borrow_mut().unseen.clear();
    harness.advance(REFRESH);
    let moved = showing(harness.world())[0].1;
    assert_eq!(
        log.borrow().asked.last().map(|(id, crop, _)| (*id, *crop)),
        Some((3, moved.in_window)),
        "seen again, a photo of the moved slice"
    );
}

#[test]
fn a_slice_looks_its_window_up_once_until_it_moves() {
    let (mut harness, log) = stacked_beside();
    let (asks, lookups) = (asks_for(&log, 3).len(), lookups_for(&log, 3));
    arrive(&mut harness, 3, last_ask(&log, 3), Ok(photo(7)));
    for digest in 8..11 {
        harness.advance(REFRESH);
        arrive(&mut harness, 3, last_ask(&log, 3), Ok(photo(digest)));
    }
    assert_eq!(asks_for(&log, 3).len(), asks + 3, "three refreshes");
    assert_eq!(lookups_for(&log, 3), lookups, "no lookup for a refresh");

    close_four(&mut harness);
    assert_eq!(
        lookups_for(&log, 3),
        lookups + 1,
        "a lookup for the moved slice"
    );
}

#[test]
fn focus_on_a_tucked_window_slides_its_photo_in_without_a_new_photo() {
    let (mut harness, log) = painted(beside(animated(), Screen::default()), 3);
    let asks = log.borrow().asked.len();

    harness
        .world()
        .write_message(command(Operation::Focus(Direction::Last)));
    let mut places = Vec::new();
    for _ in 0..60 {
        harness.advance(Duration::from_millis(20));
        if let Some((_, slice)) = log.borrow().placed.iter().find(|(id, _)| *id == 3) {
            places.push(slice.on_screen);
        }
    }

    let rest = harness.mock_state.ax_frame(3);
    assert!(
        rest.min.x >= 0 && rest.max.x <= TEST_DISPLAY_WIDTH,
        "{rest:?} in view"
    );
    assert!(placed_ids(&log).is_empty(), "the panel closed");
    places.dedup();
    assert!(places.len() >= 5, "the photo slides through {places:?}");
    assert_eq!(
        places.last(),
        Some(&rest),
        "the photo stops where window 3 rests"
    );
    assert_eq!(log.borrow().asked.len(), asks, "the photo it had slides");
    assert_eq!(log.borrow().painted, vec![(3, Digest(7))]);
}

const DISPLAY: IRect = IRect {
    min: IVec2::ZERO,
    max: IVec2::new(TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
};

/// Whether most of `frame` is on the test display, where macOS shows it.
fn in_view(frame: IRect) -> bool {
    frame.intersect(DISPLAY).width() * 2 > frame.width()
}

/// What the user sees of a window after one frame: where it is, and its
/// panel if the panel shows a photo.
struct Seen {
    window: IRect,
    photo: Option<IRect>,
}

/// Focuses with `reveal` until window `id` rests in view, then focuses
/// with `leave`, which moves it past the shared edge, for `frames` frames.
/// The photo of window `id` arrives after `photo_after` frames, if at all,
/// and `then` focuses again after its frames.
fn leave(
    (mut harness, log): (TestHarness, ScreenLog),
    id: WinID,
    (reveal, leave): (Direction, Direction),
    photo_after: Option<usize>,
    then: Option<(usize, Direction)>,
) -> (Vec<Seen>, IRect, TestHarness) {
    harness
        .world()
        .write_message(command(Operation::Focus(reveal)));
    harness.advance(SETTLE);
    let start = harness.mock_state.ax_frame(id);
    assert!(in_view(start), "window {id} rests in view at {start:?}");
    assert!(placed_ids(&log).is_empty(), "no panel while it is in view");
    let (asks, paints) = (asks_for(&log, id).len(), log.borrow().painted.len());

    harness
        .world()
        .write_message(command(Operation::Focus(leave)));
    let mut seen = Vec::new();
    for step in 0..60 {
        if photo_after == Some(step) {
            assert_eq!(asks_for(&log, id).len(), asks + 1, "one photo of {id}");
            let asked = last_ask(&log, id);
            arrive(&mut harness, id, asked, Ok(photo(8)));
        }
        if let Some((_, direction)) = then.as_ref().filter(|(after, _)| *after == step) {
            harness
                .world()
                .write_message(command(Operation::Focus(direction.clone())));
        }
        harness.advance(Duration::from_millis(20));
        let photo = (log.borrow().painted.len() > paints)
            .then(|| {
                log.borrow()
                    .placed
                    .iter()
                    .find(|(placed, _)| *placed == id)
                    .map(|(_, slice)| slice.on_screen)
            })
            .flatten();
        seen.push(Seen {
            window: harness.mock_state.ax_frame(id),
            photo,
        });
    }
    (seen, start, harness)
}

fn assert_shown_every_frame(id: WinID, seen: &[Seen]) {
    for (step, frame) in seen.iter().enumerate() {
        assert!(
            in_view(frame.window) || frame.photo.is_some(),
            "window {id} or its photo shows after step {step}: window at {:?}",
            frame.window
        );
    }
}

/// Window `id` slides out past the shared edge with its photo arriving
/// after 5 frames, as fast as photos arrive on Sam's Mac.
fn assert_slides_out(laid_out: (TestHarness, ScreenLog), id: WinID, focus: (Direction, Direction)) {
    let (seen, start, mut harness) = leave(laid_out, id, focus, Some(5), None);

    let rest = showing(harness.world());
    let [(shown, slice)] = rest.as_slice() else {
        panic!("one slice shows at rest: {rest:?}")
    };
    assert_eq!(*shown, id, "the slice of window {id} shows");
    let end = seen.last().expect("steps");
    assert!(!in_view(end.window), "window {id} tucks: {:?}", end.window);
    assert_eq!(
        end.photo,
        Some(slice.on_screen),
        "the photo rests as the slice"
    );
    assert_shown_every_frame(id, &seen);
    let waiting = seen
        .iter()
        .take_while(|frame| frame.photo.is_none())
        .count();
    assert!(
        waiting >= 5,
        "window {id} shows itself until its photo arrives: {waiting} steps"
    );
    let on_display = |frame: IRect| frame.intersect(DISPLAY).width();
    assert!(
        on_display(seen[0].window) < on_display(start),
        "window {id} moves with the row before its photo arrives: {:?} from {start:?}",
        seen[0].window
    );
    assert_eq!(
        seen[waiting].photo,
        Some(seen[waiting - 1].window.intersect(DISPLAY)),
        "the photo first shows where window {id} was"
    );
    let mut places = seen[waiting..]
        .iter()
        .filter_map(|frame| frame.photo)
        .collect::<Vec<_>>();
    for pair in places.windows(2) {
        assert!(
            pair[1].width() <= pair[0].width(),
            "the photo only moves out: {places:?}"
        );
    }
    places.dedup();
    assert!(places.len() >= 5, "the photo slides through {places:?}");
}

#[test]
fn focus_away_from_a_window_at_the_shared_edge_slides_it_out_under_its_photo() {
    assert_slides_out(
        painted(beside(animated(), Screen::default()), 3),
        3,
        (Direction::Last, Direction::First),
    );
    assert_slides_out(
        painted_west_with(animated()),
        0,
        (Direction::First, Direction::Last),
    );
}

#[test]
fn focus_back_during_a_slide_out_slides_the_photo_back_in() {
    let laid_out = painted(beside(animated(), Screen::default()), 3);
    let focus = (Direction::Last, Direction::First);
    let (seen, _, harness) = leave(laid_out, 3, focus, Some(5), Some((8, Direction::Last)));

    assert_shown_every_frame(3, &seen);
    let rest = harness.mock_state.ax_frame(3);
    assert!(in_view(rest), "window 3 rests in view at {rest:?}");
    let end = seen.last().expect("steps");
    assert_eq!(end.photo, None, "the photo closed");
    let turned = seen
        .windows(2)
        .filter_map(|pair| Some((pair[0].photo?, pair[1].photo?)))
        .any(|(was, now)| now.width() > was.width());
    assert!(turned, "the photo turns back");
}

#[test]
fn a_window_whose_photo_never_arrives_tucks_after_a_short_wait() {
    let laid_out = painted(beside(animated(), Screen::default()), 3);
    let focus = (Direction::Last, Direction::First);
    let (seen, _, _) = leave(laid_out, 3, focus, None, None);

    let waited = seen
        .iter()
        .take_while(|frame| in_view(frame.window))
        .count();
    assert!(
        (5..=15).contains(&waited),
        "window 3 shows itself for {waited} frames, then tucks"
    );
    assert!(seen[waited..].iter().all(|frame| !in_view(frame.window)));
}

#[test]
fn focus_away_during_a_slide_in_slides_the_photo_back_out() {
    let (mut harness, log) = painted(beside(animated(), Screen::default()), 3);
    let panel = || {
        log.borrow()
            .placed
            .iter()
            .find(|(id, _)| *id == 3)
            .map(|(_, slice)| slice.on_screen)
    };
    harness
        .world()
        .write_message(command(Operation::Focus(Direction::Last)));
    harness.advance(Duration::from_millis(60));
    let turned_at = panel().expect("the photo slides in");

    harness
        .world()
        .write_message(command(Operation::Focus(Direction::First)));
    let mut places = Vec::new();
    for step in 0..40 {
        harness.advance(Duration::from_millis(20));
        let window = harness.mock_state.ax_frame(3);
        let photo = panel();
        assert!(
            in_view(window) || photo.is_some(),
            "window 3 or its photo shows after step {step}: window at {window:?}"
        );
        places.extend(photo);
    }

    let rest = showing(harness.world());
    let [(3, slice)] = rest.as_slice() else {
        panic!("window 3 shows a slice at rest: {rest:?}")
    };
    assert!(
        !in_view(harness.mock_state.ax_frame(3)),
        "window 3 stays tucked"
    );
    assert_eq!(
        places.last(),
        Some(&slice.on_screen),
        "the photo rests as the slice"
    );
    let widths = std::iter::once(turned_at.width())
        .chain(places.iter().map(IRect::width))
        .collect::<Vec<_>>();
    assert!(
        widths.windows(2).all(|pair| pair[1] <= pair[0]),
        "the photo turns back out at once: {widths:?}"
    );
    places.dedup();
    assert!(places.len() >= 5, "the photo slides through {places:?}");
}
