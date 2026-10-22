//! Dragging a tiled window by its title bar into a column, or out to a new
//! column, on its own display or another display.
//!
//! The pieces are kept apart so each one can be tested on its own:
//!
//! - [`step`] is the pure drag state machine. It turns one input into the
//!   next [`DragState`] and the effects to apply.
//! - [`hit_test`] is the pure drop rule. It turns a [`LayoutSnapshot`] and a
//!   pointer into a [`DropTarget`].
//! - [`drag_events`] feeds platform events to [`step`] and applies its
//!   effects to the [`LayoutStrip`].
//!
mod tiling;
#[cfg(test)]
pub(crate) use tiling::is_macos_tile;
pub(crate) use tiling::{NativeFrame, NativeResizeStart, PointerResize, TilingRestore, tiled_in};

use std::cell::OnceCell;
use std::ops::Range;
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate, Update};
use bevy::ecs::change_detection::{DetectChanges as _, DetectChangesMut as _};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::{
    not, on_message, resource_changed, resource_exists, resource_exists_and_changed,
};
use bevy::ecs::schedule::{IntoScheduleConfigs as _, SystemCondition as _};
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use bevy::math::{BVec2, IRect, IVec2};
use bevy::time::Time;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::debug;

use crate::config::Config;
use crate::ecs::display::{DisplayMoves, Placement};
use crate::ecs::layout::{Column, FALLBACK_TOP_BAR_HEIGHT, LayoutStrip, effective_stack_limit};
use crate::ecs::native_membership::NativeMembership;
use crate::ecs::{
    Bounds, FocusedMarker, FullWidthMarker, Initializing, LayoutPosition, ManualStripOffset,
    Position, PresentedOrigin, RepositionMarker, Scrolling, SpawnCommandsExt, TopBarHeight,
    Unmanaged,
};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Window, WindowManager, origin_from};

/// How far the pointer must move from the press before a drag can start.
const DRAG_THRESHOLD: i32 = 4;

/// How far the window's move may differ from the pointer's on each axis and
/// still count as the window following the pointer.
const MOVE_TOLERANCE: i32 = 8;

/// How far a window held at a screen edge must move on its free axis to
/// confirm a drag from that axis alone. Three times [`MOVE_TOLERANCE`], so the
/// match cannot come from the tolerance band alone: a window that barely
/// moves, or moves on its own, stays within that band and confirms nothing.
const STRONG_MOVE: i32 = 3 * MOVE_TOLERANCE;

/// How long the pointer may move past the threshold with no matching window
/// move before the press stops being a possible drag.
const CONFIRM_WINDOW: Duration = Duration::from_millis(250);

/// How long after a drop the dropped window stays watched.
const SETTLE_WINDOW: Duration = Duration::from_millis(600);

/// The most pointer samples a press keeps to match a late window move.
const MAX_SAMPLES: usize = 32;

/// How long the window server must keep reporting the left button up before
/// a press or drag counts as released. The tap can deliver the release a
/// little after the button state changes, so a shorter wait would cancel
/// drops still in flight.
pub(crate) const RELEASE_GRACE: Duration = Duration::from_millis(250);

/// The longest a press that never became a drag holds its window, even while
/// the button reads as down. A real drag moves its window within the confirm
/// window, so a hold this long is a stuck button state or a lost release.
/// Ten seconds leaves room for a user who holds a title bar before moving,
/// while a stuck button state still cannot hold a window forever.
const PRESS_LIMIT: Duration = Duration::from_secs(10);

pub(crate) type DisplayId = CGDirectDisplayID;

/// Where the windows of the dragged window's layout are, in screen points,
/// taken from the layout rather than live frames, so the dragged window does
/// not hide its own slot.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LayoutSnapshot {
    pub managed_strip_displays: Vec<DisplaySnap>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplaySnap {
    pub id: DisplayId,
    /// The whole display, which decides which display the pointer is on.
    pub bounds: IRect,
    /// The part of the display the layout uses.
    pub viewport: IRect,
    /// The strip's columns, left to right, including ones scrolled away.
    pub columns: Vec<ColumnSnap>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ColumnSnap {
    /// The column's index in its strip.
    pub index: usize,
    pub x: Range<i32>,
    /// One band per stack item, top to bottom.
    pub bands: Vec<Band>,
    pub kind: ColumnKind,
}

/// The visible part of one stack item: from its top bar down to the next
/// item's top, or to the bottom of the viewport for the last one. A native
/// tab group is one item, so it lists every tab.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Band {
    pub windows: Vec<Entity>,
    pub y: Range<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnKind {
    Tiled,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The column already holds as many windows as the stack limit allows.
    StackFull,
    /// A fullscreen column holds only its own window.
    Fullscreen,
}

/// What a drop at the pointer does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropTarget {
    /// Into column `column`, `slot` items from the top among its other items.
    IntoColumn {
        display: DisplayId,
        column: usize,
        slot: usize,
    },
    /// To a new column at gap `index`, where gap 0 is before the first column.
    NewColumn { display: DisplayId, index: usize },
    /// Nowhere: the window returns to its place.
    Refused { reason: Refusal, display: DisplayId },
    /// Back to its own place, which changes nothing.
    Home,
}

impl ColumnSnap {
    fn holds(&self, entity: Entity) -> bool {
        self.bands.iter().any(|band| band.windows.contains(&entity))
    }
}

/// The drop target for `dragged` with the pointer at `pointer`. `limit` gives
/// the stack limit of each display.
pub(crate) fn hit_test(
    snapshot: &LayoutSnapshot,
    dragged: Entity,
    pointer: IVec2,
    limit: impl Fn(DisplayId) -> usize,
) -> DropTarget {
    let Some(display) = snapshot
        .managed_strip_displays
        .iter()
        .find(|display| display.bounds.contains(pointer))
    else {
        return DropTarget::Home;
    };
    // A window alone in its column, or a native tab group alone in it,
    // already is a column of its own.
    let lone = display
        .columns
        .iter()
        .find(|column| column.holds(dragged) && column.bands.len() == 1)
        .map(|column| column.index);

    let target = match display
        .columns
        .iter()
        .find(|column| column.x.contains(&pointer.x))
    {
        Some(column) => column_target(display.id, column, dragged, pointer, limit(display.id)),
        None => DropTarget::NewColumn {
            display: display.id,
            index: gap_at(&display.columns, pointer.x),
        },
    };
    match (target, lone) {
        (DropTarget::IntoColumn { column, .. }, Some(own)) if column == own => DropTarget::Home,
        (DropTarget::NewColumn { index, .. }, Some(own)) if index == own || index == own + 1 => {
            DropTarget::Home
        }
        (target, _) => target,
    }
}

