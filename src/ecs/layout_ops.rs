//! Replays a script's [`LayoutOp`]s (returned by a Lua handler that
//! transformed a `WindowSet`) against the live world.
//!
//! Unlike other command handlers, each op names a window explicitly rather
//! than acting on the focused one. Because the snapshot a script transformed
//! may be a frame stale, each op is applied independently and best-effort: one
//! that can't be resolved is logged at debug and skipped without affecting
//! the rest.

use crate::types::windowset::LayoutOp;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::system::{Commands, Query, Res};
use bevy::math::IRect;
use bevy::platform::collections::HashSet;
use tracing::debug;

use crate::commands::{Command, MoveFocus, Operation, Side};
use crate::config::Config;
use crate::ecs::drag::NativeResizeStart;
use crate::ecs::focus::FocusWindow;
use crate::ecs::layout::{Column, LayoutStrip, RestoreTo, StackItem, effective_stack_limit};
use crate::ecs::maximize::restore_tiled;
use crate::ecs::params::Windows;
use crate::ecs::workspace::VirtualMoveMarker;
use crate::ecs::{
    ActiveWorkspaceMarker, DockPosition, SendMessageTrigger, SpawnCommandsExt, Unmanaged,
    WidthRatio, Window,
};
use crate::events::Event;
use crate::manager::{Display, Origin, Size};

type Workspaces<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        &'static ChildOf,
    ),
    Without<Window>,
>;

type Displays<'w, 's> = Query<'w, 's, (&'static Display, Option<&'static DockPosition>)>;

/// Applies the layout operations a Lua handler returned.
pub(crate) fn apply_layout_ops(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut workspaces: Workspaces,
    displays: Displays,
    config: Res<Config>,
    mut commands: Commands,
) {
    let batches: Vec<Vec<LayoutOp>> = messages
        .read()
        .filter_map(|message| match message {
            Event::Command {
                command: Command::Layout(ops),
            } => Some(ops.clone()),
            _ => None,
        })
        .collect();

    for ops in batches {
        // `Unmanaged` inserts don't take effect until commands flush, so track
        // what this batch floated to tell a just-floated window from one still
        // genuinely tiled (needed by `SetFrame`).
        let mut floated: HashSet<Entity> = HashSet::new();
        for op in ops {
            apply(
                op,
                &windows,
                &mut workspaces,
                &displays,
                &config,
                &mut floated,
                &mut commands,
            );
        }
    }
}

