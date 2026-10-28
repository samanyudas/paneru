use bevy::app::{App, Plugin, Update};
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut, Ref};
use bevy::ecs::component::Component;
use bevy::ecs::entity::{Entity, EntityHashMap, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::system::{Commands, Local, ParamSet, Populated, Query, Res};
use bevy::math::{IRect, IVec2};
use bevy::time::{Time, Timer, TimerMode};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;
use stdext::function_name;
use tracing::{Level, debug, instrument, trace};

use crate::config::{Config, StackLimit};
use crate::ecs::drag::DragState;
use crate::ecs::focus::PendingFocusLayout;
use crate::ecs::params::Windows;
use crate::ecs::workspace::{PreviousStripPosition, RestoreFocusMarker, SnapStripMarker};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, EnsureVisibleMarker, FocusedMarker, Initializing,
    LayoutPosition, ManualStripOffset, MaximumHeight, MinimumHeight, MouseHeldMarker,
    PlaceAfterArrival, Position, PresentedOrigin, RefitAfterRemoval, RepositionMarker,
    ReshuffleAroundMarker, ResizeMarker, Scrolling, SelectedVirtualMarker, SkipReshuffle,
    SpawnCommandsExt, TopBarHeight, TopBarRetry, Unmanaged, VerifyWindowSize, WidthAnchor,
    WidthLimits, WidthRun,
};
use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{Display, Margins, Origin, Size, Window, WindowManager};
use crate::platform::WorkspaceId;
use crate::types::commands::Side;
use crate::util::round_px;

/// The floor every window in a column is packed against. Heights that would
/// starve a sibling below it make [`binpack_heights`] fall back to averaging.
pub(crate) const MIN_WINDOW_HEIGHT: i32 = 200;

/// The top bar kept visible for a window whose top bar Accessibility cannot
/// measure, in points.
pub(crate) const FALLBACK_TOP_BAR_HEIGHT: i32 = 52;

/// How long to wait before reading a top bar again after Accessibility first
/// failed to read it. Each further failure doubles the wait, up to
/// [`TOP_BAR_RETRY_LIMIT`].
const TOP_BAR_RETRY_FIRST: Duration = Duration::from_secs(1);
const TOP_BAR_RETRY_LIMIT: Duration = Duration::from_secs(30);

/// How many windows a stack may hold in a column `column_height` tall: the
/// configured limit, capped at how many fit at [`MIN_WINDOW_HEIGHT`], and
/// never below one.
pub(crate) fn effective_stack_limit(config: &Config, column_height: i32) -> usize {
    let fit = usize::try_from(column_height / MIN_WINDOW_HEIGHT)
        .unwrap_or(0)
        .max(1);
    match config.stack_limit() {
        StackLimit::Count(count) => count.get().min(fit),
        StackLimit::Fit => fit,
    }
}

pub struct LayoutEventsPlugin;

/// How much of a hidden virtual workspace stays on its own display. Hiding a
/// workspace parks its strip this many pixels inside the display's bottom-right
/// corner, which keeps a small sliver on screen: macOS reassigns a window whose
/// frame has left the display entirely to whichever display it landed on.
pub(crate) const PARKED_STRIP_SLIVER: i32 = 10;

/// A strip, its entity, origin, display, whether it's the active one, and the
/// offset it is animating or settling toward if a move is already in flight.
/// Shared by [`reshuffle_layout_strip`] and [`ensure_visible_in_strip`].
type StripPlacements<'w, 's> = Query<
    'w,
    's,
    (
        &'static LayoutStrip,
        Entity,
        &'static Position,
        &'static ChildOf,
        Option<Ref<'static, ActiveWorkspaceMarker>>,
        Option<&'static RepositionMarker>,
        Option<&'static Scrolling>,
        Has<PreviousStripPosition>,
    ),
>;

/// Where a strip is headed: the target of an animation in flight, else the
/// offset a swipe is settling on, else where it is now.
fn strip_destination(
    position: &Position,
    reposition: Option<&RepositionMarker>,
    scrolling: Option<&Scrolling>,
) -> Origin {
    reposition.map_or_else(
        || {
            scrolling
                .and_then(|scrolling| scrolling.settle)
                .map_or(position.0, |x| position.0.with_x(round_px(x)))
        },
        |reposition| reposition.0,
    )
}

/// Displays paired with the Dock's current edge, which is what turns a display's
/// raw bounds into the usable viewport.
type DisplayViewports<'w, 's> = Query<'w, 's, (&'static Display, Option<&'static DockPosition>)>;

type ReshuffleRequests<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static LayoutPosition>,
        Has<RefitAfterRemoval>,
        Option<&'static PlaceAfterArrival>,
        Option<&'static WidthRun>,
    ),
    With<ReshuffleAroundMarker>,
>;

/// A strip, its entity, its own scroll position, whether it's mid-swipe, and
/// its parent display. Used by [`position_layout_windows`] to build each
/// window's [`StripWindowContext`].
type StripsForWindowPositioning<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LayoutStrip,
        &'static Position,
        Option<&'static RepositionMarker>,
        Has<Scrolling>,
        &'static ChildOf,
    ),
    With<LayoutStrip>,
>;

/// Windows whose size or target size changed this tick — the strip holding them
/// has to re-run its layout.
type ResizedWindows<'w, 's> =
    Populated<'w, 's, Entity, (Or<(Changed<Bounds>, Changed<ResizeMarker>)>, With<Window>)>;

/// Window frames as the layout writes them: the current origin, the size and
/// slot the layout pass is free to overwrite, and any in-flight resize target.
type WindowFrames<'w, 's> = Query<
    'w,
    's,
    (
        &'static Position,
        &'static mut Bounds,
        &'static mut LayoutPosition,
        Option<&'static ResizeMarker>,
    ),
    (Without<LayoutStrip>, With<Window>),
>;

/// Windows the layout just assigned a new slot to, with the frame fields that
/// slot has to be translated into.
type RepositionedWindows<'w, 's> = Populated<
    'w,
    's,
    (
        Entity,
        &'static Window,
        &'static LayoutPosition,
        &'static mut Position,
        &'static mut Bounds,
        Option<&'static ResizeMarker>,
    ),
    (Changed<LayoutPosition>, With<Window>, Without<LayoutStrip>),
>;

/// Strips whose position or target offset changed this tick.
type MovedStrips<'w, 's> =
    Populated<'w, 's, &'static LayoutStrip, Or<(Changed<Position>, Changed<RepositionMarker>)>>;

/// Clamp a window origin to the range where it still touches both viewport
/// edges. For an oversized window this range is reversed: from right-aligned
/// to left-aligned, which lets the strip pan across the hidden content.
pub(crate) fn clamp_origin_to_viewport(origin: Origin, size: Size, viewport: IRect) -> Origin {
    let far_edge = viewport.max - size;
    let minimum = viewport.min.min(far_edge);
    let maximum = viewport.min.max(far_edge);
    origin.clamp(minimum, maximum)
}

fn fitting_origin_x(
    target: i32,
    content: std::ops::Range<i32>,
    viewport: std::ops::Range<i32>,
) -> Option<i32> {
    let minimum = viewport.start - content.start;
    let maximum = viewport.end - content.end;
    (minimum <= maximum).then(|| target.clamp(minimum, maximum))
}

/// How far a stored width may drift before a [`ManualStripOffset`] counts as
/// stale. `window_frame_update` writes back the OS geometry, which can
/// land a pixel or two off the size the layout asked for.
const SIGNATURE_WIDTH_TOLERANCE: i32 = 2;

/// Fingerprints the shape of a strip for [`ManualStripOffset`]: the ordered
/// column tops with the widths they are heading for.
///
/// Column tops only, so promoting a tab or a stacked window to the front of its
/// column — which happens on every focus event — is not a layout change. Widths
/// from `moving_frame`, so a resize counts once it is requested rather than
/// once per animation frame. Neither depends on the strip's offset, which is
/// the point: scrolling must not invalidate a deliberate placement.
pub(crate) fn strip_signature(strip: &LayoutStrip, windows: &Windows) -> Vec<(Entity, i32)> {
    strip
        .all_columns()
        .into_iter()
        .filter_map(|entity| {
            windows
                .moving_frame(entity)
                .map(|frame| (entity, frame.width()))
        })
        .collect()
}

fn signature_matches(current: &[(Entity, i32)], stored: &[(Entity, i32)]) -> bool {
    current.len() == stored.len()
        && current.iter().zip(stored).all(|(now, before)| {
            now.0 == before.0 && (now.1 - before.1).abs() <= SIGNATURE_WIDTH_TOLERANCE
        })
}

/// The strip offset that shows all of a window sitting at `layout` within the
/// strip, starting from `origin` and moving no further than the window's
/// shortfall past a viewport edge. Returns `origin` unchanged when the window
/// already fits.
pub(crate) fn origin_exposing(
    layout: Origin,
    size: Size,
    origin: Origin,
    viewport: IRect,
) -> Origin {
    clamp_origin_to_viewport(layout + origin, size, viewport) - layout
}

impl Plugin for LayoutEventsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                // Wait for finish_setup before tiling: until then every window
                // sits in the active strip regardless of its real display.
                (
                    refresh_top_bars.run_if(top_bars_need_refresh),
                    layout_sizes_changed,
                    relayout_on_edge_padding_change,
                    layout_strip_changed,
                    fit_maximized_windows,
                    reshuffle_layout_strip,
                    expose_focused_survivor_after_removal,
                    ensure_visible_in_strip,
                    position_layout_strips,
                    position_layout_windows,
                )
                    .chain()
                    .in_set(super::UpdateSet::Layout)
                    .run_if(not(resource_exists::<Initializing>)),
            ),
        );
    }
}

#[derive(Component)]
pub(super) struct ExposeFocusedSurvivorAfterRemoval;

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn expose_focused_survivor_after_removal(
    pending: Populated<
        (
            Entity,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
            Has<PreviousStripPosition>,
            Has<Scrolling>,
        ),
        With<ExposeFocusedSurvivorAfterRemoval>,
    >,
    focused: Query<
        (Entity, Has<PendingFocusLayout>),
        (With<Window>, With<FocusedMarker>, Without<Unmanaged>),
    >,
    mouse_held: Query<(), With<MouseHeldMarker>>,
    drag: Option<Res<DragState>>,
    restored: Query<&RestoreFocusMarker>,
    skip_reshuffle: Res<SkipReshuffle>,
    strips: StripPlacements,
    displays: DisplayViewports,
    windows: Windows,
    config: Res<Config>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    for (strip_entity, strip, active, selected, previous_position, scrolling) in pending {
        let survivor = focused.single().ok().filter(|(entity, _)| {
            active
                && !previous_position
                && strip.contains(*entity)
                && !restored.iter().any(|marker| marker.entity == *entity)
        });
        if let Some((entity, new_focus)) = survivor {
            if scrolling
                || !mouse_held.is_empty()
                || drag.as_ref().is_some_and(|drag| {
                    drag.holds_window() || drag.settling_window() == Some(entity)
                })
            {
                continue;
            }
            if !new_focus && !skip_reshuffle.0 {
                commands.ensure_visible(entity);
            }
        } else if selected && !previous_position {
            // Focus is elsewhere, such as on another display, so nothing above
            // scrolls this strip. Its display must not be left showing only
            // slivers while its other windows sit off screen.
            if scrolling
                || !mouse_held.is_empty()
                || drag.as_ref().is_some_and(|drag| drag.holds_window())
            {
                continue;
            }
            if let Some(column) = column_to_show(
                strip_entity,
                &strips,
                &displays,
                &windows,
                &config,
                &window_manager,
            ) {
                commands.ensure_visible(column);
            }
        }
        commands
            .entity(strip_entity)
            .try_remove::<ExposeFocusedSurvivorAfterRemoval>();
    }
}

/// The column of strip `strip_entity` nearest its display, when the display
/// shows that strip's native Space and no column shows more than a sliver
/// there once the strip settles. `None` while any column still shows.
fn column_to_show(
    strip_entity: Entity,
    strips: &StripPlacements,
    displays: &DisplayViewports,
    windows: &Windows,
    config: &Config,
    window_manager: &WindowManager,
) -> Option<Entity> {
    let (strip, _, _, child, ..) = strips.get(strip_entity).ok()?;
    let (display, _) = displays.get(child.parent()).ok()?;
    if window_manager.active_display_space(display.id()).ok()? != strip.id() {
        return None;
    }
    let columns = strip.all_columns();
    let ctx = resolve_strip_scroll_context(*columns.first()?, strips, displays, config)?;
    let viewport = ctx.display_bounds.min.x..ctx.display_bounds.max.x;
    let mut nearest = None;
    for column in columns {
        let (Some(layout), Some(frame)) = (
            windows.layout_position(column),
            windows.moving_frame(column),
        ) else {
            continue;
        };
        let start = layout.x + ctx.strip_target.x;
        let end = start + frame.width();
        let shown = end.min(viewport.end) - start.max(viewport.start);
        if shown > config.sliver_width() {
            return None;
        }
        let distance = (viewport.start - end).max(start - viewport.end);
        if nearest.is_none_or(|(_, best)| distance < best) {
            nearest = Some((column, distance));
        }
    }
    nearest.map(|(column, _)| column)
}

/// Represents an item within a stack, which can either be a single window or a group of tabs.
#[derive(Clone, Debug, PartialEq)]
pub enum StackItem {
    /// A single window within the stack.
    Single(Entity),
    /// A group of tabs within the stack.
    Tabs(Vec<Entity>),
}

impl StackItem {
    /// Returns the top window entity in the item.
    pub fn top(&self) -> Option<Entity> {
        match self {
            StackItem::Single(id) => Some(*id),
            StackItem::Tabs(tabs) => tabs.first().copied(),
        }
    }

    /// Returns true if the item contains the specified entity.
    pub fn contains(&self, entity: Entity) -> bool {
        match self {
            StackItem::Single(id) => *id == entity,
            StackItem::Tabs(tabs) => tabs.contains(&entity),
        }
    }

    /// Returns an iterator over all window entities in this stack item.
    pub fn window_iter(&self) -> StackItemIter<'_> {
        match self {
            StackItem::Single(entity) => StackItemIter::Single(std::iter::once(*entity)),
            StackItem::Tabs(tabs) => StackItemIter::Tabs(tabs.iter().copied()),
        }
    }

    /// The column this item forms on its own.
    fn into_column(self) -> Column {
        match self {
            StackItem::Single(id) => Column::Single(id),
            StackItem::Tabs(tabs) => Column::Tabs(tabs),
        }
    }
}

pub enum StackItemIter<'a> {
    Single(std::iter::Once<Entity>),
    Tabs(std::iter::Copied<std::slice::Iter<'a, Entity>>),
}

impl Iterator for StackItemIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            StackItemIter::Single(iter) => iter.next(),
            StackItemIter::Tabs(iter) => iter.next(),
        }
    }
}

impl DoubleEndedIterator for StackItemIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            StackItemIter::Single(iter) => iter.next_back(),
            StackItemIter::Tabs(iter) => iter.next_back(),
        }
    }
}

/// A length as a share of the same length of the strip viewport, so it
/// carries over to a viewport of another size.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ViewportShare(f64);

impl ViewportShare {
    pub(crate) fn of(length: i32, viewport: i32) -> Self {
        Self(f64::from(length) / f64::from(viewport.max(1)))
    }

    pub(crate) fn of_viewport(self, viewport: i32) -> i32 {
        round_px(self.0 * f64::from(viewport))
    }
}

/// Where a column stood in the viewport: flush against one of its edges, or
/// around a point.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum ViewportAnchor {
    Left,
    Right,
    /// The column's centre, as a share of the viewport width from its left.
    Center(ViewportShare),
}

impl ViewportAnchor {
    /// The anchor of a column at `frame` in `viewport`, with whether it is
    /// the strip's `first` and `last` column. The same rule a width change
    /// keeps a column by.
    pub(crate) fn of(
        frame: IRect,
        viewport: IRect,
        first: bool,
        last: bool,
        auto_center: bool,
    ) -> Self {
        if !auto_center && first && frame.min.x == viewport.min.x {
            Self::Left
        } else if !auto_center && last && frame.max.x == viewport.max.x {
            Self::Right
        } else {
            Self::Center(ViewportShare::of(
                frame.center().x - viewport.min.x,
                viewport.width(),
            ))
        }
    }

    pub(crate) fn in_viewport(self, viewport: IRect) -> WidthAnchor {
        match self {
            Self::Left => WidthAnchor::Left,
            Self::Right => WidthAnchor::Right,
            Self::Center(share) => {
                WidthAnchor::Center(viewport.min.x + share.of_viewport(viewport.width()))
            }
        }
    }
}

/// A maximized window's size and place just before it was maximized.
#[derive(Clone, Debug, PartialEq)]
pub struct Before {
    width: ViewportShare,
    height: ViewportShare,
    anchor: ViewportAnchor,
    home: Option<StackHome>,
}

impl Before {
    pub(crate) fn new(width: ViewportShare, height: ViewportShare, anchor: ViewportAnchor) -> Self {
        Self {
            width,
            height,
            anchor,
            home: None,
        }
    }

    pub(crate) fn width(&self) -> ViewportShare {
        self.width
    }

    pub(crate) fn height(&self) -> ViewportShare {
        self.height
    }

    pub(crate) fn anchor(&self) -> ViewportAnchor {
        self.anchor
    }

    /// The windows that stayed in the stack the window left, top to bottom,
    /// and its place among them.
    pub(crate) fn stack_home(&self) -> Option<(&[Entity], usize)> {
        self.home
            .as_ref()
            .map(|home| (home.mates.as_slice(), home.slot))
    }

    /// The same, returning to its place `slot` among stack `mates`. Without
    /// a mate it returns to a column of its own.
    pub(crate) fn with_stack_home(mut self, mates: Vec<Entity>, slot: usize) -> Self {
        self.home = StackHome::new(mates, slot);
        self
    }
}

/// Where a window goes when it is restored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestoreTo {
    /// Back to its place in the stack it left, when one of the windows that
    /// stayed there is in a column with fewer than `limit` items. Otherwise
    /// where it stands.
    Home { limit: usize },
    /// Where it stands, a column of its own. An action that moves the window
    /// on, such as a stack command or a drop, decides where it goes from
    /// the layout the user sees.
    Here,
    /// Where it stands, as `Here`, but at its earlier height too: the window
    /// leaves the layout to float, so no column sets its height.
    Float,
}

/// The stack a maximized window left: the windows that stayed in it, top to
/// bottom, and the window's place among them.
#[derive(Clone, Debug, PartialEq)]
struct StackHome {
    mates: Vec<Entity>,
    slot: usize,
}

impl StackHome {
    /// `None` without a mate: a stack of one is not a stack to go back to.
    fn new(mates: Vec<Entity>, slot: usize) -> Option<Self> {
        (!mates.is_empty()).then_some(Self { mates, slot })
    }
}

/// A maximized window, or native tab group. The layout gives it the whole
/// strip viewport, and nothing can join it. Only [`LayoutStrip`] builds one,
/// so it always has a [`Before`] to restore.
#[derive(Clone, Debug, PartialEq)]
pub struct MaximizedColumn {
    item: StackItem,
    before: Before,
}

impl MaximizedColumn {
    pub(crate) fn before(&self) -> &Before {
        &self.before
    }
}

/// The size a restored window takes, as shares of the strip viewport: its
/// earlier width, and its earlier height when it went back into its stack
/// or is to float. `anchor` is where its column stood before it was maximized.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Restored {
    pub width: ViewportShare,
    pub height: Option<ViewportShare>,
    pub anchor: ViewportAnchor,
}

/// Represents a single panel within a `LayoutStrip`, which can either hold a single window, a stack of items, or a group of tabs.
#[derive(Clone, Debug)]
pub enum Column {
    /// A panel containing a single window, identified by its `Entity`.
    Single(Entity),
    /// A panel containing a stack of items (windows or tabs), ordered from top to bottom.
    Stack(Vec<StackItem>),
    /// A panel containing a group of native tabs.
    Tabs(Vec<Entity>),
    Fullscren(Entity),
    Maximized(MaximizedColumn),
}

