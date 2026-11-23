//! The picture cover over keyboard strip moves (miri#509).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bevy::math::{IRect, IVec2};
use bevy::prelude::*;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGDirectDisplayID, CGImage,
    CGImageAlphaInfo,
};

use super::*;
use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::slice_photos::{Digest, Photo};
use crate::ecs::strip_cover::{
    CoverScene, CoverScreen, CoverScreenHandle, CoverSlide, CoverSubject,
};
use crate::events::Event;
use crate::manager::Window;
use crate::platform::WinID;

const SETTLE: Duration = Duration::from_secs(2);
const ONE_FRAME: Duration = Duration::from_millis(20);
/// The step between the points of a slide's path, as the engine sends it.
const PATH_STEP: f64 = 1.0 / 120.0;

/// What the engine asked of the cover screen.
#[derive(Default)]
struct Screen {
    denied: bool,
    asked: Vec<(CoverSubject, u32)>,
    /// The strip origin and picture count of each cover shown.
    shown: Vec<(IVec2, usize)>,
    /// Each window's picture, by its digest, on each cover shown.
    shown_digests: Vec<Vec<(WinID, u64)>>,
    /// The first and last strip x of each slide.
    slides: Vec<(f64, f64)>,
    /// Every strip x of each slide's path, one each [`PATH_STEP`].
    paths: Vec<Vec<f64>>,
    /// Each strip x the engine told the tap about.
    tracked: Vec<f64>,
    /// Each strip x a swipe put the cover's strip at.
    placed: Vec<f64>,
    /// Where Core Animation shows the cover's strip, if the test says.
    showing: Option<f64>,
    /// Each window's frame where its picture starts and where it heads, for
    /// each morph.
    morphs: Vec<Vec<(WinID, IRect, IRect)>>,
    /// How far along Core Animation shows the morph, if the test says.
    morph_share: Option<f64>,
    /// Each picture of a window asked for: the window, its size, and the
    /// ask's generation.
    window_asks: Vec<(WinID, IVec2, u32)>,
    /// Each picture a resize's cover put in place of an older one, and the
    /// size it shows.
    replaced: Vec<(WinID, IVec2)>,
    /// Each window whose frame was opened onto the window itself.
    opened: Vec<WinID>,
    /// Each picture shown at the size its window heads to.
    headed: Vec<(WinID, IVec2)>,
    /// How many morphs had started when each of `headed` was given: the
    /// picture shows from the start of the next one.
    headed_before: Vec<usize>,
    hidden: usize,
}

type ScreenLog = Rc<RefCell<Screen>>;

struct RecordingScreen(ScreenLog);

impl CoverScreen for RecordingScreen {
    fn may_photograph(&self) -> bool {
        !self.0.borrow().denied
    }

    fn photograph_window(
        &mut self,
        window: WinID,
        size: IVec2,
        _display: CGDirectDisplayID,
        generation: u32,
    ) {
        let mut screen = self.0.borrow_mut();
        screen
            .asked
            .push((CoverSubject::Window(window), generation));
        screen.window_asks.push((window, size, generation));
    }

    fn photograph_desktop(
        &mut self,
        display: CGDirectDisplayID,
        _excluding: &[WinID],
        generation: u32,
    ) {
        self.0
            .borrow_mut()
            .asked
            .push((CoverSubject::Desktop(display), generation));
    }

    fn show(&mut self, scene: &CoverScene) {
        let mut screen = self.0.borrow_mut();
        screen.shown.push((scene.strip, scene.pictures.len()));
        screen.shown_digests.push(
            scene
                .pictures
                .iter()
                .map(|(id, _, photo)| (*id, photo.digest.0))
                .collect(),
        );
    }

    fn slide(&mut self, _display: CGDirectDisplayID, path: &[f64], _step: f64) {
        let (first, last) = (path[0], *path.last().unwrap());
        let mut screen = self.0.borrow_mut();
        screen.slides.push((first, last));
        screen.paths.push(path.to_vec());
    }

    fn morph(
        &mut self,
        _display: CGDirectDisplayID,
        frames: &[(WinID, IRect, IRect)],
        shares: &[f64],
        _step: f64,
    ) {
        assert_eq!(shares.last(), Some(&1.0), "a morph ends where it heads");
        self.0.borrow_mut().morphs.push(frames.to_vec());
    }

    fn replace_picture(
        &mut self,
        _display: CGDirectDisplayID,
        window: WinID,
        size: IVec2,
        _photo: &Photo,
    ) -> bool {
        self.0.borrow_mut().replaced.push((window, size));
        true
    }

    fn open(&mut self, _display: CGDirectDisplayID, window: WinID) {
        self.0.borrow_mut().opened.push(window);
    }

    fn head_picture(
        &mut self,
        _display: CGDirectDisplayID,
        window: WinID,
        size: IVec2,
        _photo: &Photo,
    ) -> bool {
        let mut screen = self.0.borrow_mut();
        screen.headed.push((window, size));
        let morphs = screen.morphs.len();
        screen.headed_before.push(morphs);
        true
    }

    fn morph_share(&self, _display: CGDirectDisplayID) -> Option<f64> {
        self.0.borrow().morph_share
    }

    fn track(&mut self, _display: CGDirectDisplayID, x: f64) {
        self.0.borrow_mut().tracked.push(x);
    }

    fn place(&mut self, _display: CGDirectDisplayID, x: f64) {
        self.0.borrow_mut().placed.push(x);
    }

    fn showing(&self, _display: CGDirectDisplayID) -> Option<f64> {
        self.0.borrow().showing
    }

    fn shown(&self) -> Vec<CGDirectDisplayID> {
        let screen = self.0.borrow();
        if screen.shown.len() > screen.hidden {
            vec![TEST_DISPLAY_ID]
        } else {
            Vec::new()
        }
    }

    fn hide(&mut self, _display: CGDirectDisplayID) {
        self.0.borrow_mut().hidden += 1;
    }

    fn forget(&mut self, _window: Option<WinID>) {}
}

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

fn config(on: bool, animation_speed: Option<f64>) -> Config {
    (
        MainOptions {
            slide_window_pictures: Some(on),
            animation_speed,
            swipe_gesture_fingers: Some(3),
            ..MainOptions::default()
        },
        vec![],
    )
        .into()
}

/// Four 700-point columns on the 1024-point test display, the first
/// focused, with a recording cover screen.
fn harness(config: Config) -> (TestHarness, ScreenLog) {
    harness_with(config, false)
}

/// As [`harness`], with every write to a window going through the engine's
/// per-app writers when `queued`, so a test can hold writes on their way.
fn harness_with(config: Config, queued: bool) -> (TestHarness, ScreenLog) {
    let log = ScreenLog::default();
    let mut harness = TestHarness::new().with_config(config);
    if queued {
        harness.mock_state.queue_writes(TEST_PROCESS_ID);
    }
    for id in 0..4 {
        harness = harness.with_window(id, |window| {
            window.frame.max.x = window.frame.min.x + 700;
        });
    }
    harness
        .world()
        .insert_non_send(CoverScreenHandle(Box::new(RecordingScreen(log.clone()))));
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus(Direction::First),
    ]);
    harness.advance(SETTLE);
    (harness, log)
}

fn focus(direction: Direction) -> Event {
    Event::Command {
        command: Command::Window(Operation::Focus(direction)),
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

fn covered(world: &mut World) -> bool {
    world.query::<&CoverSlide>().iter(world).next().is_some()
}

/// Answers every picture the engine asks for, frame by frame, long enough
/// for it to have a picture of each window and of the desktop. An ask the
/// engine gave up on gets its answer too, which the engine drops.
fn take_pictures(harness: &mut TestHarness, log: &ScreenLog) {
    let mut answered = 0;
    for _ in 0..30 {
        harness.advance(ONE_FRAME);
        let asked = log.borrow().asked.clone();
        for (subject, generation) in asked.iter().skip(answered) {
            send(
                harness,
                Event::CoverPhoto {
                    subject: *subject,
                    generation: *generation,
                    result: Ok(Photo {
                        digest: Digest(u64::from(*generation)),
                        image: pixel(),
                        edge_color: None,
                    }),
                },
            );
        }
        answered = asked.len();
    }
}

/// Steps one frame at a time until the cover is gone, and returns every x
/// window `id` showed on the way.
fn path_of(harness: &mut TestHarness, id: WinID) -> Vec<i32> {
    let mut path = vec![window_x(harness.world(), id)];
    for _ in 0..100 {
        harness.advance(ONE_FRAME);
        let x = window_x(harness.world(), id);
        if path.last() != Some(&x) {
            path.push(x);
        }
        if !covered(harness.world()) && path.len() > 1 {
            harness.advance(SETTLE);
            return path;
        }
    }
    panic!("the move ends");
}

/// The column east of the first one is partly hidden, so focusing it moves
/// the strip 376 points: 1400 less the 1024-point display.
const STEP: i32 = 1400 - TEST_DISPLAY_WIDTH;

/// With pictures ready, a keyboard move slides them on a cover while the
/// real windows stay put, then moves each window once, to where it lands,
/// and takes the cover down.
#[test]
fn a_keyboard_move_slides_pictures_and_moves_each_window_once() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    assert_eq!(window_x(harness.world(), 1), 700);

    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()), "the move is under a cover");
    assert_eq!(log.borrow().shown.len(), 1, "one cover goes up");
    assert_eq!(
        log.borrow().shown[0].1,
        4,
        "a picture of every window, so a turned slide shows each window"
    );
    assert_eq!(
        log.borrow().slides.first().copied(),
        Some((0.0, f64::from(-STEP))),
        "the pictures slide the strip from where it was to where it heads"
    );

    // The pictures slide for the length of the ease, about 237 ms.
    harness.advance(ONE_FRAME * 8);
    assert!(covered(harness.world()), "still sliding after 160 ms");
    assert_eq!(window_x(harness.world(), 1), 700, "the window waits");

    let path = path_of(&mut harness, 1);
    assert_eq!(path, vec![700, 700 - STEP], "the window moves once");
    assert_eq!(log.borrow().hidden, 1, "the cover comes down");

    // The tap learns where the pictures are as they slide, so a press on
    // one reaches its window.
    let tracked = log.borrow().tracked.clone();
    assert_eq!(tracked.first(), Some(&0.0), "from where the strip was");
    assert_eq!(tracked.last(), Some(&f64::from(-STEP)), "to where it lands");
    assert!(
        tracked.windows(2).all(|pair| pair[1] <= pair[0]) && tracked.len() > 3,
        "along the slide, got {tracked:?}"
    );
    assert_eq!(log.borrow().shown.len(), 1);
}

/// A move before the pictures are ready moves the real windows as before.
#[test]
fn without_pictures_the_windows_move_as_before() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    send(&mut harness, focus(Direction::East));
    let path = path_of_uncovered(&mut harness, 1);
    assert!(log.borrow().shown.is_empty(), "no cover");
    assert!(path.len() > 2, "the window eases there, got {path:?}");
    assert_eq!(path.last(), Some(&(700 - STEP)));
}

/// Steps until window `id` stops moving, and returns every x it showed.
fn path_of_uncovered(harness: &mut TestHarness, id: WinID) -> Vec<i32> {
    let mut path = vec![window_x(harness.world(), id)];
    for _ in 0..100 {
        harness.advance(ONE_FRAME);
        let x = window_x(harness.world(), id);
        if path.last() != Some(&x) {
            path.push(x);
        }
    }
    path
}

/// With animation off, as under Reduce Motion, the engine takes no pictures
/// and shows no cover: the strip moves in one step.
#[test]
fn without_animation_no_pictures_and_no_cover() {
    let (mut harness, log) = harness(config(true, None));
    harness.advance(SETTLE);
    assert!(log.borrow().asked.is_empty(), "no pictures");
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME * 3);
    assert!(log.borrow().shown.is_empty(), "no cover");
    assert_eq!(window_x(harness.world(), 1), 700 - STEP);
}

/// With the switch off, the engine takes no pictures.
#[test]
fn with_the_switch_off_no_pictures() {
    let (mut harness, log) = harness(config(false, Some(14.0)));
    harness.advance(SETTLE);
    assert!(log.borrow().asked.is_empty(), "no pictures");
}

/// Without the Screen Recording permission, the engine takes no pictures.
#[test]
fn without_permission_no_pictures() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    log.borrow_mut().denied = true;
    harness.advance(SETTLE * 2);
    log.borrow_mut().asked.clear();
    harness.advance(SETTLE);
    assert!(log.borrow().asked.is_empty(), "no pictures");
}

/// A second move while the pictures slide turns the slide from where it
/// shows, and the windows still move once, to the second target.
#[test]
fn a_second_move_turns_the_slide() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);

    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME * 3);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let slides = log.borrow().slides.clone();
    assert_eq!(slides.len(), 2, "the slide turns, got {slides:?}");
    let (turn, end) = slides[1];
    assert!(
        turn < 0.0 && turn > f64::from(-STEP),
        "it turns from partway, got {turn}"
    );
    assert!(end < f64::from(-STEP), "toward the next column, got {end}");

    let path = path_of(&mut harness, 2);
    assert_eq!(path.len(), 2, "the window moves once, got {path:?}");
    assert_eq!(log.borrow().shown.len(), 1, "one cover throughout");
}

fn swipe(delta: f64) -> Event {
    Event::Swipe {
        delta,
        fingers: 3,
        distance_cm: 0.0,
    }
}

/// With pictures ready, a swipe moves the pictures with the fingers while
/// the real windows stay put, then lands them in one step when it settles.
#[test]
fn a_swipe_moves_the_pictures_and_lands_each_window_once() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    assert_eq!(window_x(harness.world(), 1), 700);

    send(&mut harness, Event::TouchpadDown);
    let mut path = vec![window_x(harness.world(), 1)];
    for _ in 0..6 {
        send(&mut harness, swipe(0.05));
        harness.advance(ONE_FRAME);
        path.push(window_x(harness.world(), 1));
    }
    send(&mut harness, Event::TouchpadUp);
    assert_eq!(log.borrow().shown.len(), 1, "the swipe is under a cover");
    assert!(
        path.iter().all(|x| *x == 700),
        "the real window waits while the fingers move, got {path:?}"
    );
    let placed = log.borrow().placed.clone();
    assert!(
        placed.len() >= 3 && placed.windows(2).all(|pair| pair[1] <= pair[0]),
        "the pictures follow the fingers, got {placed:?}"
    );

    for _ in 0..100 {
        harness.advance(ONE_FRAME);
        let x = window_x(harness.world(), 1);
        if path.last() != Some(&x) {
            path.push(x);
        }
    }
    path.dedup();
    let landing = *path.last().unwrap();
    assert_eq!(path.len(), 2, "the window moves once, got {path:?}");
    assert!(landing < 700, "the flick moved the strip, got {landing}");
    assert_eq!(log.borrow().hidden, 1, "the cover comes down");
    assert_eq!(
        log.borrow().placed.last().copied(),
        Some(f64::from(landing - 700)),
        "the pictures land where the windows do"
    );
}

