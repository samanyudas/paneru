//! Dragging a tiled window by its title bar into a column, or out to a new
//! column, in the strip it is in or the strip another display shows. Off
//! unless `drag_to_reorder` is set.
//!
//! - [`step`] is the pure drag state machine. It turns one input into the
//!   next [`DragState`] and the drop to apply, if any.
//! - [`hit_test`] is the pure drop rule. It turns a [`LayoutSnapshot`] of
//!   each display, the pointer, and the dragged window's centre into a
//!   [`DropTarget`].
//! - [`drag_events`] feeds platform events to [`step`] and applies its drops
//!   to the [`LayoutStrip`]s.
//! - [`overlay_shape`] is the pure drop feedback. [`drag_feedback`] keeps
//!   [`DragFeedback`] on it, and [`show_drag_feedback`] draws it.
//! - [`held_edge`] and [`edge_shift`] are the pure edge scroll rules.
//!   [`drag_edge_scroll`] scrolls a strip one column at a time while the
//!   pointer of a drag holds its edge.

use std::cell::OnceCell;
use std::ops::Range;
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate, Update};
use bevy::ecs::change_detection::{DetectChanges as _, DetectChangesMut as _};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::{not, on_message, resource_changed, resource_exists};
use bevy::ecs::schedule::{IntoScheduleConfigs as _, SystemCondition as _};
use bevy::ecs::system::{
    Commands, NonSendMut, ParamSet, Populated, Query, Res, ResMut, SystemParam,
};
use bevy::math::{BVec2, IRect, IVec2};
use bevy::time::Time;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{Level, debug, instrument};

use crate::config::Config;
use crate::ecs::display::{Placement, move_to_display};
use crate::ecs::layout::{Column, LayoutStrip, MIN_WINDOW_HEIGHT};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    Bounds, DockPosition, FocusedMarker, FullWidthMarker, Initializing, LayoutPosition,
    ManualStripOffset, MissionControlActive, NativeFullscreenMarker, Position, PostUpdateSet,
    PresentedOrigin, RepositionMarker, Scrolling, SpawnCommandsExt, SystemTheme, Timeout,
    Unmanaged, UpdateSet,
};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Window, WindowManager, origin_from};
use crate::platform::{WinID, WorkspaceId};

/// How far below a window's top edge a press can start a drag. Most title
/// bars and toolbars fit in it. A press below it is in the window's content.
const TITLE_BAR_BAND: i32 = 52;

/// How far the pointer must move from the press before a drag can start.
const DRAG_THRESHOLD: i32 = 4;

/// How far the window's move may differ from the pointer's on each axis and
/// still count as the window following the pointer.
const MOVE_TOLERANCE: i32 = 8;

/// How far a window held at a screen edge must move on its free axis to
/// confirm a drag from that axis alone. Three times [`MOVE_TOLERANCE`], so a
/// window that barely moves, or that its app moves on its own, stays within
/// the tolerance and confirms nothing.
const STRONG_MOVE: i32 = 3 * MOVE_TOLERANCE;

/// How long after the pointer passes the drag threshold the window has to
/// follow it. A window that moves later is the app moving it.
const CONFIRM_WINDOW: Duration = Duration::from_millis(250);

/// How long a press that never becomes a drag can last, even with the
/// button down. A real drag moves its window within the confirm window, so
/// a hold this long is a button state stuck down or a lost release.
const PRESS_LIMIT: Duration = Duration::from_secs(10);

/// How long after a drop the dropped window stays watched.
const SETTLE_WINDOW: Duration = Duration::from_millis(600);

/// The most pointer samples a press keeps to match a late window move.
const MAX_SAMPLES: usize = 32;

/// How long the window server must keep reporting the left button up before
/// a press or drag counts as released. The tap can deliver the release a
/// little after the button state changes, so a shorter wait would cancel
/// drops still in flight.
const RELEASE_GRACE: Duration = Duration::from_millis(250);

/// How close to the left or right edge of a display the pointer must be to
/// hold that edge.
const EDGE_BAND: i32 = 6;

/// How long the pointer must hold an edge before the strip first scrolls.
const EDGE_DELAY: Duration = Duration::from_millis(500);

/// How often the strip scrolls again while the pointer still holds the edge.
const EDGE_REPEAT: Duration = Duration::from_millis(350);

/// How far apart the edges of two displays may be and still touch.
const NEIGHBOUR_TOLERANCE: u32 = 2;

/// Where the windows of a strip shown on a display are, in screen points.
/// It is taken from the layout rather than live frames, so the dragged
/// window does not hide its own slot. The drop rule takes one for each
/// display, the dragged window's own strip first.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayoutSnapshot {
    pub strip: Entity,
    /// The whole display, which decides whether the pointer is on it.
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

/// The visible part of one stack item: from its top down to the next item's
/// top, or to the bottom of the viewport for the last one. A native tab
/// group is one item, so it lists every tab.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Band {
    pub windows: Vec<Entity>,
    pub y: Range<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnKind {
    /// Its windows split the column's height.
    Split,
    /// A tabbed stack, whose windows all take the whole column.
    Tabbed,
    /// A fullscreen column holds only its own window.
    Fullscreen,
}

/// What a drop at the pointer does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropTarget {
    /// Into column `column` of `strip`, `slot` items from the top among its
    /// other items.
    IntoColumn {
        strip: Entity,
        column: usize,
        slot: usize,
    },
    /// To a new column of `strip` at gap `index`, where gap 0 is before the
    /// first column.
    NewColumn { strip: Entity, index: usize },
    /// Nowhere: the column is fullscreen or has no room for another window,
    /// so the window goes back to its slot.
    Refused,
    /// Back to its own slot, which changes nothing.
    Home,
}

impl LayoutSnapshot {
    fn column(&self, index: usize) -> Option<&ColumnSnap> {
        self.columns.iter().find(|column| column.index == index)
    }
}

impl ColumnSnap {
    fn holds(&self, entity: Entity) -> bool {
        self.bands.iter().any(|band| band.windows.contains(&entity))
    }
}

/// The drop target for `dragged` with the pointer at `pointer` and the
/// window's centre `centre_dx` points right of the pointer, over the strips
/// of `snapshots`.
///
/// The pointer picks the display and the height. The window's centre picks
/// the column and its zone, so where the title bar was grabbed does not
/// matter. A centre off the pointer's display counts as the nearest point on
/// it. Each display's own height decides whether a column there has room.
pub(crate) fn hit_test(
    snapshots: &[LayoutSnapshot],
    dragged: Entity,
    pointer: IVec2,
    centre_dx: i32,
) -> DropTarget {
    let Some(snapshot) = snapshots
        .iter()
        .find(|snapshot| snapshot.bounds.contains(pointer))
    else {
        return DropTarget::Home;
    };
    let bounds = snapshot.bounds;
    let aim = IVec2::new(
        (pointer.x + centre_dx).clamp(bounds.min.x, bounds.max.x - 1),
        pointer.y,
    );
    // A column fits as many windows as it has room for at the minimum
    // height, and always one.
    let fits = usize::try_from(snapshot.viewport.height() / MIN_WINDOW_HEIGHT)
        .unwrap_or_default()
        .max(1);
    // A window alone in its column, or a native tab group alone in it,
    // already is a column of its own.
    let lone = snapshot
        .columns
        .iter()
        .find(|column| column.holds(dragged) && column.bands.len() == 1)
        .map(|column| column.index);

    let strip = snapshot.strip;
    let target = match snapshot
        .columns
        .iter()
        .find(|column| column.x.contains(&aim.x))
    {
        Some(column) => column_target(strip, column, dragged, aim, fits),
        None => DropTarget::NewColumn {
            strip,
            index: gap_at(&snapshot.columns, aim.x),
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

/// The drop target with `aim`, the window's centre across and the pointer
/// down, over `column` of `strip`, which fits `fits` windows.
fn column_target(
    strip: Entity,
    column: &ColumnSnap,
    dragged: Entity,
    aim: IVec2,
    fits: usize,
) -> DropTarget {
    let quarter = (column.x.end - column.x.start) / 4;
    if aim.x < column.x.start + quarter {
        return DropTarget::NewColumn {
            strip,
            index: column.index,
        };
    }
    if aim.x >= column.x.end - quarter {
        return DropTarget::NewColumn {
            strip,
            index: column.index + 1,
        };
    }
    // A reorder within its own stack never adds a window.
    let own = column
        .bands
        .iter()
        .position(|band| band.windows.contains(&dragged));
    let full = match column.kind {
        ColumnKind::Split => column.bands.len() >= fits,
        ColumnKind::Tabbed => false,
        ColumnKind::Fullscreen => true,
    };
    if full && own.is_none() {
        return DropTarget::Refused;
    }
    let slot = column
        .bands
        .iter()
        .filter(|band| !band.windows.contains(&dragged))
        .filter(|band| i32::midpoint(band.y.start, band.y.end) < aim.y)
        .count();
    if own == Some(slot) {
        return DropTarget::Home;
    }
    DropTarget::IntoColumn {
        strip,
        column: column.index,
        slot,
    }
}

/// The gap a window centred at `x` in empty space opens: the nearest strip
/// end, or the gap it sits in between two columns.
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
const INSERTION_LINE: i32 = 3;

/// The size of the refusal symbol, in points.
pub(crate) const REFUSAL_SYMBOL: i32 = 22;

/// Where the refusal symbol sits from the pointer, down and to the right,
/// like a cursor's badge.
const REFUSAL_OFFSET: IVec2 = IVec2::new(14, 14);

/// What the drag overlay draws, in screen points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayShape {
    /// An insertion line where a release would put the window.
    Line(IRect),
    /// The `nosign` symbol in this square, beside the pointer, where a
    /// release would be refused.
    Refusal(IRect),
}

/// What the overlay shows for a drag of `dragged` with the pointer at
/// `pointer`, whose release would land at `target` in one of the strips of
/// `snapshots`: an insertion line, the refusal symbol, or nothing when the
/// window would go back to its slot.
pub(crate) fn overlay_shape(
    snapshots: &[LayoutSnapshot],
    dragged: Entity,
    pointer: IVec2,
    target: DropTarget,
) -> Option<OverlayShape> {
    let snapshot_of = |strip| snapshots.iter().find(|snapshot| snapshot.strip == strip);
    match target {
        DropTarget::IntoColumn {
            strip,
            column,
            slot,
        } => {
            let snapshot = snapshot_of(strip)?;
            let viewport = snapshot.viewport;
            let column = snapshot.column(column)?;
            // The gap above the `slot`-th other item, or below the last.
            let y = match column
                .bands
                .iter()
                .filter(|band| !band.windows.contains(&dragged))
                .nth(slot)
            {
                Some(band) => band.y.start,
                None => column.bands.last()?.y.end,
            };
            let x = column.x.start.max(viewport.min.x)..column.x.end.min(viewport.max.x);
            if x.is_empty() {
                return None;
            }
            let y = line_across(y, viewport.min.y..viewport.max.y)?;
            Some(OverlayShape::Line(IRect::new(
                x.start, y.start, x.end, y.end,
            )))
        }
        DropTarget::NewColumn { strip, index } => {
            let snapshot = snapshot_of(strip)?;
            let viewport = snapshot.viewport;
            let before = index
                .checked_sub(1)
                .and_then(|index| snapshot.column(index));
            let after = snapshot.column(index);
            let x = match (before, after) {
                (Some(before), Some(after)) => i32::midpoint(before.x.end, after.x.start),
                (Some(before), None) => before.x.end,
                (None, Some(after)) => after.x.start,
                // The first column of an empty strip opens at its left edge.
                (None, None) if snapshot.columns.is_empty() => viewport.min.x,
                (None, None) => return None,
            };
            let x = line_across(x, viewport.min.x..viewport.max.x)?;
            Some(OverlayShape::Line(IRect::new(
                x.start,
                viewport.min.y,
                x.end,
                viewport.max.y,
            )))
        }
        DropTarget::Refused => {
            let corner = pointer + REFUSAL_OFFSET;
            Some(OverlayShape::Refusal(IRect::from_corners(
                corner,
                corner + IVec2::splat(REFUSAL_SYMBOL),
            )))
        }
        DropTarget::Home => None,
    }
}

/// The span of an insertion line centred on `at`, moved inside `within` so
/// a line at a screen edge stays whole and on screen. `None` when `within`
/// is too short for a whole line.
fn line_across(at: i32, within: Range<i32>) -> Option<Range<i32>> {
    if within.end - within.start < INSERTION_LINE {
        return None;
    }
    let start = (at - INSERTION_LINE / 2).clamp(within.start, within.end - INSERTION_LINE);
    Some(start..start + INSERTION_LINE)
}

/// The left or the right edge of a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeSide {
    Left,
    Right,
}

/// The strip and the side whose edge `pointer` holds: the pointer is within
/// [`EDGE_BAND`] of the left or right edge of the display of one of
/// `snapshots`, and no display in `displays` touches that edge at the
/// pointer's height.
///
/// `displays` holds the whole bounds of every display, because a display
/// that shows no strip a drop can join, such as one in a native fullscreen
/// space, is still beside the edge. A display beside only part of the edge,
/// or only across a corner, does not touch the rest of it.
fn held_edge(
    snapshots: &[LayoutSnapshot],
    displays: &[IRect],
    pointer: IVec2,
) -> Option<(Entity, EdgeSide)> {
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.bounds.contains(pointer))?;
    let bounds = snapshot.bounds;
    let (side, edge) = if pointer.x < bounds.min.x + EDGE_BAND {
        (EdgeSide::Left, bounds.min.x)
    } else if pointer.x >= bounds.max.x - EDGE_BAND {
        (EdgeSide::Right, bounds.max.x)
    } else {
        return None;
    };
    // The strict comparisons leave out the pointer's own display.
    let touches = |other: &IRect| {
        let beside = match side {
            EdgeSide::Left => {
                other.min.x < edge && other.max.x.abs_diff(edge) <= NEIGHBOUR_TOLERANCE
            }
            EdgeSide::Right => {
                other.max.x > edge && other.min.x.abs_diff(edge) <= NEIGHBOUR_TOLERANCE
            }
        };
        beside && (other.min.y..other.max.y).contains(&pointer.y)
    };
    (!displays.iter().any(touches)).then_some((snapshot.strip, side))
}