impl Column {
    /// Returns the top window entity in the panel.
    /// For a `Single` panel, it's the contained window.
    /// For a `Stack` or `Tabs`, it's the first window in the vector.
    pub fn top(&self) -> Option<Entity> {
        match self {
            Column::Single(id) | Column::Fullscren(id) => Some(*id),
            Column::Stack(stack) => stack.first().and_then(StackItem::top),
            Column::Tabs(tabs) => tabs.first().copied(),
            Column::Maximized(maximized) => maximized.item.top(),
        }
    }

    /// Returns an iterator over all window entities in this column
    pub fn window_iter(&self) -> ColumnWindowIter<'_> {
        match self {
            Column::Single(entity) | Column::Fullscren(entity) => {
                ColumnWindowIter::Single(std::iter::once(*entity))
            }
            Column::Tabs(tabs) => ColumnWindowIter::Tabs(tabs.iter().copied()),
            Column::Stack(items) => {
                ColumnWindowIter::Stack(items.iter().flat_map(StackItem::window_iter))
            }
            Column::Maximized(maximized) => {
                ColumnWindowIter::Maximized(maximized.item.window_iter())
            }
        }
    }

    pub fn width<W>(&self, get_window_frame: &W) -> Option<i32>
    where
        W: Fn(Entity) -> Option<IRect>,
    {
        self.window_iter()
            .filter_map(get_window_frame)
            .map(|frame| frame.width())
            .max()
    }

    /// Returns the entity at the given index, or the last entity if the index exceeds the size.
    pub fn at_or_last(&self, index: usize) -> Option<Entity> {
        match self {
            Column::Single(id) | Column::Fullscren(id) => Some(*id),
            Column::Stack(stack) => stack
                .get(index)
                .or_else(|| stack.last())
                .and_then(StackItem::top),
            Column::Tabs(tabs) => tabs.first().copied(),
            Column::Maximized(maximized) => maximized.item.top(),
        }
    }

    /// Returns the position of an entity within this column (0 for Single/Tabs, index for Stack).
    pub fn position_of(&self, entity: Entity) -> Option<usize> {
        match self {
            Column::Single(id) | Column::Fullscren(id) => (*id == entity).then_some(0),
            Column::Stack(stack) => stack.iter().position(|item| item.contains(entity)),
            Column::Tabs(tabs) => tabs.contains(&entity).then_some(0),
            Column::Maximized(maximized) => maximized.item.contains(entity).then_some(0),
        }
    }

    /// The stack items this column holds, or `None` for a fullscreen or
    /// maximized column, which nothing can join.
    fn stack_items(&self) -> Option<Vec<StackItem>> {
        match self {
            Column::Single(id) => Some(vec![StackItem::Single(*id)]),
            Column::Tabs(tabs) => Some(vec![StackItem::Tabs(tabs.clone())]),
            Column::Stack(items) => Some(items.clone()),
            Column::Fullscren(_) | Column::Maximized(_) => None,
        }
    }

    /// Moves the specified entity to the front of stack-local ordering.
    /// Native tab ordering is stable; the focused tab is tracked by `FocusedMarker`.
    /// Returns whether the column changed, so a caller can leave change
    /// detection alone when `entity` was already in front.
    pub fn move_to_front(&mut self, entity: Entity) -> bool {
        let tabs = match self {
            Column::Stack(stack) => match stack.iter_mut().find(|item| item.contains(entity)) {
                Some(StackItem::Tabs(tabs)) => tabs,
                _ => return false,
            },
            Column::Tabs(tabs)
            | Column::Maximized(MaximizedColumn {
                item: StackItem::Tabs(tabs),
                ..
            }) => tabs,
            Column::Single(_) | Column::Fullscren(_) | Column::Maximized(_) => return false,
        };
        match tabs.iter().position(|&e| e == entity) {
            Some(pos) if pos > 0 => {
                tabs.swap(0, pos);
                true
            }
            _ => false,
        }
    }
}

pub enum ColumnWindowIter<'a> {
    Single(std::iter::Once<Entity>),
    Tabs(std::iter::Copied<std::slice::Iter<'a, Entity>>),
    Stack(
        std::iter::FlatMap<
            std::slice::Iter<'a, StackItem>,
            StackItemIter<'a>,
            fn(&'a StackItem) -> StackItemIter<'a>,
        >,
    ),
    Maximized(StackItemIter<'a>),
}

impl Iterator for ColumnWindowIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(iter) => iter.next(),
            Self::Tabs(iter) => iter.next(),
            Self::Stack(iter) => iter.next(),
            Self::Maximized(iter) => iter.next(),
        }
    }
}

/// A column taken whole out of one strip by [`LayoutStrip::take_column`], to
/// go into another with [`LayoutStrip::insert_column`]. It keeps its stack
/// order, native tabs, maximized state, and tabbed display.
#[derive(Debug)]
pub(crate) struct DetachedColumn {
    column: Column,
    tabbed: bool,
}

impl DetachedColumn {
    /// Every window of the column, top to bottom.
    pub(crate) fn windows(&self) -> Vec<Entity> {
        self.column.window_iter().collect()
    }
}

/// `LayoutStrip` manages a horizontal strip of `Panel`s, where each panel can contain a single window or a stack of windows.
/// It provides methods for manipulating the arrangement and access to windows within the pane.
#[derive(Clone, Component, Debug, Default)]
pub struct LayoutStrip {
    id: WorkspaceId,
    pub virtual_index: u32,
    columns: VecDeque<Column>,
    /// Entities belonging to a `Column::Stack` currently displayed as tabs (one visible, sharing
    /// the full column rect) instead of split heights. Membership, not a `Column` variant — a Stack
    /// is "tabbed" iff any of its member entities is in this set; the invariant that either all or
    /// none of one Stack's members are present is maintained by `toggle_tabbed_display`, by
    /// `stack`, and by the cleanup in `detach_item`/`collapse_stack`. This is deliberately
    /// independent of `Column::Tabs`/`StackItem::Tabs` (native-OS-tab detection, see
    /// `tabbed`/`tab_group` below) and of `LayoutStrip::tabbed()`, which callers like
    /// `apply_window_positions`/`give_away_focus` already key off for that unrelated feature.
    tabbed_stacks: EntityHashSet,
    /// Windows whose column splits its height evenly at the next layout,
    /// because a stack item joined or left that column.
    even_splits: EntityHashSet,
    /// Windows that left a maximized column since the last layout, so the
    /// layout gives them back their own margins.
    released: EntityHashSet,
}

impl LayoutStrip {
    pub fn new(id: WorkspaceId, virtual_index: u32) -> Self {
        Self {
            id,
            virtual_index,
            columns: VecDeque::new(),
            tabbed_stacks: EntityHashSet::default(),
            even_splits: EntityHashSet::default(),
            released: EntityHashSet::default(),
        }
    }

    pub fn fullscreen(id: WorkspaceId, entity: Entity) -> Self {
        let mut columns = VecDeque::new();
        columns.push_back(Column::Fullscren(entity));
        Self {
            id,
            virtual_index: 0,
            columns,
            tabbed_stacks: EntityHashSet::default(),
            even_splits: EntityHashSet::default(),
            released: EntityHashSet::default(),
        }
    }

    /// Finds the index of a window within the pane.
    /// If the window is part of a stack, it returns the index of the panel containing the stack.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to find.
    ///
    /// # Returns
    ///
    /// `Ok(usize)` with the index if found, otherwise `Err(Error)`.
    pub fn index_of(&self, entity: Entity) -> Result<usize> {
        self.columns
            .iter()
            .position(|column| column.position_of(entity).is_some())
            .ok_or(Error::NotFound(format!(
                "{}: can not find window {entity} in the current pane.",
                function_name!()
            )))
    }

    /// Returns `true` if the strip contains the given entity.
    pub fn contains(&self, entity: Entity) -> bool {
        self.columns
            .iter()
            .any(|column| column.position_of(entity).is_some())
    }

    /// Inserts a window ID into the pane at a specified position.
    /// The new window will be placed as a `Single` panel.
    ///
    /// # Arguments
    ///
    /// * `after` - The index at which to insert the window. If `after` is greater than or equal to the entity length,
    ///   the window is appended to the end.
    /// * `entity` - Entity of the window to insert.
    pub fn insert_at(&mut self, index: usize, entity: Entity) {
        if index >= self.len() {
            self.columns.push_back(Column::Single(entity));
        } else {
            self.columns.insert(index, Column::Single(entity));
        }
    }

    /// Appends a window ID as a `Single` panel to the end of the pane.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to append.
    pub fn append(&mut self, entity: Entity) {
        if self.contains(entity) {
            return;
        }
        self.columns.push_back(Column::Single(entity));
    }

    pub(crate) fn append_strip(&mut self, other: &mut Self) {
        self.columns.append(&mut other.columns);
        self.tabbed_stacks.extend(other.tabbed_stacks.drain());
        self.released.extend(other.released.drain());
    }

    /// Extracts all windows in `moved` from `self` and appends them to `target`,
    /// preserving column, stack, and tab structure for any column whose members
    /// all move together.
    pub(crate) fn extract_windows_into(&mut self, moved: &EntityHashSet, target: &mut Self) {
        let mut index = 0;
        while index < self.columns.len() {
            let all_moving = self.columns[index]
                .window_iter()
                .all(|entity| moved.contains(&entity));
            if all_moving {
                if let Some(column) = self.columns.remove(index) {
                    for entity in column.window_iter() {
                        if self.tabbed_stacks.remove(&entity) {
                            target.tabbed_stacks.insert(entity);
                        }
                    }
                    target.columns.push_back(column);
                }
                continue;
            }

            let moving_in_col = self.columns[index]
                .window_iter()
                .filter(|entity| moved.contains(entity))
                .collect::<Vec<_>>();
            if moving_in_col.is_empty() {
                index += 1;
                continue;
            }

            for entity in moving_in_col {
                if !self.contains(entity) {
                    continue;
                }
                let group = self
                    .tab_group(entity)
                    .unwrap_or_else(|| vec![entity])
                    .into_iter()
                    .filter(|member| moved.contains(member))
                    .collect::<Vec<_>>();
                for member in &group {
                    self.remove(*member);
                }
                target.append_tab_group(&group);
            }
            index += 1;
        }
    }

    pub fn append_tab_group(&mut self, entities: &[Entity]) {
        let group = dedup_entities(entities);
        if group.is_empty() {
            return;
        }

        // Re-grouping existing members keeps them at their lowest current index;
        // a foreign group lands at the end.
        let index = group
            .iter()
            .filter_map(|entity| self.index_of(*entity).ok())
            .min()
            .unwrap_or(self.len());

        self.insert_tab_group_at(index, &group);
    }

    /// Inserts `entities` as a single column at `index` (clamped to the column
    /// count), after removing any existing occurrences. A one-element group
    /// becomes a `Single` column, more than one a `Tabs` column.
    pub fn insert_tab_group_at(&mut self, index: usize, entities: &[Entity]) {
        let group = dedup_entities(entities);
        if group.is_empty() {
            return;
        }

        for entity in &group {
            self.remove(*entity);
        }

        let index = index.min(self.len());
        if group.len() == 1 {
            self.insert_at(index, group[0]);
        } else if index >= self.len() {
            self.columns.push_back(Column::Tabs(group));
        } else {
            self.columns.insert(index, Column::Tabs(group));
        }
    }

    /// Converts a column containing `leader` to a `Tabs` column and adds `follower`.
    pub fn convert_to_tabs(&mut self, leader: Entity, follower: Entity) -> Result<()> {
        self.remove(follower);
        let index = self.index_of(leader)?;
        let column = self.columns.remove(index).unwrap();
        match column {
            Column::Single(id) | Column::Fullscren(id) => {
                self.columns.insert(index, Column::Tabs(vec![follower, id]));
            }
            Column::Stack(mut items) => {
                if let Some(pos) = items.iter().position(|item| item.contains(leader)) {
                    match &mut items[pos] {
                        StackItem::Single(id) => {
                            let id = *id;
                            items[pos] = StackItem::Tabs(vec![follower, id]);
                        }
                        StackItem::Tabs(tabs) => {
                            if !tabs.contains(&follower) {
                                tabs.insert(0, follower);
                            }
                        }
                    }
                }
                self.columns.insert(index, Column::Stack(items));
            }
            Column::Tabs(mut tabs) => {
                if !tabs.contains(&follower) {
                    tabs.insert(0, follower);
                }
                self.columns.insert(index, Column::Tabs(tabs));
            }
            Column::Maximized(MaximizedColumn { item, before }) => {
                let item = match item {
                    StackItem::Single(id) => StackItem::Tabs(vec![follower, id]),
                    StackItem::Tabs(mut tabs) => {
                        if !tabs.contains(&follower) {
                            tabs.insert(0, follower);
                        }
                        StackItem::Tabs(tabs)
                    }
                };
                self.columns
                    .insert(index, Column::Maximized(MaximizedColumn { item, before }));
            }
        }
        Ok(())
    }

    /// Removes a window ID from the pane.
    /// If the window is part of a stack or tabs, it is removed from the vector.
    /// If the collection becomes empty or contains only one window, the panel type adjusts accordingly.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to remove.
    pub fn remove(&mut self, entity: Entity) {
        let removed = self
            .index_of(entity)
            .ok()
            .and_then(|index| self.columns.remove(index).zip(Some(index)));

        if let Some((column, index)) = removed {
            match column {
                Column::Single(_) | Column::Fullscren(_) => {
                    // Already removed from self.columns.
                }
                Column::Stack(mut stack) => {
                    self.tabbed_stacks.remove(&entity);
                    let items_before = stack.len();
                    for item in &mut stack {
                        match item {
                            StackItem::Single(_) => {}
                            StackItem::Tabs(tabs) => {
                                tabs.retain(|id| *id != entity);
                            }
                        }
                    }
                    stack.retain(|item| match item {
                        StackItem::Single(id) => *id != entity,
                        StackItem::Tabs(tabs) => !tabs.is_empty(),
                    });
                    // Only a whole item leaving changes the split: a closed
                    // native tab leaves its item, and the heights, in place.
                    if stack.len() < items_before {
                        self.even_splits
                            .extend(stack.first().and_then(StackItem::top));
                    }
                    if let Some(rest) = self.collapse_stack(stack) {
                        self.columns.insert(index, rest);
                    }
                }
                Column::Tabs(mut tabs) => {
                    tabs.retain(|id| *id != entity);
                    if tabs.len() > 1 {
                        self.columns.insert(index, Column::Tabs(tabs));
                    } else if let Some(remaining_id) = tabs.first() {
                        self.columns.insert(index, Column::Single(*remaining_id));
                    }
                }
                Column::Maximized(MaximizedColumn { item, before }) => {
                    self.released.insert(entity);
                    let item = match item {
                        StackItem::Single(_) => None,
                        StackItem::Tabs(mut tabs) => {
                            tabs.retain(|id| *id != entity);
                            match tabs.as_slice() {
                                [] => None,
                                [remaining] => Some(StackItem::Single(*remaining)),
                                _ => Some(StackItem::Tabs(tabs)),
                            }
                        }
                    };
                    if let Some(item) = item {
                        self.columns
                            .insert(index, Column::Maximized(MaximizedColumn { item, before }));
                    }
                }
            }
        }
    }

    /// Retrieves the `Panel` at a specified index in the pane.
    ///
    /// # Arguments
    ///
    /// * `at` - The index from which to retrieve the panel.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the panel if the index is valid, otherwise `Err(Error)`.
    pub fn get(&self, at: usize) -> Result<Column> {
        self.columns
            .get(at)
            .cloned()
            .ok_or(Error::InvalidInput(format!(
                "{}: {at} out of bounds",
                function_name!()
            )))
    }

    /// Swaps the positions of two panels within the pane.
    ///
    /// # Arguments
    ///
    /// * `left` - The index of the first panel.
    /// * `right` - The index of the second panel.
    pub fn swap(&mut self, left: usize, right: usize) {
        self.columns.swap(left, right);
    }

    /// Returns the number of panels in the pane.
    ///
    /// # Returns
    ///
    /// The number of panels as `usize`.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns the first `Panel` in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the first panel, otherwise `Err(Error)` if the pane is empty.
    pub fn first(&self) -> Result<Column> {
        self.columns.front().cloned().ok_or(Error::NotFound(format!(
            "{}: can not find first element.",
            function_name!()
        )))
    }

    /// Returns the last `Panel` in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the last panel, otherwise `Err(Error)` if the pane is empty.
    pub fn last(&self) -> Result<Column> {
        self.columns.back().cloned().ok_or(Error::NotFound(format!(
            "{}: can not find last element.",
            function_name!()
        )))
    }

    pub fn right_neighbour(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        let stack_pos = self.columns.get(index)?.position_of(entity)?;
        (index < self.columns.len())
            .then_some(index + 1)
            .and_then(|i| self.columns.get(i))
            .and_then(|col| col.at_or_last(stack_pos))
    }

    pub fn left_neighbour(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        let stack_pos = self.columns.get(index)?.position_of(entity)?;
        (index > 0)
            .then(|| index - 1)
            .and_then(|i| self.columns.get(i))
            .and_then(|col| col.at_or_last(stack_pos))
    }

    /// The index of the column next to `index` on `side`, if there is one.
    fn neighbour_index(&self, index: usize, side: Side) -> Option<usize> {
        match side {
            Side::West => index.checked_sub(1),
            Side::East => Some(index + 1).filter(|next| *next < self.len()),
        }
    }

    /// Moves the stack item holding `entity` (the window, or the native tab
    /// group it belongs to) to the bottom of the neighbouring column on
    /// `side`. The rest of its old column stays in place. A no-op when there
    /// is no neighbour on that side or either column is fullscreen.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if the window is not in the strip.
    pub fn stack(&mut self, entity: Entity, side: Side) -> Result<()> {
        let source = self.index_of(entity)?;
        let Some(target) = self.neighbour_index(source, side) else {
            return Ok(());
        };
        let Some(mut joined) = self.columns.get(target).and_then(Column::stack_items) else {
            return Ok(());
        };
        let mate = self.column_mate(entity);
        let Some((item, source_kept)) = self.detach_item(source, entity) else {
            return Ok(());
        };
        // The column the item joined, and the one it left, split evenly.
        self.even_splits.insert(entity);
        if source_kept {
            self.even_splits.extend(mate);
        }
        let target = if source_kept || target < source {
            target
        } else {
            target - 1
        };

        // Joining a tabbed stack makes the newcomer tabbed too, so the stack
        // never mixes tabbed and split members (see `tabbed_stacks`).
        if joined
            .iter()
            .flat_map(StackItem::window_iter)
            .any(|e| self.tabbed_stacks.contains(&e))
        {
            self.tabbed_stacks.extend(item.window_iter());
        }
        joined.push(item);
        if let Some(column) = self.columns.get_mut(target) {
            *column = Column::Stack(joined);
        }
        Ok(())
    }

    /// [`Self::stack`], unless the neighbouring column on `side` already
    /// holds `limit` stack items. Then the item leaves its own stack for a
    /// new column between the two instead, and a window that is already its
    /// own column stays put.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if the window is not in the strip.
    pub fn stack_within_limit(&mut self, entity: Entity, side: Side, limit: usize) -> Result<()> {
        let source = self.index_of(entity)?;
        let target_len = self
            .neighbour_index(source, side)
            .and_then(|target| self.columns.get(target))
            .and_then(Column::stack_items)
            .map(|items| items.len());
        if target_len.is_some_and(|len| len >= limit) {
            self.unstack(entity, side)
        } else {
            self.stack(entity, side)
        }
    }

    /// Moves the stack item holding `entity` out of its stack into a new
    /// column on `side` of the stack. A no-op if the window is not stacked.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if the window is not in the strip.
    pub fn unstack(&mut self, entity: Entity, side: Side) -> Result<()> {
        let index = self.index_of(entity)?;
        if !matches!(self.columns.get(index), Some(Column::Stack(_))) {
            return Ok(());
        }
        let mate = self.column_mate(entity);
        let Some((item, source_kept)) = self.detach_item(index, entity) else {
            return Ok(());
        };
        if source_kept {
            self.even_splits.extend(mate);
        }
        let at = match side {
            Side::West => index,
            Side::East => index + usize::from(source_kept),
        };
        self.columns.insert(at, item.into_column());
        Ok(())
    }