/// The drop target with the pointer over `column`.
fn column_target(
    display: DisplayId,
    column: &ColumnSnap,
    dragged: Entity,
    pointer: IVec2,
    limit: usize,
) -> DropTarget {
    let quarter = (column.x.end - column.x.start) / 4;
    if pointer.x < column.x.start + quarter {
        return DropTarget::NewColumn {
            display,
            index: column.index,
        };
    }
    if pointer.x >= column.x.end - quarter {
        return DropTarget::NewColumn {
            display,
            index: column.index + 1,
        };
    }
    let own = column.holds(dragged);
    if column.kind == ColumnKind::Fullscreen && !own {
        return DropTarget::Refused {
            reason: Refusal::Fullscreen,
            display,
        };
    }
    // A reorder within its own stack never adds a window.
    if !own && column.bands.len() + 1 > limit {
        return DropTarget::Refused {
            reason: Refusal::StackFull,
            display,
        };
    }
    let slot = column
        .bands
        .iter()
        .filter(|band| !band.windows.contains(&dragged))
        .filter(|band| i32::midpoint(band.y.start, band.y.end) < pointer.y)
        .count();
    DropTarget::IntoColumn {
        display,
        column: column.index,
        slot,
    }
}

/// The gap a pointer at `x` in empty space opens: the nearest strip end,
/// or the gap it sits in between two columns.
fn gap_at(columns: &[ColumnSnap], x: i32) -> usize {
    columns
        .iter()
        .find(|column| x < column.x.start)
        .map_or_else(
            || columns.last().map_or(0, |column| column.index + 1),
            |column| column.index,
        )
}

/// How thick an insertion line is, in points.
pub(crate) const INSERTION_LINE: i32 = 3;

/// The size of the refusal symbol, in points.
pub(crate) const REFUSAL_SYMBOL: i32 = 22;

/// Where the refusal symbol sits from the pointer, down and to the right,
/// like the badge of a cursor.
pub(crate) const REFUSAL_OFFSET: IVec2 = IVec2::new(14, 14);

/// What the drag overlay draws, in screen points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayShape {
    /// An insertion line where a release would put the window.
    Line(IRect),
    /// The `nosign` symbol in this square, beside the pointer, for a drop
    /// that would be refused.
    Refusal(IRect),
}

/// What the overlay shows for `state`: where a release would land, the
/// refusal symbol, or nothing.
pub(crate) fn overlay_shape(state: &DragState, snapshot: &LayoutSnapshot) -> Option<OverlayShape> {
    let DragState::Dragging(drag) = state else {
        return None;
    };
    let display_of = |id: DisplayId| {
        snapshot
            .managed_strip_displays
            .iter()
            .find(|display| display.id == id)
    };
    match drag.target {
        DropTarget::IntoColumn {
            display,
            column,
            slot,
        } => {
            let display = display_of(display)?;
            let column = display.columns.iter().find(|snap| snap.index == column)?;
            let mut others = column
                .bands
                .iter()
                .filter(|band| !band.windows.contains(&drag.window));
            // The gap above the `slot`-th other item, or below the last.
            let y = match others.nth(slot) {
                Some(band) => band.y.start,
                None => column.bands.last()?.y.end,
            };
            // Only the part of the column on screen.
            let x = column.x.start.max(display.viewport.min.x)
                ..column.x.end.min(display.viewport.max.x);
            if x.is_empty() {
                return None;
            }
            let y = line_across(y, display.viewport.min.y..display.viewport.max.y)?;
            Some(OverlayShape::Line(IRect::new(
                x.start, y.start, x.end, y.end,
            )))
        }
        DropTarget::NewColumn { display, index } => {
            let display = display_of(display)?;
            let at = |index| display.columns.iter().find(|snap| snap.index == index);
            let before = index.checked_sub(1).and_then(at).map(|snap| snap.x.end);
            let after = at(index).map(|snap| snap.x.start);
            let x = match (before, after) {
                (Some(before), Some(after)) => i32::midpoint(before, after),
                (Some(edge), None) | (None, Some(edge)) => edge,
                (None, None) if display.columns.is_empty() => display.viewport.min.x,
                (None, None) => return None,
            };
            let x = line_across(x, display.viewport.min.x..display.viewport.max.x)?;
            let viewport = display.viewport;
            Some(OverlayShape::Line(IRect::new(
                x.start,
                viewport.min.y,
                x.end,
                viewport.max.y,
            )))
        }
        DropTarget::Refused { .. } => {
            let corner = drag.pointer + REFUSAL_OFFSET;
            Some(OverlayShape::Refusal(IRect::from_corners(
                corner,
                corner + IVec2::splat(REFUSAL_SYMBOL),
            )))
        }
        DropTarget::Home => None,
    }
}

/// The span of an insertion line centered on `at`, moved inside `within` so a
/// line at a screen edge stays whole and on screen. `None` when `within` is
/// too short to hold a whole line.
fn line_across(at: i32, within: Range<i32>) -> Option<Range<i32>> {
    if within.end - within.start < INSERTION_LINE {
        return None;
    }
    let start = (at - INSERTION_LINE / 2).clamp(within.start, within.end - INSERTION_LINE);
    Some(start..start + INSERTION_LINE)
}

/// The window a press landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PressedWindow {
    pub entity: Entity,
    pub frame: IRect,
    pub top_bar_height: i32,
    /// Whether the window is in a strip, rather than floating.
    pub tiled: bool,
    /// Per axis, whether the window's frame touches a display edge on that
    /// axis, so macOS can hold the window on it while the pointer moves.
    pub screen_edges: BVec2,
}