/// A cover whose move went away, as when its strip or display did, comes
/// down on its own.
#[test]
fn a_cover_without_its_move_comes_down() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()));
    let strips = harness
        .world()
        .query_filtered::<Entity, With<CoverSlide>>()
        .iter(harness.world())
        .collect::<Vec<_>>();
    // The strip rests where the slide headed, so no new move starts.
    for strip in strips {
        let mut strip = harness.world().entity_mut(strip);
        strip.remove::<CoverSlide>();
        strip.get_mut::<crate::ecs::Position>().unwrap().0.x = -STEP;
    }
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().hidden, 1, "the orphaned cover comes down");
}

/// Modifier-scroll stays uncovered: the wheel scrolls in steps that real
/// windows follow well.
#[test]
fn a_wheel_scroll_gets_no_cover() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::Scroll { delta: 30.0 });
    harness.advance(ONE_FRAME);
    send(&mut harness, Event::Scroll { delta: 30.0 });
    harness.advance(SETTLE);
    assert!(log.borrow().shown.is_empty(), "no cover");
}

/// An `animation_speed` of 0 would make an endless slide: the strip moves
/// as before, and the engine takes no pictures.
#[test]
fn a_zero_animation_speed_gets_no_cover() {
    let (mut harness, log) = harness(config(true, Some(0.0)));
    harness.advance(SETTLE);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME * 3);
    assert!(log.borrow().asked.is_empty(), "no pictures");
    assert!(log.borrow().shown.is_empty(), "no cover");
}

/// The landing's snap guard ends as the cover lifts, so a layout change
/// right after animates again.
#[test]
fn the_landing_snap_guard_ends_with_the_cover() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    for _ in 0..60 {
        if !covered(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
    assert!(!covered(harness.world()), "the cover lifted");
    harness.advance(ONE_FRAME);
    let guards = harness
        .world()
        .query::<&crate::ecs::workspace::SnapStripMarker>()
        .iter(harness.world())
        .count();
    assert_eq!(guards, 0, "no snap guard outlives the cover");
}

/// A strip that loses its display mid-slide lets its cover go.
#[test]
fn a_strip_that_loses_its_display_lets_its_cover_go() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()));
    let strips = harness
        .world()
        .query_filtered::<Entity, With<CoverSlide>>()
        .iter(harness.world())
        .collect::<Vec<_>>();
    for strip in strips {
        harness.world().entity_mut(strip).remove::<ChildOf>();
    }
    harness.advance(ONE_FRAME);
    assert!(!covered(harness.world()), "the move ends");
    assert_eq!(log.borrow().hidden, 1, "the cover comes down");
}

/// A speed so fast that windows move in one step plays no cover.
#[test]
fn an_instant_animation_speed_gets_no_cover() {
    let (mut harness, log) = harness(config(true, Some(1000.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME * 3);
    assert!(log.borrow().shown.is_empty(), "no cover");
}

/// The slide follows what Core Animation shows: when the pictures have
/// already landed there, as after the engine stalled, the real windows land
/// without waiting out the engine's own count of the slide.
#[test]
fn the_landing_follows_the_pictures_on_screen() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()));
    log.borrow_mut().showing = Some(f64::from(-STEP));
    harness.advance(ONE_FRAME * 3);
    assert_eq!(
        window_x(harness.world(), 1),
        700 - STEP,
        "the windows land once the pictures show landed"
    );
}

/// While Core Animation still shows the pictures short of their target,
/// the real windows wait, however far the engine has counted.
#[test]
fn the_windows_wait_while_the_pictures_still_move() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = Some(f64::from(-STEP) / 2.0);
    harness.advance(ONE_FRAME * 15);
    assert!(covered(harness.world()), "still sliding on screen");
    assert_eq!(window_x(harness.world(), 1), 700, "the window waits");
    // A sample a hair off the target, as Core Animation can report.
    log.borrow_mut().showing = Some(f64::from(-STEP) + 1e-6);
    harness.advance(ONE_FRAME * 3);
    assert_eq!(window_x(harness.world(), 1), 700 - STEP, "then lands");
}

/// A key back toward where the slide began turns it there, and the focused
/// column ends up shown.
#[test]
fn a_key_back_mid_slide_turns_the_slide_home() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME * 2);
    send(&mut harness, focus(Direction::West));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().slides.len(), 2, "the slide turns back");
    harness.advance(SETTLE);
    assert_eq!(window_x(harness.world(), 0), 0, "the focused column shows");
}

/// A key during a covered swipe ends the cover, and the strip goes where
/// the key sends it.
#[test]
fn a_key_during_a_covered_swipe_ends_the_cover() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().shown.len(), 1, "the swipe is under a cover");
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME * 3);
    let swiping = harness
        .world()
        .query::<&crate::ecs::strip_cover::CoverSwipe>()
        .iter(harness.world())
        .count();
    assert_eq!(swiping, 0, "the swipe's cover ends");
    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);
    assert!(!covered(harness.world()));
}

/// A strip move that arrives during a covered swipe, as when the swipe's
/// new focus brings its column into view, keeps the cover up and slides the
/// pictures on to where the strip heads. Taking the cover down there showed
/// the windows where the swipe began, then the desktop between them.
#[test]
fn a_strip_move_during_a_covered_swipe_slides_the_pictures_on() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().shown.len(), 1, "the swipe is under a cover");
    let slides = log.borrow().slides.len();

    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().hidden, 0, "the cover stays up");
    assert!(covered(harness.world()), "the strip is still covered");
    assert_eq!(
        log.borrow().slides.len(),
        slides + 1,
        "the pictures slide on to where the strip heads"
    );
    assert_eq!(log.borrow().shown.len(), 1, "it is the same cover");

    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);
    assert!(!covered(harness.world()), "the cover lifts once it lands");
}

/// The strip under a covered swipe, once one has started.
fn swiped_strip(world: &mut World) -> Entity {
    world
        .query_filtered::<Entity, With<crate::ecs::strip_cover::CoverSwipe>>()
        .iter(world)
        .next()
        .expect("a swipe under the cover")
}

/// Starts a swipe under the cover, and holds the fingers still on the
/// trackpad so the strip rests where they put it.
fn start_covered_swipe(harness: &mut TestHarness, log: &ScreenLog) -> Entity {
    take_pictures(harness, log);
    send(harness, Event::TouchpadDown);
    send(harness, swipe(0.05));
    harness.advance(ONE_FRAME);
    send(harness, swipe(0.05));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().shown.len(), 1, "the swipe is under a cover");
    let strip = swiped_strip(harness.world());
    harness
        .world()
        .get_mut::<crate::ecs::Scrolling>(strip)
        .expect("the strip scrolls")
        .velocity = 0.0;
    strip
}

/// A move to where the strip already rests, arriving in the frame the swipe
/// lands, lands the swipe as it would without it: the real windows take
/// their places under the cover at once. It slid the pictures over no
/// distance first, which only held the cover up longer.
#[test]
fn a_swipe_that_lands_with_a_move_to_where_it_rests_lands_at_once() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    let strip = start_covered_swipe(&mut harness, &log);
    let slides = log.borrow().slides.len();
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;

    let mut entity = harness.world().entity_mut(strip);
    entity.remove::<crate::ecs::Scrolling>();
    entity.insert(crate::ecs::RepositionMarker(position));
    harness.advance(ONE_FRAME);

    let world = harness.world();
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_none(),
        "the swipe ended"
    );
    let slide = world
        .get::<CoverSlide>(strip)
        .expect("the swipe lands under the cover");
    assert_eq!(slide.heading(), None, "it has landed already");
    assert_eq!(
        log.borrow().slides.len(),
        slides,
        "no slide over no distance"
    );
    assert_eq!(log.borrow().hidden, 0, "the cover stays up while it lands");
    assert_eq!(
        log.borrow().placed.last().copied(),
        Some(f64::from(position.x)),
        "the pictures rest where the strip does"
    );
}

/// A move to where the strip already rests, arriving mid-swipe, leaves the
/// swipe and its cover going: the fingers keep the strip.
#[test]
fn a_move_to_where_a_swiped_strip_rests_keeps_the_swipe() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    let strip = start_covered_swipe(&mut harness, &log);
    let slides = log.borrow().slides.len();
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;

    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(position));
    harness.advance(ONE_FRAME);

    let world = harness.world();
    assert!(
        world.get::<crate::ecs::Scrolling>(strip).is_some(),
        "the fingers still drive the strip"
    );
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_some(),
        "the swipe stays under its cover"
    );
    assert!(world.get::<CoverSlide>(strip).is_none(), "no slide starts");
    assert_eq!(log.borrow().slides.len(), slides, "the pictures stay put");
    assert_eq!(log.borrow().hidden, 0, "the cover stays up");
    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);
    assert!(!covered(harness.world()), "the cover lifts once it lands");
}

/// A move to where a covered swipe is, arriving once the fingers lift and
/// before its settle starts, stops the strip there, as it does a strip
/// without the cover: the real windows land there under the cover at once.
/// It went unheeded, and the strip settled on past it.
#[test]
fn a_move_to_where_a_lifted_covered_swipe_is_lands_it_there() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    for _ in 0..2 {
        send(&mut harness, swipe(0.05));
        harness.advance(ONE_FRAME);
    }
    let strip = swiped_strip(harness.world());
    {
        let mut scroll = harness
            .world()
            .get_mut::<crate::ecs::Scrolling>(strip)
            .expect("the strip scrolls");
        scroll.is_user_swiping = false;
        scroll.fingers_down = false;
        assert!(scroll.velocity.abs() > 0.1, "the strip still moves");
        assert!(scroll.settle.is_none(), "no settle has started");
    }
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    let slides = log.borrow().slides.len();

    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(position));
    harness.advance(ONE_FRAME);
    let world = harness.world();
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_none(),
        "the swipe ended"
    );
    assert!(
        world.get::<crate::ecs::Scrolling>(strip).is_none(),
        "and stopped"
    );
    let slide = world
        .get::<CoverSlide>(strip)
        .expect("the swipe lands under the cover");
    assert_eq!(slide.heading(), None, "where it is");
    assert_eq!(log.borrow().slides.len(), slides, "with no slide");
    assert_eq!(
        log.borrow().placed.last().copied(),
        Some(f64::from(position.x)),
        "the pictures rest where the strip is"
    );
    harness.advance(SETTLE);
    assert!(!covered(harness.world()), "the cover lifts once it lands");
    assert_eq!(
        strip_x(harness.world(), strip),
        position.x,
        "the strip stays"
    );
}

/// The strip a keyboard slide moves, and its slide.
fn sliding_strip(world: &mut World) -> (Entity, CoverSlide) {
    world
        .query::<(Entity, &CoverSlide)>()
        .iter(world)
        .next()
        .map(|(entity, slide)| (entity, slide.clone()))
        .expect("a strip under the cover")
}

fn strip_x(world: &mut World, strip: Entity) -> i32 {
    world
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0
        .x
}

/// Whether any window of the test app has a write on its way.
fn a_write_is_pending(world: &mut World) -> bool {
    world
        .query::<&Window>()
        .iter(world)
        .any(|window| window.write_pending())
}

/// A swipe that starts while a keyboard slide's real windows land under the
/// cover, one of them still taking its write, carries on on the same
/// cover. The cover came down there, showed the windows at the slide's
/// target, and moved them again to where the fingers took the strip.
#[test]
fn a_swipe_during_a_slides_landing_carries_on_under_the_cover() {
    let (mut harness, log) = harness_with(config(true, Some(14.0)), true);
    for _ in 0..20 {
        harness.advance(ONE_FRAME);
        harness.mock_state.flush_writes(SETTLE);
    }
    take_pictures(&mut harness, &log);
    harness.mock_state.flush_writes(SETTLE);

    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, _) = sliding_strip(harness.world());
    // Core Animation shows the pictures landed, and the app is slow to
    // take the landing's writes.
    log.borrow_mut().showing = Some(f64::from(-STEP));
    harness.mock_state.hold_writes(TEST_PROCESS_ID);
    harness.advance(ONE_FRAME);
    let (_, slide) = sliding_strip(harness.world());
    assert_eq!(slide.heading(), None, "the real windows land");
    assert!(
        a_write_is_pending(harness.world()),
        "a window still takes its write"
    );
    log.borrow_mut().showing = None;

    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.02));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().hidden, 0, "the cover stays up");
    assert_eq!(log.borrow().shown.len(), 1, "it is the same cover");
    let world = harness.world();
    assert!(world.get::<CoverSlide>(strip).is_none(), "the slide ended");
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_some(),
        "the swipe goes under the cover"
    );
    let x = strip_x(harness.world(), strip);
    assert!(x < -STEP, "the fingers move the strip on, got {x}");
    assert_eq!(
        log.borrow().placed.last().copied(),
        Some(f64::from(x)),
        "the pictures show where the fingers put the strip"
    );

    harness.mock_state.release_writes(TEST_PROCESS_ID);
    send(&mut harness, Event::TouchpadUp);
    for _ in 0..100 {
        harness.advance(ONE_FRAME);
        harness.mock_state.flush_writes(SETTLE);
    }
    assert!(!covered(harness.world()), "the cover lifts once it lands");
    assert_eq!(log.borrow().hidden, 1, "and comes down once");
    assert_eq!(log.borrow().shown.len(), 1);
}

/// A swipe that catches a keyboard slide mid-way takes the pictures over
/// where they show: the strip neither jumps to the slide's target nor back
/// to where the real windows wait, and the cover stays up.
#[test]
fn a_swipe_that_catches_a_slide_takes_it_over_where_it_shows() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let showing = -100.0;
    log.borrow_mut().showing = Some(showing);
    harness.advance(ONE_FRAME);
    assert_eq!(slide.heading().map(|target| target.x), Some(-STEP));
    let rest = strip_x(harness.world(), strip);
    assert_eq!(rest, 0, "the real strip waits");

    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    assert_eq!(log.borrow().hidden, 0, "the cover stays up");
    let world = harness.world();
    assert!(world.get::<CoverSlide>(strip).is_none(), "the slide ended");
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_some(),
        "the swipe goes under the cover"
    );
    let x = strip_x(harness.world(), strip);
    assert!(
        (f64::from(x) - showing).abs() < 40.0,
        "the strip moves on from where the pictures showed, got {x}"
    );
    assert_eq!(log.borrow().placed.last().copied(), Some(f64::from(x)));

    // The next frame's fingers move the pictures as any covered swipe's.
    send(&mut harness, swipe(0.02));
    harness.advance(ONE_FRAME);
    let next = strip_x(harness.world(), strip);
    assert!(next < x, "the fingers move the strip on, got {next}");
    assert_eq!(log.borrow().placed.last().copied(), Some(f64::from(next)));

    send(&mut harness, Event::TouchpadUp);
    harness.advance(SETTLE);
    assert!(!covered(harness.world()), "the cover lifts once it lands");
    assert_eq!(log.borrow().hidden, 1, "and comes down once");
    assert_eq!(log.borrow().shown.len(), 1, "one cover throughout");
}

/// How fast a slide's pictures start out, in points per second.
fn starting_speed(path: &[f64]) -> f64 {
    (path[1] - path[0]) / PATH_STEP
}