    /// Moves the stack item holding `entity` into the column at `column`, so
    /// it sits `slot` items from the top among that column's other items.
    /// `column` counts the columns as they are before the move. A drop into
    /// the item's own stack reorders it. A no-op for a fullscreen column on
    /// either end, or a `column` past the end.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if the window is not in the strip.
    pub fn insert_into_column(&mut self, entity: Entity, column: usize, slot: usize) -> Result<()> {
        let source = self.index_of(entity)?;
        if self
            .columns
            .get(column)
            .and_then(Column::stack_items)
            .is_none()
        {
            return Ok(());
        }
        let mate = self.column_mate(entity);
        // A reorder keeps a tabbed stack tabbed, even one of two windows,
        // whose lone survivor loses the membership while the item is out.
        let tabbed_reorder = column == source && self.is_tabbed_display(entity);
        let Some((item, source_kept)) = self.detach_item(source, entity) else {
            return Ok(());
        };
        // The column the item joined, and the one it left, split evenly.
        self.even_splits.insert(entity);
        if source_kept {
            self.even_splits.extend(mate);
        }
        if !source_kept && column == source {
            // Its own lone column: nothing else to join, so it goes back.
            self.columns.insert(source, item.into_column());
            return Ok(());
        }
        let target = if source_kept || column < source {
            column
        } else {
            column - 1
        };
        let Some(joined) = self.columns.get(target).and_then(Column::stack_items) else {
            return Ok(());
        };
        self.join_items(target, joined, item, slot, tabbed_reorder);
        Ok(())
    }

    /// Puts `windows`, which come from another strip, into column `column`
    /// as one stack item, `slot` items from the top. `windows` is one window,
    /// or the native tab group it belongs to. The column splits its height
    /// evenly.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if `windows` is empty, or if there is no column
    /// `column` or it is fullscreen. The strip is then unchanged.
    pub fn join_column(&mut self, windows: &[Entity], column: usize, slot: usize) -> Result<()> {
        let item = match windows {
            [] => {
                return Err(Error::InvalidInput(format!(
                    "{}: no window to join column {column}",
                    function_name!()
                )));
            }
            [window] => StackItem::Single(*window),
            tabs => StackItem::Tabs(tabs.to_vec()),
        };
        let Some(joined) = self.columns.get(column).and_then(Column::stack_items) else {
            return Err(Error::InvalidInput(format!(
                "{}: column {column} can not take another window",
                function_name!()
            )));
        };
        self.even_splits.extend(item.top());
        self.join_items(column, joined, item, slot, false);
        Ok(())
    }

    fn join_items(
        &mut self,
        target: usize,
        mut joined: Vec<StackItem>,
        item: StackItem,
        slot: usize,
        tabbed_reorder: bool,
    ) {
        // Joining a tabbed stack makes the newcomer tabbed too, so the stack
        // never mixes tabbed and split members (see `tabbed_stacks`).
        if tabbed_reorder
            || joined
                .iter()
                .flat_map(StackItem::window_iter)
                .any(|e| self.tabbed_stacks.contains(&e))
        {
            self.tabbed_stacks.extend(item.window_iter());
            self.tabbed_stacks
                .extend(joined.iter().flat_map(StackItem::window_iter));
        }
        joined.insert(slot.min(joined.len()), item);
        if let Some(column) = self.columns.get_mut(target) {
            *column = Column::Stack(joined);
        }
    }

    /// Moves the stack item holding `entity` into a new column of its own at
    /// gap `index`, where gap 0 is before the first column. `index` counts
    /// the columns as they are before the move. A no-op for a fullscreen
    /// source column.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if the window is not in the strip.
    pub fn insert_as_column(&mut self, entity: Entity, index: usize) -> Result<()> {
        let source = self.index_of(entity)?;
        if matches!(self.columns.get(source), Some(Column::Maximized(_))) {
            // The whole column moves, so the window stays maximized.
            let at = if index <= source { index } else { index - 1 };
            if let Some(column) = self.columns.remove(source) {
                self.columns.insert(at.min(self.len()), column);
            }
            return Ok(());
        }
        let mate = self.column_mate(entity);
        let Some((item, source_kept)) = self.detach_item(source, entity) else {
            return Ok(());
        };
        if source_kept {
            self.even_splits.extend(mate);
        }
        let at = if source_kept || index <= source {
            index
        } else {
            index - 1
        };
        self.columns.insert(at.min(self.len()), item.into_column());
        Ok(())
    }

    /// Takes column `index` out of the strip whole. `None` when there is no
    /// column `index`.
    pub(crate) fn take_column(&mut self, index: usize) -> Option<DetachedColumn> {
        let column = self.columns.remove(index)?;
        let mut tabbed = false;
        for entity in column.window_iter() {
            tabbed |= self.tabbed_stacks.remove(&entity);
            self.even_splits.remove(&entity);
        }
        Some(DetachedColumn { column, tabbed })
    }

    /// Puts `detached` into the strip as a new column at gap `index`, where
    /// gap 0 is before the first column, clamped to the column count. A
    /// stack splits its new height evenly, as after any change to its
    /// membership.
    pub(crate) fn insert_column(&mut self, index: usize, detached: DetachedColumn) {
        let DetachedColumn { column, tabbed } = detached;
        if tabbed {
            self.tabbed_stacks.extend(column.window_iter());
        }
        self.even_splits.extend(column.top());
        let index = index.min(self.len());
        self.columns.insert(index, column);
    }

    /// Takes the stack item holding `entity` out of column `index`, and
    /// returns it with whether any of the column remains. Leaves the strip
    /// untouched and returns `None` for a fullscreen column. An item taken
    /// from a maximized column is no longer maximized.
    fn detach_item(&mut self, index: usize, entity: Entity) -> Option<(StackItem, bool)> {
        let position = match self.columns.get(index)? {
            Column::Fullscren(_) => return None,
            column => column.position_of(entity)?,
        };
        let item = match self.columns.remove(index)? {
            Column::Single(id) | Column::Fullscren(id) => StackItem::Single(id),
            Column::Tabs(tabs) => StackItem::Tabs(tabs),
            Column::Maximized(MaximizedColumn { item, .. }) => {
                self.released.extend(item.window_iter());
                item
            }
            Column::Stack(mut items) => {
                let item = items.remove(position);
                // The item no longer shares a column with its former
                // stack-mates, so it can't remain part of a tabbed display.
                for e in item.window_iter() {
                    self.tabbed_stacks.remove(&e);
                }
                if let Some(rest) = self.collapse_stack(items) {
                    self.columns.insert(index, rest);
                    return Some((item, true));
                }
                item
            }
        };
        Some((item, false))
    }

    /// The column a stack's remaining `items` form: none when empty, the
    /// lone item as its own column, otherwise a stack. A lone item has
    /// nothing to switch between, so it also loses tabbed membership.
    fn collapse_stack(&mut self, mut items: Vec<StackItem>) -> Option<Column> {
        match items.len() {
            0 => None,
            1 => {
                let item = items.remove(0);
                for e in item.window_iter() {
                    self.tabbed_stacks.remove(&e);
                }
                Some(item.into_column())
            }
            _ => Some(Column::Stack(items)),
        }
    }

    /// Returns a vector of all window IDs present in all panels within the pane, maintaining their order.
    /// For stacked panels, all windows in the stack are included.
    ///
    /// # Returns
    ///
    /// A `Vec<Entity>` containing all window IDs.
    pub fn all_windows(&self) -> Vec<Entity> {
        self.columns.iter().flat_map(Column::window_iter).collect()
    }

    pub fn get_column_mut(&mut self, index: usize) -> Option<&mut Column> {
        self.columns.get_mut(index)
    }

    pub fn all_columns(&self) -> Vec<Entity> {
        self.columns.iter().filter_map(Column::top).collect()
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter()
    }

    #[cfg(test)]
    pub fn relative_positions<W>(
        &self,
        viewport: Size,
        get_window_frame: &W,
    ) -> impl Iterator<Item = (Entity, IRect)>
    where
        W: Fn(Entity) -> Option<IRect>,
    {
        self.relative_positions_with_minimums(
            viewport,
            get_window_frame,
            &|_| None,
            &|_| None,
            &|_| FALLBACK_TOP_BAR_HEIGHT,
            &|_| WidthLimits::default(),
        )
        .into_iter()
    }

    /// [`Self::relative_positions`], with `get_minimum_height` giving the
    /// height below which a stacked window must not be squeezed, and
    /// `get_top_bar_height` the top bar a stack too tall for its column keeps
    /// visible. See [`overlapping_rows`]. A maximized column takes the whole
    /// `viewport`, whatever its window's frame.
    #[allow(clippy::too_many_lines)]
    #[instrument(level = Level::TRACE, skip_all, fields(viewport))]
    pub fn relative_positions_with_minimums<W, M, X, T, L>(
        &self,
        viewport: Size,
        get_window_frame: &W,
        get_minimum_height: &M,
        get_maximum_height: &X,
        get_top_bar_height: &T,
        get_width_limits: &L,
    ) -> Vec<(Entity, IRect)>
    where
        W: Fn(Entity) -> Option<IRect>,
        M: Fn(Entity) -> Option<i32>,
        X: Fn(Entity) -> Option<i32>,
        T: Fn(Entity) -> i32,
        L: Fn(Entity) -> WidthLimits,
    {
        let layout_strip_height = viewport.y;
        self.column_positions(get_window_frame, viewport.x)
            .filter_map(move |(column, position)| {
                let items: Vec<StackItem> = match column {
                    Column::Single(entity) | Column::Fullscren(entity) => {
                        vec![StackItem::Single(*entity)]
                    }
                    Column::Stack(stack) => stack.clone(),
                    Column::Tabs(tabs) => vec![StackItem::Tabs(tabs.clone())],
                    Column::Maximized(maximized) => {
                        let frame = IRect::new(position, 0, position + viewport.x, viewport.y);
                        return Some(maximized.item.window_iter().map(|e| (e, frame)).collect());
                    }
                };

                // A tabbed display shares one frame across every member,
                // sized to the whole column — the same "shared frame" idiom
                // this function already uses for native-tab `StackItem::Tabs`
                // members, just applied to the whole outer Stack. This also
                // sidesteps `binpack_heights`'s `MIN_WINDOW_HEIGHT` capacity
                // limit entirely, since it isn't called for this branch.
                let is_tabbed_column = matches!(column, Column::Stack(_))
                    && items
                        .iter()
                        .flat_map(StackItem::window_iter)
                        .any(|e| self.tabbed_stacks.contains(&e));

                let minimums = items
                    .iter()
                    .map(|item| item.top().and_then(get_minimum_height).unwrap_or(0))
                    .collect::<Vec<_>>();
                let maximums = items
                    .iter()
                    .map(|item| item.top().and_then(get_maximum_height))
                    .collect::<Vec<_>>();
                // A window that cannot be resized needs only its own height,
                // even below the minimum window height.
                let floors = minimums
                    .iter()
                    .zip(&maximums)
                    .map(|(minimum, maximum)| {
                        let floor = (*minimum).max(MIN_WINDOW_HEIGHT);
                        maximum.map_or(floor, |maximum| floor.min(maximum))
                    })
                    .collect::<Vec<_>>();

                let rows = if is_tabbed_column {
                    vec![
                        Row {
                            y: 0,
                            height: layout_strip_height
                        };
                        items.len()
                    ]
                } else if floors.iter().sum::<i32>() > layout_strip_height {
                    let top_bars = items
                        .iter()
                        .map(|item| {
                            item.top()
                                .map_or(FALLBACK_TOP_BAR_HEIGHT, get_top_bar_height)
                        })
                        .collect::<Vec<_>>();
                    overlapping_rows(layout_strip_height, &floors, &top_bars)
                } else {
                    // A column a stack item joined or left starts from full
                    // height for every item, which binpack splits evenly.
                    let splits_evenly = items
                        .iter()
                        .flat_map(StackItem::window_iter)
                        .any(|entity| self.even_splits.contains(&entity));
                    let current_heights = items
                        .iter()
                        .filter_map(|item| item.top().and_then(get_window_frame))
                        .map(|frame| {
                            if splits_evenly {
                                layout_strip_height
                            } else {
                                frame.height()
                            }
                        })
                        .collect::<Vec<_>>();
                    // A refused height arrives as a current height above its
                    // share. Taking the extra evenly from the others first
                    // keeps binpack from taking it all from the last window.
                    let mut current_heights = current_heights;
                    if !splits_evenly && minimums.iter().any(|minimum| *minimum > 0) {
                        hold_minimum_heights(&mut current_heights, &minimums, layout_strip_height);
                    }
                    // Binpack gives up when the heights it keeps leave too
                    // little for the rest, though every floor fits an even split.
                    let mut heights =
                        binpack_heights(&current_heights, MIN_WINDOW_HEIGHT, layout_strip_height)
                            .unwrap_or_else(|| {
                                even_heights(current_heights.len(), layout_strip_height)
                            });
                    // Capped first, so the spare height reaches the windows
                    // that can use it before any minimum takes its share.
                    hold_maximum_heights(&mut heights, &maximums);
                    hold_minimum_heights(&mut heights, &minimums, layout_strip_height);
                    stacked_rows(&heights)
                };

                let column_width = column_width(&items, get_window_frame, get_width_limits)?;

                let frames = items
                    .into_iter()
                    .zip(rows)
                    .filter_map(|(item, row)| {
                        let entity = item.top()?;
                        let mut frame = get_window_frame(entity)?;
                        frame.min.x = position;
                        frame.max.x = frame.min.x
                            + get_width_limits(entity)
                                .max
                                .map_or(column_width, |max| max.min(column_width));

                        frame.min.y = row.y;
                        frame.max.y = row.y + row.height;

                        // Return ALL windows in the item with the same frame
                        let results = item.window_iter().map(|e| (e, frame)).collect::<Vec<_>>();
                        Some(results)
                    })
                    .flatten()
                    .collect::<Vec<_>>();

                Some(frames)
            })
            .flatten()
            .collect()
    }

    /// Each column with its left edge, the columns laid edge to edge. A
    /// maximized column is `viewport_width` wide.
    #[instrument(level = Level::TRACE, skip_all)]
    pub fn column_positions<W>(
        &self,
        get_window_frame: &W,
        viewport_width: i32,
    ) -> impl Iterator<Item = (&Column, i32)>
    where
        W: Fn(Entity) -> Option<IRect>,
    {
        let mut left_edge = 0;

        self.columns().filter_map(move |column| {
            let width = match column {
                Column::Maximized(_) => Some(viewport_width),
                column => column.width(get_window_frame),
            };

            width.map(|width| {
                let temp = left_edge;
                left_edge += width;
                (column, temp)
            })
        })
    }

    pub fn above(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        let column = self.get(index).ok()?;
        match column {
            Column::Single(_) | Column::Tabs(_) | Column::Fullscren(_) | Column::Maximized(_) => {
                None
            }
            Column::Stack(items) => {
                let pos = items.iter().position(|item| item.contains(entity))?;
                (pos > 0).then(|| items[pos - 1].top()).flatten()
            }
        }
    }

    pub fn tabbed(&self, entity: Entity) -> bool {
        self.index_of(entity)
            .and_then(|idx| self.get(idx))
            .map(|col| match col {
                Column::Tabs(tabs)
                | Column::Maximized(MaximizedColumn {
                    item: StackItem::Tabs(tabs),
                    ..
                }) => tabs.contains(&entity),
                Column::Stack(items) => items.iter().any(|item| {
                    if let StackItem::Tabs(tabs) = item {
                        tabs.contains(&entity)
                    } else {
                        false
                    }
                }),
                Column::Single(_) | Column::Fullscren(_) | Column::Maximized(_) => false,
            })
            .is_ok_and(|t| t)
    }

    pub fn tab_group(&self, entity: Entity) -> Option<Vec<Entity>> {
        self.columns.iter().find_map(|column| match column {
            Column::Tabs(tabs)
            | Column::Maximized(MaximizedColumn {
                item: StackItem::Tabs(tabs),
                ..
            }) if tabs.contains(&entity) && tabs.len() > 1 => Some(tabs.clone()),
            Column::Stack(items) => items.iter().find_map(|item| match item {
                StackItem::Tabs(tabs) if tabs.contains(&entity) && tabs.len() > 1 => {
                    Some(tabs.clone())
                }
                StackItem::Single(_) | StackItem::Tabs(_) => None,
            }),
            Column::Single(_) | Column::Fullscren(_) | Column::Tabs(_) | Column::Maximized(_) => {
                None
            }
        })
    }

    pub fn is_fullscreen(&self) -> bool {
        self.columns
            .front()
            .is_some_and(|column| matches!(column, Column::Fullscren(_)))
    }

    /// Toggles tabbed display (one window visible at a time, sharing the
    /// full column rect) for the `Column::Stack` containing `entity`.
    /// Returns `None` — not applicable, nothing changed — unless `entity` is
    /// in a `Stack` with more than one item; matches niri, where the toggle
    /// is only meaningful on a column that actually holds multiple windows.
    /// `None` is distinct from `Some(false)` (successfully toggled *off*) so
    /// callers can tell "did nothing" from "turned tabs off" instead of
    /// reporting both the same way.
    ///
    /// Returns `Some(is_tabbed)` — whether the column is tabbed after the
    /// call — when the toggle actually applied.
    pub fn toggle_tabbed_display(&mut self, entity: Entity) -> Option<bool> {
        let index = self.index_of(entity).ok()?;
        let Column::Stack(items) = self.get(index).ok()? else {
            return None;
        };
        if items.len() < 2 {
            return None;
        }
        let members: Vec<Entity> = items.iter().flat_map(StackItem::window_iter).collect();
        let now_tabbed = !members.iter().any(|e| self.tabbed_stacks.contains(e));
        if now_tabbed {
            self.tabbed_stacks.extend(members);
        } else {
            for member in members {
                self.tabbed_stacks.remove(&member);
            }
        }
        Some(now_tabbed)
    }

    /// Clears the record of columns waiting for an even split, once the
    /// layout has applied it.
    pub fn take_even_splits(&mut self) {
        self.even_splits.clear();
    }

