//! The pointer on an edge slice panel: a press focuses the window the panel
//! shows, scrolls it into view, and reaches the app at the pressed point,
//! and a drag from the panel goes on in the app.

use std::time::Duration;

use bevy::math::{IRect, IVec2};
use bevy::prelude::With;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGEventFlags;

use super::slice_photos::{ScreenLog, painted_beside_with, painted_west_with};
use super::*;
use crate::config::{Config, MainOptions};
use crate::ecs::FocusedMarker;
use crate::events::MouseButton;
use crate::manager::WindowManager;
use crate::platform::slice_tap::{
    GIVE_UP, Mouse, Pointer, SlicePanel, SlicePress, SliceTap, Verdict,
};
use crate::platform::{Modifiers, WinID};
use crate::types::commands::Side;

const LEFT: MouseButton = MouseButton::Left;
const SLIDE: Duration = Duration::from_secs(1);

/// What reached the apps: each event the tap let through, and whether the
/// engine posted it.
type Reached = Vec<(Mouse, CGPoint, bool)>;

/// The engine with the tap and the window server around it. The user's
/// pointer events go through the tap, the ones it lets through reach the
/// engine, and the presses the engine posts come back through the tap.
struct Desk {
    harness: TestHarness,
    log: ScreenLog,
    /// The only panel, as the user first sees it.
    panel: SlicePanel,
    tap: SliceTap,
    reached: Reached,
    posted: Vec<(SlicePress, CGPoint)>,
    /// The time of the events the tap sees, which [`Desk::wait`] moves on.
    clock: Duration,
}

impl Desk {
    /// Window 3 shows the panel, on the east edge.
    fn new(focus_follows_mouse: bool) -> Self {
        Self::of(painted_beside_with(config(focus_follows_mouse)), 3)
    }

    /// Window 0 shows the panel, on the west edge.
    fn west() -> Self {
        Self::of(painted_west_with(config(false)), 0)
    }

    fn of((harness, log): (TestHarness, ScreenLog), window: WinID) -> Self {
        let placed = log.borrow().placed.clone();
        let [(shown, slice)] = placed[..] else {
            panic!("only window {window} has a panel: {placed:?}");
        };
        assert_eq!(shown, window, "{placed:?}");
        Self {
            harness,
            log,
            panel: SlicePanel {
                panel: 1000 + window,
                window,
                on_screen: slice.on_screen,
                in_window: slice.in_window,
                side: slice.side,
            },
            tap: SliceTap::default(),
            reached: Vec::new(),
            posted: Vec::new(),
            clock: Duration::ZERO,
        }
    }

    fn on_panel(&self) -> CGPoint {
        let on = self.panel.on_screen;
        CGPoint::new(
            f64::from(on.min.x) + 1.0,
            f64::from(on.min.y + on.height() / 2),
        )
    }

    /// The panel under `point`, while its window still has one there.
    fn panel_at(&self, point: CGPoint) -> Option<SlicePanel> {
        let window = self.panel.window;
        let showing = self.log.borrow().placed.iter().any(|(id, _)| *id == window);
        Some(self.panel).filter(|panel| showing && panel.contains(point))
    }

    fn user(&mut self, mouse: Mouse, point: CGPoint) {
        self.user_at(mouse, point, self.clock);
    }

    /// The user's `mouse` that the user made at `time`, which reaches the
    /// tap only now.
    fn user_at(&mut self, mouse: Mouse, point: CGPoint, time: Duration) {
        let pointer = Pointer {
            mouse,
            point,
            flags: CGEventFlags::empty(),
            time,
            tag: None,
            panel: matches!(mouse, Mouse::Down(_) | Mouse::Moved | Mouse::Scrolled)
                .then(|| self.panel_at(point))
                .flatten(),
        };
        match self.tap.handle(&pointer) {
            Verdict::Pass => self.reach(mouse, point, false),
            Verdict::Move(to) => self.reach(mouse, to, false),
            Verdict::Quiet | Verdict::Swallow => {}
            Verdict::Hold(press) => {
                self.harness
                    .world()
                    .write_message(Event::SlicePress { press });
            }
            Verdict::PassAndRelease { .. } => panic!("the user's {mouse:?} released a held press"),
        }
        self.harness.app.update();
    }

