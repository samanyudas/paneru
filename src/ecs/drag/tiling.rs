use std::time::Duration;

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::Remove;
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut, SystemParam};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;

use crate::ecs::layout::LayoutStrip;
use crate::ecs::systems::unpadded;
use crate::ecs::{Bounds, DockPosition, PresentedOrigin, ResizeMarker, SpawnCommandsExt};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Origin, Window, WindowManager, origin_from};

const EDGE_TOLERANCE: i32 = 6;
const TILE_TOLERANCE: i32 = 2;
const RESIZE_RELEASE_GRACE: Duration = Duration::from_millis(600);
/// How long after its last step a resize the engine did not start still
/// counts as one resize.
const NATIVE_RESIZE_QUIET: Duration = Duration::from_millis(600);
/// How long a resize the engine did not start must go without a step before
/// the engine adopts its frame. macOS reports the steps of a tile animation
/// every 10 to 25ms on the native rig, so a 200ms pause means it has ended.
const NATIVE_RESIZE_SETTLE: Duration = Duration::from_millis(200);
/// How long a restored slot stays authoritative after a macOS tile. macOS
/// keeps a tiled window's tile state and puts the tile frame back for about
/// 300ms after each restore. One second outlasts that with margin, and is
/// short enough that the hold ends before a deliberate later resize.
const TILE_RESTORE_HOLD: Duration = Duration::from_secs(1);

/// The windows a left press may be resizing by an edge: every tiled window
/// whose edge band holds the press point, with the native tabs grouped with
/// it. Two neighboring columns share a boundary, so a press there can resize
/// either one. A tab group resizes as one, so every tab reports the resize.
#[derive(Default, Resource)]
pub(crate) enum PointerResize {
    #[default]
    Idle,
    Held(Vec<Entity>),
    Released {
        windows: Vec<Entity>,
        until: Duration,
    },
}

impl PointerResize {
    fn resizes(&self, entity: Entity, now: Duration) -> bool {
        match self {
            Self::Held(windows) => windows.contains(&entity),
            Self::Released { windows, until } => now <= *until && windows.contains(&entity),
            Self::Idle => false,
        }
    }

    fn release(&mut self, now: Duration) {
        if let Self::Held(windows) = self {
            *self = Self::Released {
                windows: std::mem::take(windows),
                until: now + RESIZE_RELEASE_GRACE,
            };
        }
    }
}

/// A resize remains a user resize when its last AX notification arrives
/// after mouse-up. The next press starts a separate gesture.
pub(super) fn track_resize_gesture(
    mut events: MessageReader<Event>,
    mut gesture: ResMut<PointerResize>,
    windows: Query<
        (Entity, &Window, &PresentedOrigin, &Bounds),
        bevy::ecs::query::Without<crate::ecs::Unmanaged>,
    >,
    strips: Query<&LayoutStrip>,
    window_manager: Res<WindowManager>,
    time: Res<Time>,
) {
    let now = time.elapsed();
    for event in events.read() {
        match event {
            Event::MouseDown {
                point,
                button: MouseButton::Left,
                ..
            } => {
                let point = origin_from(*point);
                let mut edges: Vec<_> = windows
                    .iter()
                    .filter(|(_, window, presented, bounds)| {
                        let frame = unpadded(
                            IRect::from_corners(presented.0, presented.0 + bounds.0),
                            (window.horizontal_padding(), window.vertical_padding()),
                        );
                        frame.inflate(EDGE_TOLERANCE).contains(point)
                            && !frame.inflate(-EDGE_TOLERANCE).contains(point)
                    })
                    .flat_map(|(entity, _, _, _)| {
                        strips
                            .iter()
                            .find_map(|strip| strip.tab_group(entity))
                            .unwrap_or_else(|| vec![entity])
                    })
                    .collect();
                edges.sort_unstable();
                edges.dedup();
                *gesture = if edges.is_empty() {
                    PointerResize::Idle
                } else {
                    PointerResize::Held(edges)
                };
            }
            Event::MouseUp {
                button: MouseButton::Left,
                ..
            }
            | Event::MouseMoved { .. } => gesture.release(now),
            _ => {}
        }
    }
    match &mut *gesture {
        PointerResize::Held(tracked)
        | PointerResize::Released {
            windows: tracked, ..
        } => {
            tracked.retain(|window| windows.contains(*window));
        }
        PointerResize::Idle => {}
    }
    if matches!(*gesture, PointerResize::Held(_)) && !window_manager.left_button_down() {
        gesture.release(now);
    }
    let expired = match &*gesture {
        PointerResize::Held(tracked) => tracked.is_empty(),
        PointerResize::Released { windows, until } => windows.is_empty() || now > *until,
        PointerResize::Idle => false,
    };
    if expired {
        *gesture = PointerResize::Idle;
    }
}

/// Half, quarter, and fill rectangles in macOS's visible frame. A two-point
/// tolerance admits rounding between AX and display geometry.
pub(crate) fn is_macos_tile(frame: IRect, visible: IRect) -> bool {
    let middle = visible.center();
    let x = [
        (visible.min.x, visible.max.x),
        (visible.min.x, middle.x),
        (middle.x, visible.max.x),
    ];
    let y = [
        (visible.min.y, visible.max.y),
        (visible.min.y, middle.y),
        (middle.y, visible.max.y),
    ];
    x.into_iter().any(|(left, right)| {
        y.into_iter().any(|(top, bottom)| {
            let expected = IRect::new(left, top, right, bottom);
            (frame.min - expected.min).abs().max_element() <= TILE_TOLERANCE
                && (frame.max - expected.max).abs().max_element() <= TILE_TOLERANCE
        })
    })
}