    /// The windows of every maximized column.
    pub(crate) fn maximized_windows(&self) -> impl Iterator<Item = Entity> + '_ {
        self.columns
            .iter()
            .filter(|column| matches!(column, Column::Maximized(_)))
            .flat_map(Column::window_iter)
    }

    /// The windows that left a maximized column since the last call.
    pub(crate) fn take_released(&mut self) -> EntityHashSet {
        std::mem::take(&mut self.released)
    }

    /// What maximized window `entity` returns to, or `None` when it is not
    /// maximized.
    pub fn maximized(&self, entity: Entity) -> Option<&Before> {
        self.columns.iter().find_map(|column| match column {
            Column::Maximized(maximized) if maximized.item.contains(entity) => {
                Some(&maximized.before)
            }
            _ => None,
        })
    }

    /// Maximizes the column holding `entity`. A window or native tab group
    /// in a stack first leaves it for a column of its own on `side` of the
    /// stack, and remembers its place there. Returns `Ok(false)` when it is
    /// already maximized.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error)` if the window is not in the strip, or is in a
    /// fullscreen column.
    pub fn maximize(&mut self, entity: Entity, side: Side, mut before: Before) -> Result<bool> {
        let index = self.index_of(entity)?;
        let item = match self.columns.get(index) {
            Some(Column::Maximized(_)) => return Ok(false),
            Some(Column::Stack(items)) => {
                let slot = items
                    .iter()
                    .position(|item| item.contains(entity))
                    .unwrap_or_default();
                let mates = items
                    .iter()
                    .filter(|item| !item.contains(entity))
                    .flat_map(StackItem::window_iter)
                    .collect();
                before.home = StackHome::new(mates, slot);
                let mate = self.column_mate(entity);
                let Some((item, kept)) = self.detach_item(index, entity) else {
                    return Ok(false);
                };
                if kept {
                    self.even_splits.extend(mate);
                }
                let at = match side {
                    Side::West => index,
                    Side::East => index + usize::from(kept),
                };
                self.columns
                    .insert(at, Column::Maximized(MaximizedColumn { item, before }));
                return Ok(true);
            }
            Some(Column::Single(id)) => StackItem::Single(*id),
            Some(Column::Tabs(tabs)) => StackItem::Tabs(tabs.clone()),
            Some(Column::Fullscren(_)) | None => {
                return Err(Error::InvalidInput(format!(
                    "{}: window {entity} can not be maximized",
                    function_name!()
                )));
            }
        };
        self.columns[index] = Column::Maximized(MaximizedColumn { item, before });
        Ok(true)
    }

    /// Makes the column of `entity`, a window or native tab group that is
    /// its own column, maximized with `before` to restore. Returns whether
    /// it did.
    pub fn adopt_maximized(&mut self, entity: Entity, before: Before) -> bool {
        let Ok(index) = self.index_of(entity) else {
            return false;
        };
        let item = match &self.columns[index] {
            Column::Single(id) => StackItem::Single(*id),
            Column::Tabs(tabs) => StackItem::Tabs(tabs.clone()),
            Column::Stack(_) | Column::Fullscren(_) | Column::Maximized(_) => return false,
        };
        self.columns[index] = Column::Maximized(MaximizedColumn { item, before });
        true
    }

    /// Ends maximize for the column holding `entity`, which goes where `to`
    /// says. Returns `None` when it is not maximized.
    pub(crate) fn unmaximize(&mut self, entity: Entity, to: RestoreTo) -> Option<Restored> {
        let index = self.index_of(entity).ok()?;
        let Column::Maximized(maximized) = self.columns.get(index)? else {
            return None;
        };
        let MaximizedColumn { item, before } = maximized.clone();
        self.columns.remove(index);
        self.released.extend(item.window_iter());
        let home = match (to, before.home.as_ref()) {
            (RestoreTo::Home { limit }, Some(home)) => home.mates.iter().find_map(|mate| {
                let column = self.index_of(*mate).ok()?;
                let items = self.columns.get(column)?.stack_items()?;
                (items.len() < limit).then_some((column, items, home.slot))
            }),
            _ => None,
        };
        let height = if let Some((column, items, slot)) = home {
            self.join_items(column, items, item, slot, false);
            Some(before.height)
        } else {
            self.columns
                .insert(index.min(self.len()), item.into_column());
            (to == RestoreTo::Float).then_some(before.height)
        };
        Some(Restored {
            width: before.width,
            height,
            anchor: before.anchor,
        })
    }

    /// A window in the same column as `entity` that is not in `entity`'s own
    /// stack item, so it stays behind when that item moves to another column.
    fn column_mate(&self, entity: Entity) -> Option<Entity> {
        let column = self
            .index_of(entity)
            .and_then(|index| self.get(index))
            .ok()?;
        let Column::Stack(items) = column else {
            return None;
        };
        items
            .iter()
            .find(|item| !item.contains(entity))
            .and_then(StackItem::top)
    }

    /// Whether `entity` belongs to a `Column::Stack` currently displayed as
    /// tabs. Independent of `tabbed()` (native-OS-tab detection).
    pub fn is_tabbed_display(&self, entity: Entity) -> bool {
        self.tabbed_stacks.contains(&entity)
    }

    /// Given `entity` in a tabbed `Column::Stack`, returns the next tab,
    /// falling back to the previous one when `entity` is last. Used only by
    /// the close-active-tab focus fallback — normal cycling reuses
    /// `Operation::Focus` North/South directly.
    pub fn tab_display_sibling(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        let Column::Stack(items) = self.get(index).ok()? else {
            return None;
        };
        let pos = items.iter().position(|item| item.contains(entity))?;
        items
            .get(pos + 1)
            .or_else(|| pos.checked_sub(1).and_then(|p| items.get(p)))
            .and_then(StackItem::top)
    }
}

impl std::fmt::Display for LayoutStrip {
    /// Formats the `LayoutStrip` for display, showing the arrangement of its panels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = self
            .columns
            .iter()
            .map(|column| format!("{column:?}"))
            .collect::<Vec<_>>();
        write!(f, "[{}]", out.join(", "))
    }
}

/// Deduplicates `entities`, preserving first-seen order.
/// The width every window in a column shares: the master's (top item's), so a
/// window stacked onto a master of a different width resizes to match it
/// instead of keeping its own width. This also matches the column slot width
/// from `column_positions`, which is the widest member. A member at the widest
/// width it accepts does not set the column width, and keeps its own width at
/// the left edge, leaving the rest of the column empty. A member that refused
/// anything narrower widens the column to its minimum.
fn column_width<W, L>(
    items: &[StackItem],
    get_window_frame: &W,
    get_width_limits: &L,
) -> Option<i32>
where
    W: Fn(Entity) -> Option<IRect>,
    L: Fn(Entity) -> WidthLimits,
{
    let widths = items
        .iter()
        .filter_map(StackItem::top)
        .filter_map(|entity| {
            let width = get_window_frame(entity)?.width();
            let at_widest = get_width_limits(entity).max.is_some_and(|max| width >= max);
            Some((width, at_widest))
        })
        .collect::<Vec<_>>();
    let width = widths
        .iter()
        .find(|(_, at_widest)| !at_widest)
        .or_else(|| widths.first())
        .map(|(width, _)| *width)?;
    Some(
        items
            .iter()
            .flat_map(StackItem::window_iter)
            .filter_map(|entity| get_width_limits(entity).min)
            .fold(width, i32::max),
    )
}

fn dedup_entities(entities: &[Entity]) -> Vec<Entity> {
    let mut seen = EntityHashSet::default();
    entities
        .iter()
        .copied()
        .filter(|entity| seen.insert(*entity))
        .collect()
}

fn binpack_heights(heights: &[i32], min_height: i32, total_height: i32) -> Option<Vec<i32>> {
    let mut count = heights.len();
    let mut output = vec![];

    loop {
        let mut idx = 0;

        let mut remaining = total_height;
        while idx < count {
            let remaining_windows = heights.len() - idx;

            if heights[idx] < remaining {
                if idx + 1 == count {
                    output.push(remaining);
                } else {
                    output.push(heights[idx]);
                }
                remaining -= heights[idx];
            } else if remaining >= min_height * i32::try_from(remaining_windows).ok()? {
                output.push(remaining);
                remaining = 0;
            } else {
                break;
            }
            idx += 1;
        }

        if idx == count {
            break;
        }
        count -= 1;
        output.clear();
    }

    let remaining = i32::try_from(heights.len() - count).ok()?;
    if remaining > 0 && count > 0 {
        count -= 1;
        output.truncate(count);
        let sum = output.iter().sum::<i32>();
        // Integer division (floor) on purpose: rounding here could make the
        // heights sum past `total_height`.
        let avg_height = (total_height - sum) / (remaining + 1);
        if avg_height < min_height {
            return None;
        }

        while count < heights.len() {
            output.push(avg_height);
            count += 1;
        }
    }

    Some(output)
}

/// Where one window of a column sits: its distance from the column's top and
/// its height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub y: i32,
    pub height: i32,
}

/// Rows for windows that fit, each starting where the one above ends.
fn stacked_rows(heights: &[i32]) -> Vec<Row> {
    let mut y = 0;
    heights
        .iter()
        .map(|height| {
            let row = Row { y, height: *height };
            y += height;
            row
        })
        .collect()
}

/// `total` split into `count` heights, the last taking the rounding.
fn even_heights(count: usize, total: i32) -> Vec<i32> {
    let Some(rest) = count.checked_sub(1) else {
        return vec![];
    };
    let share = total / i32::try_from(count).unwrap_or(i32::MAX);
    let mut heights = vec![share; rest];
    heights.push(total - share * i32::try_from(rest).unwrap_or(0));
    heights
}

/// Rows for a stack whose `floors` (each window's minimum height, at least
/// `MIN_WINDOW_HEIGHT`) exceed `column_height`.
///
/// Each lower window overlaps the window above it, but only below that
/// window's top bar, given by `top_bars`. The last window ends at the column
/// bottom, and the space it leaves above is shared evenly between the upper
/// windows' visible strips. A window starts no lower than lets it fit above the
/// column bottom at its floor. A window extends past the bottom only when the
/// top bars above it and its own floor exceed the column.
///
/// Every window reaches down to the column bottom, so the focused window,
/// raised in front of the stack, shows as much of itself as the column holds.
/// The rows do not depend on focus: focus changes only the window order.
pub(crate) fn overlapping_rows(column_height: i32, floors: &[i32], top_bars: &[i32]) -> Vec<Row> {
    let Some(last) = floors.len().checked_sub(1) else {
        return vec![];
    };
    let upper_bars = top_bars[..last].iter().sum::<i32>();
    let leftover = (column_height - floors[last] - upper_bars).max(0);
    let upper_count = i32::try_from(last).unwrap_or(i32::MAX).max(1);
    let (share, remainder) = (leftover / upper_count, leftover % upper_count);
    let mut latest = floors
        .iter()
        .map(|floor| column_height - floor)
        .collect::<Vec<_>>();
    for index in (0..last).rev() {
        latest[index] = latest[index].min(latest[index + 1] - top_bars[index]);
    }

    let mut rows = Vec::with_capacity(floors.len());
    // Where the even share would put the next window, and where the top bar of
    // the window above lets it start at the earliest.
    let (mut target, mut earliest) = (0, 0);
    for (index, floor) in floors.iter().enumerate() {
        let y = earliest.max(target.min(latest[index]));
        rows.push(Row {
            y,
            height: (*floor).max(column_height - y),
        });
        let extra = share + i32::from(i32::try_from(index).unwrap_or(i32::MAX) < remainder);
        target += top_bars[index] + extra;
        earliest = y + top_bars[index];
    }
    rows
}

/// Raises every stacked height below its window's minimum to that minimum,
/// and takes the extra from the tallest of the other windows first, none going
/// below its own minimum or `MIN_WINDOW_HEIGHT`. Windows that give end up
/// level, so a window with a smaller minimum keeps an even share. Minimums
/// that cannot all fit are left for [`overlapping_rows`], which the layout
/// uses instead.
pub(crate) fn hold_minimum_heights(heights: &mut [i32], minimums: &[i32], total: i32) {
    for (height, minimum) in heights.iter_mut().zip(minimums) {
        *height = (*height).max(*minimum);
    }
    let floor = |index: usize| minimums[index].max(MIN_WINDOW_HEIGHT);
    let mut excess = heights.iter().sum::<i32>() - total;
    // One pixel at a time from the tallest window above its floor, which
    // levels the givers instead of leaving rounding steps.
    while excess > 0 {
        let Some(tallest) = (0..heights.len())
            .filter(|index| heights[*index] > floor(*index))
            .max_by_key(|index| (heights[*index], std::cmp::Reverse(*index)))
        else {
            return;
        };
        heights[tallest] -= 1;
        excess -= 1;
    }
}

/// Caps each height at its window's maximum and gives the spare height evenly
/// to the windows without one, the last taking the rounding. With no such
/// window, the spare height stays empty below the last window.
fn hold_maximum_heights(heights: &mut [i32], maximums: &[Option<i32>]) {
    let mut spare = 0;
    for (height, maximum) in heights.iter_mut().zip(maximums) {
        if let Some(maximum) = maximum
            && *height > *maximum
        {
            spare += *height - maximum;
            *height = *maximum;
        }
    }
    let takers = (0..heights.len())
        .filter(|index| maximums[*index].is_none())
        .collect::<Vec<_>>();
    if spare == 0 || takers.is_empty() {
        return;
    }
    for (index, share) in takers.iter().zip(even_heights(takers.len(), spare)) {
        heights[*index] += share;
    }
}

/// Watches for size changes to windows and if they are changed, signals to the layout strip.
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn layout_sizes_changed(
    changed_sizes: ResizedWindows,
    workspaces: Query<&mut LayoutStrip>,
) {
    let changed_entities = changed_sizes.iter().collect::<EntityHashSet>();
    workspaces.into_iter().for_each(|mut strip| {
        if strip_has_changed_window(&strip, &changed_entities) {
            strip.set_changed();
        }
    });
}

fn strip_has_changed_window(strip: &LayoutStrip, changed_entities: &EntityHashSet) -> bool {
    strip
        .columns
        .iter()
        .any(|column| column_has_changed_window(column, changed_entities))
}

fn column_has_changed_window(column: &Column, changed_entities: &EntityHashSet) -> bool {
    column
        .window_iter()
        .any(|entity| changed_entities.contains(&entity))
}

fn top_bars_need_refresh(
    mut messages: MessageReader<Event>,
    mut lost: RemovedComponents<FocusedMarker>,
    retries: Query<Entity, With<TopBarRetry>>,
) -> bool {
    let resized = messages
        .read()
        .filter(|event| matches!(event, Event::WindowResized { .. }))
        .count()
        > 0;
    let lost_focus = lost.read().count() > 0;
    resized || lost_focus || !retries.is_empty()
}

/// Measures a top bar again when it may have changed, and relays out the
/// strip that holds the window when it did. Only windows an overlapping stack
/// has measured before, or failed to, are measured.
///
/// - A window that loses focus goes back behind the window below it, so its
///   top bar is measured then. Its app can change the top bar only while it
///   is focused or as it resizes, such as when a browser shows its bookmarks
///   bar, so this finds such a change before the top bar is covered.
/// - A window that resizes may lay out its top bar again.
/// - A top bar Accessibility could not read is read again when its
///   [`TopBarRetry`] timer finishes.
///
/// A failed read keeps the last measurement and schedules a retry, waiting
/// twice as long after each failure, so a busy app is not asked every frame.
#[instrument(level = Level::DEBUG, skip_all)]
fn refresh_top_bars(
    mut messages: MessageReader<Event>,
    mut lost: RemovedComponents<FocusedMarker>,
    retries: Query<Entity, With<TopBarRetry>>,
    mut windows: Query<(
        Entity,
        &Window,
        Option<&mut TopBarHeight>,
        Option<&mut TopBarRetry>,
    )>,
    mut strips: Query<&mut LayoutStrip>,
    time: Res<Time>,
    mut commands: Commands,
) {
    let mut stale = lost.read().collect::<EntityHashSet>();
    let resized = messages
        .read()
        .filter_map(|event| match event {
            Event::WindowResized { window_id } => Some(*window_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !resized.is_empty() {
        for (entity, window, _, _) in &windows {
            if resized.contains(&window.id()) {
                stale.insert(entity);
            }
        }
    }
    for entity in &retries {
        if let Ok((_, _, _, Some(mut retry))) = windows.get_mut(entity)
            && retry.0.tick(time.delta()).is_finished()
        {
            stale.insert(entity);
        }
    }

    for entity in stale {
        let Ok((_, window, top_bar, retry)) = windows.get_mut(entity) else {
            continue;
        };
        if top_bar.is_none() && retry.is_none() {
            // No overlapping stack has needed this top bar yet.
            continue;
        }
        if retry.as_ref().is_some_and(|retry| !retry.0.is_finished()) {
            continue;
        }
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        let height = match window.top_bar_height() {
            Ok(height) => height.unwrap_or(FALLBACK_TOP_BAR_HEIGHT),
            Err(err) => {
                debug!("top bar of window {} unreadable: {err}", window.id());
                match retry {
                    Some(mut retry) => {
                        let wait = (retry.0.duration() * 2).min(TOP_BAR_RETRY_LIMIT);
                        retry.0 = Timer::new(wait, TimerMode::Once);
                    }
                    None => {
                        entity_commands.try_insert(TopBarRetry(Timer::new(
                            TOP_BAR_RETRY_FIRST,
                            TimerMode::Once,
                        )));
                    }
                }
                continue;
            }
        };
        if retry.is_some() {
            entity_commands.try_remove::<TopBarRetry>();
        }
        match top_bar {
            Some(top_bar) if top_bar.0 == height => continue,
            Some(mut top_bar) => top_bar.0 = height,
            None => {
                entity_commands.try_insert(TopBarHeight(height));
            }
        }
        for mut strip in &mut strips {
            if strip.contains(entity) {
                strip.set_changed();
            }
        }
    }
}

/// Watches for changes to `LayoutStrip` (i.e. a window added or window order changed) and
/// re-calculates the logical positions of all the windows in the layout strip.
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn layout_strip_changed(
    mut changed_strips: Populated<(&mut LayoutStrip, &ChildOf), Changed<LayoutStrip>>,
    mut windows: WindowFrames,
    size_limits: Query<(
        Option<&MinimumHeight>,
        Option<&MaximumHeight>,
        Option<&WidthLimits>,
    )>,
    top_bars: Query<(&Window, Option<&TopBarHeight>, Has<TopBarRetry>)>,
    displays: DisplayViewports,
    config: Res<Config>,
    mut commands: Commands,
) {
    let get_window_frame = |entity| {
        windows
            .get(entity)
            .map(|(position, bounds, _, resize)| {
                // Width changes reserve their final column slot immediately.
                // Stack rows follow the animated height so the native window
                // can grow before its bottom edge moves upward.
                let size = resize.map_or(bounds.0, |resize| bounds.0.with_x(resize.0.x));
                IRect::from_corners(position.0, position.0 + size)
            })
            .ok()
    };
    let get_minimum_height = |entity| {
        size_limits
            .get(entity)
            .ok()
            .and_then(|(minimum, _, _)| minimum.map(|m| m.0))
    };
    let get_maximum_height = |entity| {
        size_limits
            .get(entity)
            .ok()
            .and_then(|(_, maximum, _)| maximum.map(|m| m.0))
    };
    let get_width_limits = |entity| {
        size_limits
            .get(entity)
            .ok()
            .and_then(|(_, _, limits)| limits.copied())
            .unwrap_or_default()
    };
    let measured = RefCell::new(Vec::new());
    // A window waiting to retry an unreadable top bar keeps the fallback until
    // `refresh_top_bars` reads it.
    let get_top_bar_height = |entity| match top_bars.get(entity) {
        Ok((_, Some(top_bar), _)) => top_bar.0,
        Ok((window, None, false)) => {
            let height = window.top_bar_height();
            measured
                .borrow_mut()
                .push((entity, height.as_ref().ok().copied()));
            height.ok().flatten().unwrap_or(FALLBACK_TOP_BAR_HEIGHT)
        }
        Ok((_, None, true)) | Err(_) => FALLBACK_TOP_BAR_HEIGHT,
    };

    let mut changed = Vec::new();
    for (mut layout_strip, child_of) in &mut changed_strips {
        let Ok((display, dock)) = displays.get(child_of.parent()) else {
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config).size();
        changed.extend(layout_strip.relative_positions_with_minimums(
            viewport,
            &get_window_frame,
            &get_minimum_height,
            &get_maximum_height,
            &get_top_bar_height,
            &get_width_limits,
        ));
        // Applied now, so later passes keep the heights it set. Clearing it
        // is bookkeeping, not a layout change.
        layout_strip.bypass_change_detection().take_even_splits();
    }

    for (entity, height) in measured.into_inner() {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            match height {
                Some(height) => entity_commands
                    .try_insert(TopBarHeight(height.unwrap_or(FALLBACK_TOP_BAR_HEIGHT))),
                None => entity_commands.try_insert(TopBarRetry(Timer::new(
                    TOP_BAR_RETRY_FIRST,
                    TimerMode::Once,
                ))),
            };
        }
    }

    for (entity, frame) in changed {
        if let Ok((_, mut bounds, mut layout_position, resize)) = windows.get_mut(entity) {
            if layout_position.0 != frame.min {
                layout_position.0 = frame.min;
            }
            if resize.is_none() && bounds.0 != frame.size() {
                // A smaller height is checked once the window settles, so an
                // app that refuses it has its minimum recorded. Growth is not
                // checked here: a window that cannot grow gets the height it
                // has, and checking would relayout it forever. A width either
                // way is checked, because the recorded limit keeps the next
                // pass from asking for it again.
                if (frame.height() < bounds.0.y || frame.width() != bounds.0.x)
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_insert(VerifyWindowSize::default());
                }
                bounds.0 = frame.size();
            }
        }
    }
}

/// Lays every strip out again when the display edge padding changes, as a
/// config reload can do: the viewport, and with it every maximized window,
/// depends on it. Each strip in view on any display moves with its viewport,
/// so it keeps where the user scrolled it, unless the column the user sees
/// there is maximized: then that column is placed to fill the new viewport.
#[allow(clippy::type_complexity)]
fn relayout_on_edge_padding_change(
    config: Res<Config>,
    mut last: Local<Option<(i32, i32, i32, i32)>>,
    mut strips: Query<(
        Entity,
        &mut LayoutStrip,
        &ChildOf,
        &Position,
        Option<&RepositionMarker>,
        Option<&Scrolling>,
        Has<SelectedVirtualMarker>,
        Has<PreviousStripPosition>,
    )>,
    displays: DisplayViewports,
    windows: Windows,
    mut commands: Commands,
) {
    let padding = config.edge_padding();
    let Some(old) = last.replace(padding).filter(|old| *old != padding) else {
        return;
    };
    let (top, _, _, left) = padding;
    let (old_top, _, _, old_left) = old;
    let shift = IVec2::new(left - old_left, top - old_top);
    let focused = windows.focused().map(|(_, entity)| entity);
    for (entity, mut strip, child, position, reposition, scrolling, selected, parked) in &mut strips
    {
        strip.set_changed();
        if !selected || parked {
            continue;
        }
        let destination = strip_destination(position, reposition, scrolling);
        let target = displays
            .get(child.parent())
            .ok()
            .and_then(|(display, dock)| {
                let viewport = display.actual_display_bounds(dock, &config);
                origin_filling_viewport(
                    &strip,
                    destination,
                    display.bounds(),
                    viewport,
                    focused,
                    &windows,
                )
            })
            .unwrap_or(destination + shift);
        if target != destination {
            commands.reposition_entity(entity, target);
        }
    }
}

/// The origin that puts the column the user sees in `strip` at the origin
/// of `viewport`, if that column is maximized. The column seen is the one
/// with the focused window, or else the one most on `display` with the strip
/// at `destination`. Columns are placed at the width of `viewport`, since a
/// maximized column before the one seen changes its place.
fn origin_filling_viewport(
    strip: &LayoutStrip,
    destination: Origin,
    display: IRect,
    viewport: IRect,
    focused: Option<Entity>,
    windows: &Windows,
) -> Option<Origin> {
    let frame = |entity| windows.moving_frame(entity);
    let anchor = focused
        .filter(|entity| strip.contains(*entity))
        .or_else(|| {
            strip
                .columns()
                .filter_map(Column::top)
                .filter_map(|entity| {
                    let min = windows.layout_position(entity)?.0 + destination;
                    let size = frame(entity)?.size();
                    let shown = IRect::from_corners(min, min + size).intersect(display);
                    (shown.width() > 0).then_some((entity, shown.width()))
                })
                .max_by_key(|(_, shown)| *shown)
                .map(|(entity, _)| entity)
        })?;
    let (column, x) = strip
        .column_positions(&frame, viewport.width())
        .find(|(column, _)| column.position_of(anchor).is_some())?;
    matches!(column, Column::Maximized(_)).then(|| Origin::new(viewport.min.x - x, viewport.min.y))
}

/// Has every window of a maximized column fill the display's edge padding,
/// and gives every other window of a changed strip, and every window that
/// left a maximized column, its own margins back. A window whose margins
/// change is written again.
fn fit_maximized_windows(
    mut strips: Query<&mut LayoutStrip>,
    mut windows: Query<(&mut Window, &mut Bounds)>,
    config: Res<Config>,
) {
    let (top, right, bottom, left) = config.edge_padding();
    let edges = Margins {
        min: IVec2::new(left, top),
        max: IVec2::new(right, bottom),
    };
    let mut filled = EntityHashSet::default();
    let mut refit = Vec::new();
    for mut strip in &mut strips {
        filled.extend(strip.maximized_windows());
        if strip.is_changed() {
            refit.extend(strip.all_windows());
        }
        refit.extend(strip.bypass_change_detection().take_released());
    }
    for entity in refit {
        let fill = filled.contains(&entity).then_some(edges);
        if let Ok((mut window, mut bounds)) = windows.get_mut(entity)
            && window.fill() != fill
        {
            window.set_fill(fill);
            bounds.set_changed();
        }
    }
}

pub(crate) fn strip_content_range(
    strip: &LayoutStrip,
    windows: &Windows,
) -> Option<std::ops::Range<i32>> {
    let first = strip.first().ok()?.top()?;
    let last = strip.last().ok()?.top()?;
    let start = windows.layout_position(first)?.x;
    let end = windows.layout_position(last)?.x + windows.moving_frame(last)?.width();
    Some(start..end)
}

fn overflowing_arrival_x(content: std::ops::Range<i32>, viewport: IRect) -> Option<i32> {
    (content.end - content.start > viewport.width()).then_some(viewport.max.x - content.end)
}

/// Resolved owning strip and display viewport for a window entity participating
/// in [`reshuffle_layout_strip`] or [`ensure_visible_in_strip`].
struct StripScrollContext<'a> {
    strip: &'a LayoutStrip,
    strip_entity: Entity,
    strip_origin: Origin,
    strip_target: Origin,
    display_bounds: IRect,
    raw_display_bounds: IRect,
    is_new_activation: bool,
}