/// One thing that happened, as the drag state machine sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragInput {
    Down {
        point: IVec2,
        button: MouseButton,
        window: Option<PressedWindow>,
    },
    /// The pointer moved with the left button held.
    Dragged {
        point: IVec2,
    },
    /// The left button was released.
    Up {
        point: IVec2,
    },
    /// The pointer moved with no button held.
    Moved {
        point: IVec2,
    },
    /// A window moved, and `frame` is where it is now.
    WindowMoved {
        window: Entity,
        frame: IRect,
    },
    WindowResized {
        window: Entity,
    },
    /// A periodic check while a window is held, with whether the window
    /// server sees the left button down.
    Tick {
        button_down: bool,
    },
    /// A window is no longer managed, or no longer exists.
    WindowGone {
        window: Entity,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Press {
    pub window: Entity,
    pub point: IVec2,
    /// The window's frame when it was pressed.
    pub frame: IRect,
    /// Recent pointer positions, so a window move that lags behind the
    /// pointer still matches where the pointer was.
    pub samples: Vec<IVec2>,
    /// When the pointer first moved past the drag threshold.
    pub moved_at: Option<Duration>,
    /// How far the window had moved from `frame` at its latest move.
    pub window_moved: Option<IVec2>,
    /// When the button first read as up, if it reads up now.
    pub button_up_since: Option<Duration>,
    /// Per axis, whether a display edge can hold the window on that axis.
    pub screen_edges: BVec2,
    /// When the button went down.
    pub at: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Drag {
    pub window: Entity,
    pub pointer: IVec2,
    /// What a release here would do.
    pub target: DropTarget,
    /// When the button first read as up, if it reads up now.
    pub button_up_since: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settle {
    pub window: Entity,
    pub deadline: Duration,
}

/// Where a title-bar drag of a tiled window stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub(crate) enum DragState {
    #[default]
    Idle,
    /// The left button went down in a tiled window's top bar.
    Pressed(Press),
    /// The button was released before its window move was confirmed.
    Released { press: Press, point: IVec2 },
    /// The window did not follow the pointer, and the button may still be
    /// down. macOS may still be dragging the window, so the press keeps it
    /// until the release, and only then does the window go back to its slot.
    Unconfirmed(Press),
    /// The window follows the pointer.
    Dragging(Drag),
    /// A drop just happened.
    Settling(Settle),
}

impl DragState {
    /// Whether a press or drag holds a window, so nothing else should move
    /// it.
    pub(crate) fn holds_window(&self) -> bool {
        matches!(
            self,
            Self::Pressed(_) | Self::Released { .. } | Self::Unconfirmed(_) | Self::Dragging(_)
        )
    }

    /// The window a press, drag, or settling drop is about.
    pub(crate) fn window(&self) -> Option<Entity> {
        match self {
            Self::Idle => None,
            Self::Pressed(press) | Self::Released { press, .. } | Self::Unconfirmed(press) => {
                Some(press.window)
            }
            Self::Dragging(drag) => Some(drag.window),
            Self::Settling(settle) => Some(settle.window),
        }
    }

    /// The window just dropped, while it settles.
    pub(crate) fn settling_window(&self) -> Option<Entity> {
        match self {
            Self::Settling(settle) => Some(settle.window),
            _ => None,
        }
    }
}

/// Draws the drag feedback on screen. The app uses
/// [`DragOverlay`](crate::overlay::DragOverlay), and tests record the calls.
pub(crate) trait DragOverlayApi {
    /// Shows an insertion line in `line`, in absolute screen points, drawn in
    /// `color`.
    fn show_line(&mut self, line: IRect, color: (f64, f64, f64));
    /// Shows the refusal symbol in `square`, in absolute screen points.
    fn show_refusal(&mut self, square: IRect);
    fn hide(&mut self);
    /// Closes every overlay window at once, so the next show builds them
    /// again for the screens as they are then.
    fn remove(&mut self);
}

/// The drag overlay the engine draws with, as a non-send resource.
pub(crate) struct DragOverlayHandle(pub Box<dyn DragOverlayApi>);

/// The shape the drag overlay shows now and its most recent line color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Resource)]
pub(crate) struct DragFeedback {
    pub shape: Option<OverlayShape>,
    line_color: (f64, f64, f64),
}

/// What the ECS side has to do after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragEffect {
    /// Apply a drop of `window` at `target`.
    Drop { window: Entity, target: DropTarget },
}

/// The drag state after `input` arrives at `now`. `hit` gives the drop
/// target for a window and pointer.
pub(crate) fn step(
    state: DragState,
    input: DragInput,
    now: Duration,
    hit: &dyn Fn(Entity, IVec2) -> DropTarget,
) -> (DragState, Vec<DragEffect>) {
    let state = match state {
        DragState::Settling(settle) if now >= settle.deadline => DragState::Idle,
        state => state,
    };
    if let DragInput::Down {
        point,
        button,
        window,
    } = input
        && button == MouseButton::Left
    {
        // A press always starts afresh. One that arrives mid-drag means the
        // release was lost, so the dragged window goes home first.
        let effects = match state {
            DragState::Dragging(drag) => vec![DragEffect::Drop {
                window: drag.window,
                target: DropTarget::Home,
            }],
            _ => vec![],
        };
        return (press(point, button, window, now), effects);
    }

    match (state, input) {
        // Nothing is left to place, so nothing is dropped.
        (state, DragInput::WindowGone { window }) if state.window() == Some(window) => {
            (DragState::Idle, vec![])
        }
        (DragState::Pressed(press), DragInput::Up { point })
            if press
                .moved_at
                .is_some_and(|at| now.saturating_sub(at) <= CONFIRM_WINDOW) =>
        {
            (DragState::Released { press, point }, vec![])
        }
        (DragState::Released { press, point }, input) => {
            if press
                .moved_at
                .is_none_or(|at| now.saturating_sub(at) > CONFIRM_WINDOW)
            {
                return (DragState::Idle, vec![]);
            }
            // The release point stays fixed while native movement catches up.
            if !matches!(
                input,
                DragInput::WindowMoved { .. } | DragInput::WindowResized { .. }
            ) {
                return (DragState::Released { press, point }, vec![]);
            }
            match pressed(press, input, now, hit) {
                DragState::Dragging(drag) => {
                    step(DragState::Dragging(drag), DragInput::Up { point }, now, hit)
                }
                DragState::Pressed(press) => (DragState::Released { press, point }, vec![]),
                // The button is already up, so nothing holds the window.
                DragState::Unconfirmed(_) => (DragState::Idle, vec![]),
                next => (next, vec![]),
            }
        }
        (DragState::Unconfirmed(press), input) => (unconfirmed(press, input, now), vec![]),
        (DragState::Pressed(mut press), DragInput::Tick { button_down }) => {
            if press_ended(&mut press, button_down, now) {
                (DragState::Idle, vec![])
            } else {
                (DragState::Pressed(press), vec![])
            }
        }
        (DragState::Dragging(mut drag), DragInput::Tick { button_down }) => {
            if let_go(&mut drag.button_up_since, button_down, now) {
                (
                    DragState::Idle,
                    vec![DragEffect::Drop {
                        window: drag.window,
                        target: DropTarget::Home,
                    }],
                )
            } else {
                (DragState::Dragging(drag), vec![])
            }
        }
        (DragState::Pressed(press), input) => (pressed(press, input, now, hit), vec![]),
        (DragState::Dragging(mut drag), DragInput::Dragged { point }) => {
            drag.pointer = point;
            drag.target = hit(drag.window, point);
            drag.button_up_since = None;
            (DragState::Dragging(drag), vec![])
        }
        (DragState::Dragging(drag), DragInput::Up { point }) => (
            DragState::Settling(Settle {
                window: drag.window,
                deadline: now + SETTLE_WINDOW,
            }),
            vec![DragEffect::Drop {
                window: drag.window,
                target: hit(drag.window, point),
            }],
        ),
        // The pointer moving with no button held means the release never
        // reached the tap.
        (DragState::Dragging(drag), DragInput::Moved { .. }) => (
            DragState::Idle,
            vec![DragEffect::Drop {
                window: drag.window,
                target: DropTarget::Home,
            }],
        ),
        (state, _) => (state, vec![]),
    }
}

/// The state a press leaves: [`DragState::Pressed`] for the left button in
/// the top bar of a tiled window, otherwise idle.
fn press(
    point: IVec2,
    button: MouseButton,
    window: Option<PressedWindow>,
    now: Duration,
) -> DragState {
    let Some(window) = window else {
        return DragState::Idle;
    };
    let frame = window.frame;
    let in_top_bar = (frame.min.x..frame.max.x).contains(&point.x)
        && (frame.min.y..frame.min.y + window.top_bar_height).contains(&point.y);
    if button != MouseButton::Left || !window.tiled || !in_top_bar {
        return DragState::Idle;
    }
    DragState::Pressed(Press {
        window: window.entity,
        point,
        frame,
        samples: Vec::new(),
        moved_at: None,
        window_moved: None,
        button_up_since: None,
        screen_edges: window.screen_edges,
        at: now,
    })
}

/// The next state of a press waiting for its window to follow the pointer.
///
/// The window follows when one of its moves matches one of the pointer's.
/// The two arrive on separate channels, so either can be ahead: a window
/// read late is already past the pointer samples seen so far, and an app
/// slow to report is behind them. Only the confirm window ends a press whose
/// window never follows.
fn pressed(
    mut press: Press,
    input: DragInput,
    now: Duration,
    hit: &dyn Fn(Entity, IVec2) -> DropTarget,
) -> DragState {
    match input {
        DragInput::Dragged { point } => {
            if press.samples.len() == MAX_SAMPLES {
                press.samples.remove(0);
            }
            press.samples.push(point);
            // A drag event means the button is down.
            press.button_up_since = None;
            if press.moved_at.is_none() && past_threshold(point - press.point) {
                press.moved_at = Some(now);
            }
        }
        DragInput::WindowMoved { window, frame } if window == press.window => {
            if frame.size() != press.frame.size() {
                return DragState::Unconfirmed(press);
            }
            let moved = frame.min - press.frame.min;
            // An app reporting a move it did not make, or a move too small to
            // tell apart from a click, says nothing about a drag.
            if past_threshold(moved) {
                press.window_moved = Some(moved);
            }
        }
        DragInput::WindowResized { window } if window == press.window => {
            return DragState::Unconfirmed(press);
        }
        DragInput::Up { .. } | DragInput::Moved { .. } => return DragState::Idle,
        _ => return DragState::Pressed(press),
    }

    // A window that follows only after the deadline is an app moving it, not
    // the pointer.
    if press
        .moved_at
        .is_some_and(|moved_at| now.saturating_sub(moved_at) > CONFIRM_WINDOW)
    {
        return DragState::Unconfirmed(press);
    }
    let follows = press.window_moved.is_some_and(|moved| {
        press
            .samples
            .iter()
            .any(|sample| follows_pointer(moved, *sample - press.point, press.screen_edges))
    });
    match press.samples.last() {
        Some(pointer) if follows => DragState::Dragging(Drag {
            window: press.window,
            pointer: *pointer,
            target: hit(press.window, *pointer),
            button_up_since: press.button_up_since,
        }),
        _ => DragState::Pressed(press),
    }
}

/// Whether a window move of `moved` follows a pointer move of `pointer`.
///
/// Normally the window must match the pointer within [`MOVE_TOLERANCE`] on
/// both axes. A display edge can hold a window on one axis: a window whose
/// left edge is on a display's left edge moves only vertically while the
/// pointer also moves sideways. Only for an axis in `screen_edges`, the
/// other axis alone confirms, when it matches the pointer and moves at least
/// [`STRONG_MOVE`].
fn follows_pointer(moved: IVec2, pointer: IVec2, screen_edges: BVec2) -> bool {
    if !past_threshold(pointer) {
        return false;
    }
    let gap = (pointer - moved).abs();
    let along = |gap: i32, moved: i32| gap <= MOVE_TOLERANCE && moved.abs() >= STRONG_MOVE;
    (gap.x <= MOVE_TOLERANCE && gap.y <= MOVE_TOLERANCE)
        || (screen_edges.x && along(gap.y, moved.y))
        || (screen_edges.y && along(gap.x, moved.x))
}

/// The next state of a press that did not become a drag. It keeps its
/// window until the button is released, or until [`PRESS_LIMIT`].
fn unconfirmed(mut press: Press, input: DragInput, now: Duration) -> DragState {
    match input {
        DragInput::Up { .. } | DragInput::Moved { .. } => DragState::Idle,
        DragInput::Tick { button_down } if press_ended(&mut press, button_down, now) => {
            DragState::Idle
        }
        _ => DragState::Unconfirmed(press),
    }
}

/// Whether a press that is not a drag ends at a read of the button: the
/// button has been let go, or the press has reached [`PRESS_LIMIT`] even
/// with the button reading down.
fn press_ended(press: &mut Press, button_down: bool, now: Duration) -> bool {
    let_go(&mut press.button_up_since, button_down, now)
        || now.saturating_sub(press.at) > PRESS_LIMIT
}

/// Records a read of the button in `up_since`, and returns whether the
/// press or drag has been let go: the button has read as up for the whole
/// grace. A read of the button down starts the grace over. A held button
/// never ends a drag, however long the pointer stays still.
fn let_go(up_since: &mut Option<Duration>, button_down: bool, now: Duration) -> bool {
    if button_down {
        *up_since = None;
        return false;
    }
    let since = *up_since.get_or_insert(now);
    now.saturating_sub(since) > RELEASE_GRACE
}

/// Whether a move of `delta` is more than [`DRAG_THRESHOLD`] long.
fn past_threshold(delta: IVec2) -> bool {
    delta.length_squared() > DRAG_THRESHOLD * DRAG_THRESHOLD
}

const EDGE_BAND: i32 = 6;
const EDGE_DELAY: Duration = Duration::from_millis(500);
const EDGE_REPEAT: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeSide {
    Left,
    Right,
}

#[derive(Component)]
pub(crate) struct EdgeHold {
    display: DisplayId,
    side: EdgeSide,
    next_scroll: Duration,
}

/// How far apart two display edges may be and still touch.
const NEIGHBOR_TOLERANCE: u32 = 2;

/// The display side whose edge band holds `pointer`, when no other display
/// touches that edge at the pointer's height. `snapshot` names the displays
/// whose strip can scroll. `physical_displays` lists every display, because a
/// neighbor that shows no managed strip, such as one in a native fullscreen
/// space, is still a neighbor. A display beside only part of the edge, or
/// only diagonally across a corner, is no neighbor where it does not touch.
fn held_edge(
    snapshot: &LayoutSnapshot,
    physical_displays: &[(DisplayId, IRect)],
    pointer: IVec2,
) -> Option<(DisplayId, EdgeSide)> {
    let display = snapshot
        .managed_strip_displays
        .iter()
        .find(|display| display.bounds.contains(pointer))?;
    let (side, edge) = if pointer.x < display.bounds.min.x + EDGE_BAND {
        (EdgeSide::Left, display.bounds.min.x)
    } else if pointer.x >= display.bounds.max.x - EDGE_BAND {
        (EdgeSide::Right, display.bounds.max.x)
    } else {
        return None;
    };
    let touches = |other: &IRect| {
        let adjacent = match side {
            EdgeSide::Left => {
                other.min.x < edge && other.max.x.abs_diff(edge) <= NEIGHBOR_TOLERANCE
            }
            EdgeSide::Right => {
                other.max.x > edge && other.min.x.abs_diff(edge) <= NEIGHBOR_TOLERANCE
            }
        };
        adjacent && (other.min.y..other.max.y).contains(&pointer.y)
    };
    (!physical_displays
        .iter()
        .any(|(id, bounds)| *id != display.id && touches(bounds)))
    .then_some((display.id, side))
}

type EdgeStrips<'w, 's> =
    Query<'w, 's, &'static mut Position, (With<LayoutStrip>, Without<Window>)>;

fn drag_edge_scroll(
    state: Res<DragState>,
    time: Res<Time>,
    mut holds: Query<(Entity, &mut EdgeHold)>,
    mut parameters: bevy::ecs::system::ParamSet<(DisplayMoves, EdgeStrips)>,
    placements: Query<(&LayoutPosition, &Bounds), With<Window>>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let DragState::Dragging(drag) = &*state else {
        for (entity, _) in &holds {
            commands.entity(entity).despawn();
        }
        return;
    };
    let snapshot = snapshot_of(
        drag.window,
        &parameters.p0(),
        |entity| {
            placements
                .get(entity)
                .ok()
                .map(|(position, bounds)| (position.0, bounds.0))
        },
        &config,
    );
    let physical_displays: Vec<_> = parameters
        .p0()
        .displays
        .iter()
        .map(|(_, display, _, _)| (display.id(), display.physical_bounds()))
        .collect();
    let edge = held_edge(&snapshot, &physical_displays, drag.pointer);
    let mut current = None;
    for (entity, hold) in &mut holds {
        if edge == Some((hold.display, hold.side)) {
            current = Some((entity, hold));
        } else {
            commands.entity(entity).despawn();
        }
    }
    let Some((display, side)) = edge else {
        return;
    };
    let Some((_, mut hold)) = current else {
        commands.spawn(EdgeHold {
            display,
            side,
            next_scroll: time.elapsed() + EDGE_DELAY,
        });
        return;
    };
    if time.elapsed() < hold.next_scroll {
        return;
    }
    hold.next_scroll = time.elapsed() + EDGE_REPEAT;
    let Some(view) = snapshot
        .managed_strip_displays
        .iter()
        .find(|view| view.id == display)
    else {
        return;
    };
    let shift = match side {
        EdgeSide::Left => view
            .columns
            .iter()
            .rev()
            .find(|column| column.x.start < view.viewport.min.x)
            .map(|column| view.viewport.min.x - column.x.start),
        EdgeSide::Right => view
            .columns
            .iter()
            .find(|column| column.x.end > view.viewport.max.x)
            .map(|column| view.viewport.max.x - column.x.end),
    };
    let Some(shift) = shift else {
        return;
    };
    let Some(strip) = parameters.p0().shown_strip(display) else {
        return;
    };
    if let Ok(mut position) = parameters.p1().get_mut(strip) {
        position.0.x += shift;
        commands
            .entity(strip)
            .remove::<(RepositionMarker, ManualStripOffset, Scrolling)>();
    }
}

pub struct DragEventsPlugin;

impl Plugin for DragEventsPlugin {
    fn build(&self, app: &mut App) {
        let in_progress = |state: Res<DragState>| *state != DragState::Idle;
        app.init_resource::<DragState>()
            .init_resource::<DragFeedback>()
            .init_resource::<tiling::PointerResize>()
            .add_systems(
                Update,
                tiling::track_resize_gesture
                    .before(super::systems::window_frame_update)
                    .in_set(super::UpdateSet::Events)
                    .run_if(on_message::<Event>.or_eager(|resize: Res<PointerResize>| {
                        !matches!(*resize, PointerResize::Idle)
                    })),
            )
            .add_observer(tiling::adopt_cancelled_native_resize)
            .add_systems(
                Update,
                poll_pressed_window
                    .before(super::systems::window_frame_update)
                    .in_set(super::UpdateSet::Events)
                    .run_if(|state: Res<DragState>| awaiting_confirmation(&state).is_some()),
            )
            .add_systems(
                Update,
                (
                    drag_events
                        .after(super::systems::window_frame_update)
                        .before(super::mouse::mouse_up_trigger)
                        // A drop focuses the window after giving it its new
                        // column's width. Run the other way round, the focus
                        // check would read the old size back from the window
                        // before the new one is committed.
                        .after(super::focus::fix_window_size_on_focus)
                        .run_if(on_message::<Event>),
                    drag_watchdog.after(drag_events).run_if(in_progress),
                    drag_edge_scroll
                        .after(drag_watchdog)
                        .run_if(in_progress.or_eager(resource_changed::<DragState>)),
                )
                    .in_set(super::UpdateSet::Focus)
                    .run_if(not(resource_exists::<Initializing>)),
            )
            .add_systems(
                Update,
                // After the layout, so the shape is drawn from where the
                // layout put the windows this frame. A display or layout
                // change can move the shape with the pointer still.
                (
                    drag_feedback.run_if(
                        resource_changed::<DragState>
                            .or_eager(displays_changed)
                            .or_eager(layout_changed),
                    ),
                    // An appearance switch or a config reload recolors a
                    // line even with the pointer still.
                    drag_line_color.after(drag_feedback).run_if(
                        resource_exists_and_changed::<super::SystemTheme>
                            .or_eager(resource_changed::<Config>),
                    ),
                )
                    .in_set(super::UpdateSet::Housekeeping)
                    .run_if(not(resource_exists::<Initializing>)),
            )
            .add_systems(
                PostUpdate,
                show_drag_feedback.in_set(super::PostUpdateSet::Ui),
            );
    }
}

type WindowInfo<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Window,
        Option<&'static Unmanaged>,
        Has<FocusedMarker>,
    ),
