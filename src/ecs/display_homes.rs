//! Each display has a home: the columns, stacks, and order its windows had
//! when the last display configuration burst began. macOS moves Spaces
//! between displays during a burst, and the engine's strips follow them, so
//! the homes are what puts each window back on the display it was on.
//!
//! A home enters at the start of a burst. It leaves at the final pass of a
//! burst on which its display is present, after the restore put its windows
//! back. A home whose display is absent at that final pass waits, untouched,
//! for its display.

use std::collections::{BTreeMap, HashMap, HashSet};

use bevy::ecs::entity::{Entity, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::world::World;
use bevy::math::IRect;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::display::{compact_workspace_strips, size_for_arrival};
use super::layout::{DetachedColumn, LayoutStrip, ViewportShare};
use super::native_membership::NativeMembership;
use super::{
    Bounds, DockPosition, Initializing, NativeFullscreenMarker, SelectedVirtualMarker,
    SpawnCommandsExt, Unmanaged,
};
use crate::config::Config;
use crate::manager::{Display, Size, Window, WindowManager};
use crate::platform::WorkspaceId;

/// How far a window's width may be from its remembered width before the
/// restore resizes it.
const WIDTH_TOLERANCE: i32 = 2;

/// Which pass of a display reconciliation burst just applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pass {
    /// macOS can still move Spaces after this pass, so homes stay.
    First,
    /// The last pass of the burst. Homes of present displays are consumed.
    Final,
}

/// The layouts the engine owes each display.
///
/// A window is in at most one home. A home of an absent display is never
/// rewritten, so reordering done while its display is gone cannot reach it.
/// Keyed by display id, because `remove_display` despawns the display entity.
#[derive(Default, Resource)]
pub(crate) struct DisplayHomes {
    homes: BTreeMap<CGDirectDisplayID, Home>,
}

/// What one display held when the burst began.
#[derive(Default)]
pub(crate) struct Home {
    /// In the order their columns go back, when several land in one strip.
    strips: Vec<RememberedStrip>,
    /// A final pass found the display absent. A returning display gets its
    /// windows' widths back. A display that never left keeps the widths its
    /// windows have now.
    returning: bool,
}

/// One strip, one virtual row of one Space, as it was.
pub(crate) struct RememberedStrip {
    space: WorkspaceId,
    virtual_index: u32,
    /// Left to right.
    columns: Vec<RememberedColumn>,
}

pub(crate) struct RememberedColumn {
    column: DetachedColumn,
    /// The top window's width over the display's strip viewport.
    width: ViewportShare,
}

impl DisplayHomes {
    /// Remembers every present display that has no home yet, minus the
    /// windows another home holds. Runs on the first display event of a
    /// burst, before macOS has moved or destroyed any Space.
    pub(crate) fn freeze(world: &mut World) {
        if world.contains_resource::<Initializing>() {
            // Startup session restore still owns the strips.
            return;
        }
        let homes = &world.resource::<Self>().homes;
        let remembered = homes.keys().copied().collect::<HashSet<_>>();
        let strips = homes.values().flat_map(|home| &home.strips);
        let held = strips
            .clone()
            .flat_map(|strip| &strip.columns)
            .flat_map(|remembered| remembered.column.windows())
            .collect::<EntityHashSet>();
        let borrowed = strips.map(|strip| strip.space).collect::<HashSet<_>>();
        let unremembered = world
            .query::<(Entity, &Display)>()
            .iter(world)
            .filter(|(_, display)| !remembered.contains(&display.id()))
            .map(|(entity, display)| (entity, display.id()))
            .collect::<Vec<_>>();
        for (entity, id) in unremembered {
            let home = remember(world, entity, id, &held, &borrowed);
            if !home.strips.is_empty() {
                debug!(display = id, "Remembering the display's layout");
                world.resource_mut::<Self>().homes.insert(id, home);
            }
        }
    }