/// A copy of the strip's scroll state, which a test reads before the cover
/// takes it.
fn scrolling_of(world: &mut World, strip: Entity) -> crate::ecs::Scrolling {
    let scroll = world
        .get::<crate::ecs::Scrolling>(strip)
        .expect("the strip scrolls");
    crate::ecs::Scrolling {
        velocity: scroll.velocity,
        position: scroll.position,
        is_user_swiping: scroll.is_user_swiping,
        last_event: scroll.last_event,
        moved_at: scroll.moved_at,
        input: scroll.input,
        anchor: scroll.anchor,
        settle: scroll.settle,
        ease: scroll.ease,
        detent: scroll.detent,
        origin: scroll.origin,
        landing_focus: scroll.landing_focus,
        focus_at_start: scroll.focus_at_start,
        start: scroll.start,
        fingers_down: scroll.fingers_down,
        lifted_at: scroll.lifted_at,
        caught: scroll.caught,
        slope: scroll.slope,
        excess: scroll.excess,
    }
}

/// A Snap that sets a slide's target again in the frame a swipe starts, as
/// Snap does with the strip it moves, leaves the swipe to take the pictures
/// over. The repeated target ended the cover, and the strip jumped to the
/// slide's target.
#[test]
fn a_swipe_that_starts_as_the_slides_target_is_set_again_takes_it_over() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let target = slide.heading().expect("the pictures slide");

    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(target));
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().hidden, 0, "the cover stays up");
    let world = harness.world();
    assert!(world.get::<CoverSlide>(strip).is_none(), "the slide ended");
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_some(),
        "the swipe takes the pictures over"
    );
    assert!(
        world.get::<crate::ecs::RepositionMarker>(strip).is_none(),
        "the repeated target is spent"
    );
}

/// A new target in the frame a swipe starts over a slide turns the slide
/// there on the same cover: the move wins, as one during a covered swipe
/// does. It ended the cover, and the strip jumped to the old target.
#[test]
fn a_new_target_as_a_swipe_starts_turns_the_slide_on_its_cover() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let target = slide.heading().expect("the pictures slide");
    let slides = log.borrow().slides.len();

    let next = target.with_x(target.x - 300);
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(next));
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().hidden, 0, "the cover stays up");
    assert_eq!(log.borrow().slides.len(), slides + 1, "the slide turns");
    let (_, turned) = sliding_strip(harness.world());
    assert_eq!(turned.heading(), Some(next), "to the new target");
    assert!(
        harness
            .world()
            .get::<crate::ecs::Scrolling>(strip)
            .is_none(),
        "the swipe gives way to the move"
    );
}

/// A new target in the frame a swipe catches a slide mid-way turns the
/// slide, and the real windows stay where they wait until it lands. The
/// swipe's first frame had moved the strip, and the windows took that place,
/// which the pictures never showed.
#[test]
fn a_turn_over_a_swipe_keeps_the_real_windows_until_it_lands() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let target = slide.heading().expect("the pictures slide");
    log.borrow_mut().showing = Some(-100.0);
    harness.advance(ONE_FRAME);

    let next = target.with_x(target.x - 300);
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(next));
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.05));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    let (_, turned) = sliding_strip(harness.world());
    assert_eq!(turned.heading(), Some(next), "the slide turns");
    assert_eq!(
        strip_x(harness.world(), strip),
        0,
        "the strip stays laid out"
    );
    let path = path_of(&mut harness, 1);
    assert_eq!(
        path,
        vec![700, 700 + next.x],
        "window 1 waits, then moves once, to where it lands"
    );
}

/// A move up or down in the frame a swipe starts over a slide ends the
/// cover, as any vertical move does, rather than hand the pictures to the
/// swipe and drop the move.
#[test]
fn a_vertical_move_as_a_swipe_starts_ends_the_slide() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let target = slide.heading().expect("the pictures slide");

    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(target.with_y(target.y + 50)));
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    let world = harness.world();
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_none(),
        "the swipe does not take the pictures over"
    );
    assert!(world.get::<CoverSlide>(strip).is_none(), "the slide ended");
    assert!(
        world.get::<crate::ecs::RepositionMarker>(strip).is_some(),
        "the move goes on"
    );
    assert_eq!(log.borrow().hidden, 1, "the cover came down");
    assert_eq!(log.borrow().shown.len(), 1, "and no swipe put one up");
}

/// A swipe that catches a slide's pictures stretched past the strip's
/// start shows the strip where they were, not stretched a second time.
#[test]
fn a_swipe_that_catches_a_stretched_slide_shows_it_where_it_was() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    let strip = harness
        .world()
        .query_filtered::<Entity, With<crate::ecs::layout::LayoutStrip>>()
        .iter(harness.world())
        .next()
        .expect("a strip");
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    // A move past the strip's start, as Center gives a first column that
    // is narrower than the display.
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(position.with_x(120)));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()), "the move is under a cover");
    log.borrow_mut().showing = Some(100.0);

    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.0001));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    let x = strip_x(harness.world(), strip);
    assert!(
        (x - 100).abs() <= 1,
        "the strip shows where the pictures were, got {x}"
    );
}

/// A swipe that catches a slide while Core Animation cannot tell where it
/// shows moves on from the place this frame's slide reaches, as the cover
/// shows it, not from a frame before.
#[test]
fn a_swipe_that_catches_a_slide_starts_from_this_frames_place() {
    let start = |swiped: bool| {
        let (mut harness, log) = harness(config(true, Some(14.0)));
        take_pictures(&mut harness, &log);
        send(&mut harness, focus(Direction::East));
        harness.advance(ONE_FRAME * 3);
        if swiped {
            send(&mut harness, Event::TouchpadDown);
            send(&mut harness, swipe(0.000_01));
        }
        harness.advance(ONE_FRAME);
        let screen = log.borrow();
        if swiped {
            screen.placed.last().copied().expect("the swipe places it")
        } else {
            screen.tracked.last().copied().expect("the slide shows")
        }
    };
    let (slid, caught) = (start(false), start(true));
    assert!(
        (slid - caught).abs() <= 1.0,
        "the swipe starts where this frame's slide shows: {caught} vs {slid}"
    );
}

/// A move during a covered swipe held at the most it may stretch past the
/// strip's start starts from rest: fingers that push on there move nothing.
#[test]
fn a_move_during_a_held_stretch_starts_from_rest() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    for _ in 0..10 {
        send(&mut harness, swipe(-0.5));
        harness.advance(ONE_FRAME);
    }
    let strip = swiped_strip(harness.world());
    let scroll = scrolling_of(harness.world(), strip);
    assert!(scroll.velocity < 0.0, "the fingers push on past the start");
    assert_eq!(scroll.slope, 0.0, "the strip no longer moves with them");
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;

    let away = position.with_x(position.x + 40);
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(away));
    harness.advance(ONE_FRAME);
    let path = log.borrow().paths.last().cloned().expect("a slide");
    let carried = starting_speed(&path);
    // A slide from rest over 40 points starts at about 150 points a second.
    assert!(carried < 250.0, "the slide starts from rest, got {carried}");
}

/// Windows of `widths` side by side on the 1024-point test display, the
/// first focused, with a recording cover screen.
fn harness_of(config: Config, widths: &[i32]) -> (TestHarness, ScreenLog) {
    let log = ScreenLog::default();
    let mut harness = TestHarness::new().with_config(config);
    for (id, width) in (0..).zip(widths) {
        harness = harness.with_window(id, |window| {
            window.frame.max.x = window.frame.min.x + width;
        });
    }
    harness
        .world()
        .insert_non_send(CoverScreenHandle(Box::new(RecordingScreen(log.clone()))));
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus(Direction::First),
    ]);
    harness.advance(SETTLE);
    (harness, log)
}

/// The one strip of a test.
fn the_strip(world: &mut World) -> Entity {
    world
        .query_filtered::<Entity, With<crate::ecs::layout::LayoutStrip>>()
        .iter(world)
        .next()
        .expect("a strip")
}

/// Moves the strip under the cover to strip x `x`, as Center does.
fn cover_move_to(harness: &mut TestHarness, x: i32) -> Entity {
    let strip = the_strip(harness.world());
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(position.with_x(x)));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()), "the move is under a cover");
    strip
}

/// A window-landing swipe that catches a slide's pictures past the column
/// it would step back to counts only the fingers' own travel: it neither
/// steps there nor pulses. The stretch it took on to show the pictures
/// there counted as travel, and stepped.
#[test]
fn a_swipe_that_catches_past_a_column_does_not_step() {
    let (mut harness, log) = harness(landing_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    let (strip, _) = sliding_strip(harness.world());
    // 60 points past the first column's place, which the strip steps back
    // to from the column the slide heads to.
    log.borrow_mut().showing = Some(60.0);
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.0001));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;

    let scroll = scrolling_of(harness.world(), strip);
    assert!(
        (scroll.position - scroll.origin).abs() < 1.0,
        "the travel is the fingers' own: {}",
        scroll.position - scroll.origin
    );
    let headed = entity_of(harness.world(), 1);
    assert_eq!(scroll.detent, Some(headed), "no step back, and no pulse");
    let x = strip_x(harness.world(), strip);
    assert!((x - 60).abs() <= 1, "the strip shows where it was, got {x}");
}

/// A swipe that catches a narrow strip's pictures far from where a Center
/// sends them shows the strip where they were. A strip that fits could not
/// stretch that far, and jumped a third of the way there.
#[test]
fn a_swipe_that_catches_a_centering_narrow_strip_shows_it_where_it_was() {
    let (mut harness, log) = harness_of(config(true, Some(14.0)), &[300]);
    take_pictures(&mut harness, &log);
    let strip = cover_move_to(&mut harness, 362);
    log.borrow_mut().showing = Some(2.0);
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.0001));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    let x = strip_x(harness.world(), strip);
    assert!((x - 2).abs() <= 1, "the strip shows where it was, got {x}");

    // Fingers that move the strip back toward its place move it there
    // smoothly.
    let mut path = vec![x];
    for _ in 0..6 {
        send(&mut harness, swipe(-0.02));
        harness.advance(ONE_FRAME);
        path.push(strip_x(harness.world(), strip));
    }
    assert!(
        path.windows(2)
            .all(|pair| pair[1] >= pair[0] && pair[1] - pair[0] < 60),
        "the strip follows without a jump: {path:?}"
    );
}

/// A swipe that starts as a slide's real windows land at a place past the
/// strip's start, as Center gives, shows the strip where the pictures are.
/// The pictures had landed, so the swipe was not moved to them, and the
/// strip stretched a second time.
#[test]
fn a_swipe_over_a_landing_past_the_start_shows_the_strip_where_it_is() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    harness.mock_state.show_late(0);
    let strip = cover_move_to(&mut harness, 162);
    log.borrow_mut().showing = Some(162.0);
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    let (_, slide) = sliding_strip(harness.world());
    assert_eq!(slide.heading(), None, "the real windows land");

    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.0001));
    harness.advance(ONE_FRAME);
    harness.mock_state.show_now(0);
    let x = strip_x(harness.world(), strip);
    assert!(
        (x - 162).abs() <= 1,
        "the strip shows where it was, got {x}"
    );
}

/// A swipe that caught a strip further past its start than a stretch can
/// show, as Center leaves a narrow first column of an overflowing strip,
/// settles from where the strip shows once the fingers lift. The settle
/// began from the stretch alone, and the strip jumped most of the way back.
#[test]
fn a_caught_strip_settles_from_where_it_shows() {
    let (mut harness, log) = harness_of(config(true, Some(14.0)), &[300, 700, 700, 700]);
    take_pictures(&mut harness, &log);
    // Center puts the 300-point first column in the middle of the display.
    let strip = cover_move_to(&mut harness, 362);
    log.borrow_mut().showing = Some(362.0);
    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.0001));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    assert_eq!(strip_x(harness.world(), strip), 362, "the catch keeps it");
    assert!(
        scrolling_of(harness.world(), strip).excess.is_some(),
        "further out than a stretch can show"
    );

    send(&mut harness, Event::TouchpadUp);
    let mut path = vec![362];
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
        path.push(strip_x(harness.world(), strip));
    }
    let first = path.iter().copied().find(|x| *x != 362);
    assert!(
        first.is_some_and(|x| x > 250),
        "the settle starts where the strip shows: {path:?}"
    );
    assert!(
        path.windows(2)
            .all(|pair| pair[1] <= pair[0] && pair[0] - pair[1] < 120),
        "and eases back without a jump: {path:?}"
    );
    assert_eq!(path.last().copied(), Some(0), "to the strip's start");
}

/// A window-landing swipe released while it resists past a column settles
/// at the speed the strip shows, not the fingers' raw speed: fingers that
/// move back quickly while the strip barely follows make no flick, and the
/// strip lands without swinging past its column.
#[test]
fn a_resisting_release_settles_at_the_speed_it_shows() {
    let (mut harness, _log) = harness(landing_config());
    let strip = the_strip(harness.world());
    send(&mut harness, Event::TouchpadDown);
    for _ in 0..10 {
        send(&mut harness, swipe(0.15));
        harness.advance(ONE_FRAME);
    }
    let scroll = scrolling_of(harness.world(), strip);
    assert!(scroll.slope < 0.35, "the strip resists: {}", scroll.slope);
    let shown = strip_x(harness.world(), strip);
    // The fingers turn back at 800 points a second, which the strip shows
    // at under the 256 a second a flick takes.
    let velocity = -800.0 / f64::from(TEST_DISPLAY_WIDTH);
    assert!(-velocity * f64::from(TEST_DISPLAY_WIDTH) * scroll.slope < 256.0);
    harness
        .world()
        .get_mut::<crate::ecs::Scrolling>(strip)
        .expect("the strip scrolls")
        .velocity = velocity;
    send(&mut harness, Event::TouchpadUp);

    let mut ease = None;
    for _ in 0..10 {
        harness.advance(ONE_FRAME);
        ease = harness
            .world()
            .get::<crate::ecs::Scrolling>(strip)
            .and_then(|scroll| scroll.ease);
        if ease.is_some() {
            break;
        }
    }
    let ease = ease.expect("the swipe settles");
    assert!(
        (ease.from - f64::from(shown)).abs() <= 1.0,
        "the settle starts where the strip shows: {} vs {shown}",
        ease.from
    );
    assert!(ease.flick.is_none(), "no flick");
    let mut path = vec![strip_x(harness.world(), strip)];
    for _ in 0..60 {
        harness.advance(ONE_FRAME);
        path.push(strip_x(harness.world(), strip));
    }
    let landed = *path.last().expect("a place");
    assert!(
        path.windows(2).all(|pair| pair[1] >= pair[0]) && path.iter().all(|x| *x <= landed),
        "it lands without swinging past: {path:?}"
    );
}

/// With `auto_center`, a strip held at its start shows no speed for fingers
/// that push on past it, even in a frame whose push rounds to nothing: a
/// move then starts from rest.
#[test]
fn an_auto_centered_strip_held_at_its_start_has_no_speed() {
    let centered: Config = (
        MainOptions {
            slide_window_pictures: Some(true),
            animation_speed: Some(14.0),
            swipe_gesture_fingers: Some(3),
            auto_center: Some(true),
            ..MainOptions::default()
        },
        vec![],
    )
        .into();
    let (mut harness, log) = harness(centered);
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    let mut held = Vec::new();
    for _ in 0..8 {
        send(&mut harness, swipe(-0.2));
        harness.advance(ONE_FRAME);
        let strip = swiped_strip(harness.world());
        held.push(strip_x(harness.world(), strip));
    }
    let strip = swiped_strip(harness.world());
    let bound = *held.last().expect("a place");
    assert_eq!(held[held.len() - 2], bound, "held at the start: {held:?}");
    // Fingers that still push past the start, too slowly to move the strip
    // a whole point this frame.
    harness
        .world()
        .get_mut::<crate::ecs::Scrolling>(strip)
        .expect("the strip scrolls")
        .velocity = -0.02;
    harness.advance(ONE_FRAME);
    let scroll = scrolling_of(harness.world(), strip);
    assert_eq!(strip_x(harness.world(), strip), bound, "still held");
    assert_eq!(scroll.slope, 0.0, "the strip does not move with them");
    let speed = crate::ecs::scroll::strip_speed(
        &scroll,
        f64::from(TEST_DISPLAY_WIDTH),
        &config(true, Some(14.0)),
    );
    assert_eq!(speed, 0.0, "a move from here starts from rest");
}