>;
type Presented<'w, 's> = Query<'w, 's, (&'static PresentedOrigin, Option<&'static TopBarHeight>)>;
type Placements<'w, 's> =
    Query<'w, 's, (&'static mut LayoutPosition, &'static mut Bounds), With<Window>>;

/// The press whose pointer has passed the drag threshold and whose window
/// has not yet been seen to follow it.
fn awaiting_confirmation(state: &DragState) -> Option<&Press> {
    match state {
        DragState::Pressed(press) | DragState::Released { press, .. }
            if press.moved_at.is_some() =>
        {
            Some(press)
        }
        _ => None,
    }
}

/// Reports a pressed window's move as soon as its own frame shows it.
///
/// An app's `WindowMoved` notification can reach the engine long after the
/// window moved: the window ID lookup behind it took 188ms on a native rig,
/// past the confirm window. While a press waits for its window to follow
/// the pointer, its frame is read each frame instead, and a changed frame is
/// reported as a move. [`step`] still requires the move to match the
/// pointer within the confirm window, so a text selection, which moves no
/// window, never becomes a drag.
fn poll_pressed_window(
    state: Res<DragState>,
    mut windows: Query<&mut Window>,
    mut messages: bevy::ecs::message::MessageWriter<Event>,
) {
    let Some(press) = awaiting_confirmation(&state) else {
        return;
    };
    let Ok(mut window) = windows.get_mut(press.window) else {
        return;
    };
    if window
        .update_frame()
        .is_ok_and(|frame| frame.min != press.frame.min)
    {
        messages.write(Event::WindowMoved {
            window_id: window.id(),
        });
    }
}

