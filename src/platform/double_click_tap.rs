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
//!
//! Each held press has its own token, which its `Event::MouseDown` carries,
//! so an answer to an earlier press never acts on a later one. A hold ends
//! by itself after [`HOLD_LIMIT`], so a release that never arrives, or an
//! engine that never answers, cannot keep the tap swallowing the mouse.

use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// How long a hold lasts at most: an engine that waits on a busy app
/// answers well within it, and a press held longer is dropped, so a lost
/// release or a stalled engine never keeps the mouse from the apps.
const HOLD_LIMIT: Duration = Duration::from_secs(2);

/// What the event tap should do with a mouse event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TapAction {
    /// Let the event reach both Bevy and the target app as usual.
    Pass,
    /// Let the event reach the target app without notifying Bevy (replayed).
    PassQuiet,
    /// Send `Event::MouseDown` to Bevy with this hold's token, but swallow
    /// the event from the app while waiting for `double_click_toggle` to
    /// call [`consume`] or [`replay`] with the token.
    HoldForEngine(u64),
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
    /// A held press the engine has yet to answer, since `since`. It lasts
    /// until the engine answers, the next press, or [`HOLD_LIMIT`]: an
    /// engine that answers late, as while it waits on a busy app, still
    /// gets its press replayed.
    Waiting {
        token: u64,
        since: Instant,
        point: CGPoint,
        flags: CGEventFlags,
        released: Option<(CGPoint, CGEventFlags)>,
    },
    /// A press made at `since` that the engine took, whose release is still
    /// to come, for [`HOLD_LIMIT`] from the press at most.
    Consumed {
        since: Instant,
    },
}

struct State {
    top_bars: Vec<(WinID, IRect)>,
    hold: Hold,
    /// The token of the last press held.
    token: u64,
}

impl State {
    const fn new() -> Self {
        Self {
            top_bars: Vec::new(),
            hold: Hold::Idle,
            token: 0,
        }
    }

