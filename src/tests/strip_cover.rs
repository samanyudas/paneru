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
    /// Where Core Animation shows the cover's strip, if the test says.
    showing: Option<f64>,
    /// Each window's frame where its picture starts and where it heads, for
    /// each morph.
    morphs: Vec<Vec<(WinID, IRect, IRect)>>,
    /// How far along Core Animation shows the morph, if the test says.
    morph_share: Option<f64>,
    fades: usize,
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

    fn fade(&mut self, _display: CGDirectDisplayID, _seconds: f64) {
        self.0.borrow_mut().fades += 1;
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
/// frame once, under the cover. The cover then fades out and comes down.
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
    assert_eq!(log.borrow().fades, 1, "the cover fades out");
    assert_eq!(log.borrow().hidden, 1, "and comes down");
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
    assert_eq!(log.borrow().fades, 1);
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
/// column, so each one's picture stretches together.
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
/// size after the write returned does, and fades once it shows the new one.
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
    assert_eq!(log.borrow().fades, 0, "no fade while the old frame shows");
    harness.mock_state.show_now(0);
    harness.advance(ONE_FRAME * 3);
    assert_eq!(
        log.borrow().fades,
        1,
        "the cover fades once the new frame shows"
    );
}

/// A neighbor that a resize pushes off the cover's area is still checked
/// before the fade: the window server may still show it at its old frame,
/// under the cover.
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
    assert_eq!(log.borrow().fades, 1, "only the first resize's cover faded");
    harness.mock_state.show_now(1);
    harness.advance(ONE_FRAME * 3);
    assert_eq!(
        log.borrow().fades,
        2,
        "the cover fades once the neighbor shows"
    );
}