    /// Runs the engine for `duration`, sending each press it posts back
    /// through the tap.
    fn wait(&mut self, duration: Duration) {
        for _ in 0..duration.as_millis().div_ceil(20) {
            self.harness.advance(Duration::from_millis(20));
            self.clock += Duration::from_millis(20);
            for (press, point) in self.harness.mock_state.take_posted_presses() {
                self.posted.push((press, point));
                let pointer = Pointer {
                    mouse: Mouse::Down(press.button),
                    point,
                    flags: press.flags,
                    time: self.clock,
                    tag: Some(press.id),
                    panel: self.panel_at(point),
                };
                match self.tap.handle(&pointer) {
                    Verdict::Pass => self.reach(pointer.mouse, point, true),
                    Verdict::PassAndRelease { id, button } => {
                        self.reach(pointer.mouse, point, true);
                        let release = Pointer {
                            mouse: Mouse::Up(button),
                            tag: Some(id),
                            ..pointer
                        };
                        assert_eq!(self.tap.handle(&release), Verdict::Pass);
                        self.reach(release.mouse, point, true);
                    }
                    Verdict::Quiet | Verdict::Swallow | Verdict::Move(_) | Verdict::Hold(_) => {}
                }
            }
        }
    }

    /// `mouse` reaches the app under `point`, and the engine hears of it.
    fn reach(&mut self, mouse: Mouse, point: CGPoint, posted: bool) {
        self.reached.push((mouse, point, posted));
        let modifiers = Modifiers::empty();
        let event = match mouse {
            Mouse::Down(button) => Event::MouseDown {
                point,
                modifiers,
                button,
                window_id: None,
                clicks: 1,
            },
            Mouse::Dragged(button) => Event::MouseDragged {
                point,
                modifiers,
                button,
            },
            Mouse::Up(button) => Event::MouseUp {
                point,
                modifiers,
                button,
            },
            Mouse::Moved => Event::MouseMoved { point, modifiers },
            Mouse::Scrolled => Event::Scroll { delta: 0.5 },
        };
        self.harness.world().write_message(event);
    }

    fn focused(&mut self) -> Vec<WinID> {
        let world = self.harness.world();
        let mut query = world.query_filtered::<&Window, With<FocusedMarker>>();
        query.iter(world).map(|window| window.id()).collect()
    }

    fn frame(&self, window: WinID) -> IRect {
        self.harness.mock_state.ax_frame(window)
    }

    fn window_at(&mut self, point: CGPoint) -> Option<WinID> {
        self.harness
            .world()
            .resource::<WindowManager>()
            .find_window_at_point(&point)
            .ok()
    }

    /// Where a press on the panel at `pressed` lands once the window is in
    /// view: on the pressed row, 12 points inside the edge the panel shows.
    fn pressed_in_view(&self, pressed: CGPoint) -> CGPoint {
        let panel = self.panel;
        let frame = self.frame(panel.window);
        let x = match panel.side {
            Side::West => frame.max.x - 12,
            Side::East => frame.min.x + 12,
        };
        CGPoint::new(
            f64::from(x),
            f64::from(frame.min.y + panel.in_window.min.y - panel.on_screen.min.y) + pressed.y,
        )
    }
}

fn config(focus_follows_mouse: bool) -> Config {
    (
        MainOptions {
            shared_edge_slices: Some(true),
            focus_follows_mouse: Some(focus_follows_mouse),
            ..MainOptions::default()
        },
        vec![],
    )
        .into()
}