/// Feeds mouse and window events to [`step`] and applies the drops it asks
/// for.
#[allow(clippy::too_many_arguments)]
pub(super) fn drag_events(
    mut messages: MessageReader<Event>,
    mut state: ResMut<DragState>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    windows: WindowInfo,
    mut placements: Placements,
    presented: Presented,
    mut moves: DisplayMoves,
    config: Res<Config>,
    mut native: ResMut<NativeMembership>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    for event in messages.read() {
        let Some(input) = drag_input(
            event,
            &state,
            &window_manager,
            &windows,
            &moves,
            &presented,
            &placements,
        ) else {
            continue;
        };
        if *state == DragState::Idle && !matches!(input, DragInput::Down { .. }) {
            continue;
        }

        let (next, effects) = {
            let snapshot = OnceCell::new();
            let hit = |dragged, pointer| {
                let snapshot = snapshot.get_or_init(|| {
                    snapshot_of(
                        dragged,
                        &moves,
                        |entity| {
                            placements
                                .get(entity)
                                .ok()
                                .map(|(position, bounds)| (position.0, bounds.0))
                        },
                        &config,
                    )
                });
                hit_test(snapshot, dragged, pointer, |id| {
                    stack_limit(snapshot, id, &config)
                })
            };
            step(state.clone(), input, now, &hit)
        };
        if effects.is_empty() {
            restore_ended_hold(&state, &next, &moves, &mut placements);
        }
        // Written only when it differs, so a drag held still does not
        // count as a change and rebuild the feedback.
        state.set_if_neq(next);

        for DragEffect::Drop { window, target } in effects {
            debug!("dropping {window} at {target:?}");
            let focused = windows.get(window).is_ok_and(|(_, _, _, focused)| focused);
            apply_drop(
                window,
                target,
                focused,
                &mut moves,
                &mut placements,
                &mut commands,
                &mut native,
            );
        }
    }
}

