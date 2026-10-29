use std::time::Duration;

use bevy::app::{App, PostUpdate};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, With};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{In, IntoSystem as _, Local, Query, Res};
use bevy::math::{IRect, IVec2};
use tracing::debug;

use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::{PostUpdateSet, SelectedVirtualMarker};
use crate::events::Event;
use crate::manager::{Display, Window, WindowManager};
use crate::platform::WinID;
use crate::types::commands::Side;

pub(crate) fn register(app: &mut App) {
    app.add_systems(
        PostUpdate,
        showing_edge_slices
            .pipe(log_showing_edge_slices)
            .in_set(PostUpdateSet::Ui)
            .run_if(showing_may_change),
    );
}

/// Where a window tucked off a display edge shared with another display would
/// show its sliver, had macOS let it stay there. Layout writes it in the step
/// that tucks the window and removes it in the step that stops tucking it.
/// Only [`showing_edge_slices`] decides which of them show.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EdgeSlice {
    /// The would-be sliver's shown frame within its display, in absolute
    /// points: what the same layout shows there with the other display
    /// moved away.
    pub on_screen: IRect,
    /// `on_screen` in the window's own points, from the top left corner of
    /// its shown frame.
    pub in_window: IRect,
    /// The size of the window's shown frame, which its photo covers.
    pub size: IVec2,
    pub side: Side,
    pub display: Entity,
}

impl EdgeSlice {
    /// The slice of a window showing `shown` at its sliver position on a
    /// display with bounds `own`. `None` when none of it is on `own`.
    pub(crate) fn new(shown: IRect, own: IRect, display: Entity) -> Option<Self> {
        let side = if shown.center().x < own.center().x {
            Side::West
        } else {
            Side::East
        };
        Self::on_side(shown, own, display, side)
    }

    fn on_side(shown: IRect, own: IRect, display: Entity, side: Side) -> Option<Self> {
        let on_screen = shown.intersect(own);
        if on_screen.is_empty() {
            return None;
        }
        Some(Self {
            on_screen,
            in_window: IRect::from_corners(on_screen.min - shown.min, on_screen.max - shown.min),
            size: shown.size(),
            side,
            display,
        })
    }

    /// The window's shown frame that this slice is a part of.
    pub(crate) fn shown(&self) -> IRect {
        let min = self.on_screen.min - self.in_window.min;
        IRect::from_corners(min, min + self.size)
    }

    /// This slice with the window's shown frame moved by `by`, still cut by
    /// the display bounds `own` and still on the side it came from.
    pub(crate) fn moved(&self, by: IVec2, own: IRect) -> Option<Self> {
        let shown = self.shown();
        let shown = IRect::from_corners(shown.min + by, shown.max + by);
        Self::on_side(shown, own, self.display, self.side)
    }
}

/// A window leaving its tuck for a place on its display while its slice
/// photo slides there, as the row it belongs to scrolls. The window stays
/// tucked until the photo arrives, because macOS would show a window that
/// crosses the shared edge on the other display. Layout writes where the
/// window goes to [`PresentedOrigin`](crate::ecs::PresentedOrigin) and
/// leaves its `Position` and [`EdgeSlice`] to the slide.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlideIn {
    /// The photo shows the window as if its frame had its origin at `at`.
    /// `slice` is the slice it showed with its frame at `from`.
    Moving {
        at: IVec2,
        from: IVec2,
        slice: EdgeSlice,
    },
    /// The window has moved to where the photo rests. The photo stays over
    /// it until macOS has taken the move, or until the engine time `until`.
    Landing { until: Duration },
}

/// The windows of `strip` that show a slice: on each side, every window of
/// the hidden column nearest the edge in strip order, and no other, each with
/// the part of its slice no window in front of it covers. A native tab group
/// shows its front tab, and a tabbed stack the tab focused last.
pub(crate) fn showing(
    strip: &LayoutStrip,
    slice_of: impl Fn(Entity) -> Option<EdgeSlice>,
    history: &FocusHistory,
) -> Vec<(Entity, EdgeSlice)> {
    let on_side = |column: &Column, side: Side| {
        uncovered(
            back_to_front(strip, column, history)
                .into_iter()
                .filter_map(|entity| Some((entity, slice_of(entity)?)))
                .filter(|(_, slice)| slice.side == side)
                .collect(),
        )
    };
    let west = strip
        .columns()
        .map(|column| on_side(column, Side::West))
        .filter(|slices| !slices.is_empty())
        .last();
    let east = strip
        .columns()
        .map(|column| on_side(column, Side::East))
        .find(|slices| !slices.is_empty());
    west.into_iter().chain(east).flatten().collect()
}

