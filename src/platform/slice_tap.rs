//! What the event tap does with the pointer on an edge slice panel. The
//! window a panel shows is off screen, where no event reaches its views, so
//! the tap holds a press on a panel back. The engine posts it again once the
//! window is in view, and the tap then lets that press and the rest of the
//! gesture through.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use bevy::math::IRect;
use objc2::MainThreadMarker;
use objc2_app_kit::NSWindow;
use objc2_core_foundation::{CGPoint, CGVector};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
};
use objc2_foundation::NSPoint;
use tracing::{debug, warn};

use crate::events::MouseButton;
use crate::overlay::primary_screen_height;
use crate::platform::WinID;
use crate::types::commands::Side;

/// Marks the user data of the events the engine posts, with the press id in
/// the low 32 bits.
const TAG: i64 = 0x6d69_7269 << 32;
const TAG_MASK: i64 = !0xffff_ffff;

/// How far inside the edge its slice shows a press lands. The outer points
/// of a window are its resize border, where a press reaches no view and a
/// double click stretches the window.
const EDGE_INSET: f64 = 12.0;

/// How long a held press waits for its window to reach the pressed point,
/// in the tap and in the engine. Focus and the scroll stay when it gives up.
pub(crate) const GIVE_UP: Duration = Duration::from_millis(1500);

/// A painted slice panel, and the slice of the window it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlicePanel {
    pub panel: WinID,
    pub window: WinID,
    pub on_screen: IRect,
    /// `on_screen` in the window's points, from the top left corner of its
    /// shown frame.
    pub in_window: IRect,
    pub side: Side,
}

impl SlicePanel {
    pub fn contains(&self, point: CGPoint) -> bool {
        let (min, max) = (self.on_screen.min.as_dvec2(), self.on_screen.max.as_dvec2());
        (min.x..max.x).contains(&point.x) && (min.y..max.y).contains(&point.y)
    }
}

static SLICE_PANELS: LazyLock<ArcSwap<Vec<SlicePanel>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replaces the panels the tap and the window lookups know of.
pub(crate) fn set_slice_panels(panels: Vec<SlicePanel>) {
    SLICE_PANELS.store(Arc::new(panels));
}

pub(crate) fn is_slice_panel(window: WinID) -> bool {
    SLICE_PANELS
        .load()
        .iter()
        .any(|panel| panel.panel == window)
}

/// The panel that a press at `point` would reach, as the window server
/// decides it.
fn panel_at(point: CGPoint) -> Option<SlicePanel> {
    let panels = SLICE_PANELS.load();
    let panel = panels.iter().find(|panel| panel.contains(point))?;
    let mtm = MainThreadMarker::new()?;
    let hit = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(
        NSPoint::new(point.x, primary_screen_height(mtm) - point.y),
        0,
        mtm,
    );
    (WinID::try_from(hit).ok() == Some(panel.panel)).then_some(*panel)
}

/// A press the tap held back on a panel, for the engine to post again once
/// the window is in view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SlicePress {
    /// Tells the posted copy of this press apart from any other.
    pub id: u32,
    pub window_id: WinID,
    /// The pressed point in the window, from the top left corner of its
    /// shown frame.
    pub at: CGPoint,
    pub side: Side,
    pub button: MouseButton,
    pub flags: CGEventFlags,
}