/// The drag input `event` makes, if it makes one.
fn drag_input(
    event: &Event,
    state: &DragState,
    window_manager: &WindowManager,
    windows: &WindowInfo,
    moves: &DisplayMoves,
    presented: &Presented,
    placements: &Placements,
) -> Option<DragInput> {
    let find = |window_id| {
        windows
            .iter()
            .find(|(_, window, _, _)| window.id() == window_id)
    };
    // macOS may report either tab of a native tab group as the one that
    // moved, so a move of any tab counts as a move of the pressed one.
    let pressed_tab = |entity: Entity| match state {
        DragState::Pressed(press) | DragState::Released { press, .. }
            if press.window != entity
                && moves.strips.iter().any(|(_, strip, _, _, _)| {
                    strip
                        .tab_group(press.window)
                        .is_some_and(|group| group.contains(&entity))
                }) =>
        {
            press.window
        }
        _ => entity,
    };
    let input = match event {
        Event::MouseDown {
            point,
            button,
            window_id,
            ..
        } => {
            // The window the tap saw under the press, because by now the
            // pressed window may have moved off the point.
            let on_top = window_id.and_then(find).or_else(|| {
                window_manager
                    .find_window_at_point(point)
                    .ok()
                    .and_then(find)
            });
            DragInput::Down {
                point: origin_from(*point),
                button: *button,
                window: pressed_window(origin_from(*point), on_top, moves, presented, placements),
            }
        }
        Event::MouseDragged {
            point,
            button: MouseButton::Left,
            ..
        } => DragInput::Dragged {
            point: origin_from(*point),
        },
        Event::MouseUp {
            point,
            button: MouseButton::Left,
            ..
        } => DragInput::Up {
            point: origin_from(*point),
        },
        Event::MouseMoved { point, .. } => DragInput::Moved {
            point: origin_from(*point),
        },
        Event::WindowMoved { window_id } => {
            let (entity, window, _, _) = find(*window_id)?;
            DragInput::WindowMoved {
                window: pressed_tab(entity),
                frame: window.frame(),
            }
        }
        Event::WindowResized { window_id } => {
            let (entity, _, _, _) = find(*window_id)?;
            DragInput::WindowResized {
                window: pressed_tab(entity),
            }
        }
        _ => return None,
    };
    Some(input)
}

/// Per axis, whether `frame` has an edge on the matching edge of a display.
fn screen_edges(frame: IRect, moves: &DisplayMoves) -> BVec2 {
    let near = |a: i32, b: i32| (a - b).abs() <= MOVE_TOLERANCE;
    moves
        .displays
        .iter()
        .fold(BVec2::FALSE, |edges, (_, display, _, _)| {
            let bounds = display.bounds();
            edges
                | BVec2::new(
                    near(frame.min.x, bounds.min.x) || near(frame.max.x, bounds.max.x),
                    near(frame.min.y, bounds.min.y) || near(frame.max.y, bounds.max.y),
                )
        })
}

/// The window a press at `point` landed on, given the window the window
/// server reports on top there now.
///
/// A tiled window is found where the layout last presented it, not by its
/// live frame. The tap can hand over a whole gesture in one update, and by
/// then the window has already followed the pointer away from the press.
/// The presented origin also gives the frame the window had when it was
/// pressed, including the offsets of a window parked as an edge sliver.
fn pressed_window(
    point: IVec2,
    on_top: Option<(Entity, &Window, Option<&Unmanaged>, bool)>,
    moves: &DisplayMoves,
    presented: &Presented,
    placements: &Placements,
) -> Option<PressedWindow> {
    let slots = moves
        .strips
        .iter()
        .flat_map(|(_, strip, _, _, _)| {
            strip.all_windows().into_iter().filter_map(|entity| {
                let (origin, top_bar) = presented.get(entity).ok()?;
                // An app can resize its window after the layout placed it,
                // so the size comes from the current bounds.
                let (_, bounds) = placements.get(entity).ok()?;
                let frame = IRect::from_corners(origin.0, origin.0 + bounds.0);
                let height = top_bar.map_or(FALLBACK_TOP_BAR_HEIGHT, |height| height.0);
                frame.contains(point).then_some((entity, frame, height))
            })
        })
        .collect::<Vec<_>>();
    let tiled = |(entity, frame, top_bar_height): (Entity, IRect, i32)| PressedWindow {
        entity,
        frame,
        top_bar_height,
        tiled: true,
        screen_edges: screen_edges(frame, moves),
    };

    if let Some((entity, window, unmanaged, _)) = on_top {
        // Of the tiled windows whose slot holds the press, such as the tabs
        // of a group, the one on top is the one pressed.
        if let Some(slot) = slots.iter().find(|(slotted, _, _)| *slotted == entity) {
            return Some(tiled(*slot));
        }
        let floating = unmanaged.is_some()
            || !moves
                .strips
                .iter()
                .any(|(_, strip, _, _, _)| strip.contains(entity));
        if floating {
            return Some(PressedWindow {
                entity,
                frame: window.frame(),
                top_bar_height: FALLBACK_TOP_BAR_HEIGHT,
                tiled: false,
                screen_edges: BVec2::FALSE,
            });
        }
    }
    slots.first().copied().map(tiled)
}

