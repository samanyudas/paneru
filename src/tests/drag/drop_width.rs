//! A tiled window whose app widens it by a few points while the user drags it
//! below the screen and drops it at home returns to its column's slot and
//! width (samanyudas/miri#414).

use super::*;
use crate::ecs::Bounds;

const DRAGGED: WinID = 1;
const LEFT: WinID = 0;
const GROWTH: i32 = 2;

/// A drag of the middle of three columns from its top bar to just above the
/// display's bottom edge, `dx` points sideways within its own column, so the
/// drop puts it back home.
struct HomeDrop {
    harness: TestHarness,
    slot: IRect,
    left_slot: IRect,
    press: Origin,
    to: Origin,
}

impl HomeDrop {
    fn new(dx: i32) -> Self {
        let mut harness = three_columns();
        let slot = frame(harness.world(), DRAGGED);
        let left_slot = frame(harness.world(), LEFT);
        let press = grab(slot);
        let to = Origin::new(press.x + dx, TEST_DISPLAY_HEIGHT - 5);
        Self {
            harness,
            slot,
            left_slot,
            press,
            to,
        }
    }

    /// The window follows the pointer to the drop point in six steps, one
    /// frame apart, and its app widens it to the right at step `grow_at`,
    /// keeping its origin. Step 7 widens it at the drop point, after the
    /// window has rested there for a few frames. `reported` says whether the tap reports the
    /// pointer's moves.
    fn follow(&mut self, reported: bool, grow_at: i32) {
        for at in 1..=7 {
            if at <= 6 {
                let point = self.press + (self.to - self.press) * at / 6;
                if reported {
                    self.harness.world().write_message(mouse_dragged(point));
                }
                self.harness
                    .mock_state
                    .os_move_window(DRAGGED, self.slot.min + (point - self.press));
            }
            if at == 7 {
                self.harness.advance(FRAME * 3);
            }
            if at == grow_at {
                self.harness
                    .mock_state
                    .update_window(DRAGGED, |window| window.frame.max.x += GROWTH);
                self.harness
                    .world()
                    .write_message(Event::WindowResized { window_id: DRAGGED });
            }
            self.harness.advance(FRAME);
        }
    }

    fn column_width(&mut self) -> i32 {
        let world = self.harness.world();
        let entity = world
            .query::<(Entity, &Window)>()
            .iter(world)
            .find_map(|(entity, window)| (window.id() == DRAGGED).then_some(entity))
            .expect("dragged window exists");
        world.get::<Bounds>(entity).expect("bounds").0.x
    }

    /// Holds the button still at the drop point, longer than an app's own
    /// resize takes to settle.
    fn hold(&mut self) {
        self.harness.advance(ms(400));
        assert_eq!(
            self.column_width(),
            self.slot.width(),
            "the column keeps its width while the button is held"
        );
        assert_eq!(
            frame(self.harness.world(), LEFT),
            self.left_slot,
            "the left neighbour stays put while the button is held"
        );
    }

    fn release(&mut self) {
        self.harness.mock_state.set_left_button_down(false);
        self.harness.world().write_message(mouse_up(self.to));
        self.harness.advance(ms(1500));
    }

    fn assert_back_in_its_column(&mut self) {
        assert_eq!(
            frame(self.harness.world(), DRAGGED),
            self.slot,
            "the window is back in its column's slot at its width"
        );
        assert_eq!(
            self.column_width(),
            self.slot.width(),
            "the column keeps its width"
        );
        assert_eq!(
            frame(self.harness.world(), LEFT),
            self.left_slot,
            "the left neighbour does not move"
        );
    }
}

/// The engine never sees the press, as when the tap misses it, so it sees
/// only the window's moves and the app's resize.
fn unseen_drag(dx: i32, grow_at: i32) {
    let mut drop = HomeDrop::new(dx);
    drop.harness.mock_state.set_left_button_down(true);
    drop.harness.advance(FRAME);
    drop.follow(true, grow_at);
    drop.hold();
    drop.release();
    drop.assert_back_in_its_column();
}

#[test]
fn an_app_widening_its_window_during_an_unseen_drag_keeps_the_column_width() {
    unseen_drag(40, 3);
}

#[test]
fn an_app_widening_its_window_during_an_unseen_straight_down_drag_keeps_the_column_width() {
    unseen_drag(0, 3);
}

#[test]
fn an_app_widening_its_window_after_the_last_move_of_an_unseen_drag_keeps_the_column_width() {
    unseen_drag(40, 7);
}

#[test]
fn an_app_widening_its_window_during_a_press_that_never_confirms_keeps_the_column_width() {
    let mut drop = HomeDrop::new(40);
    press_at(&mut drop.harness, drop.press);
    drop.harness.advance(FRAME);
    drop.follow(false, 3);
    drop.hold();
    let held = IRect::from_corners(
        drop.slot.min + (drop.to - drop.press),
        drop.slot.max + (drop.to - drop.press) + IVec2::new(GROWTH, 0),
    );
    assert_eq!(
        frame(drop.harness.world(), DRAGGED),
        held,
        "the engine leaves a pressed window where the user holds it"
    );
    drop.release();
    drop.assert_back_in_its_column();
}

#[test]
fn an_app_widening_its_window_during_a_confirmed_drag_keeps_the_column_width() {
    let mut drop = HomeDrop::new(40);
    press_at(&mut drop.harness, drop.press);
    drop.harness.advance(FRAME);
    drop.follow(true, 3);
    assert!(
        matches!(drag_state(drop.harness.world()), DragState::Dragging(_)),
        "the drag confirms"
    );
    drop.hold();
    drop.release();
    drop.assert_back_in_its_column();
}