/// The cover's config with window-landing swipes, which step one column.
fn landing_config() -> Config {
    Config::try_from(
        "[options]\nanimation_speed = 14\nslide_window_pictures = true\n[swipe.gesture]\nfingers_count = 3\nwindow_landing = true\nhaptics = false\n[bindings]\n",
    )
    .expect("config should parse")
}

/// A swipe that catches a slide near the strip's end resists past the end
/// from its first frame, and steps from the column the slide headed to. The
/// scroll systems ran before the swipe moved to where the pictures showed,
/// so the strip jumped past the end for a frame, and the landing column was
/// the one the real windows waited at.
#[test]
fn a_swipe_that_catches_a_slide_resists_and_steps_from_its_first_frame() {
    let (mut harness, log) = harness(landing_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::Last));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let end = slide.heading().expect("the pictures slide").x;
    let showing = f64::from(end) + 16.0;
    log.borrow_mut().showing = Some(showing);

    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.5));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    assert!(
        harness
            .world()
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_some(),
        "the swipe takes the pictures over"
    );
    // Half a swipe across the 1024-point display, at the default
    // sensitivity, toward the strip's end.
    let unresisted = showing - 0.5 * f64::from(TEST_DISPLAY_WIDTH) * 0.35;
    let x = strip_x(harness.world(), strip);
    assert!(
        f64::from(x) > unresisted + 20.0,
        "the strip resists past its end at once: {x}, unresisted {unresisted}"
    );
    assert_eq!(
        log.borrow().placed.last().copied(),
        Some(f64::from(x)),
        "the pictures show it there"
    );
    let last = entity_of(harness.world(), 3);
    assert_eq!(
        scrolling_of(harness.world(), strip).detent,
        Some(last),
        "the swipe steps from the last column, where the slide headed"
    );
}

/// Lifts the fingers of a covered window-landing swipe, whose settle
/// focuses its column only once it lands, and runs frames until the settle
/// moves the strip. Returns the strip and how fast it settles.
fn settling_covered_swipe(harness: &mut TestHarness, log: &ScreenLog) -> (Entity, f64) {
    take_pictures(harness, log);
    send(harness, Event::TouchpadDown);
    for _ in 0..4 {
        send(harness, swipe(0.05));
        harness.advance(ONE_FRAME);
    }
    let strip = swiped_strip(harness.world());
    send(harness, Event::TouchpadUp);
    for _ in 0..20 {
        harness.advance(ONE_FRAME);
        let scroll = scrolling_of(harness.world(), strip);
        let speed = crate::ecs::scroll::strip_speed(
            &scroll,
            f64::from(TEST_DISPLAY_WIDTH),
            &landing_config(),
        );
        if scroll.ease.is_some() && speed.abs() > 300.0 {
            return (strip, speed);
        }
    }
    panic!("the swipe settles");
}

/// A move that arrives while a covered swipe settles, as Center does, slides
/// the pictures on at the speed the settle had. The settle's state was gone
/// before the cover saw the move, so the pictures started from rest.
#[test]
fn a_move_during_a_covered_settle_carries_its_speed_on() {
    let (mut harness, log) = harness(landing_config());
    let (strip, speed) = settling_covered_swipe(&mut harness, &log);
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    // A short move, which a slide from rest would start far slower than
    // the settle moves.
    #[allow(clippy::cast_possible_truncation)]
    let away = position.with_x(position.x + 40 * speed.signum() as i32);
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(away));
    harness.advance(ONE_FRAME);

    let (_, slide) = sliding_strip(harness.world());
    assert_eq!(slide.heading(), Some(away), "the pictures slide on");
    let path = log.borrow().paths.last().cloned().expect("a slide");
    let carried = starting_speed(&path);
    assert!(
        carried * speed > 0.0 && carried.abs() > 0.5 * speed.abs(),
        "the slide starts at the settle's speed: {carried} vs {speed}"
    );
}

/// A move to where a settling covered swipe already is stops the settle
/// there, as it stops a settle without the cover: the real windows land
/// there under the cover at once. A settle that heads there goes on.
#[test]
fn a_move_to_where_a_covered_settle_is_lands_it_there() {
    let (mut harness, log) = harness(landing_config());
    let (strip, _) = settling_covered_swipe(&mut harness, &log);
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    let target = scrolling_of(harness.world(), strip)
        .settle
        .expect("the strip settles");
    assert_ne!(target.round(), f64::from(position.x), "elsewhere");
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(position));
    harness.advance(ONE_FRAME);

    let world = harness.world();
    assert!(
        world.get::<crate::ecs::Scrolling>(strip).is_none(),
        "the settle stops"
    );
    let slide = world
        .get::<CoverSlide>(strip)
        .expect("the swipe lands under the cover");
    assert_eq!(slide.heading(), None, "where the strip is");
    harness.advance(SETTLE);
    assert!(!covered(harness.world()), "the cover lifts once it lands");
    assert_eq!(
        strip_x(harness.world(), strip),
        position.x,
        "the strip stays"
    );
}

/// A move to where a covered settle heads, arriving as it gets there,
/// leaves the settle going under its cover.
#[test]
fn a_move_to_where_a_covered_settle_heads_leaves_it_settling() {
    let (mut harness, log) = harness(landing_config());
    let (strip, _) = settling_covered_swipe(&mut harness, &log);
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    // The settle heads where the strip shows now, as in its last frame.
    harness
        .world()
        .get_mut::<crate::ecs::Scrolling>(strip)
        .expect("the strip scrolls")
        .settle = Some(f64::from(position.x));
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(position));
    harness.advance(ONE_FRAME);

    let world = harness.world();
    assert!(
        world.get::<crate::ecs::Scrolling>(strip).is_some(),
        "the settle goes on"
    );
    assert!(
        world
            .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
            .is_some(),
        "under the swipe's cover"
    );
    harness.advance(SETTLE);
    assert!(!covered(harness.world()), "the cover lifts once it lands");
}

/// A move during a covered swipe that stretches the strip past its start
/// carries on at the speed the strip shows, which the stretch slows, not at
/// the fingers' speed.
#[test]
fn a_move_during_a_stretched_swipe_carries_the_speed_the_strip_shows() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    for _ in 0..4 {
        send(&mut harness, swipe(-0.05));
        harness.advance(ONE_FRAME);
    }
    let strip = swiped_strip(harness.world());
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    assert!(position.x > 0, "the strip stretches past its start");
    let scroll = scrolling_of(harness.world(), strip);
    // The fingers move the strip right, toward its start.
    let fingers = -scroll.velocity * f64::from(TEST_DISPLAY_WIDTH);
    assert!(fingers > 200.0, "the fingers move: {fingers}");
    assert!(scroll.slope < 0.6, "the strip resists: {}", scroll.slope);

    // A short move, which a slide from rest would start far slower than
    // the fingers move.
    let away = position.with_x(position.x + 40);
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(away));
    harness.advance(ONE_FRAME);
    let (_, slide) = sliding_strip(harness.world());
    assert_eq!(slide.heading(), Some(away), "the pictures slide on");
    let path = log.borrow().paths.last().cloned().expect("a slide");
    let carried = starting_speed(&path);
    assert!(
        carried > 0.25 * fingers && carried < 0.75 * fingers,
        "the slide starts at the stretched strip's speed: {carried}, fingers {fingers}"
    );
}

/// A move that arrives once the fingers lift from a stretched covered
/// swipe, before its settle starts, slides the pictures on from where the
/// strip shows, at the speed it shows. The scroll systems first coasted the
/// strip a frame on and held it to its start, so the pictures jumped there
/// and slid on from rest.
#[test]
fn a_move_as_a_stretched_swipe_lifts_slides_on_from_where_it_shows() {
    let config = config(true, Some(14.0));
    let (mut harness, log) = harness(config.clone());
    take_pictures(&mut harness, &log);
    send(&mut harness, Event::TouchpadDown);
    for _ in 0..4 {
        send(&mut harness, swipe(-0.05));
        harness.advance(ONE_FRAME);
    }
    let strip = swiped_strip(harness.world());
    let position = harness
        .world()
        .get::<crate::ecs::Position>(strip)
        .expect("a strip position")
        .0;
    assert!(position.x > 0, "the strip stretches past its start");
    // The fingers lift, and the swipe has timed out with no settle yet.
    {
        let mut scroll = harness
            .world()
            .get_mut::<crate::ecs::Scrolling>(strip)
            .expect("the strip scrolls");
        scroll.is_user_swiping = false;
        scroll.fingers_down = false;
        assert!(scroll.settle.is_none(), "no settle has started");
    }
    let scroll = scrolling_of(harness.world(), strip);
    let shows = crate::ecs::scroll::strip_speed(&scroll, f64::from(TEST_DISPLAY_WIDTH), &config);
    assert!(shows > 100.0, "the strip still moves: {shows}");

    let away = position.with_x(position.x + 40);
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(away));
    harness.advance(ONE_FRAME);
    let (_, slide) = sliding_strip(harness.world());
    assert_eq!(slide.heading(), Some(away), "the pictures slide on");
    assert!(
        harness
            .world()
            .get::<crate::ecs::Scrolling>(strip)
            .is_none(),
        "the swipe gives way to the move"
    );
    let path = log.borrow().paths.last().cloned().expect("a slide");
    assert_eq!(
        path.first().copied(),
        Some(f64::from(position.x)),
        "the slide starts where the strip shows"
    );
    let carried = starting_speed(&path);
    assert!(
        carried > 0.5 * shows,
        "at the speed it shows: {carried} vs {shows}"
    );
}

/// A swipe that takes over a slide's landing keeps every window the slide's
/// cover hid, and its own landing waits for each of them, as for a window
/// the window server still shows where it was.
#[test]
fn a_chained_swipe_keeps_the_windows_its_cover_hid() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    harness.mock_state.show_late(0);
    send(&mut harness, focus(Direction::Last));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let target = slide.heading().expect("the pictures slide");
    log.borrow_mut().showing = Some(f64::from(target.x));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    let (_, slide) = sliding_strip(harness.world());
    assert_eq!(slide.heading(), None, "the real windows land");
    let hid = slide.hid().to_vec();
    let window = entity_of(harness.world(), 0);
    assert!(hid.contains(&window), "the slide's cover hid window 0");

    send(&mut harness, Event::TouchpadDown);
    send(&mut harness, swipe(0.01));
    harness.advance(ONE_FRAME);
    let swiped = harness
        .world()
        .get::<crate::ecs::strip_cover::CoverSwipe>(strip)
        .cloned()
        .expect("the swipe takes the pictures over");
    assert!(
        hid.iter().all(|entity| swiped.hid().contains(entity)),
        "the swipe keeps them: {hid:?} in {:?}",
        swiped.hid()
    );

    send(&mut harness, Event::TouchpadUp);
    for _ in 0..30 {
        harness.advance(ONE_FRAME);
        if harness.world().get::<CoverSlide>(strip).is_some() {
            break;
        }
    }
    let landing = harness
        .world()
        .get::<CoverSlide>(strip)
        .cloned()
        .expect("the swipe lands under the cover");
    assert!(
        hid.iter().all(|entity| landing.hid().contains(entity)),
        "and so does its landing"
    );
    harness.advance(ONE_FRAME * 5);
    assert!(covered(harness.world()), "it waits for window 0 to show");
    harness.mock_state.show_now(0);
    harness.advance(ONE_FRAME * 3);
    assert!(!covered(harness.world()), "then lifts");
}

/// Runs a keyboard move under the cover to `direction` while the window
/// server shows window `late` where it was, and returns once the real
/// windows have landed and waited a few frames, within the landing's time
/// limit.
fn land_with_a_late_window(
    harness: &mut TestHarness,
    log: &ScreenLog,
    direction: Direction,
    late: WinID,
) {
    take_pictures(harness, log);
    harness.mock_state.show_late(late);
    send(harness, focus(direction));
    harness.advance(ONE_FRAME);
    let (strip, slide) = sliding_strip(harness.world());
    let target = slide.heading().expect("the pictures slide");
    log.borrow_mut().showing = Some(f64::from(target.x));
    harness.advance(ONE_FRAME);
    log.borrow_mut().showing = None;
    let slide = harness
        .world()
        .get::<CoverSlide>(strip)
        .cloned()
        .expect("the slide lands under the cover");
    assert_eq!(slide.heading(), None, "the real windows land");
    harness.advance(ONE_FRAME * 5);
}

/// A slide's cover stays up after its windows took their writes while the
/// window server still shows one where it was, as an app that takes its
/// new place after the write returned does, and comes down once it shows
/// the new one.
#[test]
fn a_slide_waits_for_the_window_server_to_show_the_landing() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    land_with_a_late_window(&mut harness, &log, Direction::East, 1);
    assert!(covered(harness.world()), "the cover is still up");
    assert_eq!(log.borrow().hidden, 0, "it stays while the old frame shows");
    harness.mock_state.show_now(1);
    harness.advance(ONE_FRAME * 3);
    assert!(!covered(harness.world()), "the move is done");
    assert_eq!(log.borrow().hidden, 1, "the cover comes down once it shows");
}

/// A window the move parks at the display's edge is still checked before the
/// cover comes down: the window server may still show it where it was, in
/// full, under the cover.
#[test]
fn a_slide_waits_for_a_window_it_parks_at_the_edge() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    let before = window_frame(harness.world(), 0);
    land_with_a_late_window(&mut harness, &log, Direction::Last, 0);
    let after = window_frame(harness.world(), 0);
    assert!(
        before.min.x >= 0 && after.max.x < 50,
        "window 0 leaves the display but for its edge: {before:?} to {after:?}"
    );
    assert!(covered(harness.world()), "the cover is still up");
    harness.mock_state.show_now(0);
    harness.advance(ONE_FRAME * 3);
    assert!(!covered(harness.world()), "the move is done");
    assert_eq!(log.borrow().hidden, 1);
}

/// A focus move into a column of stacked windows goes under the cover too.
#[test]
fn a_move_into_a_stack_is_covered() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    harness.run(vec![
        focus(Direction::Last),
        Event::Command {
            command: Command::Window(Operation::Stack(crate::commands::Side::West)),
        },
        focus(Direction::First),
    ]);
    harness.advance(SETTLE);
    take_pictures(&mut harness, &log);
    let before = log.borrow().shown.len();
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    harness.advance(SETTLE);
    send(&mut harness, focus(Direction::West));
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().shown.len() > before,
        "moves around a stack go under the cover"
    );
}

/// Marks the strip changed without changing its layout, as some systems do
/// when they take it mutably.
fn touch_strip(harness: &mut TestHarness) {
    let strips = harness
        .world()
        .query_filtered::<Entity, With<crate::ecs::layout::LayoutStrip>>()
        .iter(harness.world())
        .collect::<Vec<_>>();
    for strip in strips {
        harness
            .world()
            .get_mut::<crate::ecs::layout::LayoutStrip>(strip)
            .unwrap()
            .set_changed();
    }
}