impl SlicePress {
    /// Where the press lands in its window once the window shows `shown`:
    /// on the pressed row, at least [`EDGE_INSET`] inside the edge the slice
    /// showed, or in the middle of a narrower window.
    pub fn point_in(&self, shown: IRect) -> CGPoint {
        let (min, max) = (f64::from(shown.min.x), f64::from(shown.max.x));
        let inset = EDGE_INSET.min((max - min) / 2.0);
        let x = min + self.at.x;
        let x = match self.side {
            Side::West => x.min(max - inset),
            Side::East => x.max(min + inset),
        };
        CGPoint::new(x, f64::from(shown.min.y) + self.at.y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mouse {
    Down(MouseButton),
    Dragged(MouseButton),
    Up(MouseButton),
    Moved,
    Scrolled,
}

impl Mouse {
    pub fn of(kind: CGEventType) -> Option<Self> {
        Some(match kind {
            CGEventType::LeftMouseDown => Self::Down(MouseButton::Left),
            CGEventType::RightMouseDown => Self::Down(MouseButton::Right),
            CGEventType::LeftMouseDragged => Self::Dragged(MouseButton::Left),
            CGEventType::RightMouseDragged => Self::Dragged(MouseButton::Right),
            CGEventType::LeftMouseUp => Self::Up(MouseButton::Left),
            CGEventType::RightMouseUp => Self::Up(MouseButton::Right),
            CGEventType::MouseMoved => Self::Moved,
            CGEventType::ScrollWheel => Self::Scrolled,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pointer {
    pub mouse: Mouse,
    pub point: CGPoint,
    pub flags: CGEventFlags,
    /// When the user made the event, or when the tap read one the engine
    /// posted, since startup.
    pub time: Duration,
    /// The press id of an event the engine posted. `None` for the user's.
    pub tag: Option<u32>,
    /// The panel under a press, move, or scroll of the user's.
    pub panel: Option<SlicePanel>,
}

impl Pointer {
    pub fn read(mouse: Mouse, event: &CGEvent) -> Self {
        let point = CGEvent::location(Some(event));
        let data = CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData);
        let tag = (data & TAG_MASK == TAG)
            .then(|| u32::try_from(data & !TAG_MASK).ok())
            .flatten();
        let hit_tested =
            tag.is_none() && matches!(mouse, Mouse::Down(_) | Mouse::Moved | Mouse::Scrolled);
        let time = if tag.is_some() {
            uptime()
        } else {
            Duration::from_nanos(CGEvent::timestamp(Some(event)))
        };
        Self {
            mouse,
            point,
            flags: CGEvent::flags(Some(event)),
            time,
            tag,
            panel: hit_tested.then(|| panel_at(point)).flatten(),
        }
    }
}

/// The clock of the user's event timestamps.
fn uptime() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &raw mut now) };
    Duration::new(
        u64::try_from(now.tv_sec).unwrap_or_default(),
        u32::try_from(now.tv_nsec).unwrap_or_default(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Lets the event through, and tells the engine as usual.
    Pass,
    /// Lets the event through to the panel, and tells the engine nothing.
    Quiet,
    /// Drops the event, and tells the engine nothing.
    Swallow,
    /// Lets the event through at another point, and tells the engine as
    /// usual.
    Move(CGPoint),
    /// Drops the press, and sends it to the engine to post again.
    Hold(SlicePress),
    /// Lets the posted press through and tells the engine as usual, then
    /// posts the release the tap held back.
    PassAndRelease { id: u32, button: MouseButton },
}

/// Where the gesture that began on a panel is. Only the tap changes it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Gesture {
    #[default]
    Idle,
    /// The tap holds `button` back until the engine posts press `id` or
    /// `deadline` passes, and notes whether the user dragged it meanwhile.
    /// The user pressed at `pressed` on the screen.
    Holding {
        id: u32,
        button: MouseButton,
        dragged: bool,
        pressed: CGPoint,
        deadline: Duration,
    },
    /// As `Holding`, after the user released the button.
    Released {
        id: u32,
        button: MouseButton,
        dragged: bool,
        deadline: Duration,
    },
    /// The posted press went through, and the user's drags and release of
    /// its button now reach the app.
    Passing(Passed),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Passed {
    button: MouseButton,
    drags: u32,
    /// When the posted press went through. The drags and release the user
    /// made before then, with the pointer still on the panel, reach the app
    /// moved `by` as far as the press.
    posted_at: Duration,
    by: CGVector,
}

#[derive(Debug, Default)]
pub(crate) struct SliceTap {
    gesture: Gesture,
    presses: u32,
    /// The buttons of the presses the tap dropped, whose drags and releases
    /// it drops too.
    dropped: Vec<MouseButton>,
    /// When the last held press stopped waiting for its window. The user's
    /// presses made before then can reach the tap after it.
    waited_until: Duration,
}

impl SliceTap {
    pub fn handle(&mut self, pointer: &Pointer) -> Verdict {
        self.expire(pointer.time);
        if let Some(id) = pointer.tag {
            return self.posted(pointer, id);
        }
        match (pointer.mouse, self.gesture) {
            (Mouse::Down(button), gesture)
                if pointer.time < self.waited_until
                    || matches!(gesture, Gesture::Holding { .. } | Gesture::Released { .. }) =>
            {
                debug!(
                    "edge slice press dropped: pressed at ({:.0}, {:.0}) while a press waits for its window",
                    pointer.point.x, pointer.point.y
                );
                self.dropped.push(button);
                Verdict::Swallow
            }
            (Mouse::Dragged(button) | Mouse::Up(button), _) if self.dropped.contains(&button) => {
                if let Mouse::Up(_) = pointer.mouse {
                    self.dropped.retain(|dropped| *dropped != button);
                }
                Verdict::Swallow
            }
            (Mouse::Down(button), _) => self.press(pointer, button),
            (
                Mouse::Dragged(dragged),
                Gesture::Holding {
                    id,
                    button,
                    pressed,
                    deadline,
                    ..
                },
            ) if dragged == button => {
                self.gesture = Gesture::Holding {
                    id,
                    button,
                    dragged: true,
                    pressed,
                    deadline,
                };
                Verdict::Swallow
            }
            (
                Mouse::Up(released),
                Gesture::Holding {
                    id,
                    button,
                    dragged,
                    deadline,
                    ..
                },
            ) if released == button => {
                self.gesture = Gesture::Released {
                    id,
                    button,
                    dragged,
                    deadline,
                };
                Verdict::Swallow
            }
            (Mouse::Dragged(moved) | Mouse::Up(moved), Gesture::Passing(passed))
                if moved == passed.button =>
            {
                self.passing(pointer, passed)
            }
            (Mouse::Moved, _) if pointer.panel.is_some() => Verdict::Quiet,
            (Mouse::Scrolled, _) if pointer.panel.is_some() => Verdict::Swallow,
            _ => Verdict::Pass,
        }
    }

    /// Lets go of a held press whose window has not arrived by its deadline.
    /// The tap drops the rest of a press the user still holds.
    fn expire(&mut self, now: Duration) {
        let (Gesture::Holding { id, deadline, .. } | Gesture::Released { id, deadline, .. }) =
            self.gesture
        else {
            return;
        };
        if now < deadline {
            return;
        }
        if let Gesture::Holding { button, .. } = self.gesture {
            self.dropped.push(button);
        }
        debug!("edge slice press expired: press {id} waited {GIVE_UP:?} for its window");
        self.waited_until = deadline;
        self.gesture = Gesture::Idle;
    }

    /// A press of the user's: held back on a panel, let through elsewhere.
    fn press(&mut self, pointer: &Pointer, button: MouseButton) -> Verdict {
        let (x, y) = (pointer.point.x, pointer.point.y);
        let Some(panel) = pointer.panel else {
            self.gesture = Gesture::Idle;
            return Verdict::Pass;
        };
        self.presses = self.presses.wrapping_add(1);
        let id = self.presses;
        self.gesture = Gesture::Holding {
            id,
            button,
            dragged: false,
            pressed: pointer.point,
            deadline: pointer.time + GIVE_UP,
        };
        debug!(
            "edge slice press swallowed: press {id} window {} at ({x:.0}, {y:.0})",
            panel.window
        );
        Verdict::Hold(SlicePress {
            id,
            window_id: panel.window,
            at: CGPoint::new(
                x - f64::from(panel.on_screen.min.x) + f64::from(panel.in_window.min.x),
                y - f64::from(panel.on_screen.min.y) + f64::from(panel.in_window.min.y),
            ),
            side: panel.side,
            button,
            flags: pointer.flags,
        })
    }

    /// A drag or release of the posted press.
    fn passing(&mut self, pointer: &Pointer, passed: Passed) -> Verdict {
        let (x, y) = (pointer.point.x, pointer.point.y);
        let early = pointer.time < passed.posted_at;
        let to = if early {
            CGPoint::new(x + passed.by.dx, y + passed.by.dy)
        } else {
            pointer.point
        };
        if let Mouse::Up(_) = pointer.mouse {
            if early {
                debug!(
                    "edge slice release moved with its press: made {:?} before it, at ({x:.0}, {y:.0})",
                    passed.posted_at.saturating_sub(pointer.time)
                );
            }
            debug!(
                "edge slice gesture passed through: {} drags, released at ({:.0}, {:.0})",
                passed.drags, to.x, to.y
            );
            self.gesture = Gesture::Idle;
        } else {
            self.gesture = Gesture::Passing(Passed {
                drags: passed.drags.saturating_add(1),
                ..passed
            });
        }
        if early {
            Verdict::Move(to)
        } else {
            Verdict::Pass
        }
    }

    /// An event the engine posted. Only the press the tap waits for gets
    /// through: a press it no longer waits for would reach the app with no
    /// release to follow.
    fn posted(&mut self, pointer: &Pointer, tag: u32) -> Verdict {
        let Mouse::Down(posted) = pointer.mouse else {
            return Verdict::Pass;
        };
        let (x, y) = (pointer.point.x, pointer.point.y);
        match self.gesture {
            Gesture::Holding {
                id,
                button,
                pressed,
                ..
            } if id == tag && button == posted => {
                debug!("edge slice press delivered: press {id} at ({x:.0}, {y:.0}), button held");
                self.waited_until = pointer.time;
                self.gesture = Gesture::Passing(Passed {
                    button,
                    drags: 0,
                    posted_at: pointer.time,
                    by: CGVector::new(x - pressed.x, y - pressed.y),
                });
                Verdict::Pass
            }
            Gesture::Released {
                id,
                button,
                dragged: true,
                ..
            } if id == tag && button == posted => {
                debug!(
                    "edge slice press dropped: press {id} was a drag that ended before its window arrived"
                );
                self.waited_until = pointer.time;
                self.gesture = Gesture::Idle;
                Verdict::Swallow
            }
            Gesture::Released { id, button, .. } if id == tag && button == posted => {
                debug!(
                    "edge slice press delivered: press {id} at ({x:.0}, {y:.0}), with its release"
                );
                self.waited_until = pointer.time;
                self.gesture = Gesture::Idle;
                Verdict::PassAndRelease { id, button }
            }
            _ => {
                debug!("edge slice press dropped: press {tag} is no longer held");
                Verdict::Swallow
            }
        }
    }

    /// Forgets the gesture, as after macOS switched the tap off and it may
    /// have missed events.
    pub fn reset(&mut self) {
        self.gesture = Gesture::Idle;
        self.dropped.clear();
    }
}

/// Posts press `id`, or its release, where a real mouse's events enter, so
/// the window server sends it to the window under `point` and moves the
/// pointer there.
pub(crate) fn post(id: u32, button: MouseButton, down: bool, point: CGPoint, flags: CGEventFlags) {
    let (kind, cg_button) = match (button, down) {
        (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
        (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
    };
    let Some(event) = CGEvent::new_mouse_event(None, kind, point, cg_button) else {
        warn!("edge slice press {id} not posted: no mouse event");
        return;
    };
    CGEvent::set_flags(Some(&event), flags);
    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 1);
    CGEvent::set_integer_value_field(
        Some(&event),
        CGEventField::EventSourceUserData,
        TAG | i64::from(id),
    );
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}