/// Ends a press or drag the tap never saw end: the button reads as up, or
/// nothing has come for a long time, or the window it holds is gone. A
/// lost release would otherwise keep [`DragState::holds_window`] true and
/// stop every later focus from centering its window.
#[allow(clippy::too_many_arguments)]
pub(super) fn drag_watchdog(
    mut state: ResMut<DragState>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    windows: WindowInfo,
    mut placements: Placements,
    mut moves: DisplayMoves,
    mut native: ResMut<NativeMembership>,
    mut commands: Commands,
) {
    let Some(window) = state.window() else {
        return;
    };
    let gone = windows
        .get(window)
        .ok()
        .is_none_or(|(_, _, unmanaged, _)| unmanaged.is_some())
        || !moves
            .strips
            .iter()
            .any(|(_, strip, _, _, _)| strip.contains(window));
    let input = if gone {
        DragInput::WindowGone { window }
    } else {
        DragInput::Tick {
            button_down: window_manager.left_button_down(),
        }
    };
    // Neither input asks where the pointer is.
    let home = |_, _| DropTarget::Home;
    let (next, effects) = step(state.clone(), input, time.elapsed(), &home);
    if effects.is_empty() {
        restore_ended_hold(&state, &next, &moves, &mut placements);
    }
    state.set_if_neq(next);
    for DragEffect::Drop { window, target } in effects {
        let focused = windows.get(window).is_ok_and(|(_, _, _, focused)| focused);
        apply_drop(
            window,
            target,
            focused,
            &mut moves,
            &mut placements,
            &mut commands,
            &mut native,
        );
    }
}

/// Whether any display was added or changed since the last check.
fn displays_changed(displays: Query<(), Changed<Display>>) -> bool {
    !displays.is_empty()
}

type MovedStrips<'w, 's> = Query<
    'w,
    's,
    (),
    (
        With<LayoutStrip>,
        Or<(Changed<LayoutStrip>, Changed<Position>)>,
    ),
>;
type MovedWindows<'w, 's> =
    Query<'w, 's, (), (With<Window>, Or<(Changed<LayoutPosition>, Changed<Bounds>)>)>;

/// Whether the layout moved a strip or any window's slot or size since the
/// last check.
fn layout_changed(strips: MovedStrips, windows: MovedWindows) -> bool {
    !strips.is_empty() || !windows.is_empty()
}

/// Keeps [`DragFeedback`] on what the overlay should show for the drag
/// state, which hides it whenever no drag would move a window.
fn drag_feedback(
    mut state: ResMut<DragState>,
    mut feedback: ResMut<DragFeedback>,
    moves: DisplayMoves,
    placements: Query<(&LayoutPosition, &Bounds), With<Window>>,
    config: Res<Config>,
    theme: Option<Res<super::SystemTheme>>,
) {
    let shape = match &*state {
        DragState::Dragging(drag) => {
            let snapshot = snapshot_of(
                drag.window,
                &moves,
                |entity| {
                    placements
                        .get(entity)
                        .ok()
                        .map(|(position, bounds)| (position.0, bounds.0))
                },
                &config,
            );
            let target = hit_test(&snapshot, drag.window, drag.pointer, |id| {
                stack_limit(&snapshot, id, &config)
            });
            let mut next = drag.clone();
            next.target = target;
            state.set_if_neq(DragState::Dragging(next));
            overlay_shape(&state, &snapshot)
        }
        _ => None,
    };
    let line_color = match shape {
        Some(OverlayShape::Line(_)) => {
            config.border_color(theme.is_some_and(|theme| theme.is_dark))
        }
        _ => feedback.line_color,
    };
    feedback.set_if_neq(DragFeedback { shape, line_color });
}

/// Samples the line color again for a line already shown, without
/// rebuilding the layout snapshot.
fn drag_line_color(
    mut feedback: ResMut<DragFeedback>,
    config: Res<Config>,
    theme: Option<Res<super::SystemTheme>>,
) {
    if !matches!(feedback.shape, Some(OverlayShape::Line(_))) {
        return;
    }
    let line_color = config.border_color(theme.is_some_and(|theme| theme.is_dark));
    let shape = feedback.shape;
    feedback.set_if_neq(DragFeedback { shape, line_color });
}

/// Draws [`DragFeedback`] when it changes. The line takes the focus border's
/// color.
///
/// A display change closes the overlay windows and draws again, even when
/// the shape is the same. The windows and the conversion from screen points
/// both follow the screens, and a resolution change can leave a shape's
/// screen points as they were.
fn show_drag_feedback(
    feedback: Res<DragFeedback>,
    mut messages: MessageReader<Event>,
    changed_displays: Query<(), Changed<Display>>,
    mut removed_displays: RemovedComponents<Display>,
    overlay: Option<NonSendMut<DragOverlayHandle>>,
) {
    let display_event = messages.read().fold(false, |seen, event| {
        seen | matches!(
            event,
            Event::DisplayAdded { .. }
                | Event::DisplayRemoved { .. }
                | Event::DisplayMoved { .. }
                | Event::DisplayResized { .. }
                | Event::DisplayConfigured { .. }
                | Event::DisplayChanged
        )
    });
    let screens_changed =
        display_event | !changed_displays.is_empty() | (removed_displays.read().count() > 0);
    if !screens_changed && !feedback.is_changed() {
        return;
    }
    let Some(mut overlay) = overlay else {
        return;
    };
    let overlay = &mut overlay.0;
    if screens_changed {
        overlay.remove();
    }
    match feedback.shape {
        Some(OverlayShape::Line(line)) => overlay.show_line(line, feedback.line_color),
        Some(OverlayShape::Refusal(square)) => overlay.show_refusal(square),
        None => overlay.hide(),
    }
}

/// The stack limit of display `id`, from the height its layout uses.
fn stack_limit(snapshot: &LayoutSnapshot, id: DisplayId, config: &Config) -> usize {
    snapshot
        .managed_strip_displays
        .iter()
        .find(|display| display.id == id)
        .map_or(1, |display| {
            effective_stack_limit(config, display.viewport.height())
        })
}