/// A system that only touches the strip, as on Sam's Mac with every focus
/// key, neither keeps a move from its cover nor ends a slide early: only a
/// change to the layout does.
#[test]
fn touching_the_strip_without_changing_its_layout_keeps_the_cover() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    touch_strip(&mut harness);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().shown.len(), 1, "the move is under the cover");
    touch_strip(&mut harness);
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()), "the slide goes on");
    let path = path_of(&mut harness, 1);
    assert_eq!(path, vec![700, 700 - STEP], "the window moves once");
}

/// A move whose layout changes in the same pass, as a swap does, moves the
/// real windows as before: the pictures would show the windows where the
/// layout now puts them, not where they are.
#[test]
fn a_swap_is_not_covered() {
    let (mut harness, log) = harness(config(true, Some(14.0)));
    take_pictures(&mut harness, &log);
    send(
        &mut harness,
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::East)),
        },
    );
    harness.advance(ONE_FRAME);
    assert!(log.borrow().shown.is_empty(), "no cover for a swap");
}

/// The cover's config with three column widths to cycle through: half,
/// three quarters, and the whole display.
fn resize_config() -> Config {
    (
        MainOptions {
            slide_window_pictures: Some(true),
            animation_speed: Some(14.0),
            preset_column_widths: vec![0.5, 0.75, 1.0],
            ..MainOptions::default()
        },
        vec![],
    )
        .into()
}

fn resize(direction: crate::types::commands::ResizeDirection) -> Event {
    Event::Command {
        command: Command::Window(Operation::Resize(direction)),
    }
}

fn window_frame(world: &mut World, id: WinID) -> IRect {
    let mut query = world.query::<&Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .frame()
}

fn morphed(world: &mut World) -> bool {
    world
        .query::<&crate::ecs::strip_cover::CoverMorph>()
        .iter(world)
        .next()
        .is_some()
}

/// Steps one frame at a time until the resize's cover is gone, and returns
/// every frame windows `ids` showed on the way, each window's apart.
fn frames_of(harness: &mut TestHarness, ids: &[WinID]) -> Vec<Vec<IRect>> {
    let mut paths = ids
        .iter()
        .map(|id| vec![window_frame(harness.world(), *id)])
        .collect::<Vec<_>>();
    for _ in 0..100 {
        harness.advance(ONE_FRAME);
        for (path, id) in paths.iter_mut().zip(ids) {
            let frame = window_frame(harness.world(), *id);
            if path.last() != Some(&frame) {
                path.push(frame);
            }
        }
        if !morphed(harness.world()) {
            harness.advance(SETTLE);
            return paths;
        }
    }
    panic!("the resize ends");
}

/// Runs frames until the cover has moved its pictures `count` times. A
/// maximize's pictures rest first, until the window shows its new frame.
fn until_morphs(harness: &mut TestHarness, log: &ScreenLog, count: usize) {
    for _ in 0..30 {
        if log.borrow().morphs.len() >= count {
            return;
        }
        harness.advance(ONE_FRAME);
    }
    panic!("the cover moves its pictures {count} times");
}

/// The frame a morph moves window `id`'s picture from and to.
fn morph_of(log: &ScreenLog, morph: usize, id: WinID) -> (IRect, IRect) {
    let screen = log.borrow();
    let (_, from, to) = screen.morphs[morph]
        .iter()
        .find(|(window, ..)| *window == id)
        .copied()
        .expect("the window's picture is in the morph");
    (from, to)
}

/// With pictures ready, a resize moves each picture from its window's old
/// frame to its new one, gaps kept, while each real window takes its new
/// frame once, under the cover. The cover then comes down at once, with no
/// fade.
#[test]
fn a_resize_morphs_the_pictures_and_moves_each_window_once() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let before = [0, 1].map(|id| window_frame(harness.world(), id));

    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "the resize is under a cover");
    assert_eq!(log.borrow().shown.len(), 1, "one cover goes up");
    assert_eq!(log.borrow().morphs.len(), 1, "one morph");
    assert_eq!(log.borrow().morphs[0].len(), 4, "a picture of each window");
    let (from0, to0) = morph_of(&log, 0, 0);
    let (from1, to1) = morph_of(&log, 0, 1);
    assert_eq!(
        [from0, from1],
        before,
        "the pictures start where the windows rest"
    );
    assert_ne!(
        to0.width(),
        from0.width(),
        "the focused window changes width"
    );
    assert_eq!(to1.width(), from1.width(), "its neighbor keeps its width");
    assert_eq!(
        to1.min.x - to0.max.x,
        from1.min.x - from0.max.x,
        "the gap between them holds"
    );

    assert_eq!(
        [0, 1].map(|id| window_frame(harness.world(), id)),
        [to0, to1],
        "the real windows take their new frames at once, under the cover"
    );
    let paths = frames_of(&mut harness, &[0, 1]);
    assert_eq!(paths[0], vec![to0], "the resized window changes once");
    assert_eq!(paths[1], vec![to1], "its neighbor moves once");
    assert_eq!(log.borrow().hidden, 1, "the cover comes down");
}

/// Without pictures, a resize moves the real windows as before, through the
/// frames between.
#[test]
fn without_pictures_a_resize_moves_the_windows_as_before() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    send(&mut harness, resize(ResizeDirection::Grow));
    let mut widths = vec![window_frame(harness.world(), 0).width()];
    for _ in 0..30 {
        harness.advance(ONE_FRAME);
        let width = window_frame(harness.world(), 0).width();
        if widths.last() != Some(&width) {
            widths.push(width);
        }
    }
    assert!(log.borrow().morphs.is_empty(), "no cover");
    assert!(widths.len() > 2, "the window eases through {widths:?}");
}

/// A second resize while the first one's pictures move turns them from
/// where they show to the new frames, on the same cover, and the real
/// windows go to the new frames at once.
#[test]
fn a_second_resize_turns_the_morph() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME * 3);
    assert!(morphed(harness.world()));
    let (first_from, first_to) = morph_of(&log, 0, 0);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().shown.len(), 1, "the same cover stays up");
    assert_eq!(log.borrow().morphs.len(), 2, "the morph turns");
    let (from, to) = morph_of(&log, 1, 0);
    assert!(
        from.width() > first_from.width() && from.width() < first_to.width(),
        "the turn starts where the picture shows: {from:?}"
    );
    assert_ne!(to, first_to, "and heads to the new frame");
    let paths = frames_of(&mut harness, &[0]);
    assert_eq!(
        *paths[0].last().unwrap(),
        to,
        "the window ends where its picture does"
    );
    assert_eq!(log.borrow().hidden, 1, "one cover comes down, once");
}

/// A focus key while a resize's pictures move turns them on the same cover
/// to where the strip now heads, and the real windows go there at once.
#[test]
fn a_focus_key_mid_resize_turns_the_morph() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME * 3);
    let (_, first_to) = morph_of(&log, 0, 1);
    send(&mut harness, focus(Direction::East));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().shown.len(), 1, "the same cover stays up");
    assert!(
        log.borrow().slides.is_empty(),
        "no strip slide starts over it"
    );
    assert_eq!(log.borrow().morphs.len(), 2, "the morph turns");
    let (_, to) = morph_of(&log, 1, 1);
    assert!(to.min.x < first_to.min.x, "the strip heads east: {to:?}");
    let paths = frames_of(&mut harness, &[1]);
    assert_eq!(*paths[0].last().unwrap(), to);
}

/// A resize of a column of stacked windows resizes every window of the
/// column, so each one's frame widens together.
#[test]
fn a_stacked_column_resize_morphs_every_window_of_it() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    harness.run(vec![
        focus(Direction::East),
        Event::Command {
            command: Command::Window(Operation::Stack(crate::commands::Side::West)),
        },
        focus(Direction::First),
    ]);
    harness.advance(SETTLE);
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().morphs.len(), 1, "the resize is under a cover");
    let (from0, to0) = morph_of(&log, 0, 0);
    let (from1, to1) = morph_of(&log, 0, 1);
    assert_eq!(from0.min.x, from1.min.x, "the two share a column");
    assert_eq!(to0.width(), to1.width(), "and both take the new width");
    assert_ne!(to0.width(), from0.width());
    let paths = frames_of(&mut harness, &[0, 1]);
    assert_eq!(paths[0], vec![to0], "each window changes once");
    assert_eq!(paths[1], vec![to1]);
}

/// A turn starts where Core Animation shows the pictures, even when the
/// engine's own count lags it after a stalled frame.
#[test]
fn a_turn_starts_where_the_screen_shows_the_morph() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let (from, to) = morph_of(&log, 0, 0);
    log.borrow_mut().morph_share = Some(0.9);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().morphs.len(), 2, "the morph turns");
    let (turned_from, _) = morph_of(&log, 1, 0);
    #[allow(clippy::cast_possible_truncation)]
    let shown = from.max.x + (f64::from(to.max.x - from.max.x) * 0.9).round() as i32;
    assert_eq!(
        turned_from.max.x, shown,
        "the turn starts where the screen shows"
    );
}

/// The cover stays up after the pictures land while the window server
/// still shows a window at its old frame, as an app that takes its new
/// size after the write returned does, and comes down once it shows the new
/// one.
#[test]
fn the_cover_waits_for_the_window_server_to_show_the_new_frames() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    harness.mock_state.show_late(0);
    send(&mut harness, resize(ResizeDirection::Grow));
    // Past the landing, within its time limit.
    harness.advance(ONE_FRAME * 16);
    assert!(morphed(harness.world()), "the cover is still up");
    assert_eq!(log.borrow().hidden, 0, "it stays while the old frame shows");
    harness.mock_state.show_now(0);
    harness.advance(ONE_FRAME * 3);
    assert!(!morphed(harness.world()), "the resize is done");
    assert_eq!(
        log.borrow().hidden,
        1,
        "the cover comes down once the new frame shows"
    );
}

/// A neighbor that a resize pushes off the cover's area is still checked
/// before the cover comes down: the window server may still show it at its
/// old frame, under the cover.
#[test]
fn the_cover_waits_for_a_neighbor_pushed_off_it() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(SETTLE);
    take_pictures(&mut harness, &log);
    harness.mock_state.show_late(1);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().morphs.len(), 2, "the second resize is covered");
    let (from, to) = morph_of(&log, 1, 1);
    assert!(
        from.min.x < TEST_DISPLAY_WIDTH && to.min.x >= TEST_DISPLAY_WIDTH,
        "the neighbor leaves the display: {from:?} to {to:?}"
    );
    harness.advance(ONE_FRAME * 16);
    assert_eq!(
        log.borrow().hidden,
        1,
        "only the first resize's cover came down"
    );
    harness.mock_state.show_now(1);
    harness.advance(ONE_FRAME * 3);
    assert_eq!(
        log.borrow().hidden,
        2,
        "the cover comes down once the neighbor shows"
    );
}

fn entity_of(world: &mut World, id: WinID) -> Entity {
    let mut query = world.query::<(Entity, &Window)>();
    query
        .iter(world)
        .find(|(_, window)| window.id() == id)
        .map(|(entity, _)| entity)
        .expect("window not found")
}

/// A resize to a width the window refused before moves the real windows as
/// before: the layout clamps the width a pass later, which would turn the
/// pictures twice.
#[test]
fn a_resize_past_a_refused_width_is_not_covered() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let entity = entity_of(harness.world(), 0);
    harness
        .world()
        .entity_mut(entity)
        .insert(crate::ecs::WidthLimits {
            min: None,
            max: Some(720),
        });
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert!(log.borrow().morphs.is_empty(), "no cover");
}

/// A stack while a resize's pictures move ends the cover: windows that move
/// up or down are no resize.
#[test]
fn a_stack_mid_resize_ends_the_cover() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME * 2);
    assert!(morphed(harness.world()));
    send(
        &mut harness,
        Event::Command {
            command: Command::Window(Operation::Stack(crate::commands::Side::East)),
        },
    );
    harness.advance(ONE_FRAME * 2);
    assert!(!morphed(harness.world()), "the cover ends");
    assert_eq!(log.borrow().morphs.len(), 1, "no turn into the stack");
}

/// A resize during a covered strip slide ends the slide at once, so the
/// resized window never changes width under a slide's pictures, even the
/// last column's, whose resize moves no other window.
#[test]
fn a_resize_mid_slide_ends_the_slide() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, focus(Direction::Last));
    harness.advance(ONE_FRAME);
    assert!(covered(harness.world()), "the move is under a cover");
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert!(
        !covered(harness.world()),
        "the slide ends in the resize's pass"
    );
}

/// The pictures of windows asked for after the first `since`.
fn window_asks_since(log: &ScreenLog, since: usize) -> Vec<(WinID, IVec2, u32)> {
    log.borrow().window_asks[since..].to_vec()
}

/// Answers the ask `generation` for window `id` with a picture.
fn answer(harness: &mut TestHarness, id: WinID, generation: u32) {
    send(
        harness,
        Event::CoverPhoto {
            subject: CoverSubject::Window(id),
            generation,
            result: Ok(Photo {
                digest: Digest(u64::from(generation)),
                image: pixel(),
                edge_color: Some([0.2, 0.4, 0.6, 1.0]),
            }),
        },
    );
}

/// Once the window server shows a widened window at its new frame, the
/// cover asks for a fresh picture of it at that size, and puts it in place
/// of the old one while the frame still moves. A neighbor that only moves
/// gets no fresh picture.
#[test]
fn a_widened_window_gets_a_fresh_picture_mid_resize() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "the resize is under a cover");
    let (from, to) = morph_of(&log, 0, 0);
    assert!(to.width() > from.width(), "window 0 widens");
    let asks = window_asks_since(&log, since);
    assert_eq!(
        asks.iter()
            .map(|(id, size, _)| (*id, *size))
            .collect::<Vec<_>>(),
        vec![(0, to.size())],
        "one fresh picture, of the widened window at its new size"
    );
    harness.advance(ONE_FRAME * 3);
    assert_eq!(
        window_asks_since(&log, since).len(),
        1,
        "the cover asks once a size"
    );
    assert!(morphed(harness.world()), "the frames still move");
    answer(&mut harness, 0, asks[0].2);
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().replaced,
        vec![(0, to.size())],
        "the fresh picture replaces the old one"
    );
    let paths = frames_of(&mut harness, &[0]);
    assert_eq!(paths[0], vec![to], "the window still changes once");
}

/// The fresh picture waits until the window server shows the widened window
/// at its new frame: a picture before that shows its old layout.
#[test]
fn a_fresh_picture_waits_for_the_window_server() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    harness.mock_state.show_late(0);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME * 3);
    assert!(morphed(harness.world()));
    assert!(
        window_asks_since(&log, since).is_empty(),
        "no picture while the old frame shows"
    );
    harness.mock_state.show_now(0);
    harness.advance(ONE_FRAME);
    let (_, to) = morph_of(&log, 0, 0);
    assert_eq!(
        window_asks_since(&log, since)
            .iter()
            .map(|(id, size, _)| (*id, *size))
            .collect::<Vec<_>>(),
        vec![(0, to.size())],
        "the picture once the new frame shows"
    );
}