fn on_display(frame: IRect) -> bool {
    frame.min.x >= 0 && frame.max.x <= TEST_DISPLAY_WIDTH
}

#[test]
fn a_press_on_a_panel_focuses_and_reveals_its_window_and_posts_nothing_before_it_arrives() {
    let mut desk = Desk::new(false);
    assert!(!on_display(desk.frame(3)), "window 3 starts hidden");
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    desk.harness.app.update();

    assert_eq!(desk.focused(), vec![3], "focused at once");
    assert!(desk.posted.is_empty() && desk.reached.is_empty());
    desk.wait(SLIDE);
    assert!(on_display(desk.frame(3)), "{:?} in view", desk.frame(3));
}

#[test]
fn a_held_press_arrives_as_one_posted_press() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    desk.wait(SLIDE);

    let target = desk.pressed_in_view(pressed);
    assert_eq!(desk.window_at(target), Some(3));
    assert_eq!(desk.posted.len(), 1, "{:?}", desk.posted);
    assert_eq!((desk.posted[0].0.button, desk.posted[0].1), (LEFT, target));
    assert_eq!(desk.reached, vec![(Mouse::Down(LEFT), target, true)]);
}

#[test]
fn a_press_lands_on_its_row_12_points_inside_the_edge_its_panel_shows() {
    for mut desk in [Desk::new(false), Desk::west()] {
        let pressed = desk.on_panel();

        desk.user(Mouse::Down(LEFT), pressed);
        desk.user(Mouse::Up(LEFT), pressed);
        desk.wait(SLIDE);

        let landed = desk
            .posted
            .iter()
            .map(|(_, point)| *point)
            .collect::<Vec<_>>();
        assert_eq!(
            landed,
            vec![desk.pressed_in_view(pressed)],
            "{:?} panel",
            desk.panel.side
        );
    }
}

#[test]
fn a_click_released_before_it_arrives_reaches_the_app_with_its_release() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(Mouse::Up(LEFT), pressed);
    assert!(desk.reached.is_empty(), "the release waits for its press");
    desk.wait(SLIDE);

    let target = desk.pressed_in_view(pressed);
    assert_eq!(desk.posted.len(), 1, "{:?}", desk.posted);
    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), target, true),
            (Mouse::Up(LEFT), target, true)
        ]
    );
}

#[test]
fn a_release_made_before_its_press_arrives_but_seen_after_it_lands_with_the_press() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    let released_at = desk.clock;
    desk.wait(SLIDE);
    assert_eq!(desk.posted.len(), 1, "{:?}", desk.posted);
    desk.user_at(Mouse::Up(LEFT), pressed, released_at);

    let target = desk.pressed_in_view(pressed);
    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), target, true),
            (Mouse::Up(LEFT), target, false)
        ]
    );
}

#[test]
fn a_drag_from_a_panel_goes_on_in_the_app_once_the_press_arrives() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(
        Mouse::Dragged(LEFT),
        CGPoint::new(pressed.x - 10.0, pressed.y),
    );
    desk.wait(SLIDE);
    let target = desk.pressed_in_view(pressed);
    let (dragged, released) = (
        CGPoint::new(target.x - 40.0, target.y + 5.0),
        CGPoint::new(target.x - 60.0, target.y + 5.0),
    );
    desk.user(Mouse::Dragged(LEFT), dragged);
    desk.user(Mouse::Up(LEFT), released);

    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), target, true),
            (Mouse::Dragged(LEFT), dragged, false),
            (Mouse::Up(LEFT), released, false)
        ]
    );
}

#[test]
fn a_drag_released_before_it_arrives_reveals_its_window_and_reaches_no_app() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(
        Mouse::Dragged(LEFT),
        CGPoint::new(pressed.x - 10.0, pressed.y),
    );
    desk.user(Mouse::Up(LEFT), CGPoint::new(pressed.x - 20.0, pressed.y));
    desk.wait(SLIDE);

    assert!(desk.reached.is_empty(), "{:?}", desk.reached);
    assert!(on_display(desk.frame(3)), "{:?} in view", desk.frame(3));
    assert_eq!(desk.focused(), vec![3]);
}