/// Applies one op, or explains why it could not be.
#[allow(clippy::too_many_lines)]
fn apply(
    op: LayoutOp,
    windows: &Windows,
    workspaces: &mut Workspaces,
    displays: &Displays,
    config: &Config,
    floated: &mut HashSet<Entity>,
    commands: &mut Commands,
) {
    // Resolve up front so every arm below can assume the window still exists.
    let entity = if let Some(window_id) = op.target() {
        let Some((_, entity)) = windows.find(window_id) else {
            debug!(
                target: "paneru::lua",
                "skipping {op:?}: window {window_id} is gone since the handler ran"
            );
            return;
        };
        Some(entity)
    } else {
        None
    };

    match op {
        LayoutOp::Focus(_) => {
            let entity = entity.expect("Focus names a window");
            commands.trigger(FocusWindow {
                entity,
                raise: true,
            });
        }

        LayoutOp::Swap(_, other) => {
            let entity = entity.expect("Swap names a window");
            let Some((_, other_entity)) = windows.find(other) else {
                debug!(target: "paneru::lua", "skipping {op:?}: window {other} is gone");
                return;
            };
            let Some((mut strip, ..)) = workspaces
                .iter_mut()
                .find(|(strip, ..)| strip.contains(entity) && strip.contains(other_entity))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the two windows share no strip");
                return;
            };
            let (Ok(left), Ok(right)) = (strip.index_of(entity), strip.index_of(other_entity))
            else {
                return;
            };
            strip.swap(left, right);
            commands.reshuffle_around(entity);
        }

        LayoutOp::MoveToWorkspace {
            workspace, follow, ..
        } => {
            let entity = entity.expect("MoveToWorkspace names a window");
            // Virtual workspaces are numbered from one for a script and from
            // zero inside the layout.
            let Some(target_virtual_index) = workspace.checked_sub(1) else {
                debug!(target: "paneru::lua", "skipping {op:?}: workspaces are numbered from 1");
                return;
            };
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_insert(VirtualMoveMarker {
                    target_virtual_index,
                    move_focus: if follow {
                        MoveFocus::Follow
                    } else {
                        MoveFocus::Stay
                    },
                });
            }
        }

        LayoutOp::View { workspace } => {
            let Some(index) = workspace.checked_sub(1) else {
                debug!(target: "paneru::lua", "skipping {op:?}: workspaces are numbered from 1");
                return;
            };
            commands.trigger(SendMessageTrigger(Event::Command {
                command: Command::Window(Operation::VirtualNumber(index)),
            }));
        }

        LayoutOp::SetFloating { floating, .. } => {
            let entity = entity.expect("SetFloating names a window");
            if floating {
                floated.insert(entity);
            } else {
                floated.remove(&entity);
            }
            set_floating(entity, floating, workspaces, displays, config, commands);
        }

        LayoutOp::SetManaged { managed, .. } => {
            let entity = entity.expect("SetManaged names a window");
            if managed {
                floated.remove(&entity);
            } else {
                floated.insert(entity);
            }
            set_floating(entity, !managed, workspaces, displays, config, commands);
        }

        LayoutOp::SetWidth { ratio, .. } => {
            let entity = entity.expect("SetWidth names a window");
            if !ratio.is_finite() || ratio <= 0.0 {
                debug!(target: "paneru::lua", "skipping {op:?}: a width ratio must be positive");
                return;
            }
            let focused = windows
                .focused()
                .is_some_and(|(_, focused)| focused == entity);
            // A maximized window goes back to its stack first, as for the
            // width commands. The focused one takes `resize_window` below,
            // which restores it itself.
            if !focused {
                restore_first(
                    entity,
                    |viewport| RestoreTo::Home {
                        limit: effective_stack_limit(config, viewport.height()),
                    },
                    workspaces,
                    displays,
                    config,
                    commands,
                );
            }
            if let Some((strip, ..)) = workspaces.iter().find(|(strip, ..)| strip.contains(entity))
                && let Some(column) = strip
                    .index_of(entity)
                    .ok()
                    .and_then(|index| strip.get(index).ok())
            {
                for member in column.window_iter() {
                    if let Ok(mut entity_commands) = commands.get_entity(member) {
                        entity_commands.try_remove::<NativeResizeStart>();
                    }
                }
            }
            // The layout pipeline turns the ratio into an actual width against
            // whichever display the window is on, so this does not have to.
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_insert(WidthRatio(ratio));
            }
            // Stacked siblings share a column, and so its width.
            if let Some((strip, ..)) = workspaces.iter().find(|(strip, ..)| strip.contains(entity))
                && let Some(Column::Stack(stack)) = strip
                    .index_of(entity)
                    .ok()
                    .and_then(|index| strip.get(index).ok())
            {
                for sibling in stack.iter().flat_map(StackItem::window_iter) {
                    if sibling != entity
                        && let Ok(mut entity_commands) = commands.get_entity(sibling)
                    {
                        entity_commands.try_insert(WidthRatio(ratio));
                    }
                }
            }
            // A stored ratio does not resize anything by itself: no system
            // reacts to `WidthRatio` changes, and the next bounds sync
            // recomputes it from the live frame, discarding the request. The
            // interactive resize path (`resize_window`, used by the menubar
            // width picker) does the real work — pixel size, viewport
            // clamping, stacked siblings, reshuffle — but only for the
            // focused window, so route through it when we can.
            if focused {
                commands.trigger(SendMessageTrigger(Event::Command {
                    command: Command::Window(Operation::SetWidth(ratio)),
                }));
                return;
            }
            commands.reshuffle_around(entity);
        }

        LayoutOp::SetFrame { frame, .. } => {
            let entity = entity.expect("SetFrame names a window");
            // The layout engine owns a tiled window's geometry and will move
            // it back, so warn if the target isn't floated.
            if !floated.contains(&entity)
                && windows
                    .get_managed(entity)
                    .is_some_and(|(_, _, unmanaged)| unmanaged.is_none())
            {
                debug!(
                    target: "paneru::lua",
                    "{op:?} targets a tiled window; float it first or the layout will move it back"
                );
            }
            let origin = Origin::new(frame.x, frame.y);
            let size = Size::new(frame.width.max(1), frame.height.max(1));
            let group = workspaces
                .iter()
                .find_map(|(strip, ..)| strip.tab_group(entity))
                .unwrap_or_else(|| vec![entity]);
            for member in group {
                if let Ok(mut entity_commands) = commands.get_entity(member) {
                    entity_commands.try_remove::<NativeResizeStart>();
                }
            }
            commands.reposition_entity(entity, origin);
            commands.resize_entity(entity, size);
        }

        LayoutOp::Stack { onto, .. } => {
            let entity = entity.expect("Stack names a window");
            let Some((_, onto_entity)) = windows.find(onto) else {
                debug!(target: "paneru::lua", "skipping {op:?}: window {onto} is gone");
                return;
            };
            // The window, and a maximized column it would join, are restored
            // where they stand first, as for the stack commands.
            let target = workspaces
                .iter()
                .find(|(strip, ..)| strip.contains(entity))
                .and_then(|(strip, ..)| strip.left_neighbour(entity));
            for window in std::iter::once(entity).chain(target) {
                restore_first(
                    window,
                    |_| RestoreTo::Here,
                    workspaces,
                    displays,
                    config,
                    commands,
                );
            }
            let Some((mut strip, ..)) = workspaces
                .iter_mut()
                .find(|(strip, ..)| strip.contains(entity) && strip.contains(onto_entity))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the two windows share no strip");
                return;
            };
            if strip.stack(entity, Side::West).is_err() {
                debug!(target: "paneru::lua", "skipping {op:?}: the layout refused the stack");
                return;
            }
            commands.reshuffle_around(entity);
        }

        LayoutOp::Unstack(_) => {
            let entity = entity.expect("Unstack names a window");
            restore_first(
                entity,
                |_| RestoreTo::Here,
                workspaces,
                displays,
                config,
                commands,
            );
            let Some((mut strip, ..)) = workspaces
                .iter_mut()
                .find(|(strip, ..)| strip.contains(entity))
            else {
                return;
            };
            if strip.unstack(entity, Side::East).is_err() {
                debug!(target: "paneru::lua", "skipping {op:?}: the window is not in a stack");
                return;
            }
            commands.reshuffle_around(entity);
        }
    }
}

