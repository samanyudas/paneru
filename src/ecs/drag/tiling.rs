use std::time::Duration;

use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut, SystemParam};
use bevy::math::IRect;
use bevy::time::Time;

use crate::ecs::{Bounds, DockPosition, PresentedOrigin, ResizeMarker, SpawnCommandsExt};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Window, WindowManager, origin_from};

const EDGE_TOLERANCE: i32 = 6;
const TILE_TOLERANCE: i32 = 2;
const RESIZE_RELEASE_GRACE: Duration = Duration::from_millis(600);

#[derive(Default, Resource)]
pub(super) enum PointerResize {
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
    windows: Query<(Entity, &Window, &PresentedOrigin, &Bounds)>,
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
                        window_id.map_or_else(
                            || {
                                IRect::from_corners(presented.0, presented.0 + bounds.0)
                                    .inflate(EDGE_TOLERANCE)
                                    .contains(point)
                            },
                            |id| window.id() == id,
                        )
                    })
                    .filter(|(_, _, presented, bounds)| {
                        let frame = IRect::from_corners(presented.0, presented.0 + bounds.0);
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
            } => {
                if let PointerResize::Held(window) = *gesture {
                    *gesture = PointerResize::Released {
                        window,
                        until: time.elapsed() + RESIZE_RELEASE_GRACE,
                    };
                }
            }
            Event::MouseMoved { .. } => {
                if matches!(*gesture, PointerResize::Held(_)) {
                    *gesture = PointerResize::Idle;
                }
            }
            _ => {}
        }
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
    window_manager: Res<'w, WindowManager>,
}

impl TilingRestore<'_, '_> {
    pub(crate) fn restore(
        &self,
        entity: Entity,
        frame: IRect,
        size: bevy::math::IVec2,
        commands: &mut Commands,
    ) -> bool {
        let edge_resize = match *self.gesture {
            PointerResize::Held(window) => {
                window == entity && self.window_manager.left_button_down()
            }
            PointerResize::Released { window, until } => {
                window == entity && self.time.elapsed() <= until
            }
            PointerResize::Idle => false,
        };
        if edge_resize
            || !self
                .displays
                .iter()
                .any(|(display, dock)| is_macos_tile(frame, display.visible_bounds(dock)))
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
