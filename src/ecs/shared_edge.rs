use bevy::app::{App, PostUpdate};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, With};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{In, IntoSystem as _, Local, Query, Res};
use bevy::math::IRect;
use tracing::debug;

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
    pub side: Side,
    pub display: Entity,
}

impl EdgeSlice {
    /// The slice of a window showing `shown` at its sliver position on a
    /// display with bounds `own`. `None` when none of it is on `own`.
    pub(crate) fn new(shown: IRect, own: IRect, display: Entity) -> Option<Self> {
        let on_screen = shown.intersect(own);
        if on_screen.is_empty() {
            return None;
        }
        let side = if shown.center().x < own.center().x {
            Side::West
        } else {
            Side::East
        };
        Some(Self {
            on_screen,
            in_window: IRect::from_corners(on_screen.min - shown.min, on_screen.max - shown.min),
            side,
            display,
        })
    }
}

/// The windows of `strip` that show a slice: on each side, every window of
/// the hidden column nearest the edge in strip order, and no other. A native
/// tab group shows its front tab.
pub(crate) fn showing(
    strip: &LayoutStrip,
    slice_of: impl Fn(Entity) -> Option<EdgeSlice>,
) -> Vec<(Entity, EdgeSlice)> {
    let on_side = |column: &Column, side: Side| {
        front_windows(column)
            .into_iter()
            .filter_map(|entity| Some((entity, slice_of(entity)?)))
            .filter(|(_, slice)| slice.side == side)
            .collect::<Vec<_>>()
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

fn front_windows(column: &Column) -> Vec<Entity> {
    match column {
        Column::Stack(items) => items.iter().filter_map(StackItem::top).collect(),
        _ => column.top().into_iter().collect(),
    }
}

/// The slices that show on every display now, from the strip each display
/// shows, ordered by window id.
pub(crate) fn showing_edge_slices(
    strips: Query<(&LayoutStrip, &ChildOf), With<SelectedVirtualMarker>>,
    displays: Query<&Display>,
    windows: Query<(&Window, Option<&EdgeSlice>)>,
    window_manager: Res<WindowManager>,
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
            showing(strip, |entity| {
                windows
                    .get(entity)
                    .ok()
                    .and_then(|(_, slice)| slice.copied())
            })
        })
        .filter_map(|(entity, slice)| Some((windows.get(entity).ok()?.0.id(), slice)))
        .collect();
    slices.sort_by_key(|(id, _)| *id);
    slices
}

/// Whether the showing slices may have changed: a slice moved, appeared, or
/// went, a display switched strips, or macOS switched a Space.
pub(crate) fn showing_may_change(
    changed: Query<(), Changed<EdgeSlice>>,
    removed: RemovedComponents<EdgeSlice>,
    selected: Query<(), Added<SelectedVirtualMarker>>,
    mut messages: MessageReader<Event>,
) -> bool {
    let space_changed = messages
        .read()
        .filter(|event| matches!(event, Event::SpaceChanged))
        .count()
        > 0;
    space_changed || !changed.is_empty() || !removed.is_empty() || !selected.is_empty()
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