/// Takes a window out of the tiling layout or puts it back. Mirrors
/// `manage_window`: floating→tiled has to re-append the window to a strip
/// itself, since nothing downstream does it automatically.
fn set_floating(
    entity: Entity,
    floating: bool,
    workspaces: &mut Workspaces,
    displays: &Displays,
    config: &Config,
    commands: &mut Commands,
) {
    // A maximized window is restored first, as the float toggle does.
    if floating {
        restore_first(
            entity,
            |_| RestoreTo::Float,
            workspaces,
            displays,
            config,
            commands,
        );
    }
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        if floating {
            entity_commands.try_insert(Unmanaged::Floating);
        } else {
            entity_commands.try_remove::<Unmanaged>();
        }
    }

    if !floating
        && !workspaces.iter().any(|(strip, ..)| strip.contains(entity))
        && let Some((mut strip, ..)) = workspaces.iter_mut().find(|(_, active, _)| *active)
    {
        strip.append(entity);
        commands.reshuffle_around(entity);
    }
}

/// Restores `entity` first when it is maximized in a strip, as the native
/// command an op matches does. `to` picks where it goes from the viewport of
/// the display that shows its strip.
fn restore_first(
    entity: Entity,
    to: impl FnOnce(IRect) -> RestoreTo,
    workspaces: &mut Workspaces,
    displays: &Displays,
    config: &Config,
    commands: &mut Commands,
) {
    if let Some((mut strip, _, child_of)) = workspaces
        .iter_mut()
        .find(|(strip, ..)| strip.maximized(entity).is_some())
        && let Ok((display, dock)) = displays.get(child_of.parent())
    {
        let viewport = display.actual_display_bounds(dock, config);
        restore_tiled(entity, &mut strip, to(viewport), viewport, commands);
    }
}