fn resolve_strip_scroll_context<'a>(
    entity: Entity,
    strips: &'a StripPlacements,
    displays: &DisplayViewports,
    config: &Config,
) -> Option<StripScrollContext<'a>> {
    let (
        strip,
        strip_entity,
        strip_position,
        child,
        active_marker,
        strip_reposition,
        strip_scrolling,
        has_previous_position,
    ) = strips.into_iter().find(|strip| strip.0.contains(entity))?;
    let (display, dock) = displays.get(child.parent()).ok()?;
    let strip_target = strip_destination(strip_position, strip_reposition, strip_scrolling);
    Some(StripScrollContext {
        strip,
        strip_entity,
        strip_origin: strip_position.0,
        strip_target,
        display_bounds: display.actual_display_bounds(dock, config),
        raw_display_bounds: display.bounds(),
        is_new_activation: has_previous_position && active_marker.is_some_and(|m| m.is_added()),
    })
}

#[allow(clippy::too_many_lines)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn reshuffle_layout_strip(
    markers: ReshuffleRequests,
    strips: StripPlacements,
    manual_offsets: Query<&ManualStripOffset>,
    displays: DisplayViewports,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    for (entity, slot, refit, arrival, width_run) in markers {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<(ReshuffleAroundMarker, RefitAfterRemoval, PlaceAfterArrival)>();
        }
        let Some(slot) = slot else {
            continue;
        };
        let Some(ctx) = resolve_strip_scroll_context(entity, &strips, &displays, &config) else {
            continue;
        };
        if ctx.is_new_activation {
            trace!(
                "reshuffle_layout_strip: skipping newly active workspace {}",
                ctx.strip_entity
            );
            continue;
        }
        let Some(frame) = windows.moving_frame(entity) else {
            continue;
        };
        let strip = ctx.strip;
        let strip_entity = ctx.strip_entity;
        let strip_target = ctx.strip_target;
        let display_bounds = ctx.display_bounds;
        let size = frame.size();
        let resizing = windows.is_resizing(entity);
        let projected_min = slot.0 + strip_target;
        let projected_frame = IRect::from_corners(projected_min, projected_min + size);
        let visible_width = display_bounds.intersect(projected_frame).width();

        let content = strip_content_range(strip, &windows);
        if arrival.is_some_and(|arrival| arrival.destination == strip_entity)
            && !config.auto_center()
            && strip
                .last()
                .is_ok_and(|last| last.position_of(entity).is_some())
            && let Some(x) = content
                .clone()
                .and_then(|range| overflowing_arrival_x(range, display_bounds))
        {
            if let Ok(mut cmd) = commands.get_entity(strip_entity) {
                cmd.try_remove::<ManualStripOffset>();
            }
            commands.reposition_entity(strip_entity, Origin::new(x, display_bounds.min.y));
            continue;
        }

        // A deliberate offset (center, snap) outranks the derived one, but only
        // while the window this reshuffle is about is still fully on screen: a
        // focus event for a window scrolled past an edge has to bring it back,
        // and at that point the placement is stale. Project against the strip's
        // *target* offset — mid-animation the window's own frame is a tick
        // behind a strip that is still moving.
        if let Ok(manual) = manual_offsets.get(strip_entity) {
            // The offset only ever meant "the user placed *this* layout here",
            // so a strip that has since gained, lost, reordered or resized a
            // column no longer has a placement to keep.
            let current = strip_signature(strip, &windows);
            let projected = slot.0 + strip_target;
            if !resizing
                && signature_matches(&current, &manual.signature)
                && clamp_origin_to_viewport(projected, size, display_bounds) == projected
            {
                trace!("reshuffle_layout_strip: keeping manual offset on {strip_entity}");
                continue;
            }
            trace!("reshuffle_layout_strip: manual offset on {strip_entity} is stale");
            if let Ok(mut cmd) = commands.get_entity(strip_entity) {
                cmd.try_remove::<ManualStripOffset>();
            }
        }

        let mut desired_min = if refit || arrival.is_some() {
            frame.min
        } else {
            projected_min
        };
        if let Some(run) = width_run {
            desired_min.x = anchored_origin(run.anchor, frame, size, display_bounds).x;
        } else if config.auto_center() && !refit && arrival.is_none() {
            desired_min.x = ctx.raw_display_bounds.center().x - size.x / 2;
        } else if resizing
            && !refit
            && arrival.is_none()
            && let Some(current_frame) = windows.frame(entity)
        {
            desired_min.x = IRect::from_center_size(current_frame.center(), size).min.x;
        }
        let clamped_min = clamp_origin_to_viewport(desired_min, size, display_bounds);
        let mut strip_position = (clamped_min - slot.0).with_y(display_bounds.min.y);

        // Enforce the edge invariant when auto-center is off: the leftmost
        // window must touch the left edge and the rightmost the right edge
        // if more than 1 windows in workspace.
        if !config.auto_center()
            && !config.continuous_swipe()
            && let Some(total_strip_width) = content.as_ref().map(|range| range.end)
        {
            strip_position.x = if display_bounds.width() < total_strip_width {
                let right_aligned = display_bounds.max.x - total_strip_width;
                strip_position.x.clamp(right_aligned, display_bounds.min.x)
            } else {
                // Strip fits entirely: pin the leftmost window to the left edge.
                display_bounds.min.x
            };
        }

        let refitted_x = if refit && !config.auto_center() {
            content.and_then(|range| {
                fitting_origin_x(
                    if config.continuous_swipe() {
                        strip_target.x
                    } else {
                        strip_position.x
                    },
                    range,
                    display_bounds.min.x..display_bounds.max.x,
                )
            })
        } else {
            None
        };
        if let Some(x) = refitted_x {
            strip_position = strip_target.with_x(x);
        }

        // Check how much of the window is hidden. Slivers don't count as
        // meaningfully visible, so subtract sliver_width from the visible
        // portion. If the hidden fraction is within the allowed ratio, skip.
        // A maximized window always fills the display, so it never stays
        // partly hidden.
        let hidden_ratio = config.window_hidden_ratio();
        if !resizing
            && width_run.is_none()
            && refitted_x.is_none()
            && hidden_ratio > 0.0
            && strip.maximized(entity).is_none()
        {
            let meaningful = (visible_width - config.sliver_width()).max(0);
            let visible_fraction = f64::from(meaningful) / f64::from(frame.width().max(1));
            let hidden_fraction = 1.0 - visible_fraction;

            // Do not move the window if the hidden fraction is lower than threshold
            // or if the layout strip movement is shorter than the hidden width.
            let strip_movement = (ctx.strip_origin.x - strip_position.x).abs();
            if meaningful > 0
                && hidden_fraction <= hidden_ratio
                && frame.width() - visible_width >= strip_movement
            {
                continue;
            }
        }

        trace!("reshuffle_layout_strip: triggered for entity {entity}, offset {strip_position}");
        commands.reposition_entity(strip_entity, strip_position);
    }
}

/// Scrolls the strip the minimum amount needed to keep `EnsureVisibleMarker`
/// entities on-screen at their new layout position. If the entity already fits
/// inside the viewport with the strip where it is, the strip is left alone and
/// the per-window animator slides the entity into its slot. Only when the new
/// slot would fall past an edge does the strip translate, and only by the
/// shortfall — never to anchor the entity to a particular position.
#[allow(clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn ensure_visible_in_strip(
    markers: Query<(Entity, &LayoutPosition, &EnsureVisibleMarker)>,
    mut strip_params: ParamSet<(
        StripPlacements,
        Query<&mut Position, (With<LayoutStrip>, Without<Window>)>,
    )>,
    displays: DisplayViewports,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    for (entity, layout_position, marker) in markers {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<EnsureVisibleMarker>();
        }
        // Each marker is independent, and its own marker was already consumed
        // above: bailing out of the loop would silently drop the rest.
        // Copied out of the `StripPlacements` borrow so it can be dropped
        // before the `&mut Position` query below is touched — Bevy treats
        // the two queries as conflicting system params even though they're
        // never live at the same time here.
        let Some((strip_entity, strip_target, viewport, is_new_activation)) =
            resolve_strip_scroll_context(entity, &strip_params.p0(), &displays, &config).map(
                |ctx| {
                    (
                        ctx.strip_entity,
                        ctx.strip_target,
                        ctx.display_bounds,
                        ctx.is_new_activation,
                    )
                },
            )
        else {
            continue;
        };

        if is_new_activation {
            trace!("ensure_visible_in_strip: skipping newly active workspace {strip_entity}");
            continue;
        }
        let Some(size) = windows.moving_frame(entity).map(|frame| frame.size()) else {
            continue;
        };

        // Where the entity would appear once the strip settles. Mid-animation
        // the strip's current position is on its way somewhere else, so the
        // target offset is what the window's slot will actually be measured
        // against - the same projection `reshuffle_layout_strip` makes.
        let scroll_to = origin_exposing(layout_position.0, size, strip_target, viewport);
        if scroll_to == strip_target {
            continue;
        }
        trace!("ensure_visible_in_strip: entity {entity}, scroll strip to {scroll_to}");
        // Scrolling to expose a window overrides whatever the user placed here.
        if let Ok(mut cmd) = commands.get_entity(strip_entity) {
            cmd.try_remove::<ManualStripOffset>();
        }
        if marker.snap {
            // This correction is standing in for the virtual-workspace
            // restore itself (deferred one tick past its `is_added` guard —
            // see `show_active_workspace`), so it must respect
            // `virtual_workspace_animations` the same way the restore did,
            // not always animate like an ordinary reshuffle correction.
            if let Ok(mut position) = strip_params.p1().get_mut(strip_entity) {
                position.0 = scroll_to;
            }
            if let Ok(mut cmd) = commands.get_entity(strip_entity) {
                cmd.try_remove::<RepositionMarker>();
            }
        } else {
            commands.reposition_entity(strip_entity, scroll_to);
        }
    }
}

