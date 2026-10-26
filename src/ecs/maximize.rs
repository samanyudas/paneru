//! Maximizing a window with a double-click on its title bar, and restoring it
//! with the next one.
//!
//! A maximized window fills the usable area of its display and may cover the
//! windows around it. It keeps its place: its [`Position`] and [`Bounds`]
//! go on tracking the slot or floating frame it returns to, and the engine
//! does not write them to the window until it is restored. So the windows
//! around it keep their sizes and places, and a stacked window returns to its
//! stack.
//!
//! A maximized window moved to another display, by a display command or a
//! title-bar drag, stays maximized and fills that display's usable area.

use std::time::Duration;

use bevy::app::{App, Plugin, Update};
use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{any_with_component, on_message};
use bevy::ecs::system::{Commands, Local, Query, Res};
use bevy::ecs::world::World;
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use tracing::debug;

use crate::ecs::drag::tiling::supersede_native_resize;
use crate::ecs::layout::{FALLBACK_TOP_BAR_HEIGHT, LayoutStrip};
use crate::ecs::systems::unpadded;
use crate::ecs::{Bounds, DockPosition, Position, TopBarHeight, Unmanaged, UpdateSet};
use crate::events::{Event, InputEvent, MouseButton};
use crate::manager::{Display, Origin, Window, WindowManager, origin_from};
use objc2_core_graphics::CGDirectDisplayID;

/// How long the engine holds a window at the frame a toggle gave it. The
/// app answers the same double-click with its own zoom, which can land
/// after the engine's write and would otherwise count as a user resize.
const FRAME_HOLD: Duration = Duration::from_secs(1);

/// How far above its target's bottom edge a shrinking window first ends.
/// See [`approach`].
pub(crate) const EDGE_RELEASE: i32 = 100;

/// A window maximized by a title-bar double-click.
#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct Maximized {
    /// The frame that fills the display's usable area, padding included.
    frame: IRect,
    /// How far a floating window sat from the usable area's top-left corner,
    /// so it returns to the same place relative to its display.
    offset: IVec2,
}

/// Holds a window at the frame its last toggle gave it until `until`.
#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct FrameHold {
    until: Duration,
}

pub(crate) struct MaximizePlugin;

impl Plugin for MaximizePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (toggle_maximize, fill_drop_display)
                .in_set(UpdateSet::Input)
                .run_if(on_message::<InputEvent>),
        )
        .add_systems(
            Update,
            hold_frames
                .in_set(UpdateSet::Events)
                .run_if(any_with_component::<FrameHold>),
        );
    }
}

type Toggles<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Window,
        &'static mut Position,
        &'static mut Bounds,
        Option<&'static Unmanaged>,
        Option<&'static TopBarHeight>,
        Option<&'static Maximized>,
    ),
>;

/// Maximizes a window when its title bar is double-clicked, and restores it
/// on the next double-click. A double-click on a control in the title bar,
/// such as a button or tab, is the control's own.
fn toggle_maximize(
    mut messages: MessageReader<InputEvent>,
    mut windows: Toggles,
    strips: Query<&LayoutStrip>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    window_manager: Res<WindowManager>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        let Event::MouseDown {
            point,
            button: MouseButton::Left,
            window_id,
            clicks: 2,
            ..
        } = event
        else {
            continue;
        };
        let Some(window_id) = window_id.or_else(|| window_manager.find_window_at_point(point).ok())
        else {
            continue;
        };
        let point = origin_from(*point);
        let Some((entity, mut window, mut position, mut bounds, unmanaged, top_bar, maximized)) =
            windows
                .iter_mut()
                .find(|(_, window, ..)| window.id() == window_id)
        else {
            continue;
        };
        // The engine stops reading a maximized window's moves, so a window
        // the user dragged is read here to find its top bar.
        if maximized.is_some() && !window.write_pending() {
            _ = window.update_frame();
        }
        if matches!(unmanaged, Some(Unmanaged::Minimized | Unmanaged::Hidden))
            || !in_top_bar(&window, top_bar, point)
        {
            continue;
        }
        match window.control_at(point) {
            Ok(false) => {}
            Ok(true) => continue,
            Err(err) => {
                debug!("no maximize: element under {point} unreadable: {err}");
                continue;
            }
        }
        let Some(visible) = displays
            .iter()
            .find(|(display, _)| display.bounds().contains(point))
            .map(|(display, dock)| display.visible_bounds(dock))
        else {
            continue;
        };
        let padding = IVec2::new(window.horizontal_padding(), window.vertical_padding());
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        entity_commands.try_insert(FrameHold {
            until: time.elapsed() + FRAME_HOLD,
        });

        if let Some(maximized) = maximized {
            debug!("restoring maximized window {window_id}");
            entity_commands.try_remove::<Maximized>();
            let floating =
                unmanaged.is_some() || !strips.iter().any(|strip| strip.contains(entity));
            if floating {
                // Set without a change, so the frame is not written at once
                // and the window can first end clear of the screen's edge.
                let frame = floating_frame(bounds.0, maximized.offset, visible, padding);
                position.bypass_change_detection().0 = frame.min;
                bounds.bypass_change_detection().0 = frame.size();
            }
            let from = window.frame();
            approach(
                &mut window,
                from,
                IRect::from_corners(position.0, position.0 + bounds.0),
            );
        } else {
            debug!("maximizing window {window_id}");
            let frame = filled(&window, visible);
            entity_commands.try_insert(Maximized {
                frame,
                offset: position.0 - visible.min,
            });
            // A native resize the window was settling would otherwise adopt
            // the maximized size into the slot.
            supersede_native_resize(&[entity], &mut commands);
            window.place(frame);
        }
    }
}