    /// Ends a hold that has lasted [`HOLD_LIMIT`] by `now`.
    fn expire(&mut self, now: Instant) {
        let since = match self.hold {
            Hold::Idle => return,
            Hold::Waiting { since, .. } | Hold::Consumed { since } => since,
        };
        if now.saturating_duration_since(since) >= HOLD_LIMIT {
            debug!(hold = ?self.hold, "double-click hold ended at its time limit");
            self.hold = Hold::Idle;
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
        now: Instant,
    ) -> TapAction {
        self.expire(now);
        match event_type {
            CGEventType::LeftMouseDown => {
                if clicks == 2 && self.in_top_bar(window_id, point) {
                    self.token = self.token.wrapping_add(1);
                    self.hold = Hold::Waiting {
                        token: self.token,
                        since: now,
                        point,
                        flags,
                        released: None,
                    };
                    TapAction::HoldForEngine(self.token)
                } else {
                    self.hold = Hold::Idle;
                    TapAction::Pass
                }
            }
            CGEventType::LeftMouseDragged => match self.hold {
                Hold::Waiting { .. } | Hold::Consumed { .. } => TapAction::SwallowQuiet,
                Hold::Idle => TapAction::Pass,
            },
            CGEventType::LeftMouseUp => match &mut self.hold {
                Hold::Waiting { released, .. } => {
                    *released = Some((point, flags));
                    TapAction::SwallowQuiet
                }
                Hold::Consumed { .. } => {
                    self.hold = Hold::Idle;
                    TapAction::SwallowQuiet
                }
                Hold::Idle => TapAction::Pass,
            },
            _ => TapAction::Pass,
        }
    }

    /// Whether the press of `token` is the one held and waiting.
    fn waits(&self, token: u64) -> bool {
        matches!(self.hold, Hold::Waiting { token: held, .. } if held == token)
    }

    /// Takes the held press of `token` for the engine. Returns `false`
    /// when that press is not held, or no longer is.
    fn consume(&mut self, token: u64, now: Instant) -> bool {
        self.expire(now);
        if !self.waits(token) {
            return false;
        }
        if let Hold::Waiting {
            since, released, ..
        } = self.hold
        {
            // The hold's time limit runs from the press, whenever the
            // engine takes it.
            self.hold = if released.is_some() {
                Hold::Idle
            } else {
                Hold::Consumed { since }
            };
        }
        true
    }

    fn take_replay(&mut self, token: u64, now: Instant) -> Option<Replay> {
        self.expire(now);
        if !self.waits(token) {
            return None;
        }
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
    let action = state.handle(event_type, point, flags, window_id, clicks, Instant::now());
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

/// Marks the held title-bar double-click of `hold`, the token its press
/// carried, as handled by Miri (maximize/restore), so neither the press nor
/// its matching release reaches the app. Returns whether the engine may act
/// on the press: `true` for a press the tap did not hold, which reached the
/// engine alone, and for the press held now, and `false` for one the tap
/// gave up, at its time limit or for a newer press.
pub(crate) fn consume(hold: Option<u64>) -> bool {
    let Some(token) = hold else {
        return true;
    };
    STATE
        .lock()
        .is_ok_and(|mut state| state.consume(token, Instant::now()))
}

/// Re-posts the held title-bar double-click of `hold` (and its release if
/// already lifted) to macOS so a double-click on a control, URL bar, or tab
/// reaches the app. A press the tap did not hold, or no longer holds, is
/// left alone: it reached the app already, or was dropped.
pub(crate) fn replay(hold: Option<u64>) {
    let Some(token) = hold else {
        return;
    };
    let pending = STATE
        .lock()
        .ok()
        .and_then(|mut state| state.take_replay(token, Instant::now()));
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

    const BAR: IRect = IRect {
        min: IVec2::new(0, 30),
        max: IVec2::new(800, 120),
    };

    fn state() -> State {
        let mut state = State::new();
        state.top_bars = vec![(42, BAR)];
        state
    }

    /// Sends the tap one event of `kind` at `(x, y)` at `now`.
    fn event(
        state: &mut State,
        kind: CGEventType,
        (x, y): (f64, f64),
        window_id: Option<WinID>,
        clicks: i64,
        now: Instant,
    ) -> TapAction {
        state.handle(
            kind,
            CGPoint::new(x, y),
            CGEventFlags::empty(),
            window_id,
            clicks,
            now,
        )
    }

    /// The token of a press the tap held, or a panic.
    fn held(action: TapAction) -> u64 {
        let TapAction::HoldForEngine(token) = action else {
            panic!("the press is held, got {action:?}");
        };
        token
    }

    #[test]
    fn consumed_double_click_swallows_down_drag_and_up() {
        let mut state = state();
        let now = Instant::now();
        // Single click passes through.
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDown,
                (200.0, 50.0),
                Some(42),
                1,
                now
            ),
            TapAction::Pass
        );
        // Second click in top bar is held for the engine.
        let token = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        // Engine consumes it for maximize/restore.
        state.consume(token, now);
        // Matching drag and release are swallowed quietly.
        for kind in [CGEventType::LeftMouseDragged, CGEventType::LeftMouseUp] {
            assert_eq!(
                event(&mut state, kind, (201.0, 50.0), Some(42), 2, now),
                TapAction::SwallowQuiet
            );
        }
        // Subsequent release/click passes normally.
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDown,
                (200.0, 50.0),
                Some(42),
                1,
                now
            ),
            TapAction::Pass
        );
    }

    #[test]
    fn tap_to_click_replay_returns_both_down_and_up() {
        let mut state = state();
        let now = Instant::now();
        let token = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 70.0),
            Some(42),
            2,
            now,
        ));
        // Tap-to-click lifts 1 ms later before engine decides.
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseUp,
                (200.0, 70.0),
                Some(42),
                2,
                now
            ),
            TapAction::SwallowQuiet
        );
        let replayed = state.take_replay(token, now).expect("replay payload");
        assert!(replayed.released.is_some(), "includes lifted MouseUp");
        assert!(
            state.take_replay(token, now).is_none(),
            "cleared after replay"
        );
    }

    /// A real press reaches the HID tap before the window server names the
    /// window under it. Left to pass for that, a title-bar double-click
    /// reached the app, whose own zoom then fought the engine's (Helium's
    /// animated for 350 ms, 23 November 2026).
    #[test]
    fn a_double_click_without_a_window_is_held_by_its_place() {
        let mut state = state();
        let now = Instant::now();
        let token = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            None,
            2,
            now,
        ));
        state.consume(token, now);
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseUp,
                (200.0, 50.0),
                None,
                2,
                now
            ),
            TapAction::SwallowQuiet
        );
        // Outside every top bar, it passes.
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDown,
                (200.0, 300.0),
                None,
                2,
                now
            ),
            TapAction::Pass
        );
    }

    #[test]
    fn floating_or_content_double_click_passes_untouched() {
        let mut state = state();
        let now = Instant::now();
        // Different window (e.g. floating window or popover).
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDown,
                (200.0, 50.0),
                Some(99),
                2,
                now
            ),
            TapAction::Pass
        );
        // Below top bar on tiled window.
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDown,
                (200.0, 300.0),
                Some(42),
                2,
                now
            ),
            TapAction::Pass
        );
    }

    /// An answer that arrives late, for a press the user has since followed
    /// with another, acts on neither: the newer press waits for its own.
    #[test]
    fn a_late_answer_for_an_earlier_press_leaves_the_newer_one_held() {
        let mut state = state();
        let now = Instant::now();
        let first = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        let second = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (300.0, 50.0),
            Some(42),
            2,
            now,
        ));
        assert_ne!(first, second, "each press has its own token");

        state.consume(first, now);
        assert!(
            state.take_replay(first, now).is_none(),
            "the earlier press is gone"
        );
        assert!(state.waits(second), "the newer press still waits");
        let replayed = state.take_replay(second, now).expect("its own answer");
        assert!(
            (replayed.point.x - 300.0).abs() < f64::EPSILON,
            "the newer press is the one replayed"
        );
    }

    /// An answer for a press the tap gave up, at its time limit or for a
    /// newer press, tells the engine to leave the press alone.
    #[test]
    fn consuming_a_press_no_longer_held_says_so() {
        let mut state = state();
        let now = Instant::now();
        let first = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        let second = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        assert!(!state.consume(first, now), "the earlier press is gone");
        assert!(
            !state.consume(second, now + HOLD_LIMIT),
            "the newer one ended at the time limit"
        );
        let third = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        assert!(state.consume(third, now), "the press held now is taken");
    }

    /// A press the engine takes late ends its hold at the time limit from
    /// the press, not from when the engine took it.
    #[test]
    fn a_consumed_hold_ends_at_the_limit_from_its_press() {
        let mut state = state();
        let now = Instant::now();
        let token = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        let late = now + HOLD_LIMIT.saturating_sub(Duration::from_millis(100));
        assert!(state.consume(token, late), "the engine takes it in time");
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDragged,
                (210.0, 50.0),
                Some(42),
                2,
                late
            ),
            TapAction::SwallowQuiet
        );
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDragged,
                (220.0, 50.0),
                Some(42),
                2,
                now + HOLD_LIMIT
            ),
            TapAction::Pass,
            "the hold ends 2 s after the press"
        );
    }

    /// A press whose engine never answers ends its hold at the time limit,
    /// and the mouse reaches the apps again. A late answer then does
    /// nothing.
    #[test]
    fn a_press_never_answered_ends_its_hold_at_the_time_limit() {
        let mut state = state();
        let now = Instant::now();
        let token = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        let soon = now + HOLD_LIMIT / 2;
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDragged,
                (210.0, 50.0),
                Some(42),
                2,
                soon
            ),
            TapAction::SwallowQuiet,
            "within the limit, the hold goes on"
        );
        let late = now + HOLD_LIMIT;
        for kind in [CGEventType::LeftMouseDragged, CGEventType::LeftMouseUp] {
            assert_eq!(
                event(&mut state, kind, (220.0, 50.0), Some(42), 2, late),
                TapAction::Pass,
                "past the limit, {kind:?} reaches the app"
            );
        }
        assert!(
            state.take_replay(token, late).is_none(),
            "a late replay posts nothing"
        );
    }

    /// A press the engine took, whose release never arrives, stops holding
    /// the mouse back at the time limit.
    #[test]
    fn a_consumed_press_whose_release_is_lost_ends_at_the_time_limit() {
        let mut state = state();
        let now = Instant::now();
        let token = held(event(
            &mut state,
            CGEventType::LeftMouseDown,
            (200.0, 50.0),
            Some(42),
            2,
            now,
        ));
        state.consume(token, now);
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDragged,
                (210.0, 50.0),
                Some(42),
                2,
                now + HOLD_LIMIT / 2
            ),
            TapAction::SwallowQuiet
        );
        assert_eq!(
            event(
                &mut state,
                CGEventType::LeftMouseDragged,
                (220.0, 50.0),
                Some(42),
                1,
                now + HOLD_LIMIT
            ),
            TapAction::Pass,
            "the drag reaches the app"
        );
    }
}