/// Reacts to changes in the position or target offset of the `LayoutStrip`, and
/// marks all the windows in the strip as requiring re-positioning.
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn position_layout_strips(
    moved_strips: MovedStrips,
    mut windows: Query<&mut LayoutPosition, (With<Window>, Without<LayoutStrip>)>,
) {
    for strip in moved_strips {
        for entity in strip.all_windows() {
            if let Ok(mut position) = windows.get_mut(entity) {
                position.set_changed();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct StripWindowContext {
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    stacked: bool,
    /// See [`crate::ecs::workspace::SnapStripMarker`]: forces this window to
    /// snap directly to its target in `position_layout_windows`, bypassing
    /// the offscreen/parking magnitude heuristic.
    snap_settling: bool,
}

#[allow(clippy::too_many_arguments)]
fn insert_strip_window_contexts(
    contexts: &mut EntityHashMap<StripWindowContext>,
    strip: &LayoutStrip,
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    snap_settling: bool,
) {
    for column in &strip.columns {
        insert_column_window_contexts(
            contexts,
            column,
            strip_position,
            swiping,
            display_entity,
            matches!(column, Column::Stack(_)),
            snap_settling,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_column_window_contexts(
    contexts: &mut EntityHashMap<StripWindowContext>,
    column: &Column,
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    stacked: bool,
    snap_settling: bool,
) {
    match column {
        Column::Single(entity) | Column::Fullscren(entity) => {
            contexts.insert(
                *entity,
                StripWindowContext {
                    strip_position,
                    swiping,
                    display_entity,
                    stacked,
                    snap_settling,
                },
            );
        }
        Column::Stack(items) => {
            for item in items {
                insert_stack_item_window_contexts(
                    contexts,
                    item,
                    strip_position,
                    swiping,
                    display_entity,
                    stacked,
                    snap_settling,
                );
            }
        }
        Column::Tabs(entities) => {
            for entity in entities {
                contexts.insert(
                    *entity,
                    StripWindowContext {
                        strip_position,
                        swiping,
                        display_entity,
                        stacked,
                        snap_settling,
                    },
                );
            }
        }
        Column::Maximized(maximized) => {
            insert_stack_item_window_contexts(
                contexts,
                &maximized.item,
                strip_position,
                swiping,
                display_entity,
                stacked,
                snap_settling,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_stack_item_window_contexts(
    contexts: &mut EntityHashMap<StripWindowContext>,
    item: &StackItem,
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    stacked: bool,
    snap_settling: bool,
) {
    match item {
        StackItem::Single(entity) => {
            contexts.insert(
                *entity,
                StripWindowContext {
                    strip_position,
                    swiping,
                    display_entity,
                    stacked,
                    snap_settling,
                },
            );
        }
        StackItem::Tabs(entities) => {
            for entity in entities {
                contexts.insert(
                    *entity,
                    StripWindowContext {
                        strip_position,
                        swiping,
                        display_entity,
                        stacked,
                        snap_settling,
                    },
                );
            }
        }
    }
}

/// Reacts to changes of logical window layout in the strip and any have been changed, reposition
/// the layout strip against the current display viewport.
#[instrument(level = Level::DEBUG, skip_all)]
fn position_layout_windows(
    positioned_windows: RepositionedWindows,
    workspaces: StripsForWindowPositioning,
    snap_guards: Query<&SnapStripMarker>,
    displays: DisplayViewports,
    drag: Option<Res<crate::ecs::drag::DragState>>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let offscreen_sliver_width = config.sliver_width();
    let (_, pad_right, _, pad_left) = config.edge_padding();
    let mut strip_contexts = EntityHashMap::default();
    for (strip_entity, layout_strip, Position(strip_position), reposition, swiping, child_of) in
        &workspaces
    {
        let snap_settling = snap_guards.iter().any(|guard| guard.strip == strip_entity);
        let strip_target = reposition.map_or(*strip_position, |target| target.0);
        insert_strip_window_contexts(
            &mut strip_contexts,
            layout_strip,
            strip_target,
            swiping,
            child_of.parent(),
            snap_settling,
        );
    }

    let held = drag
        .as_ref()
        .filter(|drag| drag.holds_window())
        .and_then(|drag| drag.window());
    let held_group = held
        .map(|held| {
            workspaces
                .iter()
                .find_map(|(_, strip, _, _, _, _)| strip.tab_group(held))
                .unwrap_or_else(|| vec![held])
        })
        .unwrap_or_default();
    for (entity, window, layout_position, mut position, mut bounds, resize) in positioned_windows {
        if held_group.contains(&entity) {
            continue;
        }
        let Some(context) = strip_contexts.get(&entity) else {
            continue;
        };
        let Ok((display, dock)) = displays.get(context.display_entity) else {
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        // Gets 80% of the display height as threshold.
        let Ok(vertical_move_threshold) = u32::try_from(viewport.height() * 8 / 10) else {
            continue;
        };

        // The window shows inside its frame by its margins, so they are
        // taken off here for the window to land exactly sliver_width pixels
        // from the screen edge.
        let margins = window.margins();
        let target_size = resize.map_or(bounds.0, |resize| bounds.0.with_x(resize.0.x));
        let mut frame = IRect::from_corners(layout_position.0, layout_position.0 + target_size);
        let width = frame.width();
        frame.min += context.strip_position;
        frame.max += context.strip_position;

        let mut offscreen = false;
        if frame.max.x <= viewport.min.x + margins.max.x {
            // Window hidden to the left — position so exactly
            // sliver_width CG pixels are visible from the real
            // display edge. The right margin can leave a window just
            // inside the viewport edge while its CG frame is fully
            // past it.
            frame.min.x =
                viewport.min.x - width + offscreen_sliver_width - pad_left + margins.max.x;
            offscreen = true;
        } else if frame.min.x >= viewport.max.x - margins.min.x {
            // Window hidden to the right — mirror of above.
            frame.min.x = viewport.max.x - offscreen_sliver_width + pad_right - margins.min.x;
            offscreen = true;
        }
        frame.max.x = frame.min.x + width;

        // During swipe, keep full height. The vertical sliver inset only
        // applies to horizontally off-screen windows, so they expose just
        // a `sliver_height` fraction of their height at the viewport's
        // vertical center.
        if !context.swiping && offscreen {
            // Don't compress stacked windows vertically when off-screen.
            // The height reduction corrupts their proportions: when the
            // column scrolls back on-screen, binpack_heights makes the
            // last window absorb all remaining space.
            if !context.stacked {
                let inset =
                    round_px(f64::from(viewport.height()) * (1.0 - config.sliver_height()) / 2.0);
                frame.min.y += inset;
                frame.max.y += inset;
            }
        }

        // Keep the window anchored to its own display. The parked strip origin
        // only accounts for the first window: everything sitting below it in a
        // stack picks up that offset on top of the corner and lands past the
        // bottom edge, fully inside the display underneath. macOS then adopts
        // the window onto that display, and showing the workspace again brings
        // it back there instead of here. Park those at the corner as well.
        let park_row = viewport.max.y - PARKED_STRIP_SLIVER;
        if frame.min.y > park_row {
            let height = frame.height();
            frame.min.y = park_row;
            frame.max.y = park_row + height;
        }

        let frame = keep_on_own_display(frame, display, &displays, offscreen_sliver_width, margins);

        if resize.is_none() && bounds.0 != frame.size() {
            bounds.0 = frame.size();
        }
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(PresentedOrigin(frame.min));
        }

        if position.0 != frame.min {
            // Direct-assign (snap) when:
            //   - The user is actively swiping: windows must track the finger in lockstep.
            //   - A workspace switch just moved the strip vertically: jumping the full off-screen
            //   distance should be instantaneous.
            // Otherwise (programmatic strip animation, or pure layout change), animate toward the
            // new position so layout changes (swap/add/remove) slide instead of teleport. When the
            // strip is also being animated, the per-window target is recomputed each tick from the
            // strip's current position, so the two motions compose: e.g., on swap, the focused
            // window's target converges back to its old visual position as the strip settles, while
            // the other window slides past.
            // Parking and un-parking have to snap as one move. The clamp above
            // leaves the lower stack members a short hop from their visible
            // slot, which the distance threshold alone reads as an ordinary
            // layout change - they would slide across the screen while the
            // window above them teleports.
            let parking = frame.min.y >= park_row || position.0.y >= park_row;
            let offscreen_move =
                parking || position.0.y.abs_diff(frame.min.y) > vertical_move_threshold;
            // `snap_settling`: a strip restore only fixes the strip's own
            // offset instantly - a member window (a lower stack member
            // especially) can still need a correction that neither
            // `parking` nor the distance threshold recognizes as
            // restore-driven, since its last position can land close enough
            // to its target. See `SnapStripMarker`.
            if context.swiping
                || context.snap_settling
                || offscreen_move && !config.virtual_workspace_animations()
            {
                position.0 = frame.min;
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_remove::<RepositionMarker>();
                }
            } else {
                commands.reposition_entity(entity, frame.min);
            }
        }
    }
}

fn overlap_area(a: IRect, b: IRect) -> i64 {
    let shared = a.intersect(b);
    if shared.is_empty() {
        0
    } else {
        i64::from(shared.width()) * i64::from(shared.height())
    }
}

/// Whether `shown`, the frame a window shows, has its center or more of
/// itself on another display than on `own`. macOS then moves the window into
/// that display's space and draws it there.
fn lies_mostly_elsewhere(shown: IRect, own: IRect, others: &[IRect]) -> bool {
    let home = overlap_area(shown, own);
    others
        .iter()
        .any(|other| other.contains(shown.center()) || overlap_area(shown, *other) > home)
}

/// `frame`, or its bottom tuck when more of it lies on another of `displays`
/// than on `display`.
fn keep_on_own_display(
    frame: IRect,
    display: &Display,
    displays: &DisplayViewports,
    sliver: i32,
    margins: Margins,
) -> IRect {
    let own = display.bounds();
    let others: Vec<IRect> = displays
        .iter()
        .map(|(other, _)| other.bounds())
        .filter(|other| *other != own)
        .collect();
    if lies_mostly_elsewhere(margins.shown(frame), own, &others) {
        bottom_tuck(frame, own, sliver, margins, &others).unwrap_or(frame)
    } else {
        frame
    }
}

/// `frame` tucked below the bottom edge of `own`, against the side it hangs
/// past, with `sliver` points of the window showing. It hangs past that
/// corner when no other display is there, and otherwise shows flush inside the
/// side. `None` when a display below `own` takes most of either.
fn bottom_tuck(
    frame: IRect,
    own: IRect,
    sliver: i32,
    margins: Margins,
    others: &[IRect],
) -> Option<IRect> {
    let size = frame.size();
    let y = own.max.y - sliver - margins.min.y;
    let (corner, flush) = if frame.center().x < own.center().x {
        (
            own.min.x - size.x + sliver + margins.max.x,
            own.min.x - margins.min.x,
        )
    } else {
        (
            own.max.x - sliver - margins.min.x,
            own.max.x - size.x + margins.max.x,
        )
    };
    let at = |x: i32| IRect::from_corners(IVec2::new(x, y), IVec2::new(x, y) + size);
    let corner = at(corner);
    if others
        .iter()
        .all(|other| overlap_area(margins.shown(corner), *other) == 0)
    {
        return Some(corner);
    }
    let flush = at(flush);
    (!lies_mostly_elsewhere(margins.shown(flush), own, others)).then_some(flush)
}

/// Where a column of `size` goes for `anchor`, kept inside `viewport`.
pub(crate) fn anchored_origin(
    anchor: WidthAnchor,
    frame: IRect,
    size: Size,
    viewport: IRect,
) -> Origin {
    let center = match anchor {
        WidthAnchor::Center(x) => Origin::new(x, frame.center().y),
        WidthAnchor::Left | WidthAnchor::Right => frame.center(),
    };
    let mut origin =
        clamp_origin_to_viewport(IRect::from_center_size(center, size).min, size, viewport);
    match anchor {
        WidthAnchor::Left => origin.x = viewport.min.x,
        WidthAnchor::Right => origin.x = viewport.max.x - size.x,
        WidthAnchor::Center(_) => {}
    }
    origin
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn harness_with_orphaned_display() -> crate::tests::TestHarness {
        use std::time::Duration;

        use crate::events::Event;
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_DISPLAY_HEIGHT, TEST_DISPLAY_ID, TEST_DISPLAY_WIDTH, TEST_WORKSPACE_ID,
            TestHarness,
        };

        let mut harness = TestHarness::new()
            .with_display(
                EXT_DISPLAY_ID,
                IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
                vec![EXT_WORKSPACE_ID],
            )
            .with_workspace_window(100, EXT_WORKSPACE_ID, |_| {})
            .with_window(0, |_| {});
        // Make the external display active, matching a disconnection while
        // one of its windows is focused.
        harness.mock_state.remove_display(TEST_DISPLAY_ID);
        harness.mock_state.remove_display(EXT_DISPLAY_ID);
        harness.mock_state.add_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        );
        harness.mock_state.add_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID],
        );
        harness.advance(Duration::from_millis(500));

        harness.mock_state.remove_display(EXT_DISPLAY_ID);
        harness
            .world()
            .write_message::<Event>(Event::DisplayRemoved {
                display_id: EXT_DISPLAY_ID,
            });
        harness.advance(Duration::from_millis(650));
        harness
    }

    #[test]
    fn orphaned_window_does_not_block_other_display_positions() {
        use bevy::ecs::change_detection::DetectChangesMut;

        use crate::tests::{EXT_WORKSPACE_ID, TEST_WORKSPACE_ID, find_window_entity};

        let mut harness = harness_with_orphaned_display();

        // Keep one system instance so its Changed<LayoutPosition> filter sees
        // only the changes below, as it would in the normal schedule.
        let layout_system = harness.world().register_system(position_layout_windows);
        harness.world().run_system(layout_system).unwrap();

        let orphan = find_window_entity(100, harness.world());
        let survivor = find_window_entity(0, harness.world());
        let survivor_strip = harness
            .world()
            .query::<(&LayoutStrip, Entity)>()
            .iter(harness.world())
            .find(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
            .unwrap()
            .1;
        harness
            .world()
            .get_mut::<Position>(survivor_strip)
            .unwrap()
            .0
            .x += 80;
        harness
            .world()
            .entity_mut(survivor)
            .remove::<RepositionMarker>();
        harness
            .world()
            .get_mut::<LayoutPosition>(orphan)
            .unwrap()
            .set_changed();
        harness
            .world()
            .get_mut::<LayoutPosition>(survivor)
            .unwrap()
            .set_changed();
        harness.world().run_system(layout_system).unwrap();

        assert_eq!(
            harness
                .world()
                .get::<RepositionMarker>(survivor)
                .map(|marker| marker.0),
            Some(Origin::new(80, 20)),
            "an orphaned window must not stop another display's layout update"
        );

        // A strip may also retain a parent whose display entity is no longer
        // available while reconciliation is in flight.
        let orphan_strip = harness
            .world()
            .query::<(&LayoutStrip, Entity)>()
            .iter(harness.world())
            .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
            .unwrap()
            .1;
        let stale_parent = harness.world().spawn_empty().id();
        harness
            .world()
            .entity_mut(orphan_strip)
            .insert(ChildOf(stale_parent));
        harness
            .world()
            .get_mut::<Position>(survivor_strip)
            .unwrap()
            .0
            .x += 40;
        harness
            .world()
            .entity_mut(survivor)
            .remove::<RepositionMarker>();
        harness
            .world()
            .get_mut::<LayoutPosition>(orphan)
            .unwrap()
            .set_changed();
        harness
            .world()
            .get_mut::<LayoutPosition>(survivor)
            .unwrap()
            .set_changed();
        harness.world().run_system(layout_system).unwrap();

        assert_eq!(
            harness
                .world()
                .get::<RepositionMarker>(survivor)
                .map(|marker| marker.0),
            Some(Origin::new(120, 20)),
            "a stale display parent must not stop another display's layout update"
        );
    }

    #[test]
    fn top_bar_refresh_stays_idle_without_changes() {
        let mut harness = crate::tests::TestHarness::new().with_windows(1);
        harness.run(vec![crate::events::Event::MenuOpened { window_id: 0 }]);
        harness.advance(Duration::from_secs(2));
        let system_type = refresh_top_bars.system_type_id();
        let last_run = |world: &World| {
            world
                .resource::<bevy::ecs::schedule::Schedules>()
                .get(Update)
                .unwrap()
                .systems()
                .unwrap()
                .find(|(_, system)| system.system_type() == system_type)
                .unwrap()
                .1
                .get_last_run()
        };
        let before = last_run(harness.world());
        harness.advance(Duration::from_millis(200));
        assert_eq!(last_run(harness.world()), before);
    }

    #[test]
    fn fitting_origin_moves_only_to_the_nearest_fitting_edge() {
        assert_eq!(fitting_origin_x(-500, 0..1906, 4..2044), Some(4));
        assert_eq!(fitting_origin_x(500, 0..1906, 4..2044), Some(138));
        assert_eq!(fitting_origin_x(100, 0..1906, 4..2044), Some(100));
        assert_eq!(fitting_origin_x(-2500, 20..1926, -2044..-4), Some(-2064));
        assert_eq!(fitting_origin_x(100, 0..2040, 4..2044), Some(4));
        assert_eq!(fitting_origin_x(100, 0..2041, 4..2044), None);
    }

    #[test]
    fn signature_tolerates_pixel_drift_but_not_a_reshape() {
        let mut world = World::new();
        let first = world.spawn_empty().id();
        let second = world.spawn_empty().id();
        let stored = vec![(first, 400), (second, 400)];

        assert!(signature_matches(&[(first, 401), (second, 399)], &stored));
        assert!(!signature_matches(&[(first, 420), (second, 400)], &stored));
        assert!(!signature_matches(&[(second, 400), (first, 400)], &stored));
        assert!(!signature_matches(&[(first, 400)], &stored));
    }

    #[test]
    fn origin_exposing_moves_only_by_the_shortfall() {
        let viewport = IRect::new(0, 0, 1000, 800);
        let size = Size::new(400, 700);

        // Window at layout x 0 with the strip scrolled 600px to the left sits
        // entirely off-screen; the strip has to come back to 0.
        assert_eq!(
            origin_exposing(Origin::new(0, 0), size, Origin::new(-600, 0), viewport),
            Origin::new(0, 0)
        );

        // A window past the right edge only moves by what hangs over it.
        assert_eq!(
            origin_exposing(Origin::new(800, 0), size, Origin::ZERO, viewport),
            Origin::new(-200, 0)
        );

        // Already fully visible: the offset is left alone.
        assert_eq!(
            origin_exposing(Origin::new(100, 0), size, Origin::new(50, 0), viewport),
            Origin::new(50, 0)
        );
    }

    #[test]
    fn clamp_origin_supports_oversized_windows() {
        let viewport = IRect::new(0, 20, 1024, 768);
        let size = Size::new(2048, 748);

        assert_eq!(
            clamp_origin_to_viewport(Origin::new(300, 20), size, viewport),
            Origin::new(0, 20)
        );
        assert_eq!(
            clamp_origin_to_viewport(Origin::new(-1600, 20), size, viewport),
            Origin::new(-1024, 20)
        );
        assert_eq!(
            clamp_origin_to_viewport(Origin::new(-600, 20), size, viewport),
            Origin::new(-600, 20)
        );
    }

    #[test]
    fn clamp_origin_keeps_regular_windows_inside_viewport() {
        let viewport = IRect::new(0, 20, 1024, 768);
        let size = Size::new(400, 300);

        assert_eq!(
            clamp_origin_to_viewport(Origin::new(-100, 900), size, viewport),
            Origin::new(0, 468)
        );
    }

    fn setup_world_and_strip() -> (World, LayoutStrip, Vec<Entity>) {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);

        (world, strip, entities)
    }

    /// Renders the strip with each window named by its letter in `names`:
    /// `A | [B, {C D}] | !E`, where `[..]` is a stack, `{..}` native tabs and
    /// `!` a fullscreen column.
    fn render(strip: &LayoutStrip, names: &[Entity]) -> String {
        let name = |entity: &Entity| {
            let index = names.iter().position(|e| e == entity).unwrap();
            char::from(b'A' + u8::try_from(index).unwrap()).to_string()
        };
        let tabs = |tabs: &[Entity]| {
            format!(
                "{{{}}}",
                tabs.iter().map(name).collect::<Vec<_>>().join(" ")
            )
        };
        strip
            .columns()
            .map(|column| match column {
                Column::Single(entity) => name(entity),
                Column::Fullscren(entity) => format!("!{}", name(entity)),
                Column::Tabs(entities) => tabs(entities),
                Column::Maximized(maximized) => match &maximized.item {
                    StackItem::Single(entity) => format!("^{}", name(entity)),
                    StackItem::Tabs(entities) => format!("^{}", tabs(entities)),
                },
                Column::Stack(items) => format!(
                    "[{}]",
                    items
                        .iter()
                        .map(|item| match item {
                            StackItem::Single(entity) => name(entity),
                            StackItem::Tabs(entities) => tabs(entities),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn strip_of(count: usize) -> (World, LayoutStrip, Vec<Entity>) {
        let mut world = World::new();
        let entities = world
            .spawn_batch(std::iter::repeat_n((), count))
            .collect::<Vec<Entity>>();
        let mut strip = LayoutStrip::default();
        for entity in &entities {
            strip.append(*entity);
        }
        (world, strip, entities)
    }

    /// What a window half the viewport wide and tall restores to.
    fn half() -> Before {
        Before::new(ViewportShare(0.5), ViewportShare(0.5), ViewportAnchor::Left)
    }

    #[test]
    fn a_maximized_column_takes_the_viewport_whatever_its_bounds() {
        let (_world, mut strip, e) = strip_of(3);
        strip.maximize(e[1], Side::East, half()).unwrap();
        let frame = |_| Some(IRect::new(0, 0, 300, 500));

        let frames = strip
            .relative_positions(Size::new(1000, 700), &frame)
            .collect::<Vec<_>>();
        assert_eq!(
            frames,
            vec![
                (e[0], IRect::new(0, 0, 300, 700)),
                (e[1], IRect::new(300, 0, 1300, 700)),
                (e[2], IRect::new(1300, 0, 1600, 700)),
            ]
        );
    }

    #[test]
    fn maximizing_twice_changes_nothing() {
        let (_world, mut strip, e) = strip_of(2);
        assert!(strip.maximize(e[0], Side::East, half()).unwrap());
        assert!(!strip.maximize(e[0], Side::East, half()).unwrap());
        assert_eq!(render(&strip, &e), "^A | B");
        assert_eq!(strip.maximized(e[0]), Some(&half()));
    }

    #[test]
    fn a_stacked_window_maximizes_beside_its_stack_and_restores_into_it() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[1], Side::West).unwrap();
        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, B, C] | D");

        strip.maximize(e[1], Side::East, half()).unwrap();
        assert_eq!(render(&strip, &e), "[A, C] | ^B | D");
        strip.maximize(e[3], Side::West, half()).unwrap();
        assert_eq!(render(&strip, &e), "[A, C] | ^B | ^D");

        let restored = strip.unmaximize(e[1], home(3)).unwrap();
        assert_eq!(render(&strip, &e), "[A, B, C] | ^D");
        assert_eq!(restored.height, Some(ViewportShare(0.5)));
        assert_eq!(strip.unmaximize(e[1], home(3)), None, "no longer maximized");
    }

    fn home(limit: usize) -> RestoreTo {
        RestoreTo::Home { limit }
    }

    #[test]
    fn a_window_restored_here_stays_a_column_where_it_stands() {
        let (_world, mut strip, e) = strip_of(3);
        strip.stack(e[1], Side::West).unwrap();
        strip.maximize(e[1], Side::East, half()).unwrap();
        let restored = strip.unmaximize(e[1], RestoreTo::Here).unwrap();
        assert_eq!(render(&strip, &e), "A | B | C");
        assert_eq!(restored.height, None);
    }

    #[test]
    fn a_restored_window_stays_a_column_when_its_stack_is_gone_or_full() {
        let (_world, mut strip, e) = strip_of(3);
        strip.stack(e[1], Side::West).unwrap();
        strip.maximize(e[1], Side::East, half()).unwrap();
        strip.remove(e[0]);
        let restored = strip.unmaximize(e[1], home(3)).unwrap();
        assert_eq!(render(&strip, &e), "B | C");
        assert_eq!(restored.height, None);
    }

    /// Window A, B's mate, moves into C's column while B is maximized.
    fn a_mate_moved_away() -> (World, LayoutStrip, Vec<Entity>) {
        let (world, mut strip, e) = strip_of(4);
        strip.stack(e[1], Side::West).unwrap();
        strip.maximize(e[1], Side::East, half()).unwrap();
        strip.insert_as_column(e[0], 3).unwrap();
        strip.stack(e[0], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "^B | [C, A] | D");
        (world, strip, e)
    }

    #[test]
    fn a_restored_window_goes_back_to_a_mate_that_moved() {
        let (_world, mut strip, e) = a_mate_moved_away();
        strip.unmaximize(e[1], home(3));
        assert_eq!(render(&strip, &e), "[C, B, A] | D");
    }

    #[test]
    fn a_restored_window_stays_a_column_when_its_mates_column_is_full() {
        let (_world, mut strip, e) = a_mate_moved_away();
        strip.unmaximize(e[1], home(2));
        assert_eq!(render(&strip, &e), "B | [C, A] | D");
    }

    #[test]
    fn nothing_joins_a_maximized_column() {
        let (_world, mut strip, e) = strip_of(3);
        strip.maximize(e[1], Side::East, half()).unwrap();
        strip.stack(e[0], Side::East).unwrap();
        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | ^B | C");
        assert!(strip.join_column(&[e[0]], 1, 0).is_err());
    }

    #[test]
    fn a_reorder_keeps_a_column_maximized() {
        let (_world, mut strip, e) = strip_of(3);
        strip.maximize(e[1], Side::East, half()).unwrap();
        strip.insert_as_column(e[1], 3).unwrap();
        assert_eq!(render(&strip, &e), "A | C | ^B");
        strip.insert_as_column(e[1], 0).unwrap();
        assert_eq!(render(&strip, &e), "^B | A | C");
        assert_eq!(strip.maximized(e[1]), Some(&half()));
    }

    #[test]
    fn a_window_leaving_a_maximized_column_is_released() {
        let (_world, mut strip, e) = strip_of(3);
        strip.maximize(e[0], Side::East, half()).unwrap();
        strip.maximize(e[1], Side::East, half()).unwrap();
        strip.maximize(e[2], Side::East, half()).unwrap();
        strip.unmaximize(e[0], home(3));
        strip.remove(e[1]);
        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, C]");
        assert_eq!(strip.take_released(), e.iter().copied().collect());
        assert!(strip.take_released().is_empty());
    }

    #[test]
    fn an_adopted_window_restores_what_it_carried() {
        let (_world, mut strip, e) = strip_of(2);
        assert!(strip.adopt_maximized(e[1], half()));
        assert_eq!(render(&strip, &e), "A | ^B");
        let restored = strip.unmaximize(e[1], home(3)).unwrap();
        assert_eq!(restored.width, ViewportShare(0.5));
        assert_eq!(render(&strip, &e), "A | B");
    }

    #[test]
    fn a_column_anchor_carries_to_another_viewport() {
        let small = IRect::new(0, 0, 1000, 700);
        let large = IRect::new(1000, 0, 3000, 1200);
        let middle = ViewportAnchor::of(IRect::new(400, 0, 600, 700), small, false, false, false);
        assert_eq!(middle.in_viewport(large), WidthAnchor::Center(2000));
        let left = ViewportAnchor::of(IRect::new(0, 0, 600, 700), small, true, false, false);
        assert_eq!(left.in_viewport(large), WidthAnchor::Left);
    }

    #[test]
    fn stacking_next_to_a_fullscreen_column_keeps_every_window() {
        let (_world, mut strip, e) = strip_of(3);
        *strip.get_column_mut(1).unwrap() = Column::Fullscren(e[1]);

        strip.stack(e[1], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | !B | C");

        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | !B | C");

        strip.stack(e[0], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "A | !B | C");
    }

    #[test]
    fn stacking_west_moves_only_the_window_one_column_per_press() {
        let (_world, mut strip, e) = strip_of(4);

        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | B | [C, D]");
        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, D] | C");
        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, D] | B | C");
        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, D] | B | C");
    }

    #[test]
    fn stacking_east_moves_only_the_window_one_column_per_press() {
        let (_world, mut strip, e) = strip_of(4);

        strip.stack(e[0], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "[B, A] | C | D");
        strip.stack(e[0], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "B | [C, A] | D");
        strip.stack(e[0], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "B | C | [D, A]");
        strip.stack(e[0], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "B | C | [D, A]");
    }

    #[test]
    fn stacking_from_inside_a_stack_leaves_the_rest_in_place() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[2], Side::West).unwrap();
        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, C, D]");

        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, C] | [B, D]");

        strip.stack(e[1], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, C, B] | D");

        strip.stack(e[2], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "[A, B] | [D, C]");
    }

    #[test]
    fn stacking_a_native_tab_moves_its_whole_tab_group() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[2], Side::West).unwrap();
        strip.convert_to_tabs(e[2], e[3]).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, {D C}]");

        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, {D C}] | B");
    }

    #[test]
    fn unstacking_opens_a_column_on_the_chosen_side() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[1], Side::West).unwrap();
        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, B, C] | D");

        strip.unstack(e[1], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "B | [A, C] | D");

        strip.unstack(e[2], Side::East).unwrap();
        assert_eq!(render(&strip, &e), "B | A | C | D");

        strip.unstack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "B | A | C | D");
    }

    #[test]
    fn a_drop_into_a_column_lands_at_the_slot() {
        let (_world, mut strip, e) = strip_of(4);

        strip.insert_into_column(e[3], 0, 0).unwrap();
        assert_eq!(render(&strip, &e), "[D, A] | B | C");
        strip.insert_into_column(e[2], 0, 1).unwrap();
        assert_eq!(render(&strip, &e), "[D, C, A] | B");
        strip.insert_into_column(e[1], 0, 3).unwrap();
        assert_eq!(render(&strip, &e), "[D, C, A, B]");
    }

    #[test]
    fn a_drop_into_a_column_right_of_a_vanishing_column_finds_it() {
        let (_world, mut strip, e) = strip_of(3);

        strip.insert_into_column(e[0], 2, 0).unwrap();
        assert_eq!(render(&strip, &e), "B | [A, C]");
    }

    #[test]
    fn a_slot_past_the_bottom_makes_the_bottom_window() {
        let (_world, mut strip, e) = strip_of(3);

        strip.insert_into_column(e[2], 0, 9).unwrap();
        assert_eq!(render(&strip, &e), "[A, C] | B");
    }

    #[test]
    fn a_drop_into_its_own_stack_reorders_it() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[1], Side::West).unwrap();
        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, B, C] | D");

        strip.insert_into_column(e[0], 0, 2).unwrap();
        assert_eq!(render(&strip, &e), "[B, C, A] | D");
        strip.insert_into_column(e[0], 0, 0).unwrap();
        assert_eq!(render(&strip, &e), "[A, B, C] | D");
        strip.insert_into_column(e[2], 0, 1).unwrap();
        assert_eq!(render(&strip, &e), "[A, C, B] | D");
    }

    #[test]
    fn a_drop_out_of_a_stack_leaves_the_rest_in_place() {
        let (_world, mut strip, e) = strip_of(3);
        strip.stack(e[1], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, B] | C");

        strip.insert_into_column(e[1], 1, 0).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, C]");
        assert!(strip.even_splits.contains(&e[0]), "the column it left");
        assert!(strip.even_splits.contains(&e[1]), "the column it joined");
    }

    #[test]
    fn a_dropped_native_tab_moves_its_whole_tab_group() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[2], Side::West).unwrap();
        strip.convert_to_tabs(e[2], e[3]).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, {D C}]");

        strip.insert_into_column(e[2], 0, 0).unwrap();
        assert_eq!(render(&strip, &e), "[{D C}, A] | B");
    }

    #[test]
    fn a_fullscreen_column_takes_no_drop() {
        let (_world, mut strip, e) = strip_of(3);
        *strip.get_column_mut(1).unwrap() = Column::Fullscren(e[1]);

        strip.insert_into_column(e[0], 1, 0).unwrap();
        assert_eq!(render(&strip, &e), "A | !B | C");
        strip.insert_into_column(e[1], 0, 0).unwrap();
        assert_eq!(render(&strip, &e), "A | !B | C");
        strip.insert_as_column(e[1], 0).unwrap();
        assert_eq!(render(&strip, &e), "A | !B | C");
    }

    #[test]
    fn a_drop_into_a_tabbed_stack_joins_its_tabs() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[2], Side::West).unwrap();
        assert_eq!(strip.toggle_tabbed_display(e[1]), Some(true));

        strip.insert_into_column(e[3], 1, 1).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, D, C]");
        assert!(strip.is_tabbed_display(e[3]));
    }

    #[test]
    fn reordering_a_tabbed_stack_of_two_keeps_it_tabbed() {
        let (_world, mut strip, e) = strip_of(2);
        strip.stack(e[1], Side::West).unwrap();
        assert_eq!(strip.toggle_tabbed_display(e[0]), Some(true));

        strip.insert_into_column(e[1], 0, 0).unwrap();
        assert_eq!(render(&strip, &e), "[B, A]");
        assert!(strip.is_tabbed_display(e[0]));
        assert!(strip.is_tabbed_display(e[1]));
    }

    #[test]
    fn a_drop_between_columns_opens_a_column_there() {
        let (_world, mut strip, e) = strip_of(3);

        strip.insert_as_column(e[0], 3).unwrap();
        assert_eq!(render(&strip, &e), "B | C | A");
        strip.insert_as_column(e[0], 0).unwrap();
        assert_eq!(render(&strip, &e), "A | B | C");
        strip.insert_as_column(e[0], 2).unwrap();
        assert_eq!(render(&strip, &e), "B | A | C");
    }

    #[test]
    fn a_drop_between_columns_takes_a_window_out_of_its_stack() {
        let (_world, mut strip, e) = strip_of(4);
        strip.stack(e[1], Side::West).unwrap();
        strip.convert_to_tabs(e[2], e[3]).unwrap();
        assert_eq!(render(&strip, &e), "[A, B] | {D C}");

        strip.insert_as_column(e[1], 0).unwrap();
        assert_eq!(render(&strip, &e), "B | A | {D C}");
        assert!(strip.even_splits.contains(&e[0]), "the column it left");

        strip.stack(e[0], Side::West).unwrap();
        strip.insert_as_column(e[0], 2).unwrap();
        assert_eq!(render(&strip, &e), "B | {D C} | A");

        strip.insert_as_column(e[2], 0).unwrap();
        assert_eq!(render(&strip, &e), "{D C} | B | A");
    }

    #[test]
    fn stacking_carries_tabbed_display_membership_with_the_stack() {
        let (_world, mut strip, e) = strip_of(5);
        strip.stack(e[2], Side::West).unwrap();
        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(strip.toggle_tabbed_display(e[1]), Some(true));

        strip.stack(e[4], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "A | [B, C, D, E]");
        assert!(strip.is_tabbed_display(e[4]), "joining a tabbed stack");

        strip.stack(e[1], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, B] | [C, D, E]");
        assert!(!strip.is_tabbed_display(e[0]));
        assert!(!strip.is_tabbed_display(e[1]), "leaving a tabbed stack");
        assert!(strip.is_tabbed_display(e[2]));

        strip.stack(e[2], Side::West).unwrap();
        strip.stack(e[3], Side::West).unwrap();
        assert_eq!(render(&strip, &e), "[A, B, C, D] | E");
        assert!(!strip.is_tabbed_display(e[3]), "joining a split stack");
        assert!(!strip.is_tabbed_display(e[4]), "a lone survivor");
    }

    #[test]
    fn strip_has_changed_window_matches_nested_entities() {
        let (mut world, mut strip, entities) = setup_world_and_strip();
        strip.stack(entities[1], Side::West).unwrap();
        strip.convert_to_tabs(entities[0], entities[1]).unwrap();

        let mut changed = EntityHashSet::default();
        changed.insert(entities[1]);

        assert!(strip_has_changed_window(&strip, &changed));

        changed.clear();
        changed.insert(world.spawn_empty().id());

        assert!(!strip_has_changed_window(&strip, &changed));
    }

    #[test]
    fn strip_window_contexts_capture_stack_membership_once() {
        let (mut world, mut strip, entities) = setup_world_and_strip();
        strip.stack(entities[1], Side::West).unwrap();
        let display_entity = world.spawn_empty().id();
        let strip_position = Origin::new(10, 20);
        let mut contexts = EntityHashMap::default();

        insert_strip_window_contexts(
            &mut contexts,
            &strip,
            strip_position,
            true,
            display_entity,
            false,
        );

        let stacked_leader = contexts.get(&entities[0]).unwrap();
        let stacked_follower = contexts.get(&entities[1]).unwrap();
        let single_window = contexts.get(&entities[2]).unwrap();

        assert_eq!(stacked_leader.strip_position, strip_position);
        assert_eq!(stacked_leader.display_entity, display_entity);
        assert!(stacked_leader.swiping);
        assert!(stacked_leader.stacked);
        assert!(stacked_follower.stacked);
        assert!(!single_window.stacked);
    }

    #[test]
    fn test_window_pane_index_of() {
        let (_world, strip, entities) = setup_world_and_strip();
        assert_eq!(strip.index_of(entities[0]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[1]).unwrap(), 1);
        assert_eq!(strip.index_of(entities[2]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_swap() {
        let (_world, mut strip, entities) = setup_world_and_strip();
        strip.swap(0, 2);
        assert_eq!(strip.index_of(entities[2]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[0]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_stack_and_unstack() {
        let (_world, mut strip, entities) = setup_world_and_strip();

        // Stack [1] onto [0]
        strip.stack(entities[1], Side::West).unwrap();
        assert_eq!(strip.len(), 2);
        assert_eq!(strip.index_of(entities[0]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[1]).unwrap(), 0); // Both in the same panel

        // Check internal structure
        match strip.get(0).unwrap() {
            Column::Stack(stack) => {
                assert_eq!(stack.len(), 2);
                assert_eq!(stack[0], StackItem::Single(entities[0]));
                assert_eq!(stack[1], StackItem::Single(entities[1]));
            }
            Column::Single(_) | Column::Fullscren(_) | Column::Tabs(_) | Column::Maximized(_) => {
                panic!("Expected a stack")
            }
        }

        // Unstack [0]
        strip.unstack(entities[0], Side::East).unwrap();
        assert_eq!(strip.len(), 3);
        assert_eq!(strip.index_of(entities[1]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[0]).unwrap(), 1);
        assert_eq!(strip.index_of(entities[2]).unwrap(), 2);
    }

    /// A short window that cannot be resized counts at its own height when
    /// deciding whether a stack fits, not at the minimum window height.
    #[test]
    fn a_short_fixed_height_counts_at_its_height_when_fitting() {
        let (_world, mut strip, e) = strip_of(2);
        strip.stack(e[1], Side::West).unwrap();
        let frame = |entity| {
            let height = if entity == e[0] { 100 } else { 700 };
            Some(IRect::new(0, 0, 400, height))
        };
        let minimum = |entity| Some(if entity == e[0] { 100 } else { 700 });
        let maximum = |entity| (entity == e[0]).then_some(100);
        let frames = strip.relative_positions_with_minimums(
            Size::new(1000, 848),
            &frame,
            &minimum,
            &maximum,
            &|_| FALLBACK_TOP_BAR_HEIGHT,
            &|_| WidthLimits::default(),
        );
        let rows = frames
            .iter()
            .map(|(_, frame)| (frame.min.y, frame.height()))
            .collect::<Vec<_>>();
        assert_eq!(rows, vec![(0, 100), (100, 748)]);
    }

    #[test]
    fn minimum_heights_take_room_evenly_from_the_other_windows() {
        let mut heights = vec![423, 423, 424];
        hold_minimum_heights(&mut heights, &[0, 0, 500], 1270);
        assert_eq!(heights, vec![385, 385, 500]);

        // A window already at the floor gives nothing.
        let mut heights = vec![200, 500, 570];
        hold_minimum_heights(&mut heights, &[0, 0, 700], 1270);
        assert_eq!(heights, vec![200, 370, 700]);

        // A window with a smaller minimum above its share keeps an even share.
        let mut heights = vec![426, 426, 426];
        hold_minimum_heights(&mut heights, &[308, 628, 0], 1278);
        assert_eq!(heights, vec![325, 628, 325]);

        // Minimums that cannot all fit overflow instead of overlapping.
        let mut heights = vec![400, 400];
        hold_minimum_heights(&mut heights, &[600, 600], 800);
        assert_eq!(heights, vec![600, 600]);
    }

    fn rows(rows: &[(i32, i32)]) -> Vec<Row> {
        rows.iter().map(|&(y, height)| Row { y, height }).collect()
    }

    /// The last window ends at the column bottom, and the upper windows share
    /// what it leaves above evenly, each strip at least its top bar. Every
    /// window reaches the column bottom.
    #[test]
    fn overlapping_rows_share_the_space_above_the_last_window() {
        // 548 above the last window: 260 of top bars and 288 shared, 57 each
        // with the 3 left over going to the first windows.
        assert_eq!(
            overlapping_rows(748, &[200; 6], &[52; 6]),
            rows(&[
                (0, 748),
                (110, 638),
                (220, 528),
                (330, 418),
                (439, 309),
                (548, 200),
            ])
        );
    }

    /// A window starts high enough to fit above the column bottom at its
    /// floor, which leaves the window above it a smaller strip.
    #[test]
    fn overlapping_rows_fit_a_tall_window_above_the_bottom() {
        assert_eq!(
            overlapping_rows(1270, &[200, 900, 200], &[60, 40, 52]),
            rows(&[(0, 1270), (370, 900), (1070, 200)])
        );
    }

    /// Top bars that leave no room to share sit back to back.
    #[test]
    fn overlapping_rows_stack_top_bars_when_nothing_is_left() {
        assert_eq!(
            overlapping_rows(1270, &[1200; 3], &[52; 3]),
            rows(&[(0, 1270), (52, 1218), (104, 1200)])
        );
    }

    /// The stack runs past the column bottom only from the window whose top
    /// bars above it and floor no longer fit.
    #[test]
    fn overlapping_rows_spill_only_when_top_bars_do_not_fit() {
        let spilled = overlapping_rows(500, &[200; 8], &[52; 8]);
        assert_eq!(
            spilled.iter().map(|row| row.y).collect::<Vec<_>>(),
            vec![0, 52, 104, 156, 208, 260, 312, 364]
        );
        let bottoms = spilled
            .iter()
            .map(|row| row.y + row.height)
            .collect::<Vec<_>>();
        assert_eq!(bottoms, vec![500, 500, 500, 500, 500, 500, 512, 564]);
    }

    #[test]
    fn overlapping_rows_give_a_lone_window_its_floor() {
        assert_eq!(overlapping_rows(700, &[900], &[52]), rows(&[(0, 900)]));
        assert_eq!(overlapping_rows(700, &[], &[]), vec![]);
    }

    /// Binpack gives up on these heights, though an even split fits every
    /// floor. The column is still laid out instead of being left out.
    #[test]
    fn a_stack_binpack_gives_up_on_is_split_evenly() {
        assert_eq!(
            binpack_heights(&[900, 50, 50], MIN_WINDOW_HEIGHT, 1000),
            None
        );

        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();
        let mut strip = LayoutStrip::default();
        for &entity in &entities {
            strip.append(entity);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();
        strip.take_even_splits();
        let heights = [900, 50, 50];
        let frame = |entity: Entity| {
            let index = entities.iter().position(|e| *e == entity)?;
            Some(IRect::new(0, 0, 300, heights[index]))
        };

        let out = strip
            .relative_positions(Size::new(1000, 1000), &frame)
            .collect::<Vec<_>>();
        assert_eq!(
            out.iter()
                .map(|(_, frame)| (frame.min.y, frame.height()))
                .collect::<Vec<_>>(),
            vec![(0, 333), (333, 333), (666, 334)]
        );
    }

    /// Windows too many for the minimum window height overlap instead of
    /// leaving the layout.
    #[test]
    fn a_stack_too_tall_for_the_minimum_window_height_is_laid_out() {
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();
        let mut strip = LayoutStrip::default();
        for &entity in &entities {
            strip.append(entity);
        }
        for &entity in &entities[1..] {
            strip.stack(entity, Side::West).unwrap();
        }
        let frame = |_| Some(IRect::new(0, 0, 300, 300));

        let out = strip
            .relative_positions(Size::new(1000, 700), &frame)
            .collect::<Vec<_>>();
        // 500 above the last window: 156 of top bars, 344 shared.
        assert_eq!(
            out.iter()
                .map(|(_, frame)| (frame.min.y, frame.height()))
                .collect::<Vec<_>>(),
            vec![(0, 700), (167, 533), (334, 366), (500, 200)]
        );
    }

    #[test]
    fn test_binpack() {
        const MIN_HEIGHT: i32 = 100;
        let heights = [300, 300, 300, 300];

        let out = binpack_heights(&heights, MIN_HEIGHT, 1500).unwrap();
        assert_eq!(out, vec![300, 300, 300, 600]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 1024).unwrap();
        assert_eq!(out, vec![300, 300, 300, 124]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 800).unwrap();
        assert_eq!(out, vec![300, 300, 100, 100]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 440).unwrap();
        assert_eq!(out, vec![110, 110, 110, 110]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 390);
        assert_eq!(out, None);
    }

    #[test]
    fn test_layout_positioning() {
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();
        let sizes = [
            IRect::new(0, 0, 300, 300),
            IRect::new(0, 0, 300, 300),
            IRect::new(0, 0, 300, 300),
            IRect::new(0, 0, 300, 300),
        ];

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);

        _ = strip.stack(entities[2], Side::West);
        let get_window_frame = |_| Some(sizes[0]);
        let out = strip
            .relative_positions(Size::new(1000, 500), &get_window_frame)
            .collect::<Vec<_>>();

        let xpos = out.iter().map(|(_, frame)| frame.min.x).collect::<Vec<_>>();
        assert_eq!(xpos, vec![0, 300, 300, 600]);

        let height = out
            .iter()
            .map(|(_, frame)| frame.height())
            .collect::<Vec<_>>();
        // The window that joined the stack splits the column evenly.
        assert_eq!(height, vec![500, 250, 250, 500]);
    }

    /// Every single-column window must fill the full viewport height.
    #[test]
    fn test_layout_singles_get_full_viewport_height() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 400));
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 800), &get_window_frame)
            .collect();

        assert_eq!(out.len(), 3);
        for (_, f) in &out {
            assert_eq!(f.height(), 800, "single window should fill viewport height");
            assert_eq!(f.min.y, 0);
        }
        // x positions: 0, 300, 600
        let xs: Vec<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(xs, vec![0, 300, 600]);
    }

    /// Stacked windows share the viewport height; all use the top window's width.
    #[test]
    fn test_layout_stack_shares_height_and_width() {
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        // Stack e1, e2 onto e0: [Stack(e0, e1, e2), Single(e3)]
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();

        // Give the master (e0) a distinct width so we can verify children
        // adopt it. Here e0 is 500px while its stacked children are 300px;
        // children must widen to the master's 500, not keep their own width.
        let get_window_frame = |e: Entity| {
            if e == entities[0] {
                Some(IRect::new(0, 0, 500, 200))
            } else if e == entities[1] || e == entities[2] {
                Some(IRect::new(0, 0, 300, 200))
            } else {
                Some(IRect::new(0, 0, 400, 500))
            }
        };

        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 600), &get_window_frame)
            .collect();
        assert_eq!(out.len(), 4);

        // Every window in the stacked column must adopt the master's width.
        for e in [entities[0], entities[1], entities[2]] {
            let frame = out.iter().find(|(entity, _)| *entity == e).unwrap().1;
            assert_eq!(
                frame.width(),
                500,
                "stacked window must share the master's (top) width"
            );
        }

        // Stacked heights should sum to viewport height.
        let stack_heights: i32 = out
            .iter()
            .filter(|(e, _)| *e != entities[3])
            .map(|(_, f)| f.height())
            .sum();
        assert_eq!(stack_heights, 600, "stack heights must sum to viewport");

        // Stacked y positions should be contiguous from 0.
        let stack_frames: Vec<_> = out
            .iter()
            .filter(|(e, _)| *e != entities[3])
            .map(|(_, f)| *f)
            .collect();
        assert_eq!(stack_frames[0].min.y, 0);
        assert_eq!(stack_frames[0].max.y, stack_frames[1].min.y);
        assert_eq!(stack_frames[1].max.y, stack_frames[2].min.y);
        assert_eq!(stack_frames[2].max.y, 600);

        // e3 (single) gets full viewport height.
        let e3_frame = out.iter().find(|(e, _)| *e == entities[3]).unwrap().1;
        assert_eq!(e3_frame.height(), 600);
    }

    #[test]
    fn test_toggle_tabbed_display_marks_and_unmarks_all_stack_members() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();

        assert!(entities.iter().all(|&e| !strip.is_tabbed_display(e)));

        let now_tabbed = strip.toggle_tabbed_display(entities[1]);
        assert_eq!(now_tabbed, Some(true));
        assert!(entities.iter().all(|&e| strip.is_tabbed_display(e)));

        let now_tabbed = strip.toggle_tabbed_display(entities[0]);
        assert_eq!(now_tabbed, Some(false));
        assert!(entities.iter().all(|&e| !strip.is_tabbed_display(e)));
    }

    #[test]
    fn test_toggle_tabbed_display_noop_on_single_column() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(entity);

        assert_eq!(
            strip.toggle_tabbed_display(entity),
            None,
            "toggling a lone Single column must be a no-op, not report 'off'"
        );
        assert!(!strip.is_tabbed_display(entity));
    }

    #[test]
    fn test_tabbed_stack_shares_full_column_frame() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();
        strip.toggle_tabbed_display(entities[1]);

        let get_window_frame = |_| Some(IRect::new(0, 0, 400, 300));
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 700), &get_window_frame)
            .collect();

        assert_eq!(out.len(), 3);
        for (_, frame) in &out {
            assert_eq!(frame.min.y, 0, "every tab must start at the column's top");
            assert_eq!(
                frame.height(),
                700,
                "every tab must fill the full column height"
            );
            assert_eq!(frame.width(), 400);
        }
        // All three must be at the identical x too (same column slot).
        let xs: std::collections::HashSet<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(xs.len(), 1, "every tab must share the same column x");
    }

    #[test]
    fn test_tabbed_stack_untoggle_restores_split_heights() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();

        let get_window_frame = |_| Some(IRect::new(0, 0, 400, 200));

        strip.toggle_tabbed_display(entities[1]);
        strip.toggle_tabbed_display(entities[1]);

        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 600), &get_window_frame)
            .collect();
        let heights: Vec<_> = out.iter().map(|(_, f)| f.height()).collect();
        assert_eq!(
            heights.iter().sum::<i32>(),
            600,
            "heights must fill the viewport"
        );
        assert!(
            heights.iter().any(|&h| h != 600),
            "split display must not leave every window at full column height"
        );
    }

    /// Regression: an earlier version of tabbed-display support reordered
    /// the outer `Vec<StackItem>` on every focus change (mirroring how
    /// native-tab `Column::move_to_front` reorders `StackItem::Tabs`), so
    /// `Column::top()` would track the active tab. That broke cycling:
    /// `get_window_in_direction`'s North/South arms walk the stack purely by
    /// stable index, so reordering it out from under them made focus jump
    /// unpredictably instead of advancing. Tabbed-display columns must never
    /// reorder — `Column::top()` intentionally stays whatever the first item
    /// was, and the true "active tab" is tracked only by `FocusedMarker`.
    #[test]
    fn test_toggle_tabbed_display_never_reorders_the_stack() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();
        strip.toggle_tabbed_display(entities[1]);

        let index = strip.index_of(entities[2]).unwrap();
        let Column::Stack(items) = strip.get(index).unwrap() else {
            panic!("expected a Stack column");
        };
        let order: Vec<_> = items.iter().filter_map(StackItem::top).collect();
        assert_eq!(
            order, entities,
            "toggling tabbed display must not reorder the stack"
        );
    }

    #[test]
    fn test_tab_display_sibling_falls_back_at_ends() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.stack(entities[2], Side::West).unwrap();
        strip.toggle_tabbed_display(entities[1]);

        assert_eq!(strip.tab_display_sibling(entities[0]), Some(entities[1]));
        assert_eq!(strip.tab_display_sibling(entities[1]), Some(entities[2]));
        // Last item falls back to the previous one instead of wrapping.
        assert_eq!(strip.tab_display_sibling(entities[2]), Some(entities[1]));
    }

    #[test]
    fn test_removing_from_tabbed_stack_collapsing_to_single_clears_membership() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        strip.stack(entities[1], Side::West).unwrap();
        strip.toggle_tabbed_display(entities[1]);
        assert!(strip.is_tabbed_display(entities[0]));

        strip.remove(entities[1]);

        assert!(
            !strip.is_tabbed_display(entities[0]),
            "a lone surviving window can't remain a tabbed display"
        );
        let index = strip.index_of(entities[0]).unwrap();
        assert!(matches!(strip.get(index).unwrap(), Column::Single(_)));
    }

    #[test]
    fn a_column_mate_is_outside_the_native_tab_group() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e4 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.append(e2);
        strip.stack(e2, Side::West).unwrap();
        strip.convert_to_tabs(e1, e4).unwrap();
        // [Stack([Tabs([e4, e1]), Single(e2)])]

        assert_eq!(strip.column_mate(e1), Some(e2));
        assert_eq!(strip.column_mate(e4), Some(e2));
        assert_eq!(strip.column_mate(e2), Some(e4));
    }

    #[test]
    fn only_a_whole_stack_item_leaving_splits_the_column_evenly() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();
        let e4 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.append(e2);
        strip.append(e3);
        strip.stack(e2, Side::West).unwrap();
        strip.stack(e3, Side::West).unwrap();
        strip.convert_to_tabs(e1, e4).unwrap();
        // [Stack([Tabs([e4, e1]), Single(e2), Single(e3)])]
        strip.take_even_splits();

        // A closed native tab leaves its item in place.
        strip.remove(e4);
        assert!(strip.even_splits.is_empty());

        // A whole item leaving does not.
        strip.remove(e3);
        assert!(!strip.even_splits.is_empty());
    }

    #[test]
    fn test_tabs_in_stack() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();
        let e4 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.append(e2);
        strip.append(e3);

        // [Single(e1), Single(e2), Single(e3)]
        strip.stack(e2, Side::West).unwrap();
        // [Stack([Single(e1), Single(e2)]), Single(e3)]

        // Convert e1 (in stack) to tabs with e4
        strip.convert_to_tabs(e1, e4).unwrap();
        // [Stack([Tabs([e1, e4]), Single(e2)]), Single(e3)]

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Stack(items) => {
                assert_eq!(items.len(), 2);
                match &items[0] {
                    StackItem::Tabs(tabs) => assert_eq!(tabs, &vec![e4, e1]),
                    StackItem::Single(_) => panic!("Expected Tabs in stack"),
                }
            }
            _ => panic!("Expected Stack"),
        }

        // relative_positions should yield e1, e4 (same frame) and e2
        let get_window_frame = |_| Some(IRect::new(0, 0, 100, 100));
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 400), &get_window_frame)
            .collect();

        // We expect e1, e4, e2 from the first column, and e3 from the second.
        assert_eq!(out.len(), 4);

        let e1_frame = out.iter().find(|(e, _)| *e == e1).unwrap().1;
        let e4_frame = out.iter().find(|(e, _)| *e == e4).unwrap().1;
        let e2_frame = out.iter().find(|(e, _)| *e == e2).unwrap().1;

        assert_eq!(e1_frame, e4_frame);
        assert_eq!(e1_frame.max.y, e2_frame.min.y);
    }

    /// Unstacking a window from a stack gives it its own column with full height.
    #[test]
    fn test_layout_unstack_gives_full_height() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        // [Stack(e0, e1), Single(e2)]
        strip.stack(entities[1], Side::West).unwrap();

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 250));

        // Before unstack: e0 and e1 share 500px height.
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 500), &get_window_frame)
            .collect();
        let e1_height = out
            .iter()
            .find(|(e, _)| *e == entities[1])
            .unwrap()
            .1
            .height();
        assert!(e1_height < 500, "stacked e1 should not have full height");

        // Unstack e1: [Single(e0), Single(e1), Single(e2)]
        strip.unstack(entities[1], Side::East).unwrap();
        assert_eq!(strip.len(), 3);

        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 500), &get_window_frame)
            .collect();
        for (_, f) in &out {
            assert_eq!(
                f.height(),
                500,
                "after unstack every single column gets full viewport height"
            );
        }
    }

    /// Re-stacking after unstack restores shared height distribution.
    #[test]
    fn test_layout_restack_restores_shared_heights() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 250));

        // Stack: [Stack(e0, e1)]
        strip.stack(entities[1], Side::West).unwrap();
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 500), &get_window_frame)
            .collect();
        let heights: Vec<_> = out.iter().map(|(_, f)| f.height()).collect();
        assert_eq!(heights.iter().sum::<i32>(), 500);
        assert_eq!(heights.len(), 2);

        // Unstack: [Single(e0), Single(e1)]
        strip.unstack(entities[1], Side::East).unwrap();
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 500), &get_window_frame)
            .collect();
        for (_, f) in &out {
            assert_eq!(f.height(), 500);
        }

        // Re-stack: [Stack(e0, e1)] — e1 stacks onto left neighbor e0
        strip.stack(entities[1], Side::West).unwrap();
        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 500), &get_window_frame)
            .collect();
        let heights: Vec<_> = out.iter().map(|(_, f)| f.height()).collect();
        assert_eq!(heights.iter().sum::<i32>(), 500);
        assert_eq!(heights.len(), 2);
    }

    /// When window frames include padding (logical frame is wider than the visual
    /// window), columns must be placed edge-to-edge using the full logical width.
    /// This ensures the visual gap between windows equals the sum of their padding.
    #[test]
    fn test_column_positions_with_padded_frames() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }

        // Simulate windows with padding=8: logical width = OS_width + 2*8.
        // Window 0: OS width 284, logical width 300
        // Window 1: OS width 384, logical width 400
        // Window 2: OS width 484, logical width 500
        let padded_frames = [
            IRect::new(0, 0, 300, 600), // logical frame with padding included
            IRect::new(0, 0, 400, 600),
            IRect::new(0, 0, 500, 600),
        ];

        let get_window_frame = |e: Entity| {
            if e == entities[0] {
                Some(padded_frames[0])
            } else if e == entities[1] {
                Some(padded_frames[1])
            } else {
                Some(padded_frames[2])
            }
        };

        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 600), &get_window_frame)
            .collect();
        assert_eq!(out.len(), 3);

        // Columns must be edge-to-edge: each column starts where the previous ends.
        let xs: Vec<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(
            xs,
            vec![0, 300, 700],
            "columns must be edge-to-edge using logical widths"
        );

        // Right edge of each window must equal left edge of the next.
        for i in 0..out.len() - 1 {
            assert_eq!(
                out[i].1.max.x,
                out[i + 1].1.min.x,
                "window {} right edge must equal window {} left edge",
                i,
                i + 1
            );
        }
    }

    /// Frames with no padding (padding=0) should still produce edge-to-edge layout.
    #[test]
    fn test_column_positions_no_padding() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 600));

        let out: Vec<_> = strip
            .relative_positions(Size::new(1000, 600), &get_window_frame)
            .collect();
        let xs: Vec<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(xs, vec![0, 300, 600]);

        // No gaps or overlaps.
        for i in 0..out.len() - 1 {
            assert_eq!(out[i].1.max.x, out[i + 1].1.min.x);
        }
    }

    #[test]
    fn test_convert_to_tabs() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.append(e3);

        // Convert e1 to a tab group with follower e2
        strip.convert_to_tabs(e1, e2).unwrap();

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => {
                assert_eq!(tabs, vec![e2, e1]);
            }
            _ => panic!("Expected Tabs column"),
        }

        // Add another tab e4
        let e4 = world.spawn_empty().id();
        strip.convert_to_tabs(e1, e4).unwrap();
        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => {
                assert_eq!(tabs, vec![e4, e2, e1]);
            }
            _ => panic!("Expected Tabs column"),
        }
    }

    #[test]
    fn test_remove_from_tabs() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.convert_to_tabs(e1, e2).unwrap();
        strip.convert_to_tabs(e1, e3).unwrap();

        assert_eq!(strip.len(), 1);

        // Remove e2 (follower)
        strip.remove(e2);
        assert_eq!(strip.len(), 1);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e3, e1]),
            _ => panic!(),
        }

        // Remove e1 (leader)
        strip.remove(e1);
        assert_eq!(strip.len(), 1);
        // Should convert back to Single since only e3 remains
        match strip.get(0).unwrap() {
            Column::Single(id) => assert_eq!(id, e3),
            _ => panic!("Expected Single column after removing all but one tab"),
        }
    }

    #[test]
    fn test_tab_group_returns_all_siblings() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.convert_to_tabs(e1, e2).unwrap();
        strip.append(e3);

        assert_eq!(strip.tab_group(e1), Some(vec![e2, e1]));
        assert_eq!(strip.tab_group(e2), Some(vec![e2, e1]));
        assert_eq!(strip.tab_group(e3), None);
    }

    #[test]
    fn test_append_tab_group_merges_existing_members_without_duplicate_columns() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e2);
        strip.append(e3);

        strip.append_tab_group(&[e1, e2]);

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e1, e2]),
            _ => panic!("Expected merged Tabs column"),
        }
        assert_eq!(strip.index_of(e1).unwrap(), 0);
        assert_eq!(strip.index_of(e2).unwrap(), 0);
        assert_eq!(strip.index_of(e3).unwrap(), 1);
        assert_eq!(strip.all_windows(), vec![e1, e2, e3]);
    }

    #[test]
    fn test_tab_relative_positions_use_stable_slot_representative() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.convert_to_tabs(e1, e2).unwrap();
        strip
            .get_column_mut(0)
            .expect("tab column")
            .move_to_front(e2);

        let get_window_frame = |entity| {
            if entity == e1 {
                Some(IRect::new(0, 0, 300, 600))
            } else {
                Some(IRect::new(900, 0, 1200, 400))
            }
        };

        let out = strip
            .relative_positions(Size::new(1000, 600), &get_window_frame)
            .collect::<Vec<_>>();

        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|(_, frame)| *frame == IRect::new(0, 0, 300, 600))
        );
    }

    #[test]
    fn test_overlapping_frame_strategy_simulation() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);

        // Simulate detection logic from spawn_window_trigger
        let leader_match = Some(e1); // Mocked match from frame comparison

        if let Some(leader) = leader_match {
            strip.convert_to_tabs(leader, e2).unwrap();
        } else {
            strip.append(e2);
        }

        assert_eq!(strip.len(), 1);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e2, e1]),
            _ => panic!(),
        }
    }

    // Mirrors the real `detect_tabbed_windows` flow: spawn_window_trigger
    // appends the new window to the strip first, and only then does the
    // tab detector merge it into the leader's column. Before the fix, the
    // follower was left in both columns, and right_neighbour from the
    // duplicated entity would self-loop because index_of returned the Tabs
    // column index, while the column at the next index was the orphaned
    // Single(follower).
    #[test]
    fn test_convert_to_tabs_removes_pre_existing_follower_column() {
        let mut world = World::new();
        let leader = world.spawn_empty().id();
        let follower = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(leader);
        strip.append(follower);

        strip.convert_to_tabs(leader, follower).unwrap();

        assert_eq!(
            strip.len(),
            1,
            "follower must not remain in its own column after being tabbed onto leader",
        );
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![follower, leader]),
            other => panic!("expected Tabs column, got {other:?}"),
        }
    }

    #[test]
    fn test_convert_to_tabs_no_self_loop_on_neighbour() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let leader = world.spawn_empty().id();
        let follower = world.spawn_empty().id();
        let b = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(a);
        strip.append(leader);
        strip.append(follower);
        strip.append(b);

        strip.convert_to_tabs(leader, follower).unwrap();

        // Both leader-as-focus and follower-as-focus must navigate to `b`
        // east, not back onto themselves.
        assert_eq!(strip.right_neighbour(leader), Some(b));
        assert_eq!(strip.right_neighbour(follower), Some(b));
        assert_eq!(strip.left_neighbour(leader), Some(a));
        assert_eq!(strip.left_neighbour(follower), Some(a));
    }

    #[test]
    fn test_convert_to_tabs_handles_follower_left_of_leader() {
        let mut world = World::new();
        let follower = world.spawn_empty().id();
        let leader = world.spawn_empty().id();
        let b = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(follower);
        strip.append(leader);
        strip.append(b);

        strip.convert_to_tabs(leader, follower).unwrap();

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![follower, leader]),
            other => panic!("expected Tabs column, got {other:?}"),
        }
        assert_eq!(strip.right_neighbour(leader), Some(b));
        assert_eq!(strip.right_neighbour(follower), Some(b));
    }
}
