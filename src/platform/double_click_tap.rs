//! Event-tap gate for title-bar double-clicks on tiled (and command-maximized
//! floating) windows.
//!
//! When a user double-clicks a window's title bar, macOS and apps such as
//! Chromium also answer that double-click with their own native `performZoom:`
//! ~200 to 450 ms later, flashing the app's un-zoomed frame and desktop after
//! Miri's cover morph lifts.
//!
//! To stop native zoom without blocking double-clicking URL bars, tabs, or
//! buttons (and without calling slow Accessibility IPC inside the `CGEventTap`
//! callback), the tap holds `LeftMouseDown { clicks: 2 }` when it lands inside
//! a known tiled window's top bar (checked in nanoseconds against the engine's
//! published in-memory list). On the next frame (~2 to 8 ms),
//! `maximize::double_click_toggle` checks `control_at(point)` and calls either:
//!
//! - [`consume`]: the double-click toggled maximize/restore, so the held press
//!   and its matching release are swallowed and the app never runs native zoom.
//! - [`replay`]: the double-click hit a control, text field, or non-toggleable
//!   area, so the tap immediately re-posts the `clicks == 2` press (and its
//!   release if already lifted) tagged with [`DOUBLE_CLICK_TAG`] so it passes
//!   straight through to the app.

use std::sync::Mutex;

use bevy::math::{IRect, IVec2};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
};

use tracing::debug;

use crate::platform::WinID;

/// Magic `EventSourceUserData` marker on double-clicks re-posted by [`replay`],
/// distinct from `slice_tap::TAG`, so the tap lets them pass straight through
/// without re-intercepting or re-notifying the ECS.
const DOUBLE_CLICK_TAG: i64 = 0x5041_4E45_5255_4443;

/// What the event tap should do with a mouse event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TapAction {
    /// Let the event reach both Bevy and the target app as usual.
    Pass,
    /// Let the event reach the target app without notifying Bevy (replayed).
    PassQuiet,
    /// Send `Event::MouseDown` to Bevy, but swallow the event from the app
    /// while waiting for `double_click_toggle` to call [`consume`] or [`replay`].
    HoldForEngine,
    /// Swallow the event completely without notifying Bevy (drag or release of
    /// a held or consumed double-click).
    SwallowQuiet,
}

#[derive(Clone, Copy, Debug)]
struct Replay {
    point: CGPoint,
    flags: CGEventFlags,
    released: Option<(CGPoint, CGEventFlags)>,
}

#[derive(Clone, Copy, Debug)]
enum Hold {
    Idle,
    /// A held press the engine has yet to answer. It lasts until the engine
    /// answers or the next press: an engine that answers late, as while it
    /// waits on a busy app, still gets its press replayed, where a time
    /// limit would drop the press.
    Waiting {
        point: CGPoint,
        flags: CGEventFlags,
        released: Option<(CGPoint, CGEventFlags)>,
    },
    /// A press the engine took, whose release is still to come, however
    /// long the button is held.
    Consumed,
}

struct State {
    top_bars: Vec<(WinID, IRect)>,
    hold: Hold,
}

impl State {
    const fn new() -> Self {
        Self {
            top_bars: Vec::new(),
            hold: Hold::Idle,
        }
    }

    /// Whether `point` lies in a published top bar. The HID tap sees a
    /// press before the window server names the window under it, so a press
    /// without a window is matched by its place alone: the engine then finds
    /// the window, and replays a press that was another window's.
    fn in_top_bar(&self, window_id: Option<WinID>, point: CGPoint) -> bool {
        #[allow(clippy::cast_possible_truncation)]
        let p = IVec2::new(point.x.round() as i32, point.y.round() as i32);
        self.top_bars
            .iter()
            .any(|&(id, rect)| window_id.is_none_or(|under| under == id) && rect.contains(p))
    }

