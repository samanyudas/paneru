use std::time::Duration;

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::Remove;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut, SystemParam};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;

use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::systems::unpadded;
use crate::ecs::{
    Bounds, DockPosition, PresentedOrigin, RepositionMarker, ResizeMarker, SpawnCommandsExt,
    Unmanaged, VerifyWindowSize,
};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Origin, Window, WindowManager, origin_from};

const EDGE_TOLERANCE: i32 = 6;
const TILE_TOLERANCE: i32 = 2;
const RESIZE_RELEASE_GRACE: Duration = Duration::from_millis(600);
const NATIVE_RESIZE_SETTLE: Duration = Duration::from_millis(200);
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

pub(crate) fn tiled_in(strip: &LayoutStrip, entity: Entity) -> bool {
    strip
        .index_of(entity)
        .ok()
        .and_then(|index| strip.get(index).ok())
        .is_some_and(|column| !matches!(column, Column::Fullscren(_)))
}

#[derive(Component, Clone, Copy)]
pub(crate) struct NativeResizeStart {
    origin: Origin,
    size: IVec2,
    strip_x: Option<i32>,
    phase: Phase,
}

#[derive(Clone, Copy)]
enum Phase {
    Observing {
        frame: IRect,
        changed: Duration,
    },
    Holding {
        until: Duration,
    },
    Settling {
        settlement: Settlement,
        retry: Duration,
    },
}

#[derive(Clone, Copy)]
enum Settlement {
    Adopt(IRect),
    Refused,
}

impl NativeResizeStart {
    fn slot(&self) -> IRect {
        IRect::from_corners(self.origin, self.origin + self.size)
    }

    fn settlement(&self, now: Duration) -> Option<Settlement> {
        match self.phase {
            Phase::Observing { frame, changed } => {
                (now >= changed + NATIVE_RESIZE_SETTLE).then_some(Settlement::Adopt(frame))
            }
            Phase::Holding { until } => (now >= until).then_some(Settlement::Refused),
            Phase::Settling { settlement, .. } => Some(settlement),
        }
    }

    fn due(&self, now: Duration) -> bool {
        match self.phase {
            Phase::Settling { retry, .. } => now >= retry,
            _ => self.settlement(now).is_some(),
        }
    }

    pub(crate) fn held_size(&self, now: Duration) -> Option<IVec2> {
        matches!(self.phase, Phase::Holding { until } if now < until).then_some(self.size)
    }

    pub(crate) fn needs_frame(&self) -> bool {
        !matches!(self.phase, Phase::Settling { .. })
    }

    fn adopts_when_cancelled(&self) -> bool {
        matches!(
            self.phase,
            Phase::Observing { .. }
                | Phase::Settling {
                    settlement: Settlement::Adopt(_),
                    ..
                }
        )
    }
}

pub(super) fn adopt_cancelled_native_resize(
    removed: On<Remove, NativeResizeStart>,
    mut windows: Query<(&Window, &NativeResizeStart, &mut Bounds), Without<Unmanaged>>,
    strips: Query<&LayoutStrip>,
) {
    let Ok((window, start, mut bounds)) = windows.get_mut(removed.entity) else {
        return;
    };
    if !start.adopts_when_cancelled() || !strips.iter().any(|strip| tiled_in(strip, removed.entity))
    {
        return;
    }
    let size = window.frame().size();
    if bounds.0 != size {
        bounds.0 = size;
    }
}

type Presented<'w, 's> = Query<
    'w,
    's,
    (
        &'static PresentedOrigin,
        Option<&'static ResizeMarker>,
        Option<&'static RepositionMarker>,
        Has<VerifyWindowSize>,
        Option<&'static NativeResizeStart>,
    ),
>;

#[derive(SystemParam)]
pub(crate) struct TilingRestore<'w, 's> {
    displays: Query<'w, 's, (&'static Display, Option<&'static DockPosition>)>,
    presented: Presented<'w, 's>,
    gesture: Res<'w, PointerResize>,
    time: Res<'w, Time>,
}

pub(crate) enum NativeFrame {
    Ordinary,
    Moved,
    Deferred,
    Restored(Option<i32>),
    Settled,
    Refused,
}

impl TilingRestore<'_, '_> {
    pub(crate) fn due(&self, entity: Entity) -> bool {
        let now = self.time.elapsed();
        self.presented
            .get(entity)
            .is_ok_and(|(_, resize, reposition, verifying, start)| {
                start.is_some_and(|start| {
                    start.due(now) && !Self::blocks(start, now, resize, reposition, verifying)
                })
            })
    }