/// How far right to move the strip of `snapshot` so that the nearest column
/// that reaches past its `side` edge shows in full at that edge. `None` when
/// no column reaches past that edge.
fn edge_shift(snapshot: &LayoutSnapshot, side: EdgeSide) -> Option<i32> {
    let viewport = snapshot.viewport;
    match side {
        EdgeSide::Left => snapshot
            .columns
            .iter()
            .rev()
            .find(|column| column.x.start < viewport.min.x)
            .map(|column| viewport.min.x - column.x.start),
        EdgeSide::Right => snapshot
            .columns
            .iter()
            .find(|column| column.x.end > viewport.max.x)
            .map(|column| viewport.max.x - column.x.end),
    }
}

/// Draws the drop feedback on screen. The app draws with
/// [`DragOverlay`](crate::overlay::DragOverlay), and tests record the calls.
pub(crate) trait DragOverlayApi {
    /// Shows an insertion line over `line`, in screen points, in `color`.
    fn show_line(&mut self, line: IRect, color: (f64, f64, f64));
    /// Shows the refusal symbol in `square`, in screen points.
    fn show_refusal(&mut self, square: IRect);
    fn hide(&mut self);
    /// Closes the overlay's windows.
    fn remove(&mut self);
}

/// The drag overlay the engine draws with, as a non-send resource.
pub(crate) struct DragOverlayHandle(pub Box<dyn DragOverlayApi>);

/// What the drag overlay shows now, and the color of its line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Resource)]
pub(crate) struct DragFeedback {
    shape: Option<OverlayShape>,
    line_color: (f64, f64, f64),
}

/// A tiled window a press landed on, with the frame the layout gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PressedWindow {
    pub entity: Entity,
    pub frame: IRect,
    /// Per axis, whether the frame touches an edge of its display on that
    /// axis, where macOS can hold the window while the pointer moves on.
    pub screen_edges: BVec2,
}

/// One thing that happened, as the drag state machine sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragInput {
    /// A button went down. `window` is the tiled window under the press, if
    /// there is one and a drag may start now.
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
    Moved,
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
    /// Drags stopped being allowed: the option was turned off, or Mission
    /// Control opened.
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Press {
    pub window: Entity,
    pub point: IVec2,
    /// The window's frame when it was pressed.
    pub frame: IRect,
    /// When the button went down.
    pub at: Duration,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Drag {
    pub window: Entity,
    /// Where the pointer is now.
    pub pointer: IVec2,
    /// How far right of the pointer the window's centre is, fixed at the
    /// press.
    pub centre_dx: i32,
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
    /// The left button went down in a tiled window's title bar.
    Pressed(Press),
    /// The button came up at `point` before the window was seen to follow
    /// the pointer. Its move can still arrive within the confirm window.
    Released { press: Press, point: IVec2 },
    /// The window did not follow the pointer in time, but macOS may still be
    /// dragging it, so the press holds it until the release.
    Unconfirmed(Press),
    /// The window follows the pointer.
    Dragging(Drag),
    /// A drop just happened.
    Settling(Settle),
}

impl DragState {
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

    /// The window a press or drag holds, which nothing else should move: a
    /// dragged window, or a pressed one once the pointer has passed the drag
    /// threshold, because macOS may be moving it with the pointer.
    pub(crate) fn held(&self) -> Option<Entity> {
        match self {
            Self::Pressed(press) if press.moved_at.is_some() => Some(press.window),
            Self::Released { press, .. } | Self::Unconfirmed(press) => Some(press.window),
            Self::Dragging(drag) => Some(drag.window),
            _ => None,
        }
    }

    /// The press whose window may still follow the pointer: it has passed
    /// the drag threshold, and the confirm window has not passed.
    fn awaiting_move(&self, now: Duration) -> Option<&Press> {
        match self {
            Self::Pressed(press) | Self::Released { press, .. }
                if press
                    .moved_at
                    .is_some_and(|at| now.saturating_sub(at) <= CONFIRM_WINDOW) =>
            {
                Some(press)
            }
            _ => None,
        }
    }

    /// The window just dropped, while it settles.
    pub(crate) fn settling(&self) -> Option<Entity> {
        match self {
            Self::Settling(settle) => Some(settle.window),
            _ => None,
        }
    }
}

/// A drop of `window` at `target`, for the ECS side to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DragDrop {
    pub window: Entity,
    pub target: DropTarget,
}

/// The drag state after `input` arrives at `now`, and the drop it makes, if
/// any. `hit` gives the drop target for a window, pointer, and centre
/// offset.
pub(crate) fn step(
    state: DragState,
    input: DragInput,
    now: Duration,
    hit: &dyn Fn(Entity, IVec2, i32) -> DropTarget,
) -> (DragState, Option<DragDrop>) {
    let home = |window| {
        Some(DragDrop {
            window,
            target: DropTarget::Home,
        })
    };
    let state = match state {
        DragState::Settling(settle) if now >= settle.deadline => DragState::Idle,
        DragState::Pressed(press) if confirm_passed(&press, now) => DragState::Unconfirmed(press),
        state => state,
    };
    // The window that goes back to its slot when the press or drag holding
    // it ends without a drop.
    let held = state.held();

    match (state, input) {
        // A press always starts afresh, so a window the last gesture still
        // held goes back to its slot first.
        (
            _,
            DragInput::Down {
                point,
                button: MouseButton::Left,
                window,
            },
        ) => (press(point, window, now), held.and_then(home)),
        // Nothing is left to place, so nothing is dropped.
        (state, DragInput::WindowGone { window }) if state.window() == Some(window) => {
            (DragState::Idle, None)
        }
        // The pointer moving with no button held means the release of a
        // press never reached the tap.
        (_, DragInput::Cancel) | (DragState::Pressed(_), DragInput::Moved) => {
            (DragState::Idle, held.and_then(home))
        }
        (DragState::Released { press, point }, input) => released(press, point, input, now, hit),
        (DragState::Unconfirmed(mut press), input) => {
            let ended = match input {
                DragInput::Up { .. } | DragInput::Moved => true,
                DragInput::Tick { button_down } => press_ended(&mut press, button_down, now),
                _ => false,
            };
            if ended {
                (DragState::Idle, home(press.window))
            } else {
                (DragState::Unconfirmed(press), None)
            }
        }
        (DragState::Pressed(press), DragInput::Up { point }) => match press.moved_at {
            Some(_) => (DragState::Released { press, point }, None),
            None => (DragState::Idle, None),
        },
        (DragState::Pressed(mut press), DragInput::Tick { button_down }) => {
            if press_ended(&mut press, button_down, now) {
                (DragState::Idle, held.and_then(home))
            } else {
                (DragState::Pressed(press), None)
            }
        }
        (DragState::Pressed(press), input) => (pressed(press, input, now), None),
        (DragState::Dragging(mut drag), DragInput::Tick { button_down }) => {
            if let_go(&mut drag.button_up_since, button_down, now) {
                (DragState::Idle, home(drag.window))
            } else {
                (DragState::Dragging(drag), None)
            }
        }
        (DragState::Dragging(mut drag), DragInput::Dragged { point }) => {
            drag.pointer = point;
            drag.button_up_since = None;
            (DragState::Dragging(drag), None)
        }
        (DragState::Dragging(drag), DragInput::Up { point }) => (
            DragState::Settling(Settle {
                window: drag.window,
                deadline: now + SETTLE_WINDOW,
            }),
            Some(DragDrop {
                window: drag.window,
                target: hit(drag.window, point, drag.centre_dx),
            }),
        ),
        // The pointer moving with no button held means the release never
        // reached the tap.
        (DragState::Dragging(drag), DragInput::Moved) => (DragState::Idle, home(drag.window)),
        (state, _) => (state, None),
    }
}

/// The state a left-button press leaves: [`DragState::Pressed`] in the title
/// bar of a tiled window, otherwise idle.
fn press(point: IVec2, window: Option<PressedWindow>, now: Duration) -> DragState {
    let Some(PressedWindow {
        entity,
        frame,
        screen_edges,
    }) = window
    else {
        return DragState::Idle;
    };
    let in_title_bar = (frame.min.x..frame.max.x).contains(&point.x)
        && (frame.min.y..frame.min.y + TITLE_BAR_BAND).contains(&point.y);
    if !in_title_bar {
        return DragState::Idle;
    }
    DragState::Pressed(Press {
        window: entity,
        point,
        frame,
        at: now,
        samples: Vec::new(),
        moved_at: None,
        window_moved: None,
        button_up_since: None,
        screen_edges,
    })
}

/// The next state of a press waiting for its window to follow the pointer.
///
/// The window follows when one of its moves matches one of the pointer's.
/// The two arrive on separate channels, so either can be ahead: a window
/// read late is already past the pointer samples seen so far, and an app
/// slow to report is behind them.
fn pressed(mut press: Press, input: DragInput, now: Duration) -> DragState {
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
            // A window that changes size as it moves is being resized.
            if frame.size() != press.frame.size() {
                return DragState::Idle;
            }
            let moved = frame.min - press.frame.min;
            // A move too small to tell apart from a click says nothing about
            // a drag.
            if past_threshold(moved) {
                press.window_moved = Some(moved);
            }
        }
        // A resize is not a drag, and the window stays where the resize puts
        // it, as without the option.
        DragInput::WindowResized { window } if window == press.window => {
            return DragState::Idle;
        }
        _ => return DragState::Pressed(press),
    }

    let follows = press.window_moved.is_some_and(|moved| {
        press
            .samples
            .iter()
            .any(|sample| follows_pointer(moved, *sample - press.point, press.screen_edges))
    });
    if follows {
        DragState::Dragging(Drag {
            window: press.window,
            pointer: press.samples.last().copied().unwrap_or(press.point),
            centre_dx: press.frame.center().x - press.point.x,
            button_up_since: press.button_up_since,
        })
    } else {
        DragState::Pressed(press)
    }
}

/// The next state of a press released at `point` before its window was
/// seen to follow the pointer, and the drop it makes, if any. A window move
/// that confirms the drag within the confirm window drops the window at the
/// release point. Otherwise the window goes back to its slot.
fn released(
    press: Press,
    point: IVec2,
    input: DragInput,
    now: Duration,
    hit: &dyn Fn(Entity, IVec2, i32) -> DropTarget,
) -> (DragState, Option<DragDrop>) {
    let home = Some(DragDrop {
        window: press.window,
        target: DropTarget::Home,
    });
    if confirm_passed(&press, now) {
        return (DragState::Idle, home);
    }
    if !matches!(
        input,
        DragInput::WindowMoved { .. } | DragInput::WindowResized { .. }
    ) {
        return (DragState::Released { press, point }, None);
    }
    match pressed(press, input, now) {
        DragState::Dragging(drag) => {
            step(DragState::Dragging(drag), DragInput::Up { point }, now, hit)
        }
        DragState::Pressed(press) => (DragState::Released { press, point }, None),
        _ => (DragState::Idle, home),
    }
}

/// Whether the confirm window of `press` has passed with its window not
/// seen to follow the pointer.
fn confirm_passed(press: &Press, now: Duration) -> bool {
    press
        .moved_at
        .is_some_and(|at| now.saturating_sub(at) > CONFIRM_WINDOW)
}

/// Whether a window move of `moved` follows a pointer move of `pointer`.
///
/// The pointer must have moved past the threshold, and the window must
/// match it within [`MOVE_TOLERANCE`] on both axes. A display edge can hold
/// a window on one axis: a window on a display's left edge can move only
/// down while the pointer moves right and down. So on an axis in
/// `screen_edges`, the other axis alone confirms, when it matches the
/// pointer and moves at least [`STRONG_MOVE`].
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

/// Records a read of the button for a press that is not a drag, and returns
/// whether the press has ended: the button has been let go, or the press
/// has reached [`PRESS_LIMIT`] even with the button reading down.
fn press_ended(press: &mut Press, button_down: bool, now: Duration) -> bool {
    let_go(&mut press.button_up_since, button_down, now)
        || now.saturating_sub(press.at) > PRESS_LIMIT
}

/// Records a read of the button in `up_since`, and returns whether the
/// press or drag has been let go: the button has read as up for the whole
/// grace. A read of the button down starts the grace over.
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

/// The strip shown on each display, as the window server last named the
/// space each display shows: at a title-bar press, and at a space or display
/// change while a press or drag lasts. The drop rule reads it, so a pointer
/// move asks the window server nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub(crate) struct ShownStrips(Vec<Entity>);

/// How long a window a drag sent back from another display stays guarded
/// against macOS listing it in that display's space.
const RETURN_GUARD: Duration = Duration::from_secs(10);

/// Spawned, with a [`Timeout`], for each window a drag sent back to its slot
/// from `display`, another display than its strip's. macOS can keep listing
/// such a window in the space of the display it was dragged over, which
/// would make `detect_moved_windows` move it there.
/// A new title-bar press of the window despawns it, and so does the window
/// going away.
///
/// The guard is an entity of its own, because a window's own [`Timeout`]
/// slot is taken by focus verification.
#[derive(Component, Debug)]
pub(crate) struct DragReturnedFrom {
    pub window: Entity,
    pub display: CGDirectDisplayID,
}

/// Spawned while the pointer of a drag holds the `side` edge of the display
/// that shows `strip`, and a column of the strip reaches past that edge.
/// Despawned when the pointer leaves that edge, no column is left past it, or
/// the drag ends. While one exists, the event pump keeps drawing frames, so
/// the strip scrolls on time with the pointer still.
#[derive(Component, Debug)]
pub(crate) struct EdgeHold {
    strip: Entity,
    side: EdgeSide,
    /// When the strip scrolls next.
    next_scroll: Duration,
}

/// What title-bar drags know about windows macOS may list in the wrong
/// space.
#[derive(SystemParam)]
pub(crate) struct DragPlacements<'w, 's> {
    state: Option<Res<'w, DragState>>,
    returned: Query<'w, 's, &'static DragReturnedFrom>,
    displays: Query<'w, 's, &'static Display>,
}

