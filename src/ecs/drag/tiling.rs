use std::time::Duration;

use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut, SystemParam};
use bevy::math::IRect;
use bevy::time::Time;

use crate::ecs::systems::unpadded;
use crate::ecs::{Bounds, DockPosition, PresentedOrigin, ResizeMarker, SpawnCommandsExt};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Window, WindowManager, origin_from};

const EDGE_TOLERANCE: i32 = 6;
const TILE_TOLERANCE: i32 = 2;
const RESIZE_RELEASE_GRACE: Duration = Duration::from_millis(600);

#[derive(Default, Resource)]
pub(crate) enum PointerResize {
    #[default]
    Idle,
    Held(Entity),
    Released {
        window: Entity,
        until: Duration,
    },
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
    window_manager: Res<WindowManager>,
    time: Res<Time>,
) {
    for event in events.read() {
        match event {
            Event::MouseDown {
                point,
                button: MouseButton::Left,
                window_id,
                ..
            } => {
                let point = origin_from(*point);
                *gesture = windows
                    .iter()
                    .find(|(_, window, presented, bounds)| {
                        let frame = unpadded(
                            IRect::from_corners(presented.0, presented.0 + bounds.0),
                            (window.horizontal_padding(), window.vertical_padding()),
                        );
                        window_id.map_or_else(
                            || frame.inflate(EDGE_TOLERANCE).contains(point),
                            |id| window.id() == id,
                        )
                    })
                    .filter(|(_, window, presented, bounds)| {
                        let frame = unpadded(
                            IRect::from_corners(presented.0, presented.0 + bounds.0),
                            (window.horizontal_padding(), window.vertical_padding()),
                        );
                        frame.inflate(EDGE_TOLERANCE).contains(point)
                            && !frame.inflate(-EDGE_TOLERANCE).contains(point)
                    })
                    .map_or(PointerResize::Idle, |(entity, _, _, _)| {
                        PointerResize::Held(entity)
                    });
            }
            Event::MouseUp {
                button: MouseButton::Left,
                ..
            }
            | Event::MouseMoved { .. } => {
                if let PointerResize::Held(window) = *gesture {
                    *gesture = PointerResize::Released {
                        window,
                        until: time.elapsed() + RESIZE_RELEASE_GRACE,
                    };
                }
            }
            _ => {}
        }
    }
    let tracked = match *gesture {
        PointerResize::Held(window) | PointerResize::Released { window, .. } => Some(window),
        PointerResize::Idle => None,
    };
    if tracked.is_some_and(|window| windows.get(window).is_err()) {
        *gesture = PointerResize::Idle;
    }
    match *gesture {
        PointerResize::Held(window) if !window_manager.left_button_down() => {
            *gesture = PointerResize::Released {
                window,
                until: time.elapsed() + RESIZE_RELEASE_GRACE,
            };
        }
        PointerResize::Released { until, .. } if time.elapsed() > until => {
            *gesture = PointerResize::Idle;
        }
        _ => {}
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
        let edge_resize = match *self.gesture {
            PointerResize::Held(window) => window == entity,
            PointerResize::Released { window, until } => {
                window == entity && self.time.elapsed() <= until
            }
            PointerResize::Idle => false,
        };
        if edge_resize
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