/// A fresh picture that arrives right after the cover lifts (while the ask
/// is still pending, well before `ASK_TIMEOUT`) shows nowhere on a cover.
#[test]
fn a_fresh_picture_after_the_cover_lifts_shows_nowhere() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let asks = window_asks_since(&log, since);
    assert_eq!(asks.len(), 1, "a fresh picture is asked for");
    for _ in 0..40 {
        if !morphed(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
    assert!(!morphed(harness.world()), "the cover is down");
    answer(&mut harness, 0, asks[0].2);
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "the picture shows nowhere"
    );
}

/// An older superseded capture arriving after a newer one never overwrites
/// the newer cached size, so the next resize still goes under the cover.
#[test]
fn a_superseded_fresh_picture_never_overwrites_a_newer_cached_one() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let first = window_asks_since(&log, since);
    assert_eq!(first.len(), 1);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let asks = window_asks_since(&log, since);
    assert_eq!(asks.len(), 2, "second width asks anew");
    let (_, second_size, second_gen) = asks[1];
    // Deliver the newer capture first, then the superseded older one.
    answer(&mut harness, 0, second_gen);
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().replaced, vec![(0, second_size)]);
    answer(&mut harness, 0, first[0].2);
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().replaced,
        vec![(0, second_size)],
        "the older reply does not replace the newer picture"
    );
    // Let the morph finish and rest, then start another resize: the cache
    // must still hold `second_size`, not `first[0].1`.
    for _ in 0..40 {
        if !morphed(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
    assert!(!morphed(harness.world()));
    harness.advance(ONE_FRAME);
    let morphs_before = log.borrow().morphs.len();
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().morphs.len(),
        morphs_before + 1,
        "next resize stays under the cover using the newer cached size"
    );
}

/// Turning 50% -> 75% -> 100% -> 75% drops the first 75% ask on the way to
/// 100%, asks anew on the return to 75%, and ignores any stale reply from
/// the first 75% ask.
#[test]
fn a_returning_turn_asks_anew_and_rejects_an_earlier_reply() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    // 50% -> 75%
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let first = window_asks_since(&log, since);
    assert_eq!(first.len(), 1);
    let (first_id, size_75, gen_first_75) = first[0];
    assert_eq!(first_id, 0);
    // 75% -> 100%
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(window_asks_since(&log, since).len(), 2);
    // 100% -> 75%
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    let asks = window_asks_since(&log, since);
    assert_eq!(asks.len(), 3, "returning to 75% asks anew");
    let (_, returned_size, gen_second_75) = asks[2];
    assert_eq!(returned_size, size_75);
    assert_ne!(gen_second_75, gen_first_75);
    // Stale reply from the first 75% turn must be rejected.
    answer(&mut harness, 0, gen_first_75);
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "stale first-turn reply is rejected"
    );
    // Current turn's reply unveils cleanly.
    answer(&mut harness, 0, gen_second_75);
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().replaced, vec![(0, size_75)]);
}

/// A 75% morph reply arriving in the same update as a shrink turn back to
/// 50% neither unveils nor overwrites the cached 50% picture, so the next
/// resize still goes under the cover.
#[test]
fn a_same_frame_turn_back_keeps_the_cached_picture() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    // Put window 0 at the 50% preset first, then take fresh cached pictures.
    send(&mut harness, resize(ResizeDirection::Shrink));
    frames_of(&mut harness, &[0]);
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    // 50% -> 75%
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let asks = window_asks_since(&log, since);
    assert_eq!(asks.len(), 1);
    let (_, _, gen_75) = asks[0];
    // Deliver the 75% reply and shrink back to 50% in the very same frame.
    answer(&mut harness, 0, gen_75);
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "the 75% reply must not unveil after turning back to 50%"
    );
    // Let the morph finish and rest at 50%; the next resize must still be
    // covered (meaning the 50% cached photo was not overwritten by 75%).
    for _ in 0..40 {
        if !morphed(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
    assert!(!morphed(harness.world()));
    harness.advance(ONE_FRAME);
    let morphs_before = log.borrow().morphs.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().morphs.len(),
        morphs_before + 1,
        "cached 50% picture survived the same-frame turn back"
    );
}

/// A late 75% morph reply arriving after the cover lifted and an uncovered
/// shrink already started back to 50% must not overwrite the usable 50% cache.
#[test]
fn a_late_reply_after_an_uncovered_shrink_keeps_the_cached_picture() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    send(&mut harness, resize(ResizeDirection::Shrink));
    frames_of(&mut harness, &[0]);
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    // 50% -> 75% under cover, leaving the 75% ask unanswered until after lift.
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let asks = window_asks_since(&log, since);
    assert_eq!(asks.len(), 1);
    let gen_75 = asks[0].2;
    for _ in 0..40 {
        if !morphed(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
    assert!(!morphed(harness.world()), "cover lifted with 75% pending");
    // Shrink back to 50% immediately (uncovered, because no 75% picture is
    // cached yet), then deliver the late 75% reply while window 0 targets 50%.
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    assert!(!morphed(harness.world()), "shrink started uncovered");
    answer(&mut harness, 0, gen_75);
    harness.advance(ONE_FRAME);
    // Let the uncovered shrink settle and rest at 50%.
    for _ in 0..20 {
        harness.advance(ONE_FRAME);
    }
    let morphs_before = log.borrow().morphs.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().morphs.len(),
        morphs_before + 1,
        "cached 50% picture survived the late 75% reply after an uncovered shrink"
    );
}

/// When a morph widens past the returning target (e.g. 50% -> 100% reaching
/// ~90%, then turning back to 75%), a 75% capture that arrives while the
/// presented frame is still wider than 75% defers `replace_picture` until the
/// presented width fits 75%.
#[test]
fn a_fresh_picture_waits_until_the_presented_frame_fits() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    send(&mut harness, resize(ResizeDirection::Shrink));
    frames_of(&mut harness, &[0]);
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    // 50% -> 75% -> 100%, then advance until presented width exceeds 75%.
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    send(&mut harness, resize(ResizeDirection::Grow));
    for _ in 0..8 {
        harness.advance(ONE_FRAME);
    }
    // Turn back to 75% while presented width is > 75%.
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    let (turn_from, turn_to) = morph_of(&log, log.borrow().morphs.len() - 1, 0);
    assert!(
        turn_from.width() > turn_to.width(),
        "presented width ({}) must be wider than 75% target ({})",
        turn_from.width(),
        turn_to.width()
    );
    let asks = window_asks_since(&log, since);
    let (_, size_75, gen_75) = *asks.last().expect("ask at 75%");
    assert_eq!(size_75, turn_to.size());
    // Deliver 75% immediately while the frame is still wider than 75%.
    answer(&mut harness, 0, gen_75);
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "replacement defers while presented frame is wider than 75%"
    );
    // Advance until the morph finishes; replacement fires once width fits.
    for _ in 0..40 {
        if !morphed(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
    assert_eq!(
        log.borrow().replaced,
        vec![(0, size_75)],
        "replacement fired once presented frame fit 75%"
    );
}

/// Resizes a column of two stacked windows 50% -> 75% -> 100%, then back
/// to 75% while the frames are wider, and answers window 0's fresh picture
/// at 75%, which waits for its frame to fit. Returns the generation of
/// window 1's fresh picture at 75%, unanswered.
fn a_stacked_resize_with_a_waiting_picture(harness: &mut TestHarness, log: &ScreenLog) -> u32 {
    use crate::types::commands::ResizeDirection;
    // A column of two stacked windows, so a resize widens both and asks for
    // a fresh picture of each.
    harness.run(vec![
        focus(Direction::East),
        Event::Command {
            command: Command::Window(Operation::Stack(crate::commands::Side::West)),
        },
        focus(Direction::First),
    ]);
    harness.advance(SETTLE);
    send(harness, resize(ResizeDirection::Shrink));
    frames_of(harness, &[0, 1]);
    take_pictures(harness, log);
    let since = log.borrow().window_asks.len();
    send(harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    send(harness, resize(ResizeDirection::Grow));
    for _ in 0..8 {
        harness.advance(ONE_FRAME);
    }
    send(harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    let (_, turn_to) = morph_of(log, log.borrow().morphs.len() - 1, 0);
    let asks = window_asks_since(log, since);
    let ask = |id: WinID| {
        asks.iter()
            .rev()
            .find(|&&(window, size, _)| window == id && size == turn_to.size())
            .map(|&(_, _, generation)| generation)
            .expect("a fresh picture at 75%")
    };
    let (first, second) = (ask(0), ask(1));
    answer(harness, 0, first);
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "window 0's picture waits for its frame to fit"
    );
    second
}

/// Sends a refusal of window 1's picture `generation`, ahead of `then` in
/// the same frame, runs that one frame, and checks nothing is kept.
fn refuse_window_1(harness: &mut TestHarness, generation: u32, then: Vec<Event>) {
    send(
        harness,
        Event::CoverPhoto {
            subject: CoverSubject::Window(1),
            generation,
            result: Err(crate::ecs::slice_photos::PhotoError::Refused),
        },
    );
    for event in then {
        send(harness, event);
    }
    harness.app.update();
    assert!(
        harness
            .world()
            .resource::<crate::ecs::strip_cover::CoverPhotos>()
            .kept_nothing(),
        "the refusal keeps no picture"
    );
}

/// Runs frames until the resize's cover is down.
fn until_unmorphed(harness: &mut TestHarness) {
    for _ in 0..40 {
        if !morphed(harness.world()) {
            break;
        }
        harness.advance(ONE_FRAME);
    }
}

/// A title change that arrives after a refusal of pictures, in the same
/// frame, still keeps a resize's cover from showing a fresh picture it holds
/// until its frame fits, which shows the old title. The refusal ended the
/// frame's events early, the title change went unread, and the cover showed
/// the old tab once the frame fit.
#[test]
fn a_title_change_after_a_refusal_still_drops_a_waiting_picture() {
    let (mut harness, log) = harness(resize_config());
    let second = a_stacked_resize_with_a_waiting_picture(&mut harness, &log);
    refuse_window_1(
        &mut harness,
        second,
        vec![Event::WindowTitleChanged { window_id: 0 }],
    );
    until_unmorphed(&mut harness);
    assert!(
        log.borrow().replaced.is_empty(),
        "the picture of the old title never shows: {:?}",
        log.borrow().replaced
    );
}

/// A refusal of pictures drops a fresh picture a resize's cover holds until
/// its frame fits, so no picture first shows after macOS refused them.
#[test]
fn a_refusal_drops_a_waiting_picture() {
    let (mut harness, log) = harness(resize_config());
    let second = a_stacked_resize_with_a_waiting_picture(&mut harness, &log);
    refuse_window_1(&mut harness, second, Vec::new());
    until_unmorphed(&mut harness);
    assert!(
        log.borrow().replaced.is_empty(),
        "no picture shows after the refusal: {:?}",
        log.borrow().replaced
    );
}

/// Runs frames until the resize's cover has let pictures back in, or
/// shut them out, as `allowed` says.
fn until_access(harness: &mut TestHarness, allowed: bool) {
    for _ in 0..20 {
        let photos = harness
            .world()
            .resource::<crate::ecs::strip_cover::CoverPhotos>();
        if photos.allows() == allowed {
            return;
        }
        harness.app.update();
    }
    panic!("pictures are allowed: {}", !allowed);
}

/// A fresh picture a resize's cover holds until its frame fits never shows
/// once macOS stops allowing pictures, even after it allows them again
/// before the frame fits. The background refresh forgot every kept picture
/// but left the resize's, which showed once its frame fit.
#[test]
fn a_waiting_picture_never_shows_once_access_is_revoked() {
    let (mut harness, log) = harness(resize_config());
    a_stacked_resize_with_a_waiting_picture(&mut harness, &log);
    let recheck = |harness: &mut TestHarness| {
        harness.world().resource_mut::<Config>().set_changed();
    };
    log.borrow_mut().denied = true;
    recheck(&mut harness);
    until_access(&mut harness, false);
    log.borrow_mut().denied = false;
    recheck(&mut harness);
    until_access(&mut harness, true);
    assert!(morphed(harness.world()), "the resize goes on");
    assert!(log.borrow().replaced.is_empty(), "before its frame fits");

    until_unmorphed(&mut harness);
    assert!(
        log.borrow().replaced.is_empty(),
        "the picture never shows: {:?}",
        log.borrow().replaced
    );
}

/// A resize's cover shows no fresh picture it holds while pictures are not
/// allowed, even in the frame before the background refresh forgets it.
#[test]
fn a_waiting_picture_does_not_show_while_access_is_off() {
    // How many frames the waiting picture takes to show.
    let fits = {
        let (mut harness, log) = harness(resize_config());
        a_stacked_resize_with_a_waiting_picture(&mut harness, &log);
        (1..40)
            .find(|_| {
                harness.app.update();
                !log.borrow().replaced.is_empty()
            })
            .expect("the picture shows once its frame fits")
    };
    let (mut harness, log) = harness(resize_config());
    a_stacked_resize_with_a_waiting_picture(&mut harness, &log);
    for _ in 1..fits {
        harness.app.update();
    }
    assert!(
        log.borrow().replaced.is_empty(),
        "the frame does not fit yet"
    );
    harness
        .world()
        .resource_mut::<crate::ecs::strip_cover::CoverPhotos>()
        .switch_off();
    harness.app.update();
    assert!(
        log.borrow().replaced.is_empty(),
        "no picture shows as the frame fits: {:?}",
        log.borrow().replaced
    );
}

/// A resize's fresh picture asked for before the window's title changed,
/// while pictures were switched off, never comes in once they are back on,
/// before the background refresh clears what it asked for. The resize's
/// cover had forgotten its ask when it ended, so the old title's picture was
/// kept.
#[test]
fn a_picture_asked_for_before_a_title_change_while_off_never_comes_in() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let (_, size, old) = window_asks_since(&log, since)
        .into_iter()
        .find(|&(id, ..)| id == 0)
        .expect("a fresh picture of window 0");

    let on = resize_config();
    let off: Config = (
        MainOptions {
            slide_window_pictures: Some(false),
            ..on.options().clone()
        },
        vec![],
    )
        .into();
    harness.world().insert_resource(off);
    send(&mut harness, Event::WindowTitleChanged { window_id: 0 });
    harness.app.update();
    assert!(!morphed(harness.world()), "the cover ends, switched off");

    harness.world().insert_resource(on);
    answer(&mut harness, 0, old);
    harness.app.update();
    assert!(
        !harness
            .world()
            .resource::<crate::ecs::strip_cover::CoverPhotos>()
            .keeps(0, size),
        "the old title's picture is not kept"
    );
}

/// A fresh picture that arrives after the resize turned to another width
/// shows nowhere: it shows the window at a size its frame no longer heads
/// to. The turn asks for one at the new width.
#[test]
fn a_fresh_picture_after_a_turn_to_another_width_shows_nowhere() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    let first = window_asks_since(&log, since);
    assert_eq!(first.len(), 1, "a fresh picture of the first width");
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().morphs.len(), 2, "the morph turns");
    let (_, to) = morph_of(&log, 1, 0);
    assert_ne!(to.size(), first[0].1, "to another width");
    answer(&mut harness, 0, first[0].2);
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "the first width's picture shows nowhere"
    );
    let asks = window_asks_since(&log, since);
    let (_, size, generation) = *asks.last().expect("an ask");
    assert_eq!(size, to.size(), "and one is asked at the new width");
    answer(&mut harness, 0, generation);
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().replaced, vec![(0, to.size())]);
}