impl DragPlacements<'_, '_> {
    /// Whether a report that macOS lists `window` in the space of the display
    /// `listed_on` is stale, so the window stays in its strip.
    ///
    /// `strip` is the strip holding the window, with its display, and
    /// `frame` the window's frame as the engine has it. A window a press or
    /// drag is about, with its native tabs, stays until the drag ends. A
    /// window a drag sent back from `listed_on` stays while its frame's
    /// centre is on its own strip's display.
    pub(crate) fn keeps(
        &self,
        window: Entity,
        frame: Option<IRect>,
        listed_on: Option<Entity>,
        strip: Option<(&LayoutStrip, Entity)>,
    ) -> bool {
        let dragged = self.state.as_deref().and_then(DragState::window);
        let held = dragged.is_some_and(|dragged| {
            dragged == window
                || strip.is_some_and(|(strip, _)| {
                    strip
                        .tab_group(dragged)
                        .is_some_and(|tabs| tabs.contains(&window))
                })
        });
        if held {
            return true;
        }
        let Some(listed_on) = listed_on.and_then(|display| self.displays.get(display).ok()) else {
            return false;
        };
        let returned = self
            .returned
            .iter()
            .any(|guard| guard.window == window && guard.display == listed_on.id());
        returned
            && strip
                .and_then(|(_, display)| self.displays.get(display).ok())
                .zip(frame)
                .is_some_and(|(own, frame)| own.bounds().contains(frame.center()))
    }
}

pub struct DragEventsPlugin;

impl Plugin for DragEventsPlugin {
    fn build(&self, app: &mut App) {
        let in_progress = |state: Res<DragState>| *state != DragState::Idle;
        let awaiting_move =
            |state: Res<DragState>, time: Res<Time>| state.awaiting_move(time.elapsed()).is_some();
        let allowed = |config: Res<Config>, mission_control: Option<Res<MissionControlActive>>| {
            drags_allowed(&config, mission_control.as_deref())
        };
        let dragging = |state: Res<DragState>| matches!(*state, DragState::Dragging(_));
        app.init_resource::<DragState>()
            .init_resource::<DragFeedback>()
            .init_resource::<ShownStrips>()
            .add_systems(
                Update,
                (
                    drag_events
                        // A drop focuses the window after giving it its new
                        // column's width. Run the other way round, the focus
                        // check would read the old size back from the window
                        // before the new one is committed.
                        .after(super::focus::fix_window_size_on_focus)
                        .run_if(on_message::<Event>.or_eager(awaiting_move))
                        .run_if(allowed),
                    drag_watchdog.after(drag_events).run_if(in_progress),
                    forget_returns.after(drag_events),
                    drag_edge_scroll.after(drag_watchdog).run_if(dragging),
                    end_edge_holds.after(drag_watchdog).run_if(not(dragging)),
                )
                    .in_set(UpdateSet::Focus)
                    .run_if(not(resource_exists::<Initializing>)),
            )
            .add_systems(
                Update,
                // After the layout, so the shape is drawn from where the
                // layout put the windows this frame.
                drag_feedback
                    .in_set(UpdateSet::Housekeeping)
                    .run_if(
                        resource_changed::<DragState>
                            .or_eager(dragging.and_then(feedback_inputs_changed)),
                    )
                    .run_if(not(resource_exists::<Initializing>)),
            )
            .add_systems(
                PostUpdate,
                show_drag_feedback
                    .in_set(PostUpdateSet::Ui)
                    .run_if(feedback_pending),
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
type Placements<'w, 's> =
    Query<'w, 's, (&'static mut LayoutPosition, &'static mut Bounds), With<Window>>;
type Strips<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut LayoutStrip,
        &'static Position,
        &'static ChildOf,
        Has<PreviousStripPosition>,
        Entity,
    ),
>;
type Displays<'w, 's> = Query<'w, 's, (&'static Display, Option<&'static DockPosition>)>;
type ReadPlacements<'w, 's> =
    Query<'w, 's, (&'static LayoutPosition, &'static Bounds), With<Window>>;
type ReadStrips<'w, 's> = Query<
    'w,
    's,
    (
        &'static LayoutStrip,
        &'static Position,
        &'static ChildOf,
        Has<PreviousStripPosition>,
        Entity,
    ),
>;
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

/// Whether title-bar drags may move windows now: the option is on, and
/// Mission Control, which moves windows on its own, is not showing.
fn drags_allowed(config: &Config, mission_control: Option<&MissionControlActive>) -> bool {
    config.drag_to_reorder() && mission_control.is_none_or(|active| !active.0)
}

/// Feeds mouse and window events to [`step`] and applies the drops it asks
/// for. While a press waits for its window to follow the pointer, it also
/// feeds the window's frame as the window server has it.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
fn drag_events(
    mut messages: MessageReader<Event>,
    mut state: ResMut<DragState>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    windows: WindowInfo,
    presented: Query<&PresentedOrigin>,
    mut placements: Placements,
    mut strips: Strips,
    displays: Displays,
    native_fullscreen: Query<(), With<NativeFullscreenMarker>>,
    mut shown: ResMut<ShownStrips>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    let refresh = |shown: &mut ResMut<ShownStrips>, strips: &Strips| {
        shown.set_if_neq(ShownStrips(shown_strips(
            strips,
            &native_fullscreen,
            &displays,
            &window_manager,
        )));
    };
    let events = messages.read().collect::<Vec<_>>();
    for (at, event) in events.iter().enumerate() {
        if (is_display_event(event) || matches!(event, Event::SpaceChanged))
            && *state != DragState::Idle
        {
            refresh(&mut shown, &strips);
        }
        let Some(input) = drag_input(event, &state, &windows, &strips, |point, window_id| {
            // The window the tap saw under the press, because by now the
            // pressed window may have moved off the point.
            let (entity, window) =
                window_id
                    .and_then(|id| find_window(&windows, id))
                    .or_else(|| {
                        window_manager
                            .find_window_at_point(&point)
                            .ok()
                            .and_then(|id| find_window(&windows, id))
                    })?;
            // A move later in this update was already read into the cached
            // frames, so they may show the window past where it was pressed.
            let moved_later = events[at + 1..]
                .iter()
                .any(|later| matches!(later, Event::WindowMoved { .. }));
            pressed_window(
                origin_from(point),
                entity,
                (!moved_later).then(|| window.frame()),
                &strips,
                &presented,
                &placements,
                &displays,
            )
        }) else {
            continue;
        };
        let pressing = matches!(input, DragInput::Down { .. });
        advance(
            input,
            now,
            &mut state,
            &windows,
            &mut strips,
            &mut placements,
            &displays,
            &shown,
            &config,
            &mut commands,
        );
        if pressing && matches!(*state, DragState::Pressed(_)) {
            refresh(&mut shown, &strips);
        }
    }

    if let Some(input) = polled_move(&state, now, &windows, &window_manager) {
        advance(
            input,
            now,
            &mut state,
            &windows,
            &mut strips,
            &mut placements,
            &displays,
            &shown,
            &config,
            &mut commands,
        );
    }
}

/// The strip shown on each display: the one of the space the window server
/// says the display shows, unless it is a parked virtual workspace or a
/// native fullscreen space, which takes no window. It asks the window server
/// once for each display that has a strip.
fn shown_strips(
    strips: &Strips,
    native_fullscreen: &Query<(), With<NativeFullscreenMarker>>,
    displays: &Displays,
    window_manager: &WindowManager,
) -> Vec<Entity> {
    let mut spaces: Vec<(Entity, Option<WorkspaceId>)> = Vec::new();
    let mut shown: Vec<(Entity, Entity)> = Vec::new();
    for (strip, _, child_of, parked, entity) in strips {
        let display = child_of.parent();
        if parked
            || native_fullscreen.contains(entity)
            || shown.iter().any(|(on, _)| *on == display)
        {
            continue;
        }
        let asked = spaces
            .iter()
            .find(|(on, _)| *on == display)
            .map(|(_, space)| *space);
        let space = asked.unwrap_or_else(|| {
            let space = displays
                .get(display)
                .ok()
                .and_then(|(display, _)| window_manager.active_display_space(display.id()).ok());
            spaces.push((display, space));
            space
        });
        if space == Some(strip.id()) {
            shown.push((display, entity));
        }
    }
    shown.into_iter().map(|(_, strip)| strip).collect()
}

/// Runs `input` through [`step`] and applies the drop it makes, if any.
#[allow(clippy::too_many_arguments)]
fn advance(
    input: DragInput,
    now: Duration,
    state: &mut ResMut<DragState>,
    windows: &WindowInfo,
    strips: &mut Strips,
    placements: &mut Placements,
    displays: &Displays,
    shown: &ShownStrips,
    config: &Config,
    commands: &mut Commands,
) {
    if **state == DragState::Idle && !matches!(input, DragInput::Down { .. }) {
        return;
    }

    let (next, drop) = {
        let snapshots = OnceCell::new();
        let hit = |dragged, pointer, centre_dx| {
            let snapshots = snapshots.get_or_init(|| {
                snapshot_of(
                    dragged,
                    shown,
                    &strips.as_readonly(),
                    &placements.as_readonly(),
                    displays,
                    config,
                )
            });
            hit_test(snapshots, dragged, pointer, centre_dx)
        };
        step((**state).clone(), input, now, &hit)
    };
    state.set_if_neq(next);

    if let Some(drop) = drop {
        debug!("dropping {} at {:?}", drop.window, drop.target);
        apply_drop(
            drop, windows, strips, placements, displays, config, commands,
        );
    }
}

/// The move of the window a press waits for, read from the window server,
/// if the window has moved from where it was pressed.
///
/// An app's move notification can reach the engine after the confirm window
/// has passed, though the window followed the pointer in time. The window
/// server already has the new frame, and asking it asks no app. The reads
/// stop when the confirm window passes.
fn polled_move(
    state: &DragState,
    now: Duration,
    windows: &WindowInfo,
    window_manager: &WindowManager,
) -> Option<DragInput> {
    let press = state.awaiting_move(now)?;
    let (_, window, _, _) = windows.get(press.window).ok()?;
    let bounds = window_manager.window_bounds(window.id()).ok()?;
    // The engine's frames include the window's padding.
    let padding = IVec2::new(window.horizontal_padding(), window.vertical_padding());
    let frame = IRect::from_corners(bounds.min - padding, bounds.max + padding);
    (frame.min != press.frame.min).then_some(DragInput::WindowMoved {
        window: press.window,
        frame,
    })
}

fn find_window<'a>(windows: &'a WindowInfo, window_id: WinID) -> Option<(Entity, &'a Window)> {
    windows
        .iter()
        .find_map(|(entity, window, _, _)| (window.id() == window_id).then_some((entity, window)))
}

/// The drag input `event` makes, if it makes one. `pressed` finds the tiled
/// window under a press, given its point and the window the tap saw.
fn drag_input(
    event: &Event,
    state: &DragState,
    windows: &WindowInfo,
    strips: &Strips,
    pressed: impl FnOnce(CGPoint, Option<WinID>) -> Option<PressedWindow>,
) -> Option<DragInput> {
    // macOS may report any tab of a native tab group as the one that moved,
    // so a move of any tab counts as a move of the pressed one.
    let pressed_tab = |entity: Entity| match state {
        DragState::Pressed(press) | DragState::Released { press, .. }
            if press.window != entity
                && strips.iter().any(|(strip, ..)| {
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
        } => DragInput::Down {
            point: origin_from(*point),
            button: *button,
            window: (*button == MouseButton::Left)
                .then(|| pressed(*point, *window_id))
                .flatten(),
        },
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
        Event::MouseMoved { .. } => DragInput::Moved,
        Event::WindowMoved { window_id } => {
            let (entity, window) = find_window(windows, *window_id)?;
            DragInput::WindowMoved {
                window: pressed_tab(entity),
                frame: window.frame(),
            }
        }
        Event::WindowResized { window_id } => {
            let (entity, _) = find_window(windows, *window_id)?;
            DragInput::WindowResized {
                window: pressed_tab(entity),
            }
        }
        _ => return None,
    };
    Some(input)
}

/// The tiled window `entity` as a press at `point` found it: in a strip
/// shown on the display under the press, at `cached`, its frame as the
/// engine last read or wrote it.
///
/// A window still sliding to its slot, or held off it, is not where the
/// layout presented it, so a move is measured from the cached frame. When
/// `cached` is `None`, the tap handed over the gesture in one update, and
/// the window may already have followed the pointer away from the press.
/// The frame then comes from where the layout last presented the window,
/// which includes the offsets of a window parked as an edge sliver.
fn pressed_window(
    point: IVec2,
    entity: Entity,
    cached: Option<IRect>,
    strips: &Strips,
    presented: &Query<&PresentedOrigin>,
    placements: &Placements,
    displays: &Displays,
) -> Option<PressedWindow> {
    let (_, _, child_of, parked, _) = strips.iter().find(|(strip, ..)| strip.contains(entity))?;
    let (display, _) = displays.get(child_of.parent()).ok()?;
    // A parked virtual workspace is not on screen.
    if parked || !display.bounds().contains(point) {
        return None;
    }
    let frame = cached.or_else(|| {
        let origin = presented.get(entity).ok()?.0;
        // An app can resize its window after the layout placed it, so the
        // size comes from the current bounds.
        let (_, bounds) = placements.get(entity).ok()?;
        Some(IRect::from_corners(origin, origin + bounds.0))
    })?;
    Some(PressedWindow {
        entity,
        frame,
        screen_edges: screen_edges(frame, display.bounds()),
    })
}

/// Per axis, whether `frame` has an edge on the matching edge of `display`.
fn screen_edges(frame: IRect, display: IRect) -> BVec2 {
    let near = |a: i32, b: i32| (a - b).abs() <= MOVE_TOLERANCE;
    BVec2::new(
        near(frame.min.x, display.min.x) || near(frame.max.x, display.max.x),
        near(frame.min.y, display.min.y) || near(frame.max.y, display.max.y),
    )
}

/// Ends a press or drag the tap never saw end: the button reads as up for
/// the release grace, the window it holds is gone, or drags stopped being
/// allowed. A lost release would otherwise hold the window for good.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
fn drag_watchdog(
    mut state: ResMut<DragState>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mission_control: Option<Res<MissionControlActive>>,
    windows: WindowInfo,
    mut placements: Placements,
    mut strips: Strips,
    displays: Displays,
    mut commands: Commands,
) {
    let Some(window) = state.window() else {
        return;
    };
    let gone = windows
        .get(window)
        .ok()
        .is_none_or(|(_, _, unmanaged, _)| unmanaged.is_some())
        || !strips.iter().any(|(strip, ..)| strip.contains(window));
    let input = if gone {
        DragInput::WindowGone { window }
    } else if !drags_allowed(&config, mission_control.as_deref()) {
        DragInput::Cancel
    } else {
        DragInput::Tick {
            button_down: window_manager.left_button_down(),
        }
    };
    // None of these inputs asks where the pointer is.
    let (next, drop) = step(state.clone(), input, time.elapsed(), &|_, _, _| {
        DropTarget::Home
    });
    state.set_if_neq(next);
    if let Some(drop) = drop {
        apply_drop(
            drop,
            &windows,
            &mut strips,
            &mut placements,
            &displays,
            &config,
            &mut commands,
        );
    }
}

/// Despawns the guards of returned windows that a new title-bar press
/// supersedes, or whose window is gone. The rest expire with their
/// [`Timeout`].
fn forget_returns(
    guards: Populated<(Entity, &DragReturnedFrom)>,
    state: Res<DragState>,
    windows: Query<(), With<Window>>,
    strips: Query<&LayoutStrip>,
    mut commands: Commands,
) {
    let pressed = match &*state {
        DragState::Pressed(press) => strips
            .iter()
            .find_map(|strip| strip.tab_group(press.window))
            .unwrap_or_else(|| vec![press.window]),
        _ => Vec::new(),
    };
    for (entity, guard) in guards {
        if (pressed.contains(&guard.window) || !windows.contains(guard.window))
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_despawn();
        }
    }
}

type StripOffsets<'w, 's> =
    Query<'w, 's, &'static mut Position, (With<LayoutStrip>, Without<Window>)>;

/// Keeps the drag's [`EdgeHold`] on the edge its pointer holds, and scrolls
/// the held strip one column when the hold is due: [`EDGE_DELAY`] after it
/// starts, then every [`EDGE_REPEAT`].
///
/// A scroll moves the strip's offset, as a swipe does. The layout then places
/// the strip's windows again, all but the held ones, so the dragged window
/// stays under the pointer, and the drop rule reads the scrolled strip.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
fn drag_edge_scroll(
    state: Res<DragState>,
    time: Res<Time>,
    mut holds: Query<(Entity, &mut EdgeHold)>,
    shown: Res<ShownStrips>,
    mut strips: ParamSet<(ReadStrips, StripOffsets)>,
    placements: ReadPlacements,
    displays: Displays,
    config: Res<Config>,
    mut commands: Commands,
) {
    let DragState::Dragging(drag) = &*state else {
        return;
    };
    let snapshots = snapshot_of(
        drag.window,
        &shown,
        &strips.p0(),
        &placements,
        &displays,
        &config,
    );
    let bounds: Vec<_> = displays
        .iter()
        .map(|(display, _)| display.physical_bounds())
        .collect();
    // An edge with no column past it has nothing to scroll, so it holds
    // nothing, and the event loop draws no frames for it.
    let edge = held_edge(&snapshots, &bounds, drag.pointer).and_then(|(strip, side)| {
        let snapshot = snapshots.iter().find(|snapshot| snapshot.strip == strip)?;
        Some((strip, side, edge_shift(snapshot, side)?))
    });
    let now = time.elapsed();

    let mut current = None;
    for (entity, hold) in &mut holds {
        if current.is_none()
            && edge.is_some_and(|(strip, side, _)| (strip, side) == (hold.strip, hold.side))
        {
            current = Some(hold);
        } else if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_despawn();
        }
    }
    let Some((strip, side, shift)) = edge else {
        return;
    };
    let Some(mut hold) = current else {
        commands.spawn(EdgeHold {
            strip,
            side,
            next_scroll: now + EDGE_DELAY,
        });
        return;
    };
    if now < hold.next_scroll {
        return;
    }
    hold.next_scroll = now + EDGE_REPEAT;