#[test]
fn a_press_whose_window_never_arrives_gives_up_and_keeps_focus() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();
    let stuck = desk.frame(3);
    desk.harness
        .mock_state
        .update_window(3, |window| window.dropped_positions = u16::MAX);

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(Mouse::Up(LEFT), pressed);
    desk.wait(GIVE_UP + SLIDE);
    assert_eq!(desk.frame(3), stuck, "the app never moved window 3");
    assert!(desk.posted.is_empty(), "{:?}", desk.posted);

    let late = IRect::from_corners(IVec2::new(300, 20), IVec2::new(300, 20) + stuck.size());
    desk.harness
        .mock_state
        .update_window(3, |window| window.frame = late);
    desk.harness
        .world()
        .write_message(Event::WindowMoved { window_id: 3 });
    let target = desk.pressed_in_view(pressed);
    assert!(
        on_display(late) && desk.window_at(target) == Some(3),
        "window 3 arrives late"
    );
    desk.wait(SLIDE);
    assert!(desk.posted.is_empty(), "nothing after giving up");
    assert!(desk.reached.is_empty(), "{:?}", desk.reached);
    assert_eq!(desk.focused(), vec![3]);
}

#[test]
fn a_second_press_before_the_first_arrives_reaches_no_window() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();
    let elsewhere = CGPoint::new(pressed.x - 300.0, pressed.y);
    let dragged = CGPoint::new(elsewhere.x - 10.0, elsewhere.y);
    assert!(desk.panel_at(elsewhere).is_none());

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(Mouse::Up(LEFT), pressed);
    desk.user(Mouse::Down(LEFT), elsewhere);
    desk.user(Mouse::Dragged(LEFT), dragged);
    desk.user(Mouse::Up(LEFT), dragged);
    desk.wait(SLIDE);

    let target = desk.pressed_in_view(pressed);
    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), target, true),
            (Mouse::Up(LEFT), target, true)
        ]
    );
}

#[test]
fn a_double_click_on_a_panel_reaches_the_app_as_one_click() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(Mouse::Up(LEFT), pressed);
    desk.user(Mouse::Down(LEFT), pressed);
    desk.wait(SLIDE);
    desk.user(Mouse::Up(LEFT), pressed);

    let target = desk.pressed_in_view(pressed);
    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), target, true),
            (Mouse::Up(LEFT), target, true)
        ]
    );
}

#[test]
fn after_a_held_press_expires_the_next_press_reaches_the_app() {
    let mut desk = Desk::new(false);
    let pressed = desk.on_panel();
    desk.harness
        .mock_state
        .update_window(3, |window| window.dropped_positions = u16::MAX);

    desk.user(Mouse::Down(LEFT), pressed);
    desk.wait(GIVE_UP + SLIDE);
    let dragged = CGPoint::new(pressed.x - 10.0, pressed.y);
    desk.user(Mouse::Dragged(LEFT), dragged);
    desk.user(Mouse::Up(LEFT), dragged);
    assert!(desk.reached.is_empty(), "{:?}", desk.reached);

    let elsewhere = CGPoint::new(pressed.x - 300.0, pressed.y);
    desk.user(Mouse::Down(LEFT), elsewhere);
    desk.user(Mouse::Up(LEFT), elsewhere);
    assert!(desk.posted.is_empty(), "{:?}", desk.posted);
    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), elsewhere, false),
            (Mouse::Up(LEFT), elsewhere, false)
        ]
    );
}