/// A window that narrows shows its old picture cropped, and asks for no
/// fresh one.
#[test]
fn a_narrowed_window_asks_no_fresh_picture() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "the resize is under a cover");
    let (from, to) = morph_of(&log, 0, 0);
    assert!(to.width() < from.width(), "window 0 narrows");
    // While the cover is up: once it comes down, the background refresh
    // takes pictures of the new sizes.
    for _ in 0..100 {
        assert!(
            window_asks_since(&log, since).is_empty(),
            "no fresh picture"
        );
        if !morphed(harness.world()) {
            return;
        }
        harness.advance(ONE_FRAME);
    }
    panic!("the resize ends");
}

/// Whether any entity eases on a resize's own curve.
fn eases_a_resize(world: &mut World) -> bool {
    world
        .query_filtered::<(), With<crate::ecs::ResizeMotion>>()
        .iter(world)
        .next()
        .is_some()
}

/// A covered resize snaps the real windows under the cover, with no ease of
/// their own: the pictures ease instead.
#[test]
fn a_covered_resize_takes_no_ease_of_its_own() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    take_pictures(&mut harness, &log);
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()));
    assert!(!eases_a_resize(harness.world()), "no resize ease");
}

/// Without pictures, a resize eases its windows out on its own short curve,
/// as with the cover off.
#[test]
fn an_uncovered_resize_eases_out() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(resize_config());
    send(&mut harness, resize(ResizeDirection::Grow));
    harness.advance(ONE_FRAME);
    assert!(log.borrow().morphs.is_empty(), "no cover");
    assert!(eases_a_resize(harness.world()), "the resize eases out");
}

fn maximize_cmd() -> Event {
    Event::Command {
        command: Command::Window(Operation::Maximize),
    }
}

fn padded_resize_config() -> Config {
    (
        MainOptions {
            slide_window_pictures: Some(true),
            animation_speed: Some(14.0),
            preset_column_widths: vec![0.5, 0.75, 1.0],
            padding_top: Some(8),
            padding_bottom: Some(8),
            padding_left: Some(8),
            padding_right: Some(8),
            ..MainOptions::default()
        },
        vec![],
    )
        .into()
}

fn shown_window_frame(world: &mut World, id: WinID) -> IRect {
    let mut query = world.query::<&Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .shown_frame()
}

/// Steps one frame at a time until the morph cover is gone, recording each
/// window's shown (OS) frame on every frame.
fn shown_frames_of(harness: &mut TestHarness, ids: &[WinID]) -> Vec<Vec<IRect>> {
    let mut paths = ids
        .iter()
        .map(|id| vec![shown_window_frame(harness.world(), *id)])
        .collect::<Vec<_>>();
    for _ in 0..100 {
        if !morphed(harness.world()) {
            return paths;
        }
        harness.advance(ONE_FRAME);
        for (path, id) in paths.iter_mut().zip(ids) {
            let frame = shown_window_frame(harness.world(), *id);
            if path.last() != Some(&frame) {
                path.push(frame);
            }
        }
    }
    panic!("the morph ends");
}

/// Maximizing and restoring a tiled window both morph under the cover, even
/// when edge padding changes the window's top and bottom edges, and the real
/// windows move in one step without `FrameHold` writing an intermediate frame.
#[test]
fn a_maximize_and_restore_morph_under_the_cover() {
    let (mut harness, log) = harness(padded_resize_config());
    // Put window 0 at 50% so window 1 sits beside it, then focus window 1.
    send(
        &mut harness,
        resize(crate::types::commands::ResizeDirection::Shrink),
    );
    frames_of(&mut harness, &[0, 1]);
    send(&mut harness, focus(Direction::East));
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(&mut harness, &log);
    let rest_0 = shown_window_frame(harness.world(), 0);
    let rest_1 = shown_window_frame(harness.world(), 1);

    // Maximize window 1.
    let since = log.borrow().window_asks.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "maximize runs under a cover");
    until_morphs(&mut harness, &log, 1);
    assert_eq!(
        log.borrow().opened,
        vec![1],
        "the frame opens onto the maximized window itself"
    );
    let (from_1, to_1) = morph_of(&log, 0, 1);
    assert_eq!(from_1, rest_1);
    assert!(to_1.width() > from_1.width(), "window 1 widens");
    assert!(
        to_1.height() > from_1.height(),
        "window 1 fills edge padding"
    );
    let asks = window_asks_since(&log, since);
    assert_eq!(asks.len(), 1, "fresh picture asked for maximized window");
    assert_eq!(asks[0].0, 1);
    assert_eq!(asks[0].1, to_1.size());
    answer(&mut harness, 1, asks[0].2);
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().replaced,
        vec![(1, to_1.size())],
        "fresh maximized picture unveils mid-morph"
    );
    let max_paths = shown_frames_of(&mut harness, &[1]);
    assert_eq!(
        max_paths[0],
        vec![to_1],
        "real window snaps once on maximize"
    );

    // Restore window 1 immediately after landing (cached maximized photo is ready).
    harness.advance(ONE_FRAME);
    let morphs_before = log.borrow().morphs.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "restore runs under a cover");
    assert_eq!(log.borrow().morphs.len(), morphs_before + 1);
    let (r_from, r_to) = morph_of(&log, morphs_before, 1);
    assert_eq!(r_from, to_1);
    assert_eq!(r_to, rest_1);
    let restore_paths = shown_frames_of(&mut harness, &[0, 1]);
    assert_eq!(
        restore_paths[1],
        vec![rest_1],
        "real window snaps directly to restored frame with no premature hold write"
    );
    assert_eq!(restore_paths[0].last().copied(), Some(rest_0));
}

/// Maximizes window 1 of two side by side, answers the fresh picture of
/// its maximized frame, and lets the cover lift. Returns window 1's frame
/// at rest before the maximize.
fn maximized_with_its_picture(harness: &mut TestHarness, log: &ScreenLog) -> IRect {
    send(
        harness,
        resize(crate::types::commands::ResizeDirection::Shrink),
    );
    frames_of(harness, &[0, 1]);
    send(harness, focus(Direction::East));
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(harness, log);
    let rest = shown_window_frame(harness.world(), 1);
    let since = log.borrow().window_asks.len();
    send(harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    let asks = window_asks_since(log, since);
    assert_eq!(asks.len(), 1, "fresh picture asked for maximized window");
    answer(harness, 1, asks[0].2);
    harness.advance(ONE_FRAME);
    shown_frames_of(harness, &[1]);
    harness.advance(ONE_FRAME);
    rest
}

/// A window whose app has not taken its maximized frame when the cover has
/// waited its limit still maximizes: its pictures move as any resize's do,
/// onto its old picture and a fill, rather than waiting on the app.
#[test]
fn a_maximize_of_a_slow_window_starts_at_the_wait_limit() {
    let (mut harness, log) = harness(padded_resize_config());
    send(
        &mut harness,
        resize(crate::types::commands::ResizeDirection::Shrink),
    );
    frames_of(&mut harness, &[0, 1]);
    send(&mut harness, focus(Direction::East));
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(&mut harness, &log);
    // The window server shows window 1's new frame late, as a slow app's.
    harness.mock_state.show_late(1);

    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "maximize runs under a cover");
    assert!(
        log.borrow().morphs.is_empty(),
        "the pictures rest while the window has yet to take its frame"
    );
    until_morphs(&mut harness, &log, 1);
    assert!(
        log.borrow().opened.is_empty(),
        "a frame never opens onto a window that is not there yet"
    );
    harness.mock_state.show_now(1);
}

/// A restore while a maximize's pictures still rest, waiting for the window
/// to take its maximized frame, turns them from where they rest: nowhere,
/// since the restore heads back there. They moved from the maximized frame
/// back to the old one, as if the window had been maximized on screen.
#[test]
fn a_restore_while_a_maximize_waits_never_shows_the_maximized_frame() {
    let (mut harness, log) = harness(padded_resize_config());
    send(
        &mut harness,
        resize(crate::types::commands::ResizeDirection::Shrink),
    );
    frames_of(&mut harness, &[0, 1]);
    send(&mut harness, focus(Direction::East));
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(&mut harness, &log);
    let rest = shown_window_frame(harness.world(), 1);
    // The window server shows window 1's new frame late, so the pictures
    // wait for it.
    harness.mock_state.show_late(1);

    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "maximize runs under a cover");
    assert!(log.borrow().morphs.is_empty(), "the pictures wait");

    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    harness.mock_state.show_now(1);
    let morphs = log.borrow().morphs.clone();
    assert_eq!(morphs.len(), 1, "the restore turns the waiting morph");
    let (from, to) = morph_of(&log, 0, 1);
    assert_eq!(to, rest, "back to where the window was");
    assert_eq!(from, to, "from where the pictures rest");
    assert!(
        morphs[0].iter().all(|(_, from, to)| from == to),
        "no picture moves: {morphs:?}"
    );
}

/// A restore shows the window as it was before the maximize from its first
/// frame, from the picture kept of that size, so its frame closes onto the
/// layout it will have rather than onto the maximized layout's corner.
#[test]
fn a_restore_shows_the_window_as_it_was_before_the_maximize() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = maximized_with_its_picture(&mut harness, &log);
    assert!(
        log.borrow().headed.is_empty(),
        "the first maximize has no picture of the maximized window"
    );

    let morphs = log.borrow().morphs.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "restore runs under a cover");
    assert_eq!(
        log.borrow().headed,
        vec![(1, rest.size())],
        "the restore shows window 1's picture from before the maximize"
    );
    assert_eq!(
        log.borrow().headed_before,
        vec![morphs],
        "from the start of the restore's morph, in its own transaction"
    );
}

/// A turn of a restore mid-way, as a focus key makes, shows the window's
/// picture from before the maximize on as it heads to the same size. The
/// turn's morph dropped it, and the window showed its maximized layout's
/// corner for the rest of the restore.
#[test]
fn a_turned_restore_still_shows_the_window_as_it_was() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = maximized_with_its_picture(&mut harness, &log);
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().headed, vec![(1, rest.size())]);
    let morphs = log.borrow().morphs.len();

    send(&mut harness, focus(Direction::West));
    harness.advance(ONE_FRAME);
    assert_eq!(log.borrow().morphs.len(), morphs + 1, "the restore turns");
    let (_, to) = morph_of(&log, morphs, 1);
    assert_eq!(to.size(), rest.size(), "window 1 heads to the same size");
    assert_eq!(
        log.borrow().headed,
        vec![(1, rest.size()), (1, rest.size())],
        "the turn shows its picture from before the maximize again"
    );
    assert_eq!(
        log.borrow().headed_before.last().copied(),
        Some(morphs),
        "from the start of the turn's morph"
    );
}

/// A turn that only moves a restore's frames, as a focus key does once the
/// restore has its size and waits for the window, still shows the window's
/// picture from before the maximize. The turn left it out, and the window
/// showed the maximized picture cropped for the rest of the restore.
#[test]
fn a_moved_restore_at_its_size_still_shows_the_window_as_it_was() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = maximized_with_its_picture(&mut harness, &log);
    // The window server shows window 1 maximized until told otherwise, so
    // the restore waits at its size.
    harness.mock_state.show_late(1);
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME * 20);
    assert!(morphed(harness.world()), "the restore waits for the window");
    assert_eq!(log.borrow().headed.len(), 1);
    let morphs = log.borrow().morphs.len();

    send(&mut harness, focus(Direction::West));
    harness.advance(ONE_FRAME);
    harness.mock_state.show_now(1);
    assert_eq!(log.borrow().morphs.len(), morphs + 1, "the restore turns");
    let (from, to) = morph_of(&log, morphs, 1);
    assert_eq!(
        (from.size(), to.size()),
        (rest.size(), rest.size()),
        "window 1 only moves"
    );
    assert_eq!(
        log.borrow().headed.len(),
        2,
        "the turn shows its picture from before the maximize again"
    );
    assert_eq!(log.borrow().headed_before.last().copied(), Some(morphs));
}

/// A restore right after a maximize that showed the window's kept
/// maximized picture, before a fresh one arrives, shows the window as it
/// was before the maximize. The cover took the frame to hold its picture
/// from before the maximize still, and showed the maximized one cropped.
#[test]
fn a_restore_right_after_a_headed_maximize_shows_the_window_as_it_was() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = maximized_with_its_picture(&mut harness, &log);
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    shown_frames_of(&mut harness, &[1]);
    // Pictures of window 1 at rest, so the maximized one is kept as the
    // size it had before.
    take_pictures(&mut harness, &log);
    let maximized = {
        let screen = log.borrow();
        screen
            .window_asks
            .iter()
            .map(|(_, size, _)| *size)
            .find(|size| size.x > rest.width())
            .expect("a picture of window 1 maximized")
    };

    let since = log.borrow().window_asks.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().headed.last().copied(),
        Some((1, maximized)),
        "the maximize shows window 1's kept maximized picture"
    );
    harness.advance(ONE_FRAME);
    assert!(
        window_asks_since(&log, since)
            .iter()
            .any(|&(id, size, _)| id == 1 && size == maximized),
        "and still asks for a fresh one"
    );

    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().headed.last().copied(),
        Some((1, rest.size())),
        "the restore shows window 1 as it was"
    );
}

/// A restore a minute or more after its window's picture from before the
/// maximize was taken does not show it: laid over the maximized window's
/// picture, it shows content that may have changed since, as a page that
/// scrolled, beside the newer picture.
#[test]
fn a_late_restore_does_not_show_the_window_as_it_was() {
    let (mut harness, log) = harness(padded_resize_config());
    maximized_with_its_picture(&mut harness, &log);
    for _ in 0..61 {
        harness.advance(Duration::from_secs(1));
    }

    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "restore runs under a cover");
    assert!(
        log.borrow().headed.is_empty(),
        "the picture from a minute before is not shown"
    );
}

/// A window whose title changed while it was maximized shows another tab or
/// document, so its picture from before is not shown.
#[test]
fn a_restore_of_a_retitled_window_does_not_show_it_as_it_was() {
    let (mut harness, log) = harness(padded_resize_config());
    maximized_with_its_picture(&mut harness, &log);

    harness.run(vec![Event::WindowTitleChanged { window_id: 1 }]);
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "restore runs under a cover");
    assert!(
        log.borrow().headed.is_empty(),
        "the picture from before the title changed is not shown"
    );
}

/// Puts window 1 at rest beside window 0, focused, with a picture of every
/// window, and returns window 1's frame there.
fn window_1_at_rest_with_pictures(harness: &mut TestHarness, log: &ScreenLog) -> IRect {
    send(
        harness,
        resize(crate::types::commands::ResizeDirection::Shrink),
    );
    frames_of(harness, &[0, 1]);
    send(harness, focus(Direction::East));
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(harness, log);
    shown_window_frame(harness.world(), 1)
}