    /// Puts windows back on their displays after a reconcile pass applied.
    /// Running it again changes nothing the first run did not, because the
    /// plan reads where windows are now and leaves columns in place alone.
    pub(crate) fn settle(world: &mut World, pass: Pass) {
        let homes = std::mem::take(&mut world.resource_mut::<Self>().homes);
        if homes.is_empty() {
            return;
        }
        let now = Topology::read(world);
        let mut kept = BTreeMap::new();
        let mut emptied = HashSet::new();
        for (id, mut home) in homes {
            match plan(id, &home, &now) {
                Plan::Away => {
                    home.returning |= pass == Pass::Final;
                    kept.insert(id, home);
                }
                Plan::Wait if pass == Pass::First => {
                    kept.insert(id, home);
                }
                Plan::Wait => {
                    warn!(
                        display = id,
                        "The display shows no strip, so its windows stay where macOS put them"
                    );
                }
                Plan::Apply(arrivals) => {
                    emptied.extend(apply(world, arrivals));
                    if pass == Pass::First {
                        kept.insert(id, home);
                    }
                }
            }
        }
        for space in emptied {
            compact_workspace_strips(world, space);
        }
        world.resource_mut::<Self>().homes = kept;
        world.flush();
    }
}

/// What `display`'s strips hold now, minus `held` windows, unmanaged windows,
/// and native fullscreen strips. Strips of a `borrowed` Space, one that
/// another display's home holds, go after the display's own strips.
fn remember(
    world: &mut World,
    display: Entity,
    id: CGDirectDisplayID,
    held: &EntityHashSet,
    borrowed: &HashSet<WorkspaceId>,
) -> Home {
    let Some(viewport) = viewport(world, display) else {
        return Home::default();
    };
    let shown_space = world
        .resource::<WindowManager>()
        .active_display_space(id)
        .ok();
    let managed = |world: &World, window: Entity| {
        !held.contains(&window)
            && world.get::<Window>(window).is_some()
            && world.get::<Unmanaged>(window).is_none()
    };
    let mut strips = world
        .query::<(
            &LayoutStrip,
            &ChildOf,
            Has<SelectedVirtualMarker>,
            Has<NativeFullscreenMarker>,
        )>()
        .iter(world)
        .filter(|(_, child, _, fullscreen)| !fullscreen && child.parent() == display)
        .map(|(strip, _, selected, _)| {
            let columns = (0..strip.len())
                .filter_map(|index| strip.copy_column(index))
                .filter_map(|column| column.retain(|window| managed(world, window)))
                .filter_map(|column| {
                    let width = world.get::<Bounds>(column.top()?)?.0.x;
                    Some(RememberedColumn {
                        column,
                        width: ViewportShare::of(width, viewport.width()),
                    })
                })
                .collect::<Vec<_>>();
            let shown = selected && Some(strip.id()) == shown_space;
            let order = (
                borrowed.contains(&strip.id()),
                !shown,
                strip.virtual_index,
                strip.id(),
            );
            (
                order,
                RememberedStrip {
                    space: strip.id(),
                    virtual_index: strip.virtual_index,
                    columns,
                },
            )
        })
        .filter(|(_, strip)| !strip.columns.is_empty())
        .collect::<Vec<_>>();
    strips.sort_by_key(|(order, _)| *order);
    Home {
        strips: strips.into_iter().map(|(_, strip)| strip).collect(),
        returning: false,
    }
}

fn viewport(world: &World, display: Entity) -> Option<IRect> {
    let dock = world.get::<DockPosition>(display);
    let config = world.resource::<Config>();
    Some(
        world
            .get::<Display>(display)?
            .actual_display_bounds(dock, config),
    )
}

/// Where things are now, read once per pass.
#[derive(Default)]
pub(crate) struct Topology {
    displays: HashMap<CGDirectDisplayID, DisplayNow>,
    /// Strips other than native fullscreen ones.
    strips: HashMap<Entity, StripNow>,
    /// Managed windows outside native fullscreen strips. A window missing
    /// here closed or left the tiling layout, and the restore forgets it.
    windows: HashMap<Entity, WindowNow>,
}