/// The slot a window had when a resize the engine did not start began.
///
/// While such a resize runs, the engine leaves the window's `Bounds` and
/// strip offset alone, so it does not fight a native tile animation. If the
/// resize ends on a tile, the window returns to this slot, and the slot then
/// holds for [`TILE_RESTORE_HOLD`]: every tile frame macOS puts back returns
/// to it, and a size readback cannot replace it. Otherwise the engine adopts
/// the frame once the resize settles.
#[derive(Component, Clone, Copy)]
pub(crate) struct NativeResizeStart {
    origin: Origin,
    size: IVec2,
    strip_x: Option<i32>,
    last: Duration,
    hold_until: Option<Duration>,
    settled: bool,
}

impl NativeResizeStart {
    fn fresh(&self, now: Duration) -> bool {
        now <= self.last + NATIVE_RESIZE_QUIET || self.holds(now)
    }

    /// Whether a restored slot still holds against macOS's tile frames.
    pub(crate) fn holds(&self, now: Duration) -> bool {
        self.hold_until.is_some_and(|until| now <= until)
    }
}

/// Adopts the live size of a window whose deferred resize record is cleared,
/// such as by an explicit resize, so clearing the record cannot drop the
/// adoption it was deferring. A held or settled record adopts nothing here.
pub(super) fn adopt_cleared_native_resize(
    removed: On<Remove, NativeResizeStart>,
    mut windows: Query<(&Window, &NativeResizeStart, &mut Bounds)>,
    time: Res<Time>,
) {
    let Ok((window, start, mut bounds)) = windows.get_mut(removed.entity) else {
        return;
    };
    if start.settled || start.holds(time.elapsed()) {
        return;
    }
    let size = window.frame().size();
    if bounds.0 != size {
        bounds.0 = size;
    }
}

/// Re-reports a resize the engine did not start once it has settled with no
/// tile, so the engine adopts its frame. A held slot settles after its hold.
pub(super) fn settle_native_resizes(
    mut starts: Query<(&Window, &mut NativeResizeStart)>,
    time: Res<Time>,
    mut messages: MessageWriter<Event>,
) {
    let now = time.elapsed();
    for (window, mut start) in &mut starts {
        if start.settled || start.holds(now) || now < start.last + NATIVE_RESIZE_SETTLE {
            continue;
        }
        start.settled = true;
        messages.write(Event::WindowResized {
            window_id: window.id(),
        });
    }
}

#[derive(SystemParam)]
pub(crate) struct TilingRestore<'w, 's> {
    displays: Query<'w, 's, (&'static Display, Option<&'static DockPosition>)>,
    presented: Query<
        'w,
        's,
        (
            &'static PresentedOrigin,
            Option<&'static ResizeMarker>,
            Option<&'static NativeResizeStart>,
        ),
    >,
    gesture: Res<'w, PointerResize>,
    time: Res<'w, Time>,
}

/// A restore of a macOS tile: the strip offset to restore with the window,
/// if the resize began from a recorded slot.
pub(crate) struct Restored {
    pub strip_x: Option<i32>,
}

impl TilingRestore<'_, '_> {
    /// Returns the window to its slot when `frame` is a macOS tile it did not
    /// get from the user or the engine. The slot is the one recorded when the
    /// resize began, else the window's layout slot.
    pub(crate) fn restore(
        &self,
        entity: Entity,
        frame: IRect,
        size: IVec2,
        padding: (i32, i32),
        commands: &mut Commands,
    ) -> Option<Restored> {
        let now = self.time.elapsed();
        if self.gesture.resizes(entity, now)
            || !self.displays.iter().any(|(display, dock)| {
                is_macos_tile(unpadded(frame, padding), display.visible_bounds(dock))
            })
        {
            return None;
        }
        let (origin, resizing, start) = self.presented.get(entity).ok()?;
        // A settled resize is reported again to be adopted. A tile frame that
        // outlasts the hold is the app keeping that size, not macOS.
        if start.is_some_and(|start| start.settled) {
            return None;
        }
        let start = start
            .filter(|start| start.fresh(now))
            .copied()
            .unwrap_or(NativeResizeStart {
                origin: origin.0,
                size: resizing.map_or(size, |resize| resize.0),
                strip_x: None,
                last: now,
                hold_until: None,
                settled: false,
            });
        if frame.min == start.origin && frame.size() == start.size {
            return None;
        }
        commands.reposition_entity(entity, start.origin);
        commands.resize_entity(entity, start.size);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(NativeResizeStart {
                last: now,
                hold_until: Some(now + TILE_RESTORE_HOLD),
                settled: false,
                ..start
            });
        }
        Some(Restored {
            strip_x: start.strip_x,
        })
    }

    /// Whether the engine defers a resize of `entity` it did not start,
    /// recording the slot it began from. A user edge resize, and a resize
    /// that has settled with no tile, are adopted at once instead.
    pub(crate) fn defers(
        &self,
        entity: Entity,
        size: IVec2,
        strip_x: Option<i32>,
        commands: &mut Commands,
    ) -> bool {
        let now = self.time.elapsed();
        let Ok((origin, _, start)) = self.presented.get(entity) else {
            return false;
        };
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            return false;
        };
        if self.gesture.resizes(entity, now) || start.is_some_and(|start| start.settled) {
            entity_commands.try_remove::<NativeResizeStart>();
            return false;
        }
        if start.is_some_and(|start| start.holds(now)) {
            return true;
        }
        let start = start.filter(|start| start.fresh(now)).map_or(
            NativeResizeStart {
                origin: origin.0,
                size,
                strip_x,
                last: now,
                hold_until: None,
                settled: false,
            },
            |start| NativeResizeStart {
                last: now,
                ..*start
            },
        );
        entity_commands.try_insert(start);
        true
    }
}