/// Maximizes window 1, which rests at `rest`, answers the fresh picture of
/// it maximized, lets the cover lift, and starts the restore. Returns the
/// pictures the cover was given at the sizes their frames head to.
fn maximize_and_restore(
    harness: &mut TestHarness,
    log: &ScreenLog,
    rest: IRect,
) -> Vec<(WinID, IVec2)> {
    let since = log.borrow().window_asks.len();
    send(harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    let fresh = window_asks_since(log, since)
        .into_iter()
        .find(|&(id, size, _)| id == 1 && size.x > rest.width())
        .expect("a fresh picture of window 1 maximized");
    answer(harness, 1, fresh.2);
    harness.advance(ONE_FRAME);
    shown_frames_of(harness, &[1]);
    harness.advance(ONE_FRAME);

    send(harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "restore runs under a cover");
    log.borrow().headed.clone()
}

/// Runs frames until the background refresh asks for a picture of window 1
/// at `size`, and returns that ask's generation, unanswered.
fn a_background_ask_of_window_1(harness: &mut TestHarness, log: &ScreenLog, size: IVec2) -> u32 {
    let since = log.borrow().window_asks.len();
    for _ in 0..200 {
        harness.advance(ONE_FRAME);
        if let Some(&(_, _, generation)) = window_asks_since(log, since)
            .iter()
            .find(|&&(id, asked, _)| id == 1 && asked == size)
        {
            return generation;
        }
    }
    panic!("the background refresh asks for window 1");
}

/// A window whose title changed before it was maximized, as when a new tab
/// opened, does not show its picture from before the change on the restore,
/// even once a fresh picture of it maximized has arrived. That arrival made
/// the old picture the one of the size the window had before, and the
/// restore showed the old tab.
#[test]
fn a_restore_after_a_title_change_before_the_maximize_does_not_show_the_old_title() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = window_1_at_rest_with_pictures(&mut harness, &log);
    harness.run(vec![Event::WindowTitleChanged { window_id: 1 }]);
    let headed = maximize_and_restore(&mut harness, &log, rest);
    assert!(
        !headed.contains(&(1, rest.size())),
        "the picture from before the title changed is not shown: {headed:?}"
    );
}

/// A background picture asked for before the window's title changed, and
/// answered after, shows the old title: it is dropped. Kept, it counted as
/// taken after the change, and a restore showed the old tab.
#[test]
fn a_background_picture_asked_before_a_title_change_is_dropped() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = window_1_at_rest_with_pictures(&mut harness, &log);
    let old = a_background_ask_of_window_1(&mut harness, &log, rest.size());
    send(&mut harness, Event::WindowTitleChanged { window_id: 1 });
    harness.advance(ONE_FRAME);
    answer(&mut harness, 1, old);
    harness.advance(ONE_FRAME);
    let headed = maximize_and_restore(&mut harness, &log, rest);
    assert!(
        !headed.contains(&(1, rest.size())),
        "the picture from before the title changed is not shown: {headed:?}"
    );
}

/// A background picture that arrives in the frame the window's title
/// changes, ahead of the change, shows the old title too. It counted as
/// taken at the moment of the change, and a restore showed the old tab.
#[test]
fn a_background_picture_in_the_frame_of_a_title_change_is_not_shown() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = window_1_at_rest_with_pictures(&mut harness, &log);
    let old = a_background_ask_of_window_1(&mut harness, &log, rest.size());
    answer(&mut harness, 1, old);
    send(&mut harness, Event::WindowTitleChanged { window_id: 1 });
    harness.advance(ONE_FRAME);
    let headed = maximize_and_restore(&mut harness, &log, rest);
    assert!(
        !headed.contains(&(1, rest.size())),
        "the picture from before the title changed is not shown: {headed:?}"
    );
}

/// A background picture that arrives in the frame the window's title
/// changes and a maximize starts never shows: the cover starts from the
/// picture kept before it, not from the old title's. It was kept before the
/// change was read, and the maximize's cover showed it.
#[test]
fn a_background_picture_in_the_frame_of_a_title_change_never_starts_a_cover() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = window_1_at_rest_with_pictures(&mut harness, &log);
    let old = a_background_ask_of_window_1(&mut harness, &log, rest.size());
    let covers = log.borrow().shown_digests.len();
    let since = log.borrow().window_asks.len();

    answer(&mut harness, 1, old);
    send(&mut harness, Event::WindowTitleChanged { window_id: 1 });
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "the maximize runs under a cover");
    let started = log.borrow().shown_digests[covers..]
        .first()
        .cloned()
        .expect("a cover goes up");
    let shown = started
        .iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, digest)| *digest)
        .expect("a picture of window 1");
    assert_ne!(
        shown,
        u64::from(old),
        "the cover starts from the picture kept before the title changed"
    );

    let fresh = window_asks_since(&log, since)
        .into_iter()
        .find(|&(id, size, _)| id == 1 && size.x > rest.width())
        .expect("a fresh picture of window 1 maximized");
    answer(&mut harness, 1, fresh.2);
    harness.advance(ONE_FRAME);
    shown_frames_of(&mut harness, &[1]);
    harness.advance(ONE_FRAME);
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "restore runs under a cover");
    assert!(
        !log.borrow().headed.contains(&(1, rest.size())),
        "the restore shows no picture from before the title changed"
    );
}

/// Runs frames, answering every picture of a window the background refresh
/// asks for, until it asks for a picture of a desktop, which it returns
/// unanswered.
fn a_background_desktop_ask(harness: &mut TestHarness, log: &ScreenLog) -> (CoverSubject, u32) {
    let mut answered = log.borrow().asked.len();
    for _ in 0..2000 {
        harness.advance(ONE_FRAME);
        let asked = log.borrow().asked.clone();
        for &(subject, generation) in &asked[answered..] {
            match subject {
                CoverSubject::Desktop(_) => return (subject, generation),
                CoverSubject::Window(id) => answer(harness, id, generation),
            }
        }
        answered = asked.len();
    }
    panic!("the background refresh asks for the desktop");
}

/// A refusal of pictures in a frame keeps nothing that frame, even a
/// background picture that arrives after it. The refusal cleared the
/// pictures, and the background picture came in after it while the engine
/// had no leave to take any.
#[test]
fn a_refusal_keeps_no_picture_that_arrives_after_it() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = window_1_at_rest_with_pictures(&mut harness, &log);
    let (desktop, background) = a_background_desktop_ask(&mut harness, &log);

    let since = log.borrow().window_asks.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "the maximize runs under a cover");
    let (_, _, fresh) = window_asks_since(&log, since)
        .into_iter()
        .find(|&(id, size, _)| id == 1 && size.x > rest.width())
        .expect("a fresh picture of window 1 maximized");

    send(
        &mut harness,
        Event::CoverPhoto {
            subject: CoverSubject::Window(1),
            generation: fresh,
            result: Err(crate::ecs::slice_photos::PhotoError::Refused),
        },
    );
    send(
        &mut harness,
        Event::CoverPhoto {
            subject: desktop,
            generation: background,
            result: Ok(Photo {
                digest: Digest(u64::from(background)),
                image: pixel(),
                edge_color: None,
            }),
        },
    );
    harness.app.update();
    assert!(
        harness
            .world()
            .resource::<crate::ecs::strip_cover::CoverPhotos>()
            .kept_nothing(),
        "no picture is kept after the refusal"
    );
}

/// A resize's fresh picture that arrives in the frame the window's title
/// changes shows the old title: the cover drops it, shows nothing of it,
/// and asks for a picture of the new title.
#[test]
fn a_fresh_picture_in_the_frame_of_a_title_change_is_asked_for_again() {
    let (mut harness, log) = harness(padded_resize_config());
    let rest = window_1_at_rest_with_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    let (_, maximized, old) = window_asks_since(&log, since)
        .into_iter()
        .find(|&(id, size, _)| id == 1 && size.x > rest.width())
        .expect("a fresh picture of window 1 maximized");

    answer(&mut harness, 1, old);
    send(&mut harness, Event::WindowTitleChanged { window_id: 1 });
    harness.advance(ONE_FRAME);
    assert!(
        log.borrow().replaced.is_empty(),
        "the picture of the old title shows nowhere"
    );
    harness.advance(ONE_FRAME);
    let again = window_asks_since(&log, since)
        .into_iter()
        .find(|&(id, size, generation)| id == 1 && size == maximized && generation != old)
        .expect("a picture of the new title is asked for");
    answer(&mut harness, 1, again.2);
    harness.advance(ONE_FRAME);
    assert_eq!(
        log.borrow().replaced,
        vec![(1, maximized)],
        "and it shows once it arrives"
    );
}

/// Maximizing a stacked window unstacks it into a full-screen column (changing
/// both window order and height) and restoring puts it back in the stack, both
/// smoothly under the cover.
#[test]
fn a_stacked_window_maximize_and_restore_morph_under_the_cover() {
    let (mut harness, log) = harness(padded_resize_config());
    send(
        &mut harness,
        Event::Command {
            command: Command::Window(Operation::Stack(crate::commands::Side::East)),
        },
    );
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(&mut harness, &log);
    let before_0 = window_frame(harness.world(), 0);
    let before_1 = window_frame(harness.world(), 1);
    assert!(before_0.height() < 600, "window 0 starts stacked");

    let since = log.borrow().window_asks.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "stacked maximize is covered");
    until_morphs(&mut harness, &log, 1);
    let (from_0, to_0) = morph_of(&log, 0, 0);
    let (from_1, to_1) = morph_of(&log, 0, 1);
    assert_eq!(from_0, before_0);
    assert_eq!(from_1, before_1);
    assert!(
        to_0.height() > from_0.height(),
        "maximized window grows vertically"
    );
    assert!(
        to_1.height() > from_1.height(),
        "stackmate expands vertically too"
    );
    // Both grown windows get fresh pictures so cache is warm for restore.
    let asks = window_asks_since(&log, since);
    for (id, _, generation) in asks {
        answer(&mut harness, id, generation);
    }
    frames_of(&mut harness, &[0, 1]);
    harness.advance(ONE_FRAME);

    // Restore window 0 back into the stack.
    let morphs_before = log.borrow().morphs.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "stacked restore is covered");
    assert_eq!(log.borrow().morphs.len(), morphs_before + 1);
    let (r_from_0, r_to_0) = morph_of(&log, morphs_before, 0);
    assert_eq!(r_from_0, to_0);
    assert_eq!(r_to_0, before_0);
}

/// Toggling maximize again mid-morph turns the cover smoothly from its live
/// presented frames instead of aborting.
#[test]
fn a_maximize_mid_morph_turns_smoothly() {
    let (mut harness, log) = harness(padded_resize_config());
    take_pictures(&mut harness, &log);
    let rest_0 = window_frame(harness.world(), 0);
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME * 2);
    assert!(morphed(harness.world()));
    assert_eq!(log.borrow().morphs.len(), 1);

    // Restore mid-flight.
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "cover stays up and turns");
    assert_eq!(
        log.borrow().morphs.len(),
        2,
        "second morph started for the turn"
    );
    let (turn_from, turn_to) = morph_of(&log, 1, 0);
    assert_eq!(turn_to, rest_0);
    assert!(
        turn_from.width() > rest_0.width(),
        "turn starts from the live mid-flight frame"
    );
}

/// Resizing a maximized window restores it and morphs both the width and
/// edge-padding height change smoothly under the cover.
#[test]
fn a_resize_of_a_maximized_window_morphs_under_the_cover() {
    use crate::types::commands::ResizeDirection;
    let (mut harness, log) = harness(padded_resize_config());
    take_pictures(&mut harness, &log);
    let since = log.borrow().window_asks.len();
    send(&mut harness, maximize_cmd());
    harness.advance(ONE_FRAME);
    for (id, _, generation) in window_asks_since(&log, since) {
        answer(&mut harness, id, generation);
    }
    frames_of(&mut harness, &[0]);
    harness.advance(ONE_FRAME);

    let morphs_before = log.borrow().morphs.len();
    send(&mut harness, resize(ResizeDirection::Shrink));
    harness.advance(ONE_FRAME);
    assert!(
        morphed(harness.world()),
        "resizing out of maximize runs under the cover"
    );
    assert_eq!(log.borrow().morphs.len(), morphs_before + 1);
}

fn send_title_bar_double_click_down(
    harness: &mut TestHarness,
    window_id: WinID,
    point: objc2_core_foundation::CGPoint,
) {
    use crate::events::MouseButton;
    use crate::platform::Modifiers;
    send(
        harness,
        Event::MouseDown {
            point,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
            window_id: Some(window_id),
            clicks: 1,
            hold: None,
        },
    );
    harness.advance(ONE_FRAME);
    send(
        harness,
        Event::MouseUp {
            point,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        },
    );
    harness.advance(ONE_FRAME);
    send(
        harness,
        Event::MouseDown {
            point,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
            window_id: Some(window_id),
            clicks: 2,
            hold: None,
        },
    );
    harness.advance(ONE_FRAME);
}

/// A real title-bar double-click delivers the second `MouseDown` frames before
/// its `MouseUp`. It must not enter `DragState::Pressed` (which would skip the
/// cover as a drag and hold the window back) and must morph identically to the
/// maximize shortcut on both maximize and restore.
#[test]
fn a_title_bar_double_click_morphs_under_the_cover() {
    use crate::events::MouseButton;
    use crate::platform::Modifiers;
    use objc2_core_foundation::CGPoint;

    let (mut harness, log) = harness(padded_resize_config());
    send(
        &mut harness,
        resize(crate::types::commands::ResizeDirection::Shrink),
    );
    frames_of(&mut harness, &[0, 1]);
    send(&mut harness, focus(Direction::East));
    for _ in 0..40 {
        harness.advance(ONE_FRAME);
    }
    take_pictures(&mut harness, &log);
    let rest_1 = shown_window_frame(harness.world(), 1);
    let point = CGPoint::new(f64::from(rest_1.center().x), f64::from(rest_1.min.y + 10));

    let since = log.borrow().window_asks.len();
    send_title_bar_double_click_down(&mut harness, 1, point);
    assert!(
        morphed(harness.world()),
        "title-bar double-click maximize runs under the cover on MouseDown"
    );
    until_morphs(&mut harness, &log, 1);
    let (from_1, to_1) = morph_of(&log, 0, 1);
    assert_eq!(from_1, rest_1);
    assert!(to_1.width() > from_1.width());
    assert!(to_1.height() > from_1.height());
    // Release click 2 mid-morph: must not abort or start a spurious second morph.
    harness.advance(ONE_FRAME * 2);
    send(
        &mut harness,
        Event::MouseUp {
            point,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        },
    );
    for (id, _, generation) in window_asks_since(&log, since) {
        answer(&mut harness, id, generation);
    }
    harness.advance(ONE_FRAME);
    assert!(morphed(harness.world()), "MouseUp keeps the cover morph");
    assert_eq!(log.borrow().morphs.len(), 1, "no spurious turn on MouseUp");
    let max_paths = shown_frames_of(&mut harness, &[1]);
    assert_eq!(max_paths[0], vec![to_1]);

    // Double-click again to restore.
    harness.advance(ONE_FRAME);
    let max_point = CGPoint::new(f64::from(to_1.center().x), f64::from(to_1.min.y + 10));
    let morphs_before = log.borrow().morphs.len();
    send_title_bar_double_click_down(&mut harness, 1, max_point);
    assert!(
        morphed(harness.world()),
        "title-bar double-click restore runs under the cover on MouseDown"
    );
    assert_eq!(log.borrow().morphs.len(), morphs_before + 1);
    let restore_paths = shown_frames_of(&mut harness, &[1]);
    assert_eq!(restore_paths[0], vec![rest_1]);
}