/// The windows `column` shows, ordered as the stack orders them on screen:
/// each lower window in front of the one above, and in a tabbed stack the
/// tab focused last in front of the rest.
fn back_to_front(strip: &LayoutStrip, column: &Column, history: &FocusHistory) -> Vec<Entity> {
    let Column::Stack(items) = column else {
        return column.top().into_iter().collect();
    };
    let mut order = items.iter().filter_map(StackItem::top).collect::<Vec<_>>();
    if order.iter().any(|top| strip.is_tabbed_display(*top))
        && let Some(front) = history
            .last_focused_in(column)
            .and_then(|focused| items.iter().find(|item| item.contains(focused)))
            .and_then(StackItem::top)
    {
        order.retain(|top| *top != front);
        order.push(front);
    }
    order
}

/// `back_to_front` with each slice cut where a window in front of it begins,
/// and without the slices that leaves empty. A window in front covers its
/// slice from its top down, as each one reaches the column bottom.
fn uncovered(back_to_front: Vec<(Entity, EdgeSlice)>) -> Vec<(Entity, EdgeSlice)> {
    let mut shown: Vec<(Entity, EdgeSlice)> = Vec::new();
    for (entity, mut slice) in back_to_front.into_iter().rev() {
        let covered_from = shown
            .iter()
            .map(|(_, front)| front.on_screen)
            .filter(|front| !front.intersect(slice.on_screen).is_empty())
            .map(|front| front.min.y)
            .min();
        if let Some(top) = covered_from {
            slice.on_screen.max.y = slice.on_screen.max.y.min(top);
        }
        if slice.on_screen.height() <= 0 {
            continue;
        }
        slice.in_window.max.y = slice.in_window.min.y + slice.on_screen.height();
        shown.push((entity, slice));
    }
    shown.reverse();
    shown
}

/// The slices that show on every display now, from the strip each display
/// shows, ordered by window id.
pub(crate) fn showing_edge_slices(
    strips: Query<(&LayoutStrip, &ChildOf), With<SelectedVirtualMarker>>,
    displays: Query<&Display>,
    windows: Query<(&Window, Option<&EdgeSlice>)>,
    window_manager: Res<WindowManager>,
    history: Res<FocusHistory>,
) -> Vec<(WinID, EdgeSlice)> {
    let mut slices: Vec<(WinID, EdgeSlice)> = strips
        .iter()
        .filter(|(strip, child_of)| {
            displays.get(child_of.parent()).is_ok_and(|display| {
                window_manager
                    .active_display_space(display.id())
                    .is_ok_and(|space| space == strip.id())
            })
        })
        .flat_map(|(strip, _)| {
            showing(
                strip,
                |entity| {
                    windows
                        .get(entity)
                        .ok()
                        .and_then(|(_, slice)| slice.copied())
                },
                &history,
            )
        })
        .filter_map(|(entity, slice)| Some((windows.get(entity).ok()?.0.id(), slice)))
        .collect();
    slices.sort_by_key(|(id, _)| *id);
    slices
}

/// Whether the showing slices may have changed: a slice moved, appeared, or
/// went, a strip reordered its columns or tabs, a display switched strips, or
/// macOS switched a Space.
pub(crate) fn showing_may_change(
    changed: Query<(), Changed<EdgeSlice>>,
    removed: RemovedComponents<EdgeSlice>,
    strips: Query<(), Changed<LayoutStrip>>,
    selected: Query<(), Added<SelectedVirtualMarker>>,
    mut messages: MessageReader<Event>,
) -> bool {
    let space_changed = messages
        .read()
        .filter(|event| matches!(event, Event::SpaceChanged))
        .count()
        > 0;
    space_changed
        || !changed.is_empty()
        || !removed.is_empty()
        || !strips.is_empty()
        || !selected.is_empty()
}

pub(crate) fn log_showing_edge_slices(
    In(showing): In<Vec<(WinID, EdgeSlice)>>,
    mut logged: Local<Vec<(WinID, EdgeSlice)>>,
) {
    if *logged == showing {
        return;
    }
    let described = showing
        .iter()
        .map(|(id, slice)| {
            let (on, crop) = (slice.on_screen, slice.in_window);
            format!(
                "{id} {:?} on_screen=({},{})-({},{}) in_window=({},{})-({},{})",
                slice.side,
                on.min.x,
                on.min.y,
                on.max.x,
                on.max.y,
                crop.min.x,
                crop.min.y,
                crop.max.x,
                crop.max.y
            )
        })
        .collect::<Vec<_>>();
    debug!("showing edge slices: [{}]", described.join(", "));
    *logged = showing;
}