    debug!("scrolling strip {strip} by {shift} at its {side:?} edge");
    if let Ok(mut position) = strips.p1().get_mut(strip) {
        position.0.x += shift;
    }
    // The pointer drives the strip now, as a swipe would: an animation, a
    // swipe's momentum, or a placement the user made earlier would move it
    // away from the column the scroll shows.
    if let Ok(mut entity_commands) = commands.get_entity(strip) {
        entity_commands.try_remove::<(RepositionMarker, ManualStripOffset, Scrolling)>();
    }
}

/// Despawns the edge hold of a drag that ended.
fn end_edge_holds(holds: Populated<Entity, With<EdgeHold>>, mut commands: Commands) {
    for entity in holds {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_despawn();
        }
    }
}

/// Whether anything the drop feedback is drawn from, other than the drag
/// state, changed since the last check: a strip, a window's slot or size, a
/// display, the strips the displays show, the appearance, or the config.
fn feedback_inputs_changed(
    strips: MovedStrips,
    windows: MovedWindows,
    displays: Query<(), Changed<Display>>,
    shown: Res<ShownStrips>,
    theme: Option<Res<SystemTheme>>,
    config: Res<Config>,
) -> bool {
    !strips.is_empty()
        || !windows.is_empty()
        || !displays.is_empty()
        || shown.is_changed()
        || theme.is_some_and(|theme| theme.is_changed())
        || config.is_changed()
}