    fn handle(
        &mut self,
        event_type: CGEventType,
        point: CGPoint,
        flags: CGEventFlags,
        window_id: Option<WinID>,
        clicks: i64,
    ) -> TapAction {
        match event_type {
            CGEventType::LeftMouseDown => {
                if clicks == 2 && self.in_top_bar(window_id, point) {
                    self.hold = Hold::Waiting {
                        point,
                        flags,
                        released: None,
                    };
                    TapAction::HoldForEngine
                } else {
                    self.hold = Hold::Idle;
                    TapAction::Pass
                }
            }
            CGEventType::LeftMouseDragged => match self.hold {
                Hold::Waiting { .. } | Hold::Consumed => TapAction::SwallowQuiet,
                Hold::Idle => TapAction::Pass,
            },
            CGEventType::LeftMouseUp => match &mut self.hold {
                Hold::Waiting { released, .. } => {
                    *released = Some((point, flags));
                    TapAction::SwallowQuiet
                }
                Hold::Consumed => {
                    self.hold = Hold::Idle;
                    TapAction::SwallowQuiet
                }
                Hold::Idle => TapAction::Pass,
            },
            _ => TapAction::Pass,
        }
    }

    fn consume(&mut self) {
        if let Hold::Waiting { released, .. } = self.hold {
            self.hold = if released.is_some() {
                Hold::Idle
            } else {
                Hold::Consumed
            };
        }
    }

    fn take_replay(&mut self) -> Option<Replay> {
        let Hold::Waiting {
            point,
            flags,
            released,
            ..
        } = self.hold
        else {
            return None;
        };
        self.hold = Hold::Idle;
        Some(Replay {
            point,
            flags,
            released,
        })
    }
}

static STATE: Mutex<State> = Mutex::new(State::new());

/// Publishes the current screen-space top-bar rectangles of every tiled window
/// (and command-maximized floating window) for the event tap.
pub(crate) fn set_top_bars(top_bars: Vec<(WinID, IRect)>) {
    if let Ok(mut state) = STATE.lock() {
        state.top_bars = top_bars;
    }
}

/// Returns `true` when `event` was re-posted by [`replay`].
pub(crate) fn is_replayed(event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData) == DOUBLE_CLICK_TAG
}

/// Decides whether the event tap should hold, swallow, or pass `event`.
pub(crate) fn intercept(event_type: CGEventType, event: &CGEvent) -> TapAction {
    if is_replayed(event) {
        return TapAction::PassQuiet;
    }
    if !matches!(
        event_type,
        CGEventType::LeftMouseDown | CGEventType::LeftMouseDragged | CGEventType::LeftMouseUp
    ) {
        return TapAction::Pass;
    }
    let point = CGEvent::location(Some(event));
    let flags = CGEvent::flags(Some(event));
    let raw_id =
        CGEvent::integer_value_field(Some(event), CGEventField::MouseEventWindowUnderMousePointer);
    let window_id = WinID::try_from(raw_id).ok().filter(|id| *id != 0);
    let clicks = CGEvent::integer_value_field(Some(event), CGEventField::MouseEventClickState);
    let Ok(mut state) = STATE.lock() else {
        return TapAction::Pass;
    };
    let held = !matches!(state.hold, Hold::Idle);
    let action = state.handle(event_type, point, flags, window_id, clicks);
    // A multi-click, or any event of a held one: rare enough to log each.
    if event_type != CGEventType::LeftMouseDragged && (clicks > 1 || held) {
        debug!(
            ?event_type,
            clicks,
            ?window_id,
            x = point.x,
            y = point.y,
            ?action,
            "multi-click at the tap"
        );
    }
    action
}

/// Marks the held title-bar double-click as handled by Miri (maximize/restore),
/// so neither the press nor its matching release reaches the app.
pub(crate) fn consume() {
    if let Ok(mut state) = STATE.lock() {
        state.consume();
    }
}

/// Re-posts the held title-bar double-click (and its release if already lifted)
/// to macOS so a double-click on a control, URL bar, or tab reaches the app.
pub(crate) fn replay() {
    let pending = STATE.lock().ok().and_then(|mut state| state.take_replay());
    let Some(Replay {
        point,
        flags,
        released,
    }) = pending
    else {
        return;
    };
    post_click(true, point, flags);
    if let Some((up_point, up_flags)) = released {
        post_click(false, up_point, up_flags);
    }
}

