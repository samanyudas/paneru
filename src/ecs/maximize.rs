//! Maximizing a window, and restoring it with the next toggle. A
//! double-click on a window's title bar toggles that window, and the
//! maximize command toggles the focused window.
//!
//! A maximized tiled window is a column of its strip as wide as the
//! viewport, and the window fills the usable area of its display: it is
//! drawn over the edge padding, without its window padding. It scrolls with
//! the strip like any column, and moves with it to another display or
//! workspace. A stacked window leaves its stack for a column of its own to
//! the right of the stack, and goes back to its place there on restore when
//! it can. See [`LayoutStrip::maximize`].
//!
//! A maximized floating window fills the usable area of its display over the
//! layout, and returns to its frame on restore.

use std::time::Duration;

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{any_with_component, on_message};
use bevy::ecs::system::{Commands, Local, Query, Res};
use bevy::math::{IRect, IVec2};
use bevy::prelude::Event as BevyEvent;
use bevy::time::Time;
use tracing::debug;

use crate::commands::{Operation, Side, filter_window_operations};
use crate::config::Config;
use crate::ecs::drag::tiling::supersede_native_resize;
use crate::ecs::layout::{
    Before, FALLBACK_TOP_BAR_HEIGHT, LayoutStrip, ViewportAnchor, ViewportShare,
    effective_stack_limit,
};
use crate::ecs::{
    Bounds, DockPosition, FocusedMarker, Position, PreUpdateSet, ResizeMarker,
    SpawnCommandsExt as _, TopBarHeight, Unmanaged, UpdateSet, WidthRun,
};
use crate::events::{Event, InputEvent, MouseButton};
use crate::manager::{Display, Origin, Size, Window, WindowManager, origin_from};

/// How long the engine holds a window at the frame a toggle gave it. The
/// app answers the same double-click with its own zoom, which can land
/// after the engine's write and would otherwise count as a user resize.
const FRAME_HOLD: Duration = Duration::from_secs(1);

/// How far above its target's bottom edge a shrinking window first ends.
/// See [`approach`].
pub(crate) const EDGE_RELEASE: i32 = 100;

/// Maximizes `entity`, or restores it when it is maximized.
#[derive(BevyEvent)]
struct ToggleMaximize {
    entity: Entity,
    /// The point a double-click toggled the window at. A floating window
    /// fills the display under it, and without it the display under the
    /// window's center.
    at: Option<Origin>,
}

/// A maximized floating window.
#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct FloatingMaximized {
    /// The frame that fills the display's usable area, padding included.
    frame: IRect,
    /// How far the window sat from the usable area's top-left corner, so it
    /// returns to the same place relative to its display.
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
        app.add_observer(toggle_maximize)
            .add_systems(
                PreUpdate,
                command_toggle
                    .in_set(PreUpdateSet::Dispatch)
                    .run_if(on_message::<Event>),
            )
            .add_systems(
                Update,
                (double_click_toggle, fill_drop_display)
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

/// Toggles the focused window on the maximize command.
fn command_toggle(
    mut messages: MessageReader<Event>,
    focused: Query<Entity, With<FocusedMarker>>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Maximize))
        .next()
        .is_none()
    {
        return;
    }
    if let Ok(entity) = focused.single() {
        commands.trigger(ToggleMaximize { entity, at: None });
    }
}

/// Toggles a window when its title bar is double-clicked. A double-click on
/// a control in the title bar, such as a button or tab, is the control's
/// own.
fn double_click_toggle(
    mut messages: MessageReader<InputEvent>,
    mut windows: Query<(
        Entity,
        &mut Window,
        Option<&TopBarHeight>,
        Has<FloatingMaximized>,
    )>,
    window_manager: Res<WindowManager>,
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
        let Some((entity, mut window, top_bar, maximized)) = windows
            .iter_mut()
            .find(|(_, window, ..)| window.id() == window_id)
        else {
            continue;
        };
        // The engine stops reading a maximized floating window's moves, so a
        // window the user dragged is read here to find its top bar.
        if maximized && !window.write_pending() {
            _ = window.update_frame();
        }
        if !in_top_bar(&window, top_bar, point) {
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
        commands.trigger(ToggleMaximize {
            entity,
            at: Some(point),
        });
    }
}

type Toggles<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Window,
        &'static mut Position,
        &'static mut Bounds,
        Option<&'static Unmanaged>,
        Option<&'static FloatingMaximized>,
    ),
>;