#[test]
fn a_press_on_a_panel_never_focuses_the_window_beneath() {
    let mut desk = Desk::new(true);
    let pressed = desk.on_panel();
    let beneath = desk.window_at(pressed).expect("a window beneath the panel");
    assert_ne!(beneath, 3);

    desk.user(Mouse::Down(LEFT), pressed);
    desk.user(Mouse::Up(LEFT), pressed);
    for _ in 0..50 {
        desk.wait(Duration::from_millis(20));
        assert!(
            !desk.focused().contains(&beneath),
            "window {beneath} focused"
        );
    }
    assert_eq!(desk.focused(), vec![3]);
    let target = desk.pressed_in_view(pressed);
    assert_eq!(
        desk.reached,
        vec![
            (Mouse::Down(LEFT), target, true),
            (Mouse::Up(LEFT), target, true)
        ],
        "only window 3 got the click"
    );
}

#[test]
fn scrolling_and_hovering_over_a_panel_change_nothing() {
    let mut desk = Desk::new(true);
    let on_panel = desk.on_panel();
    let beneath = desk
        .window_at(on_panel)
        .expect("a window beneath the panel");
    let before = desk.focused();
    let frames = |desk: &Desk| (0..4).map(|id| desk.frame(id)).collect::<Vec<_>>();
    let laid_out = frames(&desk);

    for step in 0..10 {
        let point = CGPoint::new(
            on_panel.x + f64::from(step % 3),
            on_panel.y + f64::from(step),
        );
        desk.user(Mouse::Moved, point);
        desk.user(Mouse::Scrolled, point);
        desk.wait(Duration::from_millis(100));
    }
    assert!(desk.reached.is_empty(), "{:?}", desk.reached);
    assert_eq!(desk.focused(), before);
    assert_eq!(frames(&desk), laid_out);

    let frame = desk.frame(beneath);
    let off_panel = CGPoint::new(f64::from(frame.min.x + 20), on_panel.y);
    desk.user(Mouse::Moved, off_panel);
    desk.wait(Duration::from_millis(200));
    assert_eq!(
        desk.focused(),
        vec![beneath],
        "hovering off the panel focuses"
    );
}

#[test]
fn the_tap_lets_through_only_the_posted_press_it_waits_for() {
    let mut tap = SliceTap::default();
    let panel = SlicePanel {
        panel: 9,
        window: 42,
        on_screen: IRect::new(1019, 20, 1024, 200),
        in_window: IRect::new(395, 0, 400, 180),
        side: Side::West,
    };
    let user = |mouse, x, y, panel| Pointer {
        mouse,
        point: CGPoint::new(x, y),
        flags: CGEventFlags::MaskCommand,
        time: Duration::ZERO,
        tag: None,
        panel,
    };
    let Verdict::Hold(press) = tap.handle(&user(Mouse::Down(LEFT), 1020.0, 80.0, Some(panel)))
    else {
        panic!("a press on the panel is held back");
    };
    assert_eq!(
        (press.window_id, press.at, press.flags),
        (42, CGPoint::new(396.0, 60.0), CGEventFlags::MaskCommand)
    );
    let posted = |mouse, id| Pointer {
        mouse,
        point: CGPoint::new(500.0, 100.0),
        flags: CGEventFlags::MaskCommand,
        time: Duration::ZERO,
        tag: Some(id),
        panel: Some(panel),
    };

    assert_eq!(
        tap.handle(&posted(Mouse::Down(LEFT), press.id + 1)),
        Verdict::Swallow
    );
    assert_eq!(
        tap.handle(&posted(Mouse::Down(LEFT), press.id)),
        Verdict::Pass
    );
    assert_eq!(
        tap.handle(&posted(Mouse::Up(LEFT), press.id)),
        Verdict::Pass
    );
    assert_eq!(
        tap.handle(&posted(Mouse::Down(LEFT), press.id)),
        Verdict::Swallow
    );
    assert_eq!(
        tap.handle(&user(Mouse::Dragged(LEFT), 600.0, 100.0, None)),
        Verdict::Pass
    );
}