struct DisplayNow {
    viewport: IRect,
    /// The selected strip of the Space the display shows. A frame write
    /// moves a window into this Space and no other Space of the display.
    shown: Option<Entity>,
}

struct StripNow {
    /// `None` for an orphan strip.
    display: Option<CGDirectDisplayID>,
    space: WorkspaceId,
    virtual_index: u32,
}

struct WindowNow {
    /// `None` for a window whose Space macOS destroyed, before a native
    /// read placed it.
    strip: Option<Entity>,
    width: i32,
}

impl Topology {
    fn read(world: &mut World) -> Self {
        let display_ids = world
            .query::<(Entity, &Display)>()
            .iter(world)
            .map(|(entity, display)| (entity, display.id()))
            .collect::<HashMap<_, _>>();
        let mut strips = HashMap::new();
        let mut strip_of = HashMap::new();
        let mut fullscreen = EntityHashSet::default();
        let mut selected = Vec::new();
        for (entity, strip, child, is_selected, is_fullscreen) in world
            .query::<(
                Entity,
                &LayoutStrip,
                Option<&ChildOf>,
                Has<SelectedVirtualMarker>,
                Has<NativeFullscreenMarker>,
            )>()
            .iter(world)
        {
            if is_fullscreen {
                fullscreen.extend(strip.all_windows());
                continue;
            }
            let display = child.and_then(|child| display_ids.get(&child.parent()).copied());
            strip_of.extend(
                strip
                    .all_windows()
                    .into_iter()
                    .map(|window| (window, entity)),
            );
            if is_selected {
                selected.push((entity, display, strip.id()));
            }
            strips.insert(
                entity,
                StripNow {
                    display,
                    space: strip.id(),
                    virtual_index: strip.virtual_index,
                },
            );
        }
        let displays = display_ids
            .iter()
            .filter_map(|(&entity, &id)| {
                let viewport = viewport(world, entity)?;
                let space = world
                    .resource::<WindowManager>()
                    .active_display_space(id)
                    .ok();
                let shown = selected
                    .iter()
                    .find(|(_, display, strip_space)| {
                        *display == Some(id) && Some(*strip_space) == space
                    })
                    .map(|(strip, _, _)| *strip);
                Some((id, DisplayNow { viewport, shown }))
            })
            .collect();
        let windows = world
            .query_filtered::<(Entity, &Bounds), (With<Window>, Without<Unmanaged>)>()
            .iter(world)
            .filter(|(window, _)| !fullscreen.contains(window))
            .map(|(window, bounds)| {
                let now = WindowNow {
                    strip: strip_of.get(&window).copied(),
                    width: bounds.0.x,
                };
                (window, now)
            })
            .collect();
        Self {
            displays,
            strips,
            windows,
        }
    }

    /// Whether `window` is still managed, and in `space`, in no strip, or
    /// anywhere once macOS destroyed `space` and moved its windows elsewhere.
    fn still_in(&self, window: Entity, space: WorkspaceId) -> bool {
        let destroyed = !self.strips.values().any(|strip| strip.space == space);
        self.windows
            .get(&window)
            .is_some_and(|now| match now.strip {
                None => true,
                Some(strip) => {
                    destroyed
                        || self
                            .strips
                            .get(&strip)
                            .is_some_and(|strip| strip.space == space)
                }
            })
    }

    fn display_of(&self, window: Entity) -> Option<CGDirectDisplayID> {
        self.strips.get(&self.windows.get(&window)?.strip?)?.display
    }

