use std::time::Duration;

use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut, SystemParam};
use bevy::math::IRect;
use bevy::time::Time;

use crate::ecs::layout::LayoutStrip;
use crate::ecs::systems::unpadded;
use crate::ecs::{Bounds, DockPosition, PresentedOrigin, ResizeMarker, SpawnCommandsExt};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Window, WindowManager, origin_from};

const EDGE_TOLERANCE: i32 = 6;
const TILE_TOLERANCE: i32 = 2;
const RESIZE_RELEASE_GRACE: Duration = Duration::from_millis(600);

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

#[derive(SystemParam)]
pub(crate) struct TilingRestore<'w, 's> {
    displays: Query<'w, 's, (&'static Display, Option<&'static DockPosition>)>,
    presented: Query<'w, 's, (&'static PresentedOrigin, Option<&'static ResizeMarker>)>,
    gesture: Res<'w, PointerResize>,
    time: Res<'w, Time>,
}

impl TilingRestore<'_, '_> {
    pub(crate) fn restore(
        &self,
        entity: Entity,
        frame: IRect,
        size: bevy::math::IVec2,
        padding: (i32, i32),
        commands: &mut Commands,
    ) -> bool {
        if self.gesture.resizes(entity, self.time.elapsed())
            || !self.displays.iter().any(|(display, dock)| {
                is_macos_tile(unpadded(frame, padding), display.visible_bounds(dock))
            })
        {
            return false;
        }
        let Ok((origin, resizing)) = self.presented.get(entity) else {
            return false;
        };
        let size = resizing.map_or(size, |resize| resize.0);
        if frame.min == origin.0 && frame.size() == size {
            return false;
        }
        commands.reposition_entity(entity, origin.0);
        commands.resize_entity(entity, size);
        true
    }
}