fn snapshot_of(
    dragged: Entity,
    moves: &DisplayMoves,
    layout_slot: impl Fn(Entity) -> Option<(IVec2, IVec2)>,
    config: &Config,
) -> LayoutSnapshot {
    if !moves
        .strips
        .iter()
        .any(|(_, strip, _, _, _)| strip.contains(dragged))
    {
        return LayoutSnapshot::default();
    }
    LayoutSnapshot {
        managed_strip_displays: moves
            .displays
            .iter()
            .filter_map(|(_, display, dock, _)| {
                let strip_entity = moves.shown_strip(display.id())?;
                let (_, strip, strip_position, _, _) = moves.strips.get(strip_entity).ok()?;
                let viewport = display.actual_display_bounds(dock, config);
                let origin = strip_position.0;
                let slot = |entity: Entity| {
                    layout_slot(entity).map(|(position, size)| (position + origin, size))
                };

                let columns = strip
                    .columns()
                    .enumerate()
                    .filter_map(|(index, column)| {
                        let (items, kind) = match column {
                            Column::Single(entity) => (vec![vec![*entity]], ColumnKind::Tiled),
                            Column::Fullscren(entity) => {
                                (vec![vec![*entity]], ColumnKind::Fullscreen)
                            }
                            Column::Tabs(tabs) => (vec![tabs.clone()], ColumnKind::Tiled),
                            Column::Stack(items) => (
                                items
                                    .iter()
                                    .map(|item| item.window_iter().collect())
                                    .collect(),
                                ColumnKind::Tiled,
                            ),
                        };
                        let width = column
                            .window_iter()
                            .filter_map(slot)
                            .map(|(_, size)| size.x)
                            .max()?;
                        let tops = items
                            .iter()
                            .map(|windows: &Vec<Entity>| {
                                windows
                                    .first()
                                    .and_then(|entity| slot(*entity))
                                    .map(|(origin, _)| origin)
                            })
                            .collect::<Option<Vec<_>>>()?;
                        let left = tops.first()?.x;
                        let bands = items
                            .into_iter()
                            .enumerate()
                            .map(|(at, windows)| {
                                let top = tops[at].y;
                                let bottom = tops.get(at + 1).map_or(viewport.max.y, |next| next.y);
                                Band {
                                    windows,
                                    y: top..bottom.max(top),
                                }
                            })
                            .collect();
                        Some(ColumnSnap {
                            index,
                            x: left..left + width,
                            bands,
                            kind,
                        })
                    })
                    .collect();

                Some(DisplaySnap {
                    id: display.id(),
                    bounds: display.bounds(),
                    viewport,
                    columns,
                })
            })
            .collect(),
    }
}

/// Moves `window` to `target` in its strip, then sends it to its slot, which
/// is its old one for [`DropTarget::Home`] and a refusal.
fn apply_drop(
    window: Entity,
    target: DropTarget,
    focused: bool,
    moves: &mut DisplayMoves,
    placements: &mut Placements,
    commands: &mut Commands,
    native: &mut NativeMembership,
) {
    let Some((source, _, _, source_display, _)) = moves
        .strips
        .iter()
        .find(|(_, strip, _, _, _)| strip.contains(window))
    else {
        return;
    };
    let source_display = source_display.parent();
    let destination = match target {
        DropTarget::IntoColumn {
            display,
            column,
            slot,
        } => Some((display, Placement::Into(column, slot))),
        DropTarget::NewColumn { display, index } => Some((display, Placement::Column(index))),
        DropTarget::Home | DropTarget::Refused { .. } => None,
    };
    if let Some((display, placement)) = destination
        && moves
            .displays
            .get(source_display)
            .is_ok_and(|(_, source, _, _)| source.id() != display)
    {
        let size = placements.get(window).ok().map(|(_, bounds)| bounds.0);
        let transfer = size.and_then(|size| {
            moves.move_to_display(
                window,
                display,
                placement,
                size,
                |entity| {
                    placements.get(entity).ok().map(|(position, bounds)| {
                        IRect::from_corners(position.0, position.0 + bounds.0)
                    })
                },
                commands,
            )
        });
        if let Some(transfer) = transfer {
            tiling::supersede_native_resize(&transfer.members, commands);
            restore_drop_members(&transfer.members, placements);
            native.placed(transfer.members);
        } else if let Some((_, strip, _, _, _)) = moves
            .strips
            .iter()
            .find(|(_, strip, _, _, _)| strip.contains(window))
        {
            let members = strip.tab_group(window).unwrap_or_else(|| vec![window]);
            restore_drop_members(&members, placements);
            native.placed(members);
        }
        commands.ensure_visible(window);
        commands.focus_entity(window, false);
        return;
    }
    let Ok((_, mut strip, _, _, _)) = moves.strips.get_mut(source) else {
        return;
    };
    // A native tab group moves as one, so every tab goes where it goes.
    let members = strip.tab_group(window).unwrap_or_else(|| vec![window]);
    let source_can_move = tiling::tiled_in(&strip, window);

    let accepted = match target {
        DropTarget::IntoColumn { column, slot, .. } => {
            let destination_can_join = strip
                .get(column)
                .is_ok_and(|column| !matches!(column, Column::Fullscren(_)));
            if strip.index_of(window).ok() != Some(column) {
                // The window takes the width of the column it joins, even
                // as its new top window.
                let width = strip.get(column).ok().and_then(|column| {
                    column.width(&|entity| {
                        placements.get(entity).ok().map(|(position, bounds)| {
                            IRect::from_corners(position.0, position.0 + bounds.0)
                        })
                    })
                });
                for member in &members {
                    if let Some(width) = width
                        && let Ok((_, mut bounds)) = placements.get_mut(*member)
                    {
                        bounds.0.x = width;
                    }
                }
                if let Ok(mut entity_commands) = commands.get_entity(window) {
                    entity_commands.try_remove::<FullWidthMarker>();
                }
            }
            strip.insert_into_column(window, column, slot).is_ok() && destination_can_join
        }
        DropTarget::NewColumn { index, .. } => strip.insert_as_column(window, index).is_ok(),
        DropTarget::Refused { .. } | DropTarget::Home => false,
    };
    if source_can_move && accepted {
        tiling::supersede_native_resize(&members, commands);
    }

    // The user's drag left the window where it was released. Marking its
    // slot changed sends it there, even when the slot did not move.
    restore_drop_members(&members, placements);
    native.placed(members);
    commands.ensure_visible(window);
    if !focused {
        commands.focus_entity(window, false);
    }
}

fn restore_ended_hold(
    previous: &DragState,
    next: &DragState,
    moves: &DisplayMoves,
    placements: &mut Placements,
) {
    if previous.holds_window()
        && !next.holds_window()
        && let Some(window) = previous.window()
    {
        let members = moves
            .strips
            .iter()
            .find_map(|(_, strip, _, _, _)| strip.tab_group(window))
            .unwrap_or_else(|| vec![window]);
        restore_drop_members(&members, placements);
    }
}

fn restore_drop_members(members: &[Entity], placements: &mut Placements) {
    for &member in members {
        if let Ok((mut position, _)) = placements.get_mut(member) {
            position.set_changed();
        }
    }
}
