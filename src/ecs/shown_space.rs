//! After a display change, macOS can leave a display showing a Space that
//! holds none of its windows, while they sit together in another Space of the
//! same display. A frame write cannot move a window between two Spaces of one
//! display, so the engine focuses one of those windows instead, and macOS
//! switches the display to their Space. Then it gives focus back.

use std::time::Duration;

use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut, SystemParam};
use bevy::time::{Time, Timer, TimerMode};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::display::DisplayReconcileState;
use super::focus::FocusWindow;
use super::layout::LayoutStrip;
use super::{FocusedMarker, NativeFullscreenMarker, SelectedVirtualMarker};
use crate::manager::{Display, Window, WindowManager};
use crate::platform::WorkspaceId;

/// How long after the last pass of a burst the check waits, so the native
/// Space reads that pass queued have filed every window.
const SETTLE: Duration = Duration::from_secs(1);
/// How long macOS gets to switch the Space after the focus. A busy app can
/// take more than a second to activate.
const SWITCH: Duration = Duration::from_secs(3);

/// The displays to check after the burst that just ended.
#[derive(Default, Resource)]
pub(crate) struct ShownSpaceCheck {
    pending: Option<Check>,
}

struct Check {
    /// Displays that showed windows when the burst began.
    displays: Vec<CGDirectDisplayID>,
    /// The window focused when the burst ended. It gets focus back.
    focused: Option<Entity>,
    timer: Timer,
    switching: Option<Switch>,
    /// The engine focused a window to switch a Space, so focus goes back.
    switched: bool,
}

struct Switch {
    display_id: CGDirectDisplayID,
    space: WorkspaceId,
    /// The window the engine focused to make macOS switch.
    window: Entity,
}

impl ShownSpaceCheck {
    pub(crate) fn arm(&mut self, displays: Vec<CGDirectDisplayID>, focused: Option<Entity>) {
        self.pending = (!displays.is_empty()).then(|| Check {
            displays,
            focused,
            timer: Timer::new(SETTLE, TimerMode::Once),
            switching: None,
            switched: false,
        });
    }
}

/// Which Space each display shows, and which strips hold windows.
#[derive(SystemParam)]
pub(crate) struct Desktops<'w, 's> {
    displays: Query<'w, 's, (Entity, &'static Display)>,
    strips: Query<
        'w,
        's,
        (
            &'static LayoutStrip,
            &'static ChildOf,
            Has<SelectedVirtualMarker>,
            Has<NativeFullscreenMarker>,
        ),
    >,
    windows: Query<'w, 's, &'static Window>,
    window_manager: Res<'w, WindowManager>,
}

impl Desktops<'_, '_> {
    fn shown(&self, display_id: CGDirectDisplayID) -> Option<WorkspaceId> {
        self.window_manager.active_display_space(display_id).ok()
    }

    /// When `display_id` shows a Space without windows while every strip of
    /// it that holds windows is in one other Space, that Space and a window
    /// in it.
    fn stranded(&self, display_id: CGDirectDisplayID) -> Option<(WorkspaceId, Entity)> {
        let (entity, _) = self
            .displays
            .iter()
            .find(|(_, display)| display.id() == display_id)?;
        let shown = self.shown(display_id)?;
        let with_windows = self
            .strips
            .iter()
            .filter(|(strip, child, _, fullscreen)| {
                !fullscreen && child.parent() == entity && strip.len() > 0
            })
            .collect::<Vec<_>>();
        let mut spaces = with_windows
            .iter()
            .map(|(strip, ..)| strip.id())
            .collect::<Vec<_>>();
        spaces.sort_unstable();
        spaces.dedup();
        if spaces.contains(&shown) {
            return None;
        }
        let [space] = spaces[..] else {
            if spaces.len() > 1 {
                warn!(
                    display_id,
                    ?spaces,
                    "The display shows none of its windows, which sit in several Spaces"
                );
            }
            return None;
        };
        let window = with_windows
            .iter()
            .min_by_key(|(strip, _, selected, _)| (!selected, strip.virtual_index))
            .and_then(|(strip, ..)| strip.all_windows().first().copied())?;
        Some((space, window))
    }

    /// Whether macOS has `window` in a Space a display shows. Floating
    /// windows count too, though no strip holds them.
    fn shows(&self, window: Entity) -> bool {
        let Ok(window) = self.windows.get(window) else {
            return false;
        };
        self.displays.iter().any(|(_, display)| {
            self.shown(display.id())
                .and_then(|space| self.window_manager.windows_in_workspace(space).ok())
                .is_some_and(|ids| ids.contains(&window.id()))
        })
    }
}

pub(crate) fn check_shown_spaces(
    mut state: ResMut<ShownSpaceCheck>,
    reconcile: Res<DisplayReconcileState>,
    time: Res<Time>,
    desktops: Desktops,
    focused: Query<Entity, With<FocusedMarker>>,
    mut commands: Commands,
) {
    if reconcile.is_reconciling() {
        // A new burst re-arms the check when it ends.
        state.pending = None;
        return;
    }
    let Some(check) = state.pending.as_mut() else {
        return;
    };
    let now_focused = focused.iter().next();
    let expected = check
        .switching
        .as_ref()
        .map_or(check.focused, |switch| Some(switch.window));
    if now_focused != expected && now_focused != check.focused {
        debug!("Focus moved after the display change, so the Space check stops");
        state.pending = None;
        return;
    }
    let waited = check.timer.tick(time.delta()).is_finished();
    let switched = check
        .switching
        .as_ref()
        .is_some_and(|switch| desktops.shown(switch.display_id) == Some(switch.space));
    if !waited && !switched {
        return;
    }
    if let Some(Switch {
        display_id, space, ..
    }) = check.switching.take()
    {
        if !switched {
            warn!(
                display_id,
                space, "macOS did not switch the display to its windows' Space"
            );
            state.pending = None;
            return;
        }
        debug!(
            display_id,
            space, "The display shows its windows' Space again"
        );
    }

    while let Some(display_id) = check.displays.pop() {
        let Some((space, window)) = desktops.stranded(display_id) else {
            continue;
        };
        debug!(
            display_id,
            space, "The display shows none of its windows, focusing one to show their Space"
        );
        commands.trigger(FocusWindow {
            entity: window,
            raise: true,
        });
        check.switching = Some(Switch {
            display_id,
            space,
            window,
        });
        check.timer = Timer::new(SWITCH, TimerMode::Once);
        check.switched = true;
        return;
    }

    if check.switched
        && let Some(previous) = check.focused
        && desktops.shows(previous)
    {
        commands.trigger(FocusWindow {
            entity: previous,
            raise: true,
        });
    }
    state.pending = None;
}
