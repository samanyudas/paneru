//! The picture cover over keyboard strip moves (miri#509).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bevy::math::IVec2;
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

/// What the engine asked of the cover screen.
#[derive(Default)]
struct Screen {
    denied: bool,
    asked: Vec<(CoverSubject, u32)>,
    /// The strip origin and picture count of each cover shown.
    shown: Vec<(IVec2, usize)>,
    /// The first and last strip x of each slide.
    slides: Vec<(f64, f64)>,
    /// Each strip x the engine told the tap about.
    tracked: Vec<f64>,
    /// Each strip x a swipe put the cover's strip at.
    placed: Vec<f64>,
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
        _size: IVec2,
        _display: CGDirectDisplayID,
        generation: u32,
    ) {
        self.0
            .borrow_mut()
            .asked
            .push((CoverSubject::Window(window), generation));
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
        self.0
            .borrow_mut()
            .shown
            .push((scene.strip, scene.pictures.len()));
    }

    fn slide(&mut self, _display: CGDirectDisplayID, path: &[f64], _step: f64) {
        let (first, last) = (path[0], *path.last().unwrap());
        self.0.borrow_mut().slides.push((first, last));
    }

    fn track(&mut self, _display: CGDirectDisplayID, x: f64) {
        self.0.borrow_mut().tracked.push(x);
    }

    fn place(&mut self, _display: CGDirectDisplayID, x: f64) {
        self.0.borrow_mut().placed.push(x);
    }

    fn showing(&self, _display: CGDirectDisplayID) -> Option<f64> {
        None
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
    let log = ScreenLog::default();
    let mut harness = TestHarness::new().with_config(config);
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

/// Modifier-scroll stays uncovered: its ticks over the cover would not
/// reach the engine.
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