/// Maximizes a window, or restores it when it is maximized. A tiled window
/// becomes a column of its strip that fills its strip's display. A floating
/// window fills a display over the layout.
fn toggle_maximize(
    toggle: On<ToggleMaximize>,
    mut windows: Toggles,
    mut strips: Query<(&mut LayoutStrip, &ChildOf)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    config: Res<Config>,
    time: Res<Time>,
    mut commands: Commands,
) {
    let ToggleMaximize { entity, at } = *toggle;
    let Ok((mut window, mut position, mut bounds, unmanaged, maximized)) = windows.get_mut(entity)
    else {
        return;
    };
    if matches!(unmanaged, Some(Unmanaged::Minimized | Unmanaged::Hidden)) {
        return;
    }
    let Ok(mut entity_commands) = commands.get_entity(entity) else {
        return;
    };
    entity_commands.try_insert(FrameHold {
        until: time.elapsed() + FRAME_HOLD,
    });

    let tiled = unmanaged
        .is_none()
        .then(|| strips.iter_mut().find(|(strip, _)| strip.contains(entity)))
        .flatten();
    if let Some((mut strip, child_of)) = tiled {
        let Ok((display, dock)) = displays.get(child_of.parent()) else {
            return;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        let frame = IRect::from_corners(position.0, position.0 + bounds.0);
        toggle_tiled(
            entity,
            frame,
            &mut strip,
            viewport,
            &config,
            &mut windows,
            &mut commands,
        );
        return;
    }

    let point = at.unwrap_or_else(|| window.frame().center());
    let Some(visible) = displays
        .iter()
        .find(|(display, _)| display.bounds().contains(point))
        .map(|(display, dock)| display.visible_bounds(dock))
    else {
        return;
    };
    let padding = IVec2::new(window.horizontal_padding(), window.vertical_padding());
    if let Some(maximized) = maximized {
        debug!("restoring maximized window {}", window.id());
        entity_commands.try_remove::<FloatingMaximized>();
        // Set without a change, so the frame is not written at once and
        // the window can first end clear of the screen's edge.
        let frame = floating_frame(bounds.0, maximized.offset, visible, padding);
        position.bypass_change_detection().0 = frame.min;
        bounds.bypass_change_detection().0 = frame.size();
        let from = window.frame();
        approach(&mut window, from, frame);
    } else {
        debug!("maximizing window {}", window.id());
        let frame = filled(&window, visible);
        entity_commands.try_insert(FloatingMaximized {
            frame,
            offset: position.0 - visible.min,
        });
        window.place(frame);
    }
}

/// Maximizes tiled window `entity`, which shows `frame`, in `strip`, whose
/// viewport is `viewport`, or restores it when it is maximized.
fn toggle_tiled(
    entity: Entity,
    frame: IRect,
    strip: &mut LayoutStrip,
    viewport: IRect,
    config: &Config,
    windows: &mut Toggles,
    commands: &mut Commands,
) {
    let members = strip.tab_group(entity).unwrap_or_else(|| vec![entity]);
    let limit = effective_stack_limit(config, viewport.height());
    if let Some(restored) = strip.unmaximize(entity, limit) {
        debug!("restoring maximized window {entity}");
        let size = Size::new(
            restored.width.of_viewport(viewport.width()),
            restored.height.map_or(viewport.height(), |height| {
                height.of_viewport(viewport.height())
            }),
        );
        // The column goes back to where it stood, as after a width change,
        // rather than where the maximized column was.
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(WidthRun {
                anchor: restored.anchor.in_viewport(viewport),
            });
        }
        for member in members {
            if let Ok((_, _, mut bounds, ..)) = windows.get_mut(member) {
                bounds.0 = size;
            }
            if let Ok(mut entity_commands) = commands.get_entity(member) {
                entity_commands.try_remove::<ResizeMarker>();
            }
        }
    } else {
        debug!("maximizing window {entity}");
        let index = strip.index_of(entity).ok();
        let anchor = ViewportAnchor::of(
            frame,
            viewport,
            index == Some(0),
            index.is_some_and(|index| index + 1 == strip.len()),
            config.auto_center(),
        );
        let before = Before::new(
            ViewportShare::of(frame.width(), viewport.width()),
            ViewportShare::of(frame.height(), viewport.height()),
            anchor,
        );
        if let Err(err) = strip.maximize(entity, Side::East, before) {
            debug!("no maximize: {err}");
            return;
        }
        // A native resize the window was settling would otherwise adopt the
        // window's old size into the column it left.
        supersede_native_resize(&members, commands);
    }
    commands.reshuffle_around(entity);
}

/// The frame that fills `visible`, a display's usable area, padding included.
fn filled(window: &Window, visible: IRect) -> IRect {
    let padding = IVec2::new(window.horizontal_padding(), window.vertical_padding());
    IRect::from_corners(visible.min - padding, visible.max + padding)
}

/// Reads the frame of a maximized floating window when it is pressed, and
/// fills the usable area of the display it is dropped on, unless it already
/// fills it.
///
/// macOS keeps a dropped window on the display under the pointer, so the
/// pointer at the release picks the display. The window is the one pressed,
/// because macOS can drop it short of the pointer: on a native rig Finder
/// ended 18 points below it.
fn fill_drop_display(
    mut messages: MessageReader<InputEvent>,
    mut pressed: Local<Option<Entity>>,
    mut windows: Query<(Entity, &mut Window, &mut FloatingMaximized)>,
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
                    .iter_mut()
                    .find(|(_, window, _)| Some(window.id()) == window_id)
                    .map(|(entity, mut window, _)| {
                        // The engine stops reading a maximized floating
                        // window's moves, so an app may have moved it since.
                        if !window.write_pending() {
                            _ = window.update_frame();
                        }
                        entity
                    });
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
                let frame = filled(&window, visible);
                if maximized.frame == frame {
                    continue;
                }
                debug!("fitting maximized window {} to {frame:?}", window.id());
                maximized.frame = frame;
                window.place(frame);
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_insert(FrameHold {
                        until: time.elapsed() + FRAME_HOLD,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Whether `point` is in the window's top bar: its title bar, toolbar, or
/// navigation bar.
fn in_top_bar(window: &Window, top_bar: Option<&TopBarHeight>, point: Origin) -> bool {
    let frame = window.shown_frame();
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
        Option<&'static FloatingMaximized>,
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