    fn row(
        &self,
        display: CGDirectDisplayID,
        space: WorkspaceId,
        virtual_index: u32,
    ) -> Option<Entity> {
        self.strips
            .iter()
            .find(|(_, strip)| {
                strip.display == Some(display)
                    && strip.space == space
                    && strip.virtual_index == virtual_index
            })
            .map(|(entity, _)| *entity)
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Plan {
    /// The display is absent. The home waits.
    Away,
    /// A column must cross to the display, but it shows no strip yet.
    Wait,
    /// Columns to put in place, in this order.
    Apply(Vec<Arrival>),
}

#[derive(Debug, PartialEq)]
pub(crate) struct Arrival {
    target: Entity,
    /// The column's gap in `target`. Remembered columns lead the strip, and
    /// columns the home does not hold follow in their own order.
    gap: usize,
    column: DetachedColumn,
    /// The size each window takes, when the restore resizes the column.
    size: Option<Size>,
    /// A window is on another display or in no strip, so only a frame write
    /// can carry it home.
    crosses: bool,
}

/// Where the remembered columns of `home` go now.
///
/// The restore undoes what macOS does to Spaces in a burst: it moves them to
/// another display, or destroys them. A window that moved into another Space
/// while its own Space still exists was moved on purpose, by macOS or the
/// user, and stays there.
///
/// A frame write moves a window only into the Space its display shows. So a
/// column still on its display stays in its Space, and a column whose Space
/// moved away, or that is in no strip, crosses into the strip the display
/// shows.
pub(crate) fn plan(home: CGDirectDisplayID, remembered: &Home, now: &Topology) -> Plan {
    let Some(display) = now.displays.get(&home) else {
        return Plan::Away;
    };
    let mut arrivals: Vec<Arrival> = Vec::new();
    for strip in &remembered.strips {
        for RememberedColumn { column, width } in &strip.columns {
            let Some(column) = column
                .clone()
                .retain(|window| now.still_in(window, strip.space))
            else {
                continue;
            };
            let windows = column.windows();
            let crosses = windows
                .iter()
                .any(|window| now.display_of(*window) != Some(home));
            let target = if crosses {
                display.shown
            } else {
                now.row(home, strip.space, strip.virtual_index)
                    .or_else(|| now.windows.get(&column.top()?)?.strip)
            };
            let Some(target) = target else {
                return Plan::Wait;
            };
            let width = width.of_viewport(display.viewport.width());
            let resize = (crosses || remembered.returning)
                && windows
                    .iter()
                    .any(|window| (now.windows[window].width - width).abs() > WIDTH_TOLERANCE);
            arrivals.push(Arrival {
                target,
                gap: arrivals
                    .iter()
                    .filter(|arrival| arrival.target == target)
                    .count(),
                column,
                size: resize.then(|| Size::new(width, display.viewport.height())),
                crosses,
            });
        }
    }
    Plan::Apply(arrivals)
}

/// Splices `arrivals` into their strips, in order. Returns the Spaces that
/// lost a window.
fn apply(world: &mut World, arrivals: Vec<Arrival>) -> HashSet<WorkspaceId> {
    let mut emptied = HashSet::new();
    for Arrival {
        target,
        gap,
        column,
        size,
        crosses,
    } in arrivals
    {
        let members = column.windows();
        // A resized column arrives anew, so a stack splits the new height
        // evenly instead of keeping heights from another display.
        let in_place = size.is_none()
            && world
                .get::<LayoutStrip>(target)
                .is_some_and(|strip| strip.column_is(gap, &column));
        if !in_place {
            let mut left_behind = Vec::new();
            for mut strip in world.query::<&mut LayoutStrip>().iter_mut(world) {
                if !members.iter().any(|member| strip.contains(*member)) {
                    continue;
                }
                emptied.insert(strip.id());
                left_behind.extend(members.iter().find_map(|member| {
                    strip
                        .left_neighbour(*member)
                        .or_else(|| strip.right_neighbour(*member))
                        .filter(|neighbour| !members.contains(neighbour))
                }));
                for member in &members {
                    strip.remove(*member);
                }
            }
            if let Some(mut strip) = world.get_mut::<LayoutStrip>(target) {
                strip.insert_column(gap, column);
            }
            for neighbour in left_behind {
                world.commands().refit_after_removal(neighbour);
            }
        }
        if let Some(size) = size {
            for member in &members {
                size_for_arrival(*member, size, &mut world.commands());
            }
        }
        if crosses || size.is_some() {
            // Holds the windows against the topology read this pass queues,
            // which still lists them in the Space they are leaving.
            world
                .resource_mut::<NativeMembership>()
                .placed(members, None);
        }
    }
    emptied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Side;

    const HOME: CGDirectDisplayID = 1;
    const OTHER: CGDirectDisplayID = 3;
    const SPACE: WorkspaceId = 10;
    const OTHER_SPACE: WorkspaceId = 20;
    const NEW_SPACE: WorkspaceId = 30;
    const HALF: i32 = 500;

    fn viewport() -> IRect {
        IRect::new(0, 0, 1000, 800)
    }

    fn entities(count: usize) -> Vec<Entity> {
        World::new()
            .spawn_batch(std::iter::repeat_n((), count))
            .collect()
    }

    /// `windows` as one column, stacked top to bottom when there are more.
    fn column(windows: &[Entity]) -> DetachedColumn {
        let mut strip = LayoutStrip::default();
        for (index, window) in windows.iter().enumerate() {
            strip.append(*window);
            if index > 0 {
                strip.stack(*window, Side::West).unwrap();
            }
        }
        strip.copy_column(0).unwrap()
    }

    /// A home of `columns` in `SPACE`, each half the viewport wide.
    fn home(columns: &[&[Entity]]) -> Home {
        Home {
            strips: vec![remembered(SPACE, columns)],
            returning: false,
        }
    }

    fn remembered(space: WorkspaceId, columns: &[&[Entity]]) -> RememberedStrip {
        RememberedStrip {
            space,
            virtual_index: 0,
            columns: columns
                .iter()
                .map(|windows| RememberedColumn {
                    column: column(windows),
                    width: ViewportShare::of(HALF, viewport().width()),
                })
                .collect(),
        }
    }

    impl Topology {
        fn display(mut self, id: CGDirectDisplayID, shown: Option<Entity>) -> Self {
            let viewport = viewport();
            self.displays.insert(id, DisplayNow { viewport, shown });
            self
        }

        fn strip(mut self, strip: Entity, display: CGDirectDisplayID, space: WorkspaceId) -> Self {
            let display = Some(display);
            let virtual_index = 0;
            self.strips.insert(
                strip,
                StripNow {
                    display,
                    space,
                    virtual_index,
                },
            );
            self
        }

        fn windows(mut self, strip: Option<Entity>, windows: &[Entity], width: i32) -> Self {
            for window in windows {
                self.windows.insert(*window, WindowNow { strip, width });
            }
            self
        }
    }

    fn arrival(target: Entity, gap: usize, windows: &[Entity], crosses: bool) -> Arrival {
        Arrival {
            target,
            gap,
            column: column(windows),
            size: None,
            crosses,
        }
    }

    fn resized(arrival: Arrival, width: i32) -> Arrival {
        Arrival {
            size: Some(Size::new(width, viewport().height())),
            ..arrival
        }
    }

    #[test]
    fn the_home_of_an_absent_display_waits() {
        let [a, strip] = entities(2)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(OTHER, Some(strip))
            .strip(strip, OTHER, SPACE)
            .windows(Some(strip), &[a], HALF);
        assert_eq!(plan(HOME, &home(&[&[a]]), &now), Plan::Away);
    }

    #[test]
    fn columns_still_in_their_space_go_back_in_their_order() {
        let [a, b, c, strip] = entities(4)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, Some(strip))
            .strip(strip, HOME, SPACE)
            .windows(Some(strip), &[a, b, c], HALF);
        assert_eq!(
            plan(HOME, &home(&[&[a], &[b, c]]), &now),
            Plan::Apply(vec![
                arrival(strip, 0, &[a], false),
                arrival(strip, 1, &[b, c], false),
            ])
        );
    }

    #[test]
    fn columns_whose_space_moved_away_cross_into_the_shown_strip() {
        let [a, b, c, d, moved, borrowed, shown] = entities(7)[..] else {
            panic!()
        };
        let remembered = Home {
            strips: vec![
                remembered(SPACE, &[&[a], &[b, c]]),
                remembered(OTHER_SPACE, &[&[d]]),
            ],
            returning: false,
        };
        let now = Topology::default()
            .display(HOME, Some(shown))
            .display(OTHER, Some(moved))
            .strip(moved, OTHER, SPACE)
            .strip(borrowed, OTHER, OTHER_SPACE)
            .strip(shown, HOME, NEW_SPACE)
            .windows(Some(moved), &[a, b, c], HALF)
            .windows(Some(borrowed), &[d], HALF);
        assert_eq!(
            plan(HOME, &remembered, &now),
            Plan::Apply(vec![
                arrival(shown, 0, &[a], true),
                arrival(shown, 1, &[b, c], true),
                arrival(shown, 2, &[d], true),
            ])
        );
    }

    #[test]
    fn windows_of_a_destroyed_space_cross_with_their_stack() {
        let [a, b, shown] = entities(3)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, Some(shown))
            .strip(shown, HOME, NEW_SPACE)
            .windows(None, &[a, b], HALF);
        assert_eq!(
            plan(HOME, &home(&[&[a, b]]), &now),
            Plan::Apply(vec![arrival(shown, 0, &[a, b], true)])
        );
    }