/// Keeps [`DragFeedback`] on what the overlay should show: where a release
/// would put the dragged window, in the focus border's color. It shows
/// nothing unless a drag would move the window.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
fn drag_feedback(
    state: Res<DragState>,
    mut feedback: ResMut<DragFeedback>,
    strips: ReadStrips,
    placements: ReadPlacements,
    displays: Displays,
    shown: Res<ShownStrips>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let shape = match &*state {
        DragState::Dragging(drag) => {
            let snapshots = snapshot_of(
                drag.window,
                &shown,
                &strips,
                &placements,
                &displays,
                &config,
            );
            let target = hit_test(&snapshots, drag.window, drag.pointer, drag.centre_dx);
            overlay_shape(&snapshots, drag.window, drag.pointer, target)
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

/// Whether the overlay may have something to draw: new feedback, or a shape
/// it shows that a display change may have moved. The feedback starts
/// hidden, as the overlay does.
fn feedback_pending(feedback: Res<DragFeedback>) -> bool {
    feedback.shape.is_some() || (feedback.is_changed() && !feedback.is_added())
}

/// Draws [`DragFeedback`] when it changes. A display change draws a shown
/// shape again, even when it is the same, because the overlay's windows and
/// its conversion from screen points follow the screens.
fn show_drag_feedback(
    feedback: Res<DragFeedback>,
    mut messages: MessageReader<Event>,
    displays: Query<(), Changed<Display>>,
    overlay: Option<NonSendMut<DragOverlayHandle>>,
) {
    let display_event = messages
        .read()
        .fold(false, |seen, event| seen | is_display_event(event));
    if !feedback.is_changed() && !display_event && displays.is_empty() {
        return;
    }
    let Some(mut overlay) = overlay else {
        return;
    };
    match feedback.shape {
        Some(OverlayShape::Line(line)) => overlay.0.show_line(line, feedback.line_color),
        Some(OverlayShape::Refusal(square)) => overlay.0.show_refusal(square),
        None => overlay.0.hide(),
    }
}

/// Whether `event` reports that a display was added, removed, moved,
/// resized, or reconfigured.
fn is_display_event(event: &Event) -> bool {
    matches!(
        event,
        Event::DisplayAdded { .. }
            | Event::DisplayRemoved { .. }
            | Event::DisplayMoved { .. }
            | Event::DisplayResized { .. }
            | Event::DisplayConfigured { .. }
            | Event::DisplayChanged
    )
}

/// The snapshots of the strip holding `dragged`, first, and of the strip
/// `shown` names on each other display.
fn snapshot_of(
    dragged: Entity,
    shown: &ShownStrips,
    strips: &ReadStrips,
    placements: &ReadPlacements,
    displays: &Displays,
    config: &Config,
) -> Vec<LayoutSnapshot> {
    let Some((.., own)) = strips.iter().find(|(strip, ..)| strip.contains(dragged)) else {
        return Vec::new();
    };
    let mut seen = Vec::new();
    std::iter::once(own)
        .chain(shown.0.iter().copied())
        .filter_map(|entity| {
            let (_, _, child_of, parked, _) = strips.get(entity).ok()?;
            let display = child_of.parent();
            // A strip shown when the press began may have been parked since,
            // and a display shows one strip.
            if (parked && entity != own) || seen.contains(&display) {
                return None;
            }
            seen.push(display);
            strip_snapshot(entity, strips, placements, displays, config)
        })
        .collect()
}

/// The snapshot of `strip_entity`, on the display that strip is on.
fn strip_snapshot(
    strip_entity: Entity,
    strips: &ReadStrips,
    placements: &ReadPlacements,
    displays: &Displays,
    config: &Config,
) -> Option<LayoutSnapshot> {
    let (strip, strip_position, child_of, ..) = strips.get(strip_entity).ok()?;
    let (display, dock) = displays.get(child_of.parent()).ok()?;
    let viewport = display.actual_display_bounds(dock, config);
    let slot = |entity: Entity| {
        placements
            .get(entity)
            .ok()
            .map(|(layout_position, bounds)| (layout_position.0 + strip_position.0, bounds.0))
    };

    let columns = strip
        .columns()
        .enumerate()
        .filter_map(|(index, column)| {
            let (items, kind) = match column {
                Column::Single(entity) => (vec![vec![*entity]], ColumnKind::Split),
                Column::Fullscren(entity) => (vec![vec![*entity]], ColumnKind::Fullscreen),
                Column::Tabs(tabs) => (vec![tabs.clone()], ColumnKind::Split),
                Column::Stack(items) => {
                    let kind = if column
                        .window_iter()
                        .any(|entity| strip.is_tabbed_display(entity))
                    {
                        ColumnKind::Tabbed
                    } else {
                        ColumnKind::Split
                    };
                    let items = items
                        .iter()
                        .map(|item| item.window_iter().collect())
                        .collect();
                    (items, kind)
                }
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
                .zip(&tops)
                .enumerate()
                .map(|(at, (windows, top))| {
                    let bottom = tops.get(at + 1).map_or(viewport.max.y, |next| next.y);
                    Band {
                        windows,
                        y: top.y..bottom.max(top.y),
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

    Some(LayoutSnapshot {
        strip: strip_entity,
        bounds: display.bounds(),
        viewport,
        columns,
    })
}

/// Moves the dropped window to its target, then sends it to its slot, which
/// is its old one when the move is refused or goes home. A target in the
/// strip of another display moves the window to that display.
///
/// The strips and the window's width change only when the move succeeds, so
/// a refused drop marks nothing changed but the window's own slot.
fn apply_drop(
    DragDrop { window, target }: DragDrop,
    windows: &WindowInfo,
    strips: &mut Strips,
    placements: &mut Placements,
    displays: &Displays,
    config: &Config,
    commands: &mut Commands,
) {
    let Some(source) = strips
        .iter()
        .find_map(|(strip, .., entity)| strip.contains(window).then_some(entity))
    else {
        return;
    };
    let moved = match target {
        DropTarget::IntoColumn {
            strip,
            column,
            slot,
        } if strip != source => move_across(
            window,
            [source, strip],
            Placement::Join { column, slot },
            strips,
            placements,
            displays,
            config,
            commands,
        ),
        DropTarget::NewColumn { strip, index } if strip != source => move_across(
            window,
            [source, strip],
            Placement::Column(index),
            strips,
            placements,
            displays,
            config,
            commands,
        ),
        DropTarget::IntoColumn { column, slot, .. } => {
            move_within(window, source, column, slot, strips, placements, commands)
        }
        DropTarget::NewColumn { index, .. } => {
            strips.get_mut(source).is_ok_and(|(mut strip, ..)| {
                let moved = strip
                    .bypass_change_detection()
                    .insert_as_column(window, index)
                    .is_ok();
                if moved {
                    strip.set_changed();
                }
                moved
            })
        }
        DropTarget::Refused | DropTarget::Home => false,
    };

    let Some((strip, _, child_of, ..)) = strips.iter().find(|(strip, ..)| strip.contains(window))
    else {
        return;
    };
    // A native tab group moves as one, so every tab goes where it goes.
    let members = strip.tab_group(window).unwrap_or_else(|| vec![window]);
    let (frame, focused) = windows
        .get(window)
        .map_or((None, false), |(_, window, _, focused)| {
            (Some(window.frame()), focused)
        });
    if !moved
        && let Some(from) =
            frame.and_then(|frame| returned_from(frame, child_of.parent(), displays))
    {
        for member in &members {
            let timeout = Timeout::new(RETURN_GUARD, None, commands);
            commands.spawn((
                timeout,
                DragReturnedFrom {
                    window: *member,
                    display: from,
                },
            ));
        }
    }

    // The user's drag left the window where it was released. Marking its
    // slot changed sends it there, even when the slot did not move.
    for member in members {
        if let Ok((mut layout_position, _)) = placements.get_mut(member) {
            layout_position.set_changed();
        }
    }
    commands.ensure_visible(window);
    if !focused {
        commands.focus_entity(window, false);
    }
}

/// Moves `window` into column `column` of its own strip `source`, `slot`
/// items from the top, and gives it the column's width when it joins
/// another column. Returns whether it moved.
fn move_within(
    window: Entity,
    source: Entity,
    column: usize,
    slot: usize,
    strips: &mut Strips,
    placements: &mut Placements,
    commands: &mut Commands,
) -> bool {
    let Ok((mut strip, ..)) = strips.get_mut(source) else {
        return false;
    };
    let members = strip.tab_group(window).unwrap_or_else(|| vec![window]);
    let joins = strip.index_of(window).ok() != Some(column);
    // The window takes the width of the column it joins, even as its new
    // top window.
    let width = strip.get(column).ok().filter(|_| joins).and_then(|column| {
        column.width(&|entity| {
            placements
                .get(entity)
                .ok()
                .map(|(position, bounds)| IRect::from_corners(position.0, position.0 + bounds.0))
        })
    });
    if strip
        .bypass_change_detection()
        .insert_into_column(window, column, slot)
        .is_err()
    {
        return false;
    }
    strip.set_changed();
    for member in &members {
        if let Some(width) = width
            && let Ok((_, mut bounds)) = placements.get_mut(*member)
        {
            bounds.0.x = width;
        }
    }
    if joins && let Ok(mut entity_commands) = commands.get_entity(window) {
        entity_commands.try_remove::<FullWidthMarker>();
    }
    true
}

/// Moves `window` from the strip `source` to `placement` in the strip
/// `target` of another display. Returns whether it moved.
#[allow(clippy::too_many_arguments)]
fn move_across(
    window: Entity,
    [source, target]: [Entity; 2],
    placement: Placement,
    strips: &mut Strips,
    placements: &mut Placements,
    displays: &Displays,
    config: &Config,
    commands: &mut Commands,
) -> bool {
    let Ok([(from, _, from_display, ..), (to, _, to_display, ..)]) =
        strips.get_many_mut([source, target])
    else {
        return false;
    };
    // Each display's own viewport, with its own dock, sizes the window.
    let viewport = |child_of: &ChildOf| {
        displays
            .get(child_of.parent())
            .ok()
            .map(|(display, dock)| display.actual_display_bounds(dock, config))
    };
    let (Some(from_viewport), Some(to_viewport)) = (viewport(from_display), viewport(to_display))
    else {
        return false;
    };
    move_to_display(
        window,
        [from, to],
        [from_viewport, to_viewport],
        placement,
        placements,
        commands,
    )
    .inspect_err(|err| debug!("{window} stays on its display: {err}"))
    .is_ok()
}

/// The display a window at `frame` comes back from: the one that holds the
/// most of it, when that is not `own`, the display of its strip. A window
/// dragged by its title bar can hang below a short display, so its centre
/// can be on none.
fn returned_from(frame: IRect, own: Entity, displays: &Displays) -> Option<CGDirectDisplayID> {
    let own = displays.get(own).ok()?.0.id();
    let area = |display: &Display| {
        let overlap = display.bounds().intersect(frame);
        i64::from(overlap.width().max(0)) * i64::from(overlap.height().max(0))
    };
    displays
        .iter()
        .map(|(display, _)| display)
        .filter(|display| area(display) > 0)
        .max_by_key(|display| area(display))
        .map(Display::id)
        .filter(|from| *from != own)
}

#[cfg(test)]
mod tests {
    use bevy::ecs::world::World;

    use super::*;

    fn entities(count: usize) -> Vec<Entity> {
        let mut world = World::new();
        (0..count).map(|_| world.spawn_empty().id()).collect()
    }

    /// A hit test that names where the pointer is, so a test can see which
    /// pointer position a target came from.
    fn hit_at(_window: Entity, pointer: IVec2, _centre_dx: i32) -> DropTarget {
        DropTarget::NewColumn {
            strip: Entity::PLACEHOLDER,
            index: usize::try_from(pointer.x).unwrap_or(0),
        }
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    const FRAME: IRect = IRect {
        min: IVec2::new(0, 20),
        max: IVec2::new(400, 768),
    };
    const PRESS: IVec2 = IVec2::new(100, 30);

    fn down(window: Entity, point: IVec2) -> DragInput {
        DragInput::Down {
            point,
            button: MouseButton::Left,
            window: Some(PressedWindow {
                entity: window,
                frame: FRAME,
                screen_edges: BVec2::FALSE,
            }),
        }
    }

    fn dragged(delta: IVec2) -> DragInput {
        DragInput::Dragged {
            point: PRESS + delta,
        }
    }

    fn moved_by(window: Entity, delta: IVec2) -> DragInput {
        DragInput::WindowMoved {
            window,
            frame: IRect::from_corners(FRAME.min + delta, FRAME.max + delta),
        }
    }

    /// Runs `inputs` through the state machine from `state`, the first at
    /// `start` and each 20ms after the last.
    fn run_from(
        mut state: DragState,
        inputs: &[DragInput],
        start: Duration,
    ) -> (DragState, Vec<DragDrop>) {
        let mut drops = Vec::new();
        let mut now = start;
        for input in inputs {
            let (next, drop) = step(state, *input, now, &hit_at);
            state = next;
            drops.extend(drop);
            now += ms(20);
        }
        (state, drops)
    }

    fn run(inputs: &[DragInput]) -> (DragState, Vec<DragDrop>) {
        run_from(DragState::Idle, inputs, Duration::ZERO)
    }

    fn is_pressed(state: &DragState) -> bool {
        matches!(state, DragState::Pressed(_))
    }

    fn dragging(w: Entity) -> DragState {
        run(&[
            down(w, PRESS),
            dragged(IVec2::new(50, 0)),
            moved_by(w, IVec2::new(50, 0)),
        ])
        .0
    }

    fn home(window: Entity) -> Vec<DragDrop> {
        vec![DragDrop {
            window,
            target: DropTarget::Home,
        }]
    }

    #[test]
    fn a_press_in_the_title_bar_of_a_tiled_window_may_start_a_drag() {
        let w = entities(1)[0];
        let (state, drops) = run(&[down(w, PRESS)]);
        assert!(is_pressed(&state), "{state:?}");
        assert_eq!(drops, vec![]);

        let last_row = IVec2::new(100, FRAME.min.y + TITLE_BAR_BAND - 1);
        assert!(is_pressed(&run(&[down(w, last_row)]).0));
    }

    #[test]
    fn other_presses_never_start_a_drag() {
        let w = entities(1)[0];
        let below_the_bar = IVec2::new(100, FRAME.min.y + TITLE_BAR_BAND);
        let right_button = DragInput::Down {
            point: PRESS,
            button: MouseButton::Right,
            window: Some(PressedWindow {
                entity: w,
                frame: FRAME,
                screen_edges: BVec2::FALSE,
            }),
        };
        let no_tiled_window = DragInput::Down {
            point: PRESS,
            button: MouseButton::Left,
            window: None,
        };
        for input in [down(w, below_the_bar), right_button, no_tiled_window] {
            assert_eq!(run(&[input]).0, DragState::Idle, "{input:?}");
        }
    }

    #[test]
    fn a_window_following_the_pointer_starts_a_drag() {
        let w = entities(1)[0];
        let (state, drops) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(50, 5)),
            moved_by(w, IVec2::new(50, 5)),
        ]);
        assert_eq!(
            state,
            DragState::Dragging(Drag {
                window: w,
                pointer: PRESS + IVec2::new(50, 5),
                centre_dx: FRAME.center().x - PRESS.x,
                button_up_since: None,
            })
        );
        assert_eq!(drops, vec![]);
    }

    #[test]
    fn a_drag_keeps_the_latest_pointer_position() {
        let w = entities(1)[0];
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(50, 5)),
            moved_by(w, IVec2::new(50, 5)),
            dragged(IVec2::new(80, 40)),
        ]);
        let DragState::Dragging(drag) = state else {
            panic!("{state:?}");
        };
        assert_eq!(drag.pointer, PRESS + IVec2::new(80, 40));
    }

    #[test]
    fn a_window_move_lagging_behind_the_pointer_still_starts_a_drag() {
        let w = entities(1)[0];
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(20, 0)),
            dragged(IVec2::new(40, 0)),
            dragged(IVec2::new(60, 0)),
            moved_by(w, IVec2::new(20, 0)),
        ]);
        assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
    }

    #[test]
    fn a_window_move_ahead_of_the_pointer_starts_a_drag_once_it_catches_up() {
        let w = entities(1)[0];
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(20, 0)),
            moved_by(w, IVec2::new(40, 0)),
        ]);
        assert!(is_pressed(&state), "{state:?}");

        let (state, _) = run_from(state, &[dragged(IVec2::new(40, 0))], ms(60));
        assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
    }

    #[test]
    fn a_small_jitter_is_not_a_drag() {
        let w = entities(1)[0];
        let jitter = IVec2::new(3, 1);
        let (state, _) = run(&[down(w, PRESS), dragged(jitter), moved_by(w, jitter)]);
        assert!(is_pressed(&state), "{state:?}");

        let (state, drops) = run_from(
            state,
            &[DragInput::Up {
                point: PRESS + jitter,
            }],
            ms(60),
        );
        assert_eq!(state, DragState::Idle);
        assert_eq!(drops, vec![]);
    }

    #[test]
    fn a_window_move_with_the_pointer_below_the_threshold_is_not_a_drag() {
        let w = entities(1)[0];
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(1, 0)),
            moved_by(w, IVec2::new(5, 0)),
        ]);
        assert!(is_pressed(&state), "{state:?}");
    }

    #[test]
    fn a_resize_is_not_a_drag() {
        let w = entities(1)[0];
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(0, 30)),
            DragInput::WindowResized { window: w },
        ]);
        assert_eq!(state, DragState::Idle, "a resize notification");

        // Dragging the top edge moves the origin and changes the size together.
        let top_edge = DragInput::WindowMoved {
            window: w,
            frame: IRect::from_corners(FRAME.min + IVec2::new(0, 30), FRAME.max),
        };
        let (state, _) = run(&[down(w, PRESS), dragged(IVec2::new(0, 30)), top_edge]);
        assert_eq!(state, DragState::Idle, "a move that changes the size");
    }

    #[test]
    fn a_window_moving_away_from_the_pointer_is_not_a_drag() {
        let w = entities(1)[0];
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(50, 0)),
            moved_by(w, IVec2::new(-200, 0)),
        ]);
        assert!(is_pressed(&state), "{state:?}");
    }

    #[test]
    fn another_window_moving_changes_nothing() {
        let e = entities(2);
        let (state, _) = run(&[
            down(e[0], PRESS),
            dragged(IVec2::new(50, 0)),
            moved_by(e[1], IVec2::new(50, 0)),
        ]);
        assert!(is_pressed(&state), "{state:?}");
    }

    /// Text selection: the pointer moves, and much later the app moves its
    /// window by the same amount.
    #[test]
    fn a_window_that_follows_only_after_the_confirm_window_is_not_a_drag() {
        let w = entities(1)[0];
        let (state, _) = run(&[down(w, PRESS), dragged(IVec2::new(50, 0))]);
        assert!(is_pressed(&state), "{state:?}");

        let (state, drops) = run_from(state, &[moved_by(w, IVec2::new(50, 0))], ms(400));
        assert!(matches!(state, DragState::Unconfirmed(_)), "{state:?}");
        assert_eq!(drops, vec![]);
    }

    #[test]
    fn a_right_press_leaves_a_held_left_gesture_unchanged() {
        let w = entities(1)[0];
        let (pressed, _) = run(&[down(w, PRESS)]);
        let right = DragInput::Down {
            button: MouseButton::Right,
            point: PRESS,
            window: None,
        };
        for state in [pressed, dragging(w)] {
            let (next, drop) = step(state.clone(), right, ms(100), &hit_at);
            assert_eq!(next, state, "a right press must not replace the gesture");
            assert_eq!(drop, None, "a right press must not drop the window");
        }
    }

    #[test]
    fn a_drag_drops_where_it_is_released() {
        let w = entities(1)[0];
        let (state, drops) = run_from(dragging(w), &[dragged(IVec2::new(200, 170))], ms(100));
        assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
        assert_eq!(drops, vec![]);

        let release = IVec2::new(320, 200);
        let (state, drops) = run_from(state, &[DragInput::Up { point: release }], ms(200));
        assert_eq!(
            drops,
            vec![DragDrop {
                window: w,
                target: hit_at(w, release, 0),
            }]
        );
        assert_eq!(state.settling(), Some(w));
    }

    #[test]
    fn a_lost_release_returns_the_window() {
        let w = entities(1)[0];
        let (state, drops) = run_from(dragging(w), &[DragInput::Moved], ms(100));
        assert_eq!(state, DragState::Idle);
        assert_eq!(drops, home(w));

        let (state, drops) = run_from(dragging(w), &[down(w, PRESS)], ms(100));
        assert!(is_pressed(&state), "a new press starts afresh");
        assert_eq!(drops, home(w));
    }

    #[test]
    fn a_button_read_up_for_the_grace_ends_a_press_or_drag() {
        let w = entities(1)[0];
        let up = DragInput::Tick { button_down: false };
        let (pressed, _) = run(&[down(w, PRESS)]);

        let (state, _) = run_from(pressed, &[up], ms(100));
        assert!(is_pressed(&state), "the button just read up: {state:?}");
        let (state, drops) = run_from(state, &[up], ms(100) + RELEASE_GRACE + ms(1));
        assert_eq!(state, DragState::Idle);
        assert_eq!(drops, vec![]);

        let (state, _) = run_from(dragging(w), &[up], ms(100));
        let (state, drops) = run_from(state, &[up], ms(100) + RELEASE_GRACE + ms(1));
        assert_eq!(state, DragState::Idle);
        assert_eq!(drops, home(w));
    }

    #[test]
    fn a_read_of_the_button_down_starts_the_release_grace_over() {
        let w = entities(1)[0];
        let up = DragInput::Tick { button_down: false };
        let held = DragInput::Tick { button_down: true };

        let (state, _) = run_from(dragging(w), &[up], ms(600));
        let (state, _) = run_from(state, &[held], ms(700));
        let (state, _) = run_from(state, &[up], ms(800));
        let (state, drops) = run_from(state, &[up], ms(799) + RELEASE_GRACE);
        assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
        assert_eq!(drops, vec![]);
    }

    /// Every press ends: a button stuck down can not hold a window that never
    /// followed the pointer. A drag held still with the button down goes on.
    #[test]
    fn a_press_held_down_ends_at_its_limit() {
        let w = entities(1)[0];
        let held = DragInput::Tick { button_down: true };
        let (pressed, _) = run(&[down(w, PRESS)]);

        let (state, _) = run_from(pressed.clone(), &[held], Duration::from_secs(9));
        assert!(is_pressed(&state), "{state:?}");
        let (state, drops) = run_from(pressed, &[held], PRESS_LIMIT + ms(1));
        assert_eq!((state, drops), (DragState::Idle, vec![]));

        let (moved, _) = run(&[down(w, PRESS), dragged(IVec2::new(50, 0))]);
        let (state, _) = run_from(moved.clone(), &[held], ms(20) + CONFIRM_WINDOW);
        assert!(is_pressed(&state), "{state:?}");
        let (state, _) = run_from(moved, &[held], ms(21) + CONFIRM_WINDOW);
        assert!(matches!(state, DragState::Unconfirmed(_)), "{state:?}");
        let (still, _) = run_from(state.clone(), &[held], Duration::from_secs(9));
        assert_eq!(still, state, "held until the limit");
        let (state, drops) = run_from(state, &[held], PRESS_LIMIT + ms(1));
        assert_eq!((state, drops), (DragState::Idle, home(w)));

        let (state, drops) = run_from(dragging(w), &[held], Duration::from_secs(60));
        assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
        assert_eq!(drops, vec![]);
    }

    fn unconfirmed(w: Entity) -> DragState {
        let (state, _) = run(&[down(w, PRESS), dragged(IVec2::new(50, 0))]);
        let (state, _) = run_from(state, &[moved_by(w, IVec2::new(0, 50))], ms(40));
        let (state, _) = run_from(
            state,
            &[DragInput::Tick { button_down: true }],
            ms(21) + CONFIRM_WINDOW,
        );
        assert!(matches!(state, DragState::Unconfirmed(_)), "{state:?}");
        state
    }

    fn released(w: Entity, point: IVec2) -> DragState {
        let (state, _) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(50, 0)),
            DragInput::Up { point },
        ]);
        assert!(matches!(state, DragState::Released { .. }), "{state:?}");
        state
    }

    /// A press holds its window once the pointer has passed the threshold,
    /// because macOS may be moving it. A click holds nothing.
    #[test]
    fn a_press_holds_its_window_once_the_pointer_moves() {
        let w = entities(1)[0];
        let (clicked, _) = run(&[down(w, PRESS)]);
        assert_eq!(clicked.held(), None);
        let (moved, _) = run(&[down(w, PRESS), dragged(IVec2::new(50, 0))]);
        assert_eq!(moved.held(), Some(w));
        for state in [unconfirmed(w), released(w, PRESS), dragging(w)] {
            assert_eq!(state.held(), Some(w), "{state:?}");
        }

        let (state, drops) = run(&[down(w, PRESS), DragInput::Up { point: PRESS }]);
        assert_eq!((state, drops), (DragState::Idle, vec![]), "a click");
    }

    /// The release comes before the window's move. The drop waits for the
    /// move and lands where the button came up.
    #[test]
    fn a_quick_drop_waits_for_its_window_and_drops_at_the_release() {
        let w = entities(1)[0];
        let release = IVec2::new(320, 200);
        let state = released(w, release);

        let (state, drops) = run_from(
            state,
            &[
                DragInput::Dragged { point: PRESS },
                DragInput::Moved,
                DragInput::Tick { button_down: false },
            ],
            ms(60),
        );
        assert!(matches!(state, DragState::Released { .. }), "{state:?}");
        assert_eq!(drops, vec![]);

        let (state, drops) = run_from(state, &[moved_by(w, IVec2::new(50, 0))], ms(200));
        assert_eq!(state.settling(), Some(w));
        assert_eq!(
            drops,
            vec![DragDrop {
                window: w,
                target: hit_at(w, release, 0),
            }]
        );
    }

    /// A released press whose window never follows, follows too late, or
    /// changes size sends the window back to its slot.
    #[test]
    fn a_released_press_whose_window_does_not_follow_goes_home() {
        let w = entities(1)[0];
        let tick = DragInput::Tick { button_down: false };
        let late = ms(21) + CONFIRM_WINDOW;

        let (state, drops) = run_from(released(w, PRESS), &[tick], late);
        assert_eq!((state, drops), (DragState::Idle, home(w)));
        let (state, drops) = run_from(released(w, PRESS), &[moved_by(w, IVec2::new(50, 0))], late);
        assert_eq!((state, drops), (DragState::Idle, home(w)));
        let (state, drops) = run_from(
            released(w, PRESS),
            &[DragInput::WindowResized { window: w }],
            ms(60),
        );
        assert_eq!((state, drops), (DragState::Idle, home(w)));
    }

    /// An unconfirmed press holds its window while the button is down, and
    /// sends it back to its slot when the press ends.
    #[test]
    fn an_unconfirmed_press_holds_its_window_until_the_release() {
        let w = entities(1)[0];
        let state = unconfirmed(w);
        let held = [
            DragInput::Dragged { point: PRESS },
            moved_by(w, IVec2::new(50, 0)),
            DragInput::WindowResized { window: w },
            DragInput::Tick { button_down: true },
        ];
        let (next, drops) = run_from(state.clone(), &held, ms(400));
        assert!(matches!(next, DragState::Unconfirmed(_)), "{next:?}");
        assert_eq!(drops, vec![]);

        let up = DragInput::Tick { button_down: false };
        for ending in [
            vec![DragInput::Up { point: PRESS }],
            vec![DragInput::Moved],
            vec![up, up],
            vec![down(w, PRESS)],
            vec![DragInput::Cancel],
        ] {
            let mut times = [ms(400), ms(401) + RELEASE_GRACE].into_iter();
            let (mut next, mut drops) = (state.clone(), Vec::new());
            for input in &ending {
                let (after, drop) = step(next, *input, times.next().unwrap(), &hit_at);
                next = after;
                drops.extend(drop);
            }
            assert_eq!(drops, home(w), "{ending:?}");
            assert_eq!(next.held(), None, "{ending:?}");
        }
    }

    /// A resize is not a drag. It ends the press without sending the window
    /// back, so the window stays where the resize puts it.
    #[test]
    fn a_resize_ends_a_press_without_moving_the_window() {
        let w = entities(1)[0];
        let (state, drops) = run(&[
            down(w, PRESS),
            dragged(IVec2::new(0, -50)),
            DragInput::WindowResized { window: w },
        ]);
        assert_eq!((state, drops), (DragState::Idle, vec![]));
    }

    /// A window on a display's left edge moves only down while the pointer
    /// moves right and down. Its vertical move alone confirms the drag, but
    /// only when it is large, and only for a window at an edge.
    #[test]
    fn a_window_at_a_screen_edge_confirms_a_drag_on_its_free_axis() {
        let w = entities(1)[0];
        let at_edges = |screen_edges| DragInput::Down {
            point: PRESS,
            button: MouseButton::Left,
            window: Some(PressedWindow {
                entity: w,
                frame: FRAME,
                screen_edges,
            }),
        };
        let left_edge = BVec2::new(true, false);
        let bottom_edge = BVec2::new(false, true);
        let confirms = |screen_edges, pointer: IVec2, window: IVec2| {
            let (state, _) = run(&[
                at_edges(screen_edges),
                dragged(pointer),
                moved_by(w, window),
            ]);
            matches!(state, DragState::Dragging(_))
        };

        assert!(confirms(left_edge, IVec2::new(30, 300), IVec2::new(0, 300)));
        assert!(confirms(
            bottom_edge,
            IVec2::new(300, 30),
            IVec2::new(300, 0)
        ));
        assert!(
            !confirms(BVec2::FALSE, IVec2::new(30, 300), IVec2::new(0, 300)),
            "off the edges both axes must match"
        );
        assert!(
            !confirms(bottom_edge, IVec2::new(30, 300), IVec2::new(0, 300)),
            "the held axis must be the one at the edge"
        );
        assert!(
            !confirms(left_edge, IVec2::new(30, 20), IVec2::new(0, 20)),
            "a move under STRONG_MOVE confirms nothing"
        );
        assert!(
            !confirms(left_edge, IVec2::new(180, 0), IVec2::new(0, 240)),
            "a window moving across the pointer"
        );
    }

    #[test]
    fn a_frame_at_a_display_edge_marks_that_axis() {
        let display = IRect::new(0, 0, 1024, 768);
        let frame = |x, y, w, h| IRect::new(x, y, x + w, y + h);
        assert_eq!(
            screen_edges(frame(0, 20, 400, 748), display),
            BVec2::new(true, true)
        );
        assert_eq!(
            screen_edges(frame(400, 20, 400, 700), display),
            BVec2::FALSE
        );
        assert_eq!(
            screen_edges(frame(630, 20, 400, 700), display),
            BVec2::new(true, false),
            "within the tolerance of the right edge"
        );
        assert_eq!(
            screen_edges(frame(400, 30, 400, 700), IRect::new(0, 30, 1024, 768)),
            BVec2::new(false, true),
            "the top edge of the display"
        );
    }

    #[test]
    fn a_held_window_that_goes_away_ends_the_drag() {
        let e = entities(2);
        let (pressed, _) = run(&[down(e[0], PRESS)]);
        let (settling, _) = run_from(
            dragging(e[0]),
            &[DragInput::Up {
                point: IVec2::new(300, 200),
            }],
            ms(100),
        );
        for state in [pressed, dragging(e[0]), settling] {
            let gone = |window| [DragInput::WindowGone { window }];
            let (next, _) = run_from(state.clone(), &gone(e[1]), ms(120));
            assert_eq!(next, state, "another window");
            let (next, drops) = run_from(state.clone(), &gone(e[0]), ms(120));
            assert_eq!(next, DragState::Idle, "{state:?}");
            assert_eq!(drops, vec![], "{state:?}");
        }
    }

    #[test]
    fn a_cancel_ends_a_press_and_returns_a_dragged_window() {
        let w = entities(1)[0];
        let (pressed, _) = run(&[down(w, PRESS)]);

        assert_eq!(
            run_from(pressed, &[DragInput::Cancel], ms(100)),
            (DragState::Idle, vec![])
        );
        assert_eq!(
            run_from(released(w, PRESS), &[DragInput::Cancel], ms(100)),
            (DragState::Idle, home(w))
        );
        assert_eq!(
            run_from(dragging(w), &[DragInput::Cancel], ms(100)),
            (DragState::Idle, home(w))
        );
    }

    #[test]
    fn settling_ends_at_its_deadline_or_at_the_next_press() {
        let w = entities(1)[0];
        let (settling, _) = run_from(
            dragging(w),
            &[DragInput::Up {
                point: IVec2::new(300, 200),
            }],
            ms(100),
        );
        let tick = DragInput::Tick { button_down: false };

        let (state, _) = run_from(settling.clone(), &[tick], ms(200));
        assert_eq!(state.settling(), Some(w), "before the deadline");
        let (state, _) = run_from(settling.clone(), &[tick], ms(100) + SETTLE_WINDOW);
        assert_eq!(state, DragState::Idle, "at the deadline");
        let (state, _) = run_from(settling, &[down(w, PRESS)], ms(200));
        assert!(is_pressed(&state), "a new press");
    }

    const BOUNDS: IRect = IRect {
        min: IVec2::new(0, 0),
        max: IVec2::new(1024, 768),
    };

    fn band(windows: &[Entity], y: Range<i32>) -> Band {
        Band {
            windows: windows.to_vec(),
            y,
        }
    }

    fn column(index: usize, x: Range<i32>, bands: Vec<Band>) -> ColumnSnap {
        ColumnSnap {
            index,
            x,
            bands,
            kind: ColumnKind::Split,
        }
    }

    /// The strip of the tests' snapshots.
    const STRIP: Entity = Entity::PLACEHOLDER;

    /// A snapshot of `columns` on a 1024 by 768 display whose viewport, below
    /// a 20 point menu bar, is `height` points tall.
    fn snapshot(height: i32, columns: Vec<ColumnSnap>) -> LayoutSnapshot {
        LayoutSnapshot {
            strip: STRIP,
            bounds: BOUNDS,
            viewport: IRect::new(0, 20, 1024, 20 + height),
            columns,
        }
    }

    /// `A | [B, C] | D | E`, 400 points per column, with E past the right
    /// edge, on a viewport that fits three windows in a column.
    fn four_columns(e: &[Entity]) -> LayoutSnapshot {
        snapshot(
            748,
            vec![
                column(0, 0..400, vec![band(&[e[0]], 20..768)]),
                column(
                    1,
                    400..800,
                    vec![band(&[e[1]], 20..394), band(&[e[2]], 394..768)],
                ),
                column(2, 800..1200, vec![band(&[e[3]], 20..768)]),
                column(3, 1200..1600, vec![band(&[e[4]], 20..768)]),
            ],
        )
    }

    fn into(column: usize, slot: usize) -> DropTarget {
        DropTarget::IntoColumn {
            strip: STRIP,
            column,
            slot,
        }
    }

    fn new_column(index: usize) -> DropTarget {
        DropTarget::NewColumn {
            strip: STRIP,
            index,
        }
    }

    fn hit(snapshot: &LayoutSnapshot, dragged: Entity, x: i32, y: i32) -> DropTarget {
        hit_test(std::slice::from_ref(snapshot), dragged, IVec2::new(x, y), 0)
    }

    #[test]
    fn the_middle_half_of_a_column_drops_into_it_between_its_windows() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(hit(&snap, e[4], 200, 100), into(0, 0), "above the middle");
        assert_eq!(hit(&snap, e[4], 200, 700), into(0, 1), "below the middle");
        assert_eq!(hit(&snap, e[4], 600, 30), into(1, 0), "above the top");
        assert_eq!(hit(&snap, e[4], 600, 300), into(1, 1), "between");
        assert_eq!(hit(&snap, e[4], 600, 767), into(1, 2), "the bottom");
    }

    #[test]
    fn the_outer_quarters_of_a_column_open_a_new_column_beside_it() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(hit(&snap, e[4], 410, 300), new_column(1));
        assert_eq!(hit(&snap, e[4], 499, 300), new_column(1));
        assert_eq!(hit(&snap, e[4], 500, 300), into(1, 1));
        assert_eq!(hit(&snap, e[4], 699, 300), into(1, 1));
        assert_eq!(hit(&snap, e[4], 700, 300), new_column(2));
    }

    /// A window grabbed near one end of its title bar puts the pointer in a
    /// column's outer quarter while the window's centre is over the middle
    /// half, so the centre, not the pointer, picks the zone.
    #[test]
    fn the_dragged_windows_centre_picks_the_zone_not_the_pointer() {
        let e = entities(5);
        let snap = four_columns(&e);
        let at = |x, centre_dx| {
            hit_test(
                std::slice::from_ref(&snap),
                e[4],
                IVec2::new(x, 300),
                centre_dx,
            )
        };

        assert_eq!(at(790, -190), into(1, 1), "grabbed near the right end");
        assert_eq!(at(410, 190), into(1, 1), "grabbed near the left end");
        assert_eq!(at(600, 150), new_column(2), "centre over the right quarter");
        assert_eq!(at(600, -150), new_column(1), "centre over the left quarter");
    }

    /// The window's centre counts as the nearest point on the display, so a
    /// centre past the edge never picks a column scrolled off the display.
    #[test]
    fn a_centre_off_the_display_counts_as_its_nearest_edge() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(
            hit_test(&[snap], e[0], IVec2::new(1000, 300), 400),
            into(2, 0),
            "column 2 under the edge, not column 3 past it"
        );
    }

    #[test]
    fn empty_space_opens_a_new_column_at_the_nearest_end() {
        let e = entities(3);
        let snap = snapshot(
            748,
            vec![
                column(0, 200..500, vec![band(&[e[0]], 20..768)]),
                column(
                    1,
                    500..800,
                    vec![band(&[e[1]], 20..394), band(&[e[2]], 394..768)],
                ),
            ],
        );

        assert_eq!(hit(&snap, e[1], 50, 300), new_column(0));
        assert_eq!(hit(&snap, e[1], 900, 300), new_column(2));
    }

    #[test]
    fn a_drop_on_its_own_lone_column_goes_home() {
        let e = entities(5);
        let snap = four_columns(&e);

        for x in [10, 200, 390, 410] {
            assert_eq!(hit(&snap, e[0], x, 300), DropTarget::Home, "x {x}");
        }
        assert_eq!(hit(&snap, e[3], 790, 300), DropTarget::Home, "its gap");
    }

    #[test]
    fn a_drop_on_its_own_slot_in_its_stack_goes_home() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(hit(&snap, e[1], 600, 100), DropTarget::Home, "B over B");
        assert_eq!(hit(&snap, e[1], 600, 300), DropTarget::Home, "B above C");
        assert_eq!(hit(&snap, e[1], 600, 700), into(1, 1), "B below C");
        assert_eq!(hit(&snap, e[2], 600, 700), DropTarget::Home, "C over C");
        assert_eq!(hit(&snap, e[2], 600, 100), into(1, 0), "C above B");
    }

    /// A viewport 400 points tall fits two windows at the 200 point minimum
    /// height. A reorder never adds a window, so it is never refused.
    #[test]
    fn a_column_without_room_for_another_window_refuses_a_drop() {
        let e = entities(5);
        let mut snap = four_columns(&e);
        assert_eq!(hit(&snap, e[0], 600, 300), into(1, 1), "748 points fit 3");

        snap.viewport = IRect::new(0, 20, 1024, 420);
        assert_eq!(hit(&snap, e[0], 600, 300), DropTarget::Refused);
        assert_eq!(hit(&snap, e[1], 600, 700), into(1, 1), "a reorder");
        assert_eq!(hit(&snap, e[2], 600, 100), into(1, 0), "a reorder");
        assert_eq!(hit(&snap, e[1], 410, 300), new_column(1), "out of it");
    }

    #[test]
    fn a_tabbed_stack_always_has_room() {
        let e = entities(4);
        let mut snap = snapshot(
            400,
            vec![
                column(0, 0..400, vec![band(&[e[0]], 20..420)]),
                column(
                    1,
                    400..800,
                    vec![
                        band(&[e[1]], 20..20),
                        band(&[e[2]], 20..20),
                        band(&[e[3]], 20..420),
                    ],
                ),
            ],
        );
        assert_eq!(hit(&snap, e[0], 600, 300), DropTarget::Refused);

        snap.columns[1].kind = ColumnKind::Tabbed;
        assert_eq!(hit(&snap, e[0], 600, 300), into(1, 3));
    }

    #[test]
    fn a_fullscreen_column_refuses_a_drop() {
        let e = entities(2);
        let mut snap = snapshot(
            748,
            vec![
                column(0, 0..400, vec![band(&[e[0]], 20..768)]),
                column(1, 400..800, vec![band(&[e[1]], 20..768)]),
            ],
        );
        snap.columns[1].kind = ColumnKind::Fullscreen;

        assert_eq!(hit(&snap, e[0], 600, 300), DropTarget::Refused);
        assert_eq!(hit(&snap, e[0], 790, 300), new_column(2), "beside it");
    }

    #[test]
    fn a_native_tab_group_counts_as_one_window() {
        let e = entities(4);
        let mut snap = snapshot(
            748,
            vec![
                column(0, 0..400, vec![band(&[e[0], e[1]], 20..768)]),
                column(
                    1,
                    400..800,
                    vec![band(&[e[2]], 20..394), band(&[e[3]], 394..768)],
                ),
            ],
        );

        assert_eq!(hit(&snap, e[1], 200, 300), DropTarget::Home);
        assert_eq!(hit(&snap, e[1], 600, 300), into(1, 1));
        snap.viewport = IRect::new(0, 20, 1024, 420);
        assert_eq!(hit(&snap, e[2], 200, 300), into(0, 0), "two fit");
    }

    #[test]
    fn a_pointer_off_the_display_goes_home() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(hit(&snap, e[4], -50, 300), DropTarget::Home);
        assert_eq!(hit(&snap, e[4], 200, 900), DropTarget::Home);
        assert_eq!(
            hit_test(&[], e[4], IVec2::new(200, 300), 0),
            DropTarget::Home,
            "no strip"
        );
    }

    /// The strip `strip` of a 1920 by 1200 display right of the first one,
    /// whose viewport, below a 20 point menu bar, is `height` points tall.
    fn other_display(strip: Entity, height: i32, columns: Vec<ColumnSnap>) -> LayoutSnapshot {
        LayoutSnapshot {
            strip,
            bounds: IRect::new(1024, 0, 2944, 1200),
            viewport: IRect::new(1024, 20, 2944, 20 + height),
            columns,
        }
    }

    /// `A | [B, C] | D | E` on the first display, and the strip `e[5]` with
    /// `e[6]` in an 800 point column at the left edge of the second.
    fn two_displays(e: &[Entity], height: i32) -> [LayoutSnapshot; 2] {
        [
            four_columns(e),
            other_display(
                e[5],
                height,
                vec![column(0, 1024..1824, vec![band(&[e[6]], 20..20 + height)])],
            ),
        ]
    }

    /// Column 3 of the first strip is scrolled to x = 1200, past its
    /// display's edge, so it is under a pointer on the second display. Only
    /// the second display's strip takes a drop there.
    #[test]
    fn the_pointer_picks_the_display_whose_strip_takes_the_drop() {
        let e = entities(7);
        let snaps = two_displays(&e, 1180);
        let at = |x, y| hit_test(&snaps, e[4], IVec2::new(x, y), 0);
        let there = |column, slot| DropTarget::IntoColumn {
            strip: e[5],
            column,
            slot,
        };

        assert_eq!(at(1424, 900), there(0, 1), "below the middle");
        assert_eq!(at(1424, 100), there(0, 0), "above the middle");
        assert_eq!(
            at(1804, 600),
            DropTarget::NewColumn {
                strip: e[5],
                index: 1
            },
            "the outer quarter"
        );
        assert_eq!(
            at(2900, 600),
            DropTarget::NewColumn {
                strip: e[5],
                index: 1
            },
            "empty space"
        );
        assert_eq!(at(600, 300), into(1, 1), "its own display");
        assert_eq!(at(3000, 300), DropTarget::Home, "off every display");
    }

    /// The second display's viewport is 280 points tall, room for one window
    /// at the 200 point minimum height. The first one's fits three.
    #[test]
    fn each_display_decides_by_its_own_height_whether_a_column_has_room() {
        let e = entities(7);
        let snaps = two_displays(&e, 280);

        assert_eq!(
            hit_test(&snaps, e[4], IVec2::new(1424, 200), 0),
            DropTarget::Refused
        );
        assert_eq!(hit_test(&snaps, e[4], IVec2::new(600, 300), 0), into(1, 1));
    }

    /// The window's centre is still on the first display, 200 points left of
    /// a pointer on the second, so it counts as the second display's edge.
    #[test]
    fn a_centre_off_the_pointers_display_counts_as_the_nearest_edge_of_it() {
        let e = entities(7);
        let snaps = two_displays(&e, 1180);

        assert_eq!(
            hit_test(&snaps, e[4], IVec2::new(1030, 600), -200),
            DropTarget::NewColumn {
                strip: e[5],
                index: 0
            }
        );
    }

    #[test]
    fn an_empty_strip_takes_a_new_column() {
        let e = entities(7);
        let snaps = [four_columns(&e), other_display(e[5], 1180, vec![])];

        assert_eq!(
            hit_test(&snaps, e[4], IVec2::new(1500, 600), 0),
            DropTarget::NewColumn {
                strip: e[5],
                index: 0
            }
        );
    }

    /// The overlay shape for a drag of `dragged` with the pointer at (600,
    /// 300), whose release would land at `target`.
    fn shape(
        snapshot: &LayoutSnapshot,
        dragged: Entity,
        target: DropTarget,
    ) -> Option<OverlayShape> {
        overlay_shape(
            std::slice::from_ref(snapshot),
            dragged,
            IVec2::new(600, 300),
            target,
        )
    }

    fn line(x: Range<i32>, y: Range<i32>) -> OverlayShape {
        OverlayShape::Line(IRect::new(x.start, y.start, x.end, y.end))
    }

    #[test]
    fn a_drop_into_a_column_shows_a_line_across_it_at_the_gap() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(
            shape(&snap, e[4], into(1, 1)),
            Some(line(400..800, 393..396))
        );
    }

    #[test]
    fn a_line_at_the_top_or_bottom_of_a_column_stays_on_screen() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(
            shape(&snap, e[4], into(1, 0)),
            Some(line(400..800, 20..23)),
            "top"
        );
        assert_eq!(
            shape(&snap, e[4], into(1, 2)),
            Some(line(400..800, 765..768)),
            "bottom"
        );
        assert_eq!(
            shape(&snap, e[4], into(0, 1)),
            Some(line(0..400, 765..768)),
            "below a lone window"
        );
    }

    #[test]
    fn a_line_across_a_column_partly_off_screen_stops_at_the_screen_edge() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(
            shape(&snap, e[0], into(2, 0)),
            Some(line(800..1024, 20..23))
        );
        assert_eq!(shape(&snap, e[0], into(3, 0)), None, "wholly off screen");
    }

    #[test]
    fn a_reorder_shows_the_line_between_the_other_windows_of_its_stack() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(
            shape(&snap, e[1], into(1, 1)),
            Some(line(400..800, 765..768)),
            "B under C"
        );
        assert_eq!(
            shape(&snap, e[2], into(1, 0)),
            Some(line(400..800, 20..23)),
            "C over B"
        );
    }

    #[test]
    fn a_new_column_shows_a_line_down_the_full_height_of_its_gap() {
        let e = entities(5);
        let snap = four_columns(&e);

        assert_eq!(
            shape(&snap, e[4], new_column(1)),
            Some(line(399..402, 20..768)),
            "between A and B"
        );
        assert_eq!(
            shape(&snap, e[4], new_column(0)),
            Some(line(0..3, 20..768)),
            "before the first column"
        );
        assert_eq!(shape(&snap, e[4], new_column(5)), None, "no such gap");
    }

    #[test]
    fn a_new_column_in_a_padded_gap_shows_its_line_in_the_middle() {
        let e = entities(3);
        let snap = snapshot(
            748,
            vec![
                column(0, 200..500, vec![band(&[e[0]], 20..768)]),
                column(1, 510..800, vec![band(&[e[1]], 20..768)]),
            ],
        );

        assert_eq!(
            shape(&snap, e[2], new_column(1)),
            Some(line(504..507, 20..768)),
            "between the columns"
        );
        assert_eq!(
            shape(&snap, e[2], new_column(2)),
            Some(line(799..802, 20..768)),
            "after the last column"
        );
    }

    #[test]
    fn a_refused_drop_shows_the_refusal_symbol_beside_the_pointer() {
        let e = entities(5);

        assert_eq!(
            shape(&four_columns(&e), e[4], DropTarget::Refused),
            Some(OverlayShape::Refusal(IRect::new(614, 314, 636, 336)))
        );
    }

    #[test]
    fn a_drop_home_shows_nothing() {
        let e = entities(5);

        assert_eq!(shape(&four_columns(&e), e[4], DropTarget::Home), None);
    }

    #[test]
    fn a_viewport_too_small_for_a_line_shows_nothing() {
        let e = entities(3);
        for span in 0..INSERTION_LINE {
            let mut snap = snapshot(
                span,
                vec![
                    column(0, 0..400, vec![band(&[e[0]], 20..20 + span)]),
                    column(1, 400..800, vec![band(&[e[1]], 20..20 + span)]),
                ],
            );
            assert_eq!(shape(&snap, e[2], into(1, 0)), None, "{span} pt high");

            snap.viewport = IRect::new(400, 20, 400 + span, 768);
            assert_eq!(shape(&snap, e[2], new_column(1)), None, "{span} pt wide");
        }
    }

    #[test]
    fn a_line_shows_on_the_display_of_the_strip_it_drops_into() {
        let e = entities(7);
        let snaps = two_displays(&e, 1180);
        let target = DropTarget::IntoColumn {
            strip: e[5],
            column: 0,
            slot: 1,
        };

        assert_eq!(
            overlay_shape(&snaps, e[4], IVec2::new(1424, 900), target),
            Some(line(1024..1824, 1197..1200))
        );
    }

    #[test]
    fn a_new_column_on_an_empty_strip_shows_its_line_at_the_left_edge() {
        let e = entities(7);
        let snaps = [four_columns(&e), other_display(e[5], 1180, vec![])];
        let new_column_of = |strip| DropTarget::NewColumn { strip, index: 0 };

        assert_eq!(
            overlay_shape(&snaps, e[4], IVec2::new(1500, 600), new_column_of(e[5])),
            Some(line(1024..1027, 20..1200))
        );
        assert_eq!(
            overlay_shape(&snaps, e[4], IVec2::new(1500, 600), new_column_of(e[6])),
            None,
            "a strip with no snapshot"
        );
    }

    /// The edge `pointer` holds on the display of [`snapshot`], with the
    /// displays of `others` around it.
    fn edge_at(others: &[IRect], x: i32, y: i32) -> Option<EdgeSide> {
        let mut displays = vec![BOUNDS];
        displays.extend_from_slice(others);
        let snapshots = [snapshot(748, Vec::new())];
        held_edge(&snapshots, &displays, IVec2::new(x, y)).map(|(strip, side)| {
            assert_eq!(strip, STRIP);
            side
        })
    }

    #[test]
    fn the_pointer_holds_an_edge_within_six_points_of_it() {
        assert_eq!(edge_at(&[], 0, 500), Some(EdgeSide::Left));
        assert_eq!(edge_at(&[], 5, 500), Some(EdgeSide::Left));
        assert_eq!(edge_at(&[], 6, 500), None);
        assert_eq!(edge_at(&[], 512, 500), None);
        assert_eq!(edge_at(&[], 1017, 500), None);
        assert_eq!(edge_at(&[], 1018, 500), Some(EdgeSide::Right));
        assert_eq!(edge_at(&[], 1024, 500), Some(EdgeSide::Right));
        assert_eq!(edge_at(&[], 2, 900), None, "off the display");
    }

    /// A display touches an edge when its own edge is at most two points
    /// away and it spans the pointer's height, whether or not it shows a
    /// strip.
    #[test]
    fn a_display_touching_an_edge_at_the_pointers_height_stops_the_hold() {
        for side in [EdgeSide::Left, EdgeSide::Right] {
            for (gap, top, bottom, y, holds) in [
                (0, 0, 768, 500, false),
                (2, 0, 768, 500, false),
                (3, 0, 768, 500, true),
                (0, 768, 1536, 500, true),
                (0, -768, 0, 500, true),
                (0, 400, 768, 399, true),
                (0, 400, 768, 400, false),
                (0, 0, 400, 399, false),
                (0, 0, 400, 400, true),
            ] {
                let (other, x) = match side {
                    EdgeSide::Left => (IRect::new(-gap - 1920, top, -gap, bottom), 2),
                    EdgeSide::Right => (IRect::new(1024 + gap, top, 2944 + gap, bottom), 1022),
                };
                assert_eq!(
                    edge_at(&[other], x, y),
                    holds.then_some(side),
                    "{side:?}, gap {gap}, {top}..{bottom}, y {y}"
                );
            }
        }
    }

    /// With the pointer on the second display, its strip's edge is held.
    #[test]
    fn the_pointer_holds_the_edge_of_the_display_it_is_on() {
        let e = entities(7);
        let snaps = two_displays(&e, 1180);
        let displays = [BOUNDS, snaps[1].bounds];
        let at = |x| held_edge(&snaps, &displays, IVec2::new(x, 500));
        assert_eq!(at(2942), Some((e[5], EdgeSide::Right)));
        assert_eq!(at(1026), None, "beside the first display");
        assert_eq!(at(1022), None, "beside the second display");
    }

    /// `A | [B, C] | D | E` scrolled `by` points right.
    fn scrolled(e: &[Entity], by: i32) -> LayoutSnapshot {
        let mut snapshot = four_columns(e);
        for column in &mut snapshot.columns {
            column.x = column.x.start + by..column.x.end + by;
        }
        snapshot
    }

    /// Each scroll shows the nearest column that reaches past the held edge
    /// in full at that edge, whether it is partly on screen or not at all. A
    /// column that ends at the edge reaches no further.
    #[test]
    fn an_edge_scroll_shows_the_nearest_column_past_the_edge() {
        let e = entities(5);
        assert_eq!(edge_shift(&scrolled(&e, 0), EdgeSide::Right), Some(-176));
        assert_eq!(edge_shift(&scrolled(&e, 0), EdgeSide::Left), None);
        assert_eq!(edge_shift(&scrolled(&e, -700), EdgeSide::Left), Some(300));
        assert_eq!(edge_shift(&scrolled(&e, -1200), EdgeSide::Left), Some(400));
        assert_eq!(edge_shift(&scrolled(&e, -576), EdgeSide::Right), None);
        assert_eq!(edge_shift(&scrolled(&e, -575), EdgeSide::Right), Some(-1));
    }

    /// With nothing to draw, the event pump can wait up to 500 ms for an
    /// event, or 2 s in low power mode. An edge hold keeps it drawing frames,
    /// so the strip scrolls on time with the pointer still.
    #[test]
    fn an_edge_hold_keeps_the_event_pump_drawing_frames() {
        use bevy::ecs::system::RunSystemOnce as _;

        use crate::ecs::params::FrameActivity;

        let mut world = World::new();
        let mid_frame = |world: &mut World| {
            world
                .run_system_once(|activity: FrameActivity| activity.mid_frame())
                .expect("system runs")
        };
        assert!(!mid_frame(&mut world));
        world.spawn(EdgeHold {
            strip: STRIP,
            side: EdgeSide::Left,
            next_scroll: Duration::ZERO,
        });
        assert!(mid_frame(&mut world));
    }

    /// Which reports that macOS lists a window in another display's space
    /// [`DragPlacements::keeps`] ignores.
    #[test]
    fn a_stale_space_report_keeps_only_a_held_or_returned_window_in_place() {
        use bevy::ecs::system::RunSystemOnce as _;

        let mut world = World::new();
        let main = world
            .spawn(Display::new(1, IRect::new(0, 0, 1024, 768), 20))
            .id();
        let ext = world
            .spawn(Display::new(2, IRect::new(1024, 0, 2944, 1200), 20))
            .id();
        let e = (0..4).map(|_| world.spawn_empty().id()).collect::<Vec<_>>();
        let (returned, tab, other) = (e[0], e[1], e[2]);
        let mut strip = LayoutStrip::default();
        strip.append(other);
        strip.append_tab_group(&[returned, tab]);
        world.spawn(strip);
        world.spawn(DragReturnedFrom {
            window: returned,
            display: 2,
        });
        let on_main = Some(IRect::new(600, 20, 1000, 768));
        let on_ext = Some(IRect::new(1200, 20, 1600, 768));

        let keeps = |world: &mut World, state: DragState, window, frame, listed_on| {
            world.insert_resource(state);
            world
                .run_system_once(move |drags: DragPlacements, strips: Query<&LayoutStrip>| {
                    let strip = strips.single().expect("one strip");
                    drags.keeps(window, frame, listed_on, Some((strip, main)))
                })
                .expect("keeps runs")
        };
        let idle = DragState::Idle;
        let settling = DragState::Settling(Settle {
            window: tab,
            deadline: Duration::from_secs(1),
        });

        assert!(keeps(
            &mut world,
            idle.clone(),
            returned,
            on_main,
            Some(ext)
        ));
        assert!(
            !keeps(&mut world, idle.clone(), returned, on_ext, Some(ext)),
            "the window is on the display that lists it"
        );
        assert!(
            !keeps(&mut world, idle.clone(), returned, on_main, Some(main)),
            "it came back from another display"
        );
        assert!(
            !keeps(&mut world, idle.clone(), returned, on_main, None),
            "listed on no display"
        );
        assert!(
            !keeps(&mut world, idle.clone(), tab, on_main, Some(ext)),
            "no guard"
        );
        assert!(keeps(&mut world, settling.clone(), tab, on_ext, Some(ext)));
        assert!(
            keeps(&mut world, settling.clone(), returned, on_ext, Some(ext)),
            "a native tab of the dropped window"
        );
        assert!(!keeps(&mut world, settling, other, on_ext, Some(ext)));
    }

    /// A drop that goes home, is refused, or names a column the window can
    /// not join leaves the strip unchanged, so `Changed<LayoutStrip>` does not
    /// fire, and gives no window a new width.
    #[test]
    fn a_drop_that_goes_home_or_fails_changes_neither_the_strip_nor_a_width() {
        use bevy::ecs::change_detection::{DetectChanges as _, Ref};
        use bevy::ecs::system::RunSystemOnce as _;

        use crate::events::Event;
        use crate::tests::TestHarness;

        fn snapshot(world: &mut World) -> (String, bevy::ecs::change_detection::Tick, Vec<i32>) {
            let strip = world
                .query::<Ref<LayoutStrip>>()
                .single(world)
                .expect("one strip");
            let layout = format!("{:?}", strip.columns().collect::<Vec<_>>());
            let changed = strip.last_changed();
            let mut widths = world
                .query::<(&Window, &Bounds)>()
                .iter(world)
                .map(|(window, bounds)| (window.id(), bounds.0.x))
                .collect::<Vec<_>>();
            widths.sort_unstable();
            (
                layout,
                changed,
                widths.into_iter().map(|(_, x)| x).collect(),
            )
        }

        let commands = vec![Event::MenuOpened { window_id: 0 }];
        TestHarness::new()
            .with_windows(3)
            .on_iteration(0, |world, _| {
                let ids = world
                    .query::<(Entity, &Window)>()
                    .iter(world)
                    .map(|(entity, window)| (window.id(), entity))
                    .collect::<std::collections::HashMap<_, _>>();
                let mut strip = world
                    .query::<&mut LayoutStrip>()
                    .single_mut(world)
                    .expect("one strip");
                *strip.get_column_mut(0).expect("column 0") = Column::Fullscren(ids[&0]);
                world
                    .query::<&mut Bounds>()
                    .get_mut(world, ids[&0])
                    .expect("bounds")
                    .0
                    .x = 500;

                let strip = world
                    .query_filtered::<Entity, With<LayoutStrip>>()
                    .single(world)
                    .expect("one strip");
                for target in [
                    DropTarget::Home,
                    DropTarget::Refused,
                    DropTarget::IntoColumn {
                        strip,
                        column: 0,
                        slot: 0,
                    },
                    DropTarget::IntoColumn {
                        strip,
                        column: 7,
                        slot: 0,
                    },
                ] {
                    let before = snapshot(world);
                    let drop = DragDrop {
                        window: ids[&2],
                        target,
                    };
                    world
                        .run_system_once(
                            move |windows: WindowInfo,
                                  mut strips: Strips,
                                  mut placements: Placements,
                                  displays: Displays,
                                  config: Res<Config>,
                                  mut commands: Commands| {
                                apply_drop(
                                    drop,
                                    &windows,
                                    &mut strips,
                                    &mut placements,
                                    &displays,
                                    &config,
                                    &mut commands,
                                );
                            },
                        )
                        .expect("apply_drop runs");
                    assert_eq!(snapshot(world), before, "{target:?}");
                }
            })
            .run(commands);
    }

    /// A dragged window that closes while the button still reads down ends
    /// the drag, so nothing keeps polling the button for it.
    #[test]
    fn a_held_window_closing_leaves_no_drag_behind() {
        use crate::commands::Command;
        use crate::events::Event;
        use crate::platform::Modifiers;
        use crate::tests::TestHarness;

        let commands = vec![
            Event::MenuOpened { window_id: 0 },
            Event::MouseDown {
                point: CGPoint::new(600.0, 30.0),
                modifiers: Modifiers::empty(),
                button: MouseButton::Left,
                window_id: Some(1),
            },
            Event::MouseDragged {
                point: CGPoint::new(200.0, 600.0),
                modifiers: Modifiers::empty(),
                button: MouseButton::Left,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ];

        let harness = TestHarness::new()
            .with_config(Config::try_from("[options]\ndrag_to_reorder = true\n").unwrap())
            .with_windows(3);
        harness.mock_state.set_left_button_down(true);
        harness
            .on_iteration(1, |_, state| state.os_move_window(1, IVec2::new(0, 590)))
            .on_iteration(2, |world, state| {
                let dragging = matches!(world.resource::<DragState>(), DragState::Dragging(_));
                assert!(dragging, "{:?}", world.resource::<DragState>());
                state.os_close_window(1);
            })
            .on_iteration(3, |world, _| {
                assert_eq!(*world.resource::<DragState>(), DragState::Idle);
            })
            .run(commands);
    }

    /// While a drag holds still, nothing the drop feedback is drawn from
    /// changes, so `drag_feedback` does not run. A pointer move runs it.
    #[test]
    fn a_still_drag_does_not_run_the_drop_feedback() {
        use std::cell::Cell;
        use std::rc::Rc;

        use bevy::ecs::change_detection::Tick;
        use bevy::ecs::schedule::Schedules;
        use bevy::ecs::system::System as _;

        use crate::commands::Command;
        use crate::events::Event;
        use crate::platform::Modifiers;
        use crate::tests::TestHarness;

        fn last_feedback_run(world: &World) -> Tick {
            let feedback = bevy::ecs::system::IntoSystem::into_system(drag_feedback).system_type();
            world
                .resource::<Schedules>()
                .get(Update)
                .expect("an Update schedule")
                .systems()
                .expect("Update has run")
                .find(|(_, system)| system.system_type() == feedback)
                .map(|(_, system)| system.get_last_run())
                .expect("drag_feedback is in Update")
        }

        let drag_to = |y: f64| Event::MouseDragged {
            point: CGPoint::new(200.0, y),
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        };
        let commands = vec![
            Event::MenuOpened { window_id: 0 },
            Event::MouseDown {
                point: CGPoint::new(600.0, 30.0),
                modifiers: Modifiers::empty(),
                button: MouseButton::Left,
                window_id: Some(1),
            },
            drag_to(600.0),
            Event::Command {
                command: Command::PrintState,
            },
            drag_to(620.0),
        ];

        let harness = TestHarness::new()
            .with_config(Config::try_from("[options]\ndrag_to_reorder = true\n").unwrap())
            .with_windows(3);
        harness.mock_state.set_left_button_down(true);
        let run = Rc::new(Cell::new(None));
        let (still, moved) = (run.clone(), run.clone());
        harness
            .on_iteration(1, |_, state| state.os_move_window(1, IVec2::new(0, 590)))
            .on_iteration(2, move |world, _| {
                assert!(matches!(
                    world.resource::<DragState>(),
                    DragState::Dragging(_)
                ));
                run.set(Some(last_feedback_run(world)));
            })
            .on_iteration(3, move |world, _| {
                assert_eq!(Some(last_feedback_run(world)), still.get());
            })
            .on_iteration(4, move |world, _| {
                assert_ne!(Some(last_feedback_run(world)), moved.get());
            })
            .run(commands);
    }
}