/// The frame that fills `visible`, a display's usable area, padding included.
fn filled(window: &Window, visible: IRect) -> IRect {
    let padding = IVec2::new(window.horizontal_padding(), window.vertical_padding());
    IRect::from_corners(visible.min - padding, visible.max + padding)
}

/// Fills `visible` with a maximized window and holds it there, unless it
/// already fills it. Returns whether the window had to move.
fn fill(window: &mut Window, maximized: &mut Maximized, visible: IRect) -> bool {
    let frame = filled(window, visible);
    if maximized.frame == frame {
        return false;
    }
    debug!("fitting maximized window {} to {frame:?}", window.id());
    maximized.frame = frame;
    window.place(frame);
    true
}

/// A maximized window dropped on another display after a title-bar drag
/// fills that display's usable area. A drop on its own display leaves it
/// where the user put it.
///
/// macOS keeps a dropped window on the display under the pointer, so the
/// pointer at the release picks the display. The window is the one pressed,
/// because macOS can drop it short of the pointer: on a native rig Finder
/// ended 18 points below it.
fn fill_drop_display(
    mut messages: MessageReader<InputEvent>,
    mut pressed: Local<Option<Entity>>,
    mut windows: Query<(Entity, &mut Window, &mut Maximized)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    window_manager: Res<WindowManager>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        match event {
            Event::MouseDown {
                point,
                button: MouseButton::Left,
                window_id,
                ..
            } => {
                let window_id =
                    window_id.or_else(|| window_manager.find_window_at_point(point).ok());
                *pressed = windows
                    .iter()
                    .find(|(_, window, _)| Some(window.id()) == window_id)
                    .map(|(entity, _, _)| entity);
            }
            Event::MouseUp {
                point,
                button: MouseButton::Left,
                ..
            } => {
                let Some(entity) = pressed.take() else {
                    continue;
                };
                let Ok((_, mut window, mut maximized)) = windows.get_mut(entity) else {
                    continue;
                };
                if window.write_pending() || window.update_frame().ok() == Some(maximized.frame) {
                    continue;
                }
                let point = origin_from(*point);
                let Some(visible) = displays
                    .iter()
                    .find(|(display, _)| display.bounds().contains(point))
                    .map(|(display, dock)| display.visible_bounds(dock))
                else {
                    continue;
                };
                if fill(&mut window, &mut maximized, visible)
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_insert(FrameHold {
                        until: time.elapsed() + FRAME_HOLD,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Fills the usable area of display `display` with `entity`, when it is
/// maximized, after a display command moved it there.
pub(crate) fn fill_display(commands: &mut Commands, entity: Entity, display: CGDirectDisplayID) {
    commands.queue(move |world: &mut World| {
        let Some(visible) = world
            .query::<(&Display, Option<&DockPosition>)>()
            .iter(world)
            .find(|(candidate, _)| candidate.id() == display)
            .map(|(display, dock)| display.visible_bounds(dock))
        else {
            return;
        };
        let until = world.resource::<Time>().elapsed() + FRAME_HOLD;
        let Ok((mut window, mut maximized)) = world
            .query::<(&mut Window, &mut Maximized)>()
            .get_mut(world, entity)
        else {
            return;
        };
        if fill(&mut window, &mut maximized, visible) {
            world.entity_mut(entity).insert(FrameHold { until });
        }
    });
}

/// Whether `point` is in the window's top bar: its title bar, toolbar, or
/// navigation bar.
fn in_top_bar(window: &Window, top_bar: Option<&TopBarHeight>, point: Origin) -> bool {
    let frame = unpadded(
        window.frame(),
        (window.horizontal_padding(), window.vertical_padding()),
    );
    let height = top_bar.map_or_else(
        || {
            window
                .top_bar_height()
                .ok()
                .flatten()
                .unwrap_or(FALLBACK_TOP_BAR_HEIGHT)
        },
        |top_bar| top_bar.0,
    );
    (frame.min.x..frame.max.x).contains(&point.x)
        && (frame.min.y..frame.min.y + height).contains(&point.y)
}

/// Where a floating window of `size` returns: `offset` from the usable
/// area's top-left corner, moved and shrunk as needed to stay inside it.
fn floating_frame(size: IVec2, offset: IVec2, visible: IRect, padding: IVec2) -> IRect {
    let area = IRect::from_corners(visible.min - padding, visible.max + padding);
    let size = size.min(area.size());
    let origin = (visible.min + offset).clamp(area.min, area.max - size);
    IRect::from_corners(origin, origin + size)
}

type Held<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Window,
        &'static Position,
        &'static Bounds,
        Option<&'static Maximized>,
        &'static FrameHold,
    ),
>;

/// Puts a window back at the frame its last toggle gave it whenever the
/// window shows another frame until the hold ends, such as after the app's
/// own zoom.
fn hold_frames(mut windows: Held, time: Res<Time>, mut commands: Commands) {
    let now = time.elapsed();
    for (entity, mut window, position, bounds, maximized, hold) in &mut windows {
        if now >= hold.until
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<FrameHold>();
        }
        if window.write_pending() {
            continue;
        }
        let target = maximized.map_or_else(
            || IRect::from_corners(position.0, position.0 + bounds.0),
            |maximized| maximized.frame,
        );
        if let Ok(frame) = window.update_frame()
            && frame != target
        {
            debug!("holding window {} at {target:?}", window.id());
            approach(&mut window, frame, target);
        }
    }
}

/// Writes `target` to a window that shows `from`. A window that must get
/// shorter first ends [`EDGE_RELEASE`] above the target's bottom edge, and
/// the hold then grows it to the target. macOS ignores a small shrink that
/// leaves a window's bottom edge near the bottom of the screen, which is
/// where a slot usually ends, but lets a window grow into that zone. On a
/// native rig the zone reached between 22 and 42 points above the edge.
fn approach(window: &mut Window, from: IRect, target: IRect) {
    let lift = if from.height() > target.height() {
        EDGE_RELEASE.min(target.height() / 2)
    } else {
        0
    };
    window.place(IRect::from_corners(
        target.min,
        target.max.with_y(target.max.y - lift),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    const VISIBLE: IRect = IRect {
        min: IVec2::new(0, 30),
        max: IVec2::new(1000, 800),
    };

    #[test]
    fn a_floating_window_returns_to_its_offset() {
        let frame = floating_frame(
            IVec2::new(300, 200),
            IVec2::new(100, 50),
            VISIBLE,
            IVec2::ZERO,
        );
        assert_eq!(frame, IRect::new(100, 80, 400, 280));
    }

    #[test]
    fn a_floating_window_past_the_display_edge_moves_back_inside() {
        let frame = floating_frame(
            IVec2::new(300, 200),
            IVec2::new(900, 700),
            VISIBLE,
            IVec2::ZERO,
        );
        assert_eq!(frame, IRect::new(700, 600, 1000, 800));
    }

    #[test]
    fn a_floating_window_larger_than_the_display_shrinks_to_fit() {
        let frame = floating_frame(
            IVec2::new(1200, 900),
            IVec2::new(-50, 0),
            VISIBLE,
            IVec2::new(4, 4),
        );
        assert_eq!(frame, IRect::new(-4, 26, 1004, 804));
    }
}