/// Clears any in-flight hold, such as when macOS disables and re-enables the tap.
pub(crate) fn reset() {
    if let Ok(mut state) = STATE.lock() {
        state.hold = Hold::Idle;
    }
}

fn post_click(down: bool, point: CGPoint, flags: CGEventFlags) {
    let kind = if down {
        CGEventType::LeftMouseDown
    } else {
        CGEventType::LeftMouseUp
    };
    let Some(event) = CGEvent::new_mouse_event(None, kind, point, CGMouseButton::Left) else {
        return;
    };
    CGEvent::set_flags(Some(&event), flags);
    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 2);
    CGEvent::set_integer_value_field(
        Some(&event),
        CGEventField::EventSourceUserData,
        DOUBLE_CLICK_TAG,
    );
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> CGPoint {
        CGPoint::new(x, y)
    }

    #[test]
    fn consumed_double_click_swallows_down_drag_and_up() {
        let mut state = State::new();
        state.top_bars = vec![(42, IRect::new(0, 30, 800, 120))];

        // Single click passes through.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 50.0),
                CGEventFlags::empty(),
                Some(42),
                1,
            ),
            TapAction::Pass
        );
        // Second click in top bar is held for the engine.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 50.0),
                CGEventFlags::empty(),
                Some(42),
                2,
            ),
            TapAction::HoldForEngine
        );
        // Engine consumes it for maximize/restore.
        state.consume();
        // Matching drag and release are swallowed quietly.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDragged,
                pt(201.0, 50.0),
                CGEventFlags::empty(),
                Some(42),
                2,
            ),
            TapAction::SwallowQuiet
        );
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseUp,
                pt(201.0, 50.0),
                CGEventFlags::empty(),
                Some(42),
                2,
            ),
            TapAction::SwallowQuiet
        );
        // Subsequent release/click passes normally.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 50.0),
                CGEventFlags::empty(),
                Some(42),
                1,
            ),
            TapAction::Pass
        );
    }

    #[test]
    fn tap_to_click_replay_returns_both_down_and_up() {
        let mut state = State::new();
        state.top_bars = vec![(42, IRect::new(0, 30, 800, 120))];

        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 70.0),
                CGEventFlags::empty(),
                Some(42),
                2,
            ),
            TapAction::HoldForEngine
        );
        // Tap-to-click lifts 1 ms later before engine decides.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseUp,
                pt(200.0, 70.0),
                CGEventFlags::empty(),
                Some(42),
                2,
            ),
            TapAction::SwallowQuiet
        );
        let replayed = state.take_replay().expect("replay payload");
        assert!(replayed.released.is_some(), "includes lifted MouseUp");
        assert!(state.take_replay().is_none(), "cleared after replay");
    }

    /// A real press reaches the HID tap before the window server names the
    /// window under it. Left to pass for that, a title-bar double-click
    /// reached the app, whose own zoom then fought the engine's (Helium's
    /// animated for 350 ms, 23 November 2026).
    #[test]
    fn a_double_click_without_a_window_is_held_by_its_place() {
        let mut state = State::new();
        state.top_bars = vec![(42, IRect::new(0, 30, 800, 120))];
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 50.0),
                CGEventFlags::empty(),
                None,
                2,
            ),
            TapAction::HoldForEngine
        );
        state.consume();
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseUp,
                pt(200.0, 50.0),
                CGEventFlags::empty(),
                None,
                2,
            ),
            TapAction::SwallowQuiet
        );
        // Outside every top bar, it passes.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 300.0),
                CGEventFlags::empty(),
                None,
                2,
            ),
            TapAction::Pass
        );
    }

    #[test]
    fn floating_or_content_double_click_passes_untouched() {
        let mut state = State::new();
        state.top_bars = vec![(42, IRect::new(0, 30, 800, 120))];

        // Different window (e.g. floating window or popover).
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 50.0),
                CGEventFlags::empty(),
                Some(99),
                2,
            ),
            TapAction::Pass
        );
        // Below top bar on tiled window.
        assert_eq!(
            state.handle(
                CGEventType::LeftMouseDown,
                pt(200.0, 300.0),
                CGEventFlags::empty(),
                Some(42),
                2,
            ),
            TapAction::Pass
        );
    }
}