    #[test]
    fn crossing_waits_for_the_display_to_show_a_strip() {
        let [a, b, moved, kept] = entities(4)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, None)
            .display(OTHER, Some(moved))
            .strip(kept, HOME, OTHER_SPACE)
            .strip(moved, OTHER, SPACE)
            .windows(Some(moved), &[a, b], HALF);
        assert_eq!(plan(HOME, &home(&[&[a], &[b]]), &now), Plan::Wait);
    }

    #[test]
    fn a_window_moved_into_another_space_stays_there() {
        let [a, b, c, strip, elsewhere] = entities(5)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, Some(strip))
            .strip(strip, HOME, SPACE)
            .strip(elsewhere, HOME, OTHER_SPACE)
            .windows(Some(strip), &[b], HALF)
            .windows(Some(elsewhere), &[a, c], HALF);
        assert_eq!(
            plan(HOME, &home(&[&[a], &[b, c]]), &now),
            Plan::Apply(vec![arrival(strip, 0, &[b], false)])
        );
    }

    #[test]
    fn a_closed_window_is_forgotten() {
        let [a, b, c, strip] = entities(4)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, Some(strip))
            .strip(strip, HOME, SPACE)
            .windows(Some(strip), &[b], HALF);
        assert_eq!(
            plan(HOME, &home(&[&[a], &[b, c]]), &now),
            Plan::Apply(vec![arrival(strip, 0, &[b], false)])
        );
    }

    #[test]
    fn a_crossing_column_takes_its_remembered_width() {
        let [a, b, moved, shown] = entities(4)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, Some(shown))
            .strip(moved, OTHER, SPACE)
            .strip(shown, HOME, NEW_SPACE)
            .windows(Some(moved), &[a], 300)
            .windows(Some(moved), &[b], HALF + 2);
        assert_eq!(
            plan(HOME, &home(&[&[a], &[b]]), &now),
            Plan::Apply(vec![
                resized(arrival(shown, 0, &[a], true), HALF),
                arrival(shown, 1, &[b], true),
            ])
        );
    }

    #[test]
    fn only_a_returning_display_gets_its_widths_back() {
        let [a, strip] = entities(2)[..] else {
            panic!()
        };
        let now = Topology::default()
            .display(HOME, Some(strip))
            .strip(strip, HOME, SPACE)
            .windows(Some(strip), &[a], 300);
        let mut remembered = home(&[&[a]]);
        assert_eq!(
            plan(HOME, &remembered, &now),
            Plan::Apply(vec![arrival(strip, 0, &[a], false)]),
            "a display that never left keeps the widths its windows have"
        );

        remembered.returning = true;
        assert_eq!(
            plan(HOME, &remembered, &now),
            Plan::Apply(vec![resized(arrival(strip, 0, &[a], false), HALF)])
        );
    }
}