    fn blocks(
        start: &NativeResizeStart,
        now: Duration,
        resize: Option<&ResizeMarker>,
        reposition: Option<&RepositionMarker>,
        verifying: bool,
    ) -> bool {
        match start.settlement(now) {
            Some(Settlement::Refused) => {
                resize.is_some_and(|resize| resize.0 != start.size)
                    || reposition.is_some_and(|reposition| reposition.0 != start.origin)
            }
            _ => resize.is_some() || verifying,
        }
    }

    pub(crate) fn observe(
        &self,
        entity: Entity,
        frame: IRect,
        managed: IRect,
        strip_x: Option<i32>,
        padding: (i32, i32),
        commands: &mut Commands,
    ) -> NativeFrame {
        let now = self.time.elapsed();
        let Ok((origin, resize, reposition, verifying, start)) = self.presented.get(entity) else {
            return NativeFrame::Ordinary;
        };
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            return NativeFrame::Ordinary;
        };
        if self.gesture.resizes(entity, now) {
            if start.is_some() {
                entity_commands.try_remove::<NativeResizeStart>();
            }
            return NativeFrame::Ordinary;
        }
        let tile = self.displays.iter().any(|(display, dock)| {
            is_macos_tile(unpadded(frame, padding), display.visible_bounds(dock))
        });
        let engine = resize.is_some() || verifying;
        let Some(&start) = start else {
            if frame == managed {
                return NativeFrame::Moved;
            }
            let slot = NativeResizeStart {
                origin: origin.0,
                size: resize.map_or(managed.size(), |resize| resize.0),
                strip_x: None,
                phase: Phase::Holding {
                    until: now + TILE_RESTORE_HOLD,
                },
            };
            if tile && frame != slot.slot() {
                return Self::restore(entity, slot, commands);
            }
            if engine {
                return NativeFrame::Ordinary;
            }
            if frame.size() == managed.size() {
                return NativeFrame::Moved;
            }
            entity_commands.try_insert(NativeResizeStart {
                origin: origin.0,
                size: managed.size(),
                strip_x,
                phase: Phase::Observing {
                    frame,
                    changed: now,
                },
            });
            return NativeFrame::Deferred;
        };
        let settlement = start.settlement(now);
        match settlement {
            Some(Settlement::Refused) => {
                if Self::blocks(&start, now, resize, reposition, verifying) {
                    return NativeFrame::Deferred;
                }
                entity_commands.try_remove::<(
                    ResizeMarker,
                    RepositionMarker,
                    VerifyWindowSize,
                    NativeResizeStart,
                )>();
                return NativeFrame::Refused;
            }
            _ if tile && frame != start.slot() => {
                let phase = match start.phase {
                    Phase::Holding { until } => Phase::Holding { until },
                    _ => Phase::Holding {
                        until: now + TILE_RESTORE_HOLD,
                    },
                };
                return Self::restore(entity, NativeResizeStart { phase, ..start }, commands);
            }
            _ if engine || matches!(start.phase, Phase::Holding { .. }) => {
                return NativeFrame::Deferred;
            }
            Some(Settlement::Adopt(seen)) if seen == frame => {
                entity_commands.try_remove::<NativeResizeStart>();
                return NativeFrame::Settled;
            }
            _ => {}
        }
        if !matches!(start.phase, Phase::Observing { frame: seen, .. } if seen == frame) {
            entity_commands.try_insert(NativeResizeStart {
                phase: Phase::Observing {
                    frame,
                    changed: now,
                },
                ..start
            });
        }
        NativeFrame::Deferred
    }

    pub(crate) fn retry(&self, entity: Entity, commands: &mut Commands) {
        let now = self.time.elapsed();
        if let Ok((.., Some(start))) = self.presented.get(entity)
            && let Some(settlement) = start.settlement(now)
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_insert(NativeResizeStart {
                phase: Phase::Settling {
                    settlement,
                    retry: now + NATIVE_RESIZE_SETTLE,
                },
                ..*start
            });
        }
    }

    fn restore(entity: Entity, start: NativeResizeStart, commands: &mut Commands) -> NativeFrame {
        commands.reposition_entity(entity, start.origin);
        commands.resize_entity(entity, start.size);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(start);
        }
        NativeFrame::Restored(start.strip_x)
    }
}
