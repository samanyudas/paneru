use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, RwLock, Weak};

use bevy::prelude::*;
use objc2_core_foundation::{CFNumber, CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::prelude::RwLockExt;

use crate::errors::Error;
use crate::events::{DestroySource, Event, MouseButton};
use crate::manager::app::MockApplicationApi;
use crate::manager::window_writer::{FrameWrites, WindowWriters, WriteSink};
use crate::manager::{
    Application, Display, MockProcessApi, MockWindowApi, MockWindowManagerApi, Origin, Size,
    Window, origin_from, origin_to,
};
use crate::platform::{Modifiers, Pid, ProcessSerialNumber, WinID, WorkspaceId};
use crate::util::AXUIWrapper;

use super::*;

/// Data for a mocked application.
pub(crate) struct MockAppData {
    pub(crate) psn: ProcessSerialNumber,
    pub(crate) bundle_id: String,
    pub(crate) name: String,
    pub(crate) focused_window_id: Option<WinID>,
    pub(crate) is_frontmost: bool,
    pub(crate) connection: Option<crate::platform::ConnID>,
    /// The app's window writes go through the engine's per-app writers,
    /// like `WindowOS`'s, instead of landing before each call returns.
    /// Takes effect for windows created after it is set.
    pub(crate) queued_writes: bool,
    /// How long each queued position or size write takes, like a busy app.
    pub(crate) write_delay: std::time::Duration,
}

/// Data for a mocked window.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct MockWindowData {
    pub(crate) id: WinID,
    pub(crate) pid: Pid,
    pub(crate) frame: IRect,
    pub(crate) maximum_size: Option<Size>,
    /// The smallest size the app accepts, like an app with a minimum height.
    pub(crate) minimum_size: Option<Size>,
    pub(crate) resize_bottom_limit: Option<i32>,
    /// The screen's bottom edge, which holds on to the window's bottom edge
    /// as macOS does: a shrink that leaves the bottom within
    /// [`STICKY_ZONE`] of the edge, from a bottom already there, is ignored.
    pub(crate) sticky_bottom: Option<i32>,
    /// Size writes the app drops before it applies one, like an app that
    /// applies a size only after the call returns.
    pub(crate) dropped_resizes: u8,
    /// The size writes the app dropped, which [`MockState::os_apply_late_resizes`]
    /// applies in order, like an app busy when the calls returned.
    pub(crate) late_resizes: Vec<Size>,
    /// How long a frame read takes, like an app slow to answer.
    pub(crate) frame_read_delay: std::time::Duration,
    pub(crate) resize_writes: usize,
    pub(crate) position_writes: usize,
    pub(crate) dropped_positions: u16,
    pub(crate) frame_read_failures: u8,
    pub(crate) title: String,
    pub(crate) minimized: bool,
    pub(crate) workspace_id: WorkspaceId,
    /// Listed in every space, like a window assigned to all desktops.
    pub(crate) on_all_spaces: bool,
    pub(crate) visible: bool,
    pub(crate) role: String,
    pub(crate) subrole: String,
    pub(crate) identifier: String,
    pub(crate) is_full_screen: bool,
    /// The app does not let the window's size change.
    pub(crate) fixed_size: bool,
    pub(crate) border_radius: Option<f64>,
    /// What Accessibility reports as the window's top bar height. `None`
    /// models a window whose top bar cannot be measured.
    pub(crate) top_bar_height: Option<i32>,
    /// Models Accessibility failing to read the window, such as while its
    /// app is busy.
    pub(crate) top_bar_unreadable: bool,
    pub(crate) top_bar_reads: usize,
    /// Screen areas where a press lands on a control, such as a toolbar button.
    pub(crate) controls: Vec<IRect>,
    pub(crate) horizontal_padding: i32,
    pub(crate) vertical_padding: i32,
    pub(crate) child_role: bool,
    /// Bumped when the app replaces the window's AX element. A `Window`
    /// handle created before the bump is stale and its `role()` fails.
    pub(crate) ax_generation: u32,
    /// The app has not put the window's current AX element in its window
    /// list yet.
    pub(crate) ax_unlisted: bool,
}

/// How far above the screen's bottom edge [`MockWindowData::sticky_bottom`]
/// holds a window's bottom edge. On a native rig the zone reached between 22
/// and 42 points.
const STICKY_ZONE: i32 = 32;

impl MockWindowData {
    /// The size the app applies when asked for `size`, within its limits.
    fn accepted_size(&self, size: Size) -> Size {
        let mut size = self.maximum_size.map_or(size, |maximum| size.min(maximum));
        size = self.minimum_size.map_or(size, |minimum| size.max(minimum));
        if let Some(bottom) = self.resize_bottom_limit
            && size.y > 0
        {
            size.y = size.y.min((bottom - self.frame.min.y).max(0));
        }
        if let Some(edge) = self.sticky_bottom
            && self.frame.max.y >= edge - STICKY_ZONE
            && self.frame.min.y + size.y >= edge - STICKY_ZONE
            && size.y < self.frame.height()
        {
            size.y = self.frame.height();
        }
        size
    }
}

impl Default for MockWindowData {
    fn default() -> Self {
        Self {
            id: 0,
            pid: 0,
            frame: IRect::default(),
            maximum_size: None,
            minimum_size: None,
            resize_bottom_limit: None,
            sticky_bottom: None,
            dropped_resizes: 0,
            late_resizes: Vec::new(),
            frame_read_delay: std::time::Duration::ZERO,
            resize_writes: 0,
            position_writes: 0,
            dropped_positions: 0,
            frame_read_failures: 0,
            title: String::new(),
            minimized: false,
            workspace_id: 0,
            on_all_spaces: false,
            visible: true,
            role: "AXWindow".to_string(),
            subrole: "AXStandardWindow".to_string(),
            identifier: "testid".to_string(),
            is_full_screen: false,
            fixed_size: false,
            border_radius: None,
            top_bar_height: None,
            top_bar_unreadable: false,
            top_bar_reads: 0,
            controls: Vec::new(),
            horizontal_padding: 0,
            vertical_padding: 0,
            child_role: false,
            ax_generation: 0,
            ax_unlisted: false,
        }
    }
}

/// Data for a mocked display.
struct MockDisplayData {
    id: u32,
    bounds: IRect,
    workspaces: Vec<WorkspaceId>,
}

/// The internal state of our "Virtual macOS".
struct MockStateInner {
    apps: HashMap<Pid, MockAppData>,
    windows: HashMap<WinID, MockWindowData>,
    displays: HashMap<u32, MockDisplayData>,
    fullscreen_spaces: HashSet<WorkspaceId>,
    membership_failures: HashMap<WorkspaceId, u16>,
    membership_reads: HashMap<WorkspaceId, usize>,
    window_list_reads: HashMap<Pid, usize>,
    active_display_id: u32,
    cursor_position: Origin,
    /// The display bounds whose desktop the engine last focused.
    focused_desktop: Option<IRect>,
    /// Whether the window server sees the left mouse button down.
    left_button_down: bool,
    /// Windows raised above the rest, bottom to top. Windows not in it sit
    /// below, the lowest ID on top, so overlapping windows resolve the same
    /// way on every run.
    raised: Vec<WinID>,
    event_queue: VecDeque<Event>,
    /// Windows that are gone but which the app's AX window list still reports,
    /// modelling the lag real apps show right after a window closes.
    stale_window_ids: HashMap<WinID, Pid>,
    unordered_windows: HashSet<WinID>,
    /// Windows the app keeps out of its accessibility window list while the
    /// window server still reports them on screen: a background native tab.
    background_tabs: HashSet<WinID>,
    /// How many times any window was raised.
    raise_count: usize,
    /// Whether raising a window also makes it its app's focused window, as
    /// `AXRaise` does for a window of the frontmost app.
    raise_takes_focus: bool,
    /// Whether apps answer a moment late, as on macOS: focusing a window
    /// activates its app only when the engine next reads platform events, and
    /// an app in the background reorders a window it was asked to raise only
    /// after `AXRaise` has returned.
    lags: bool,
    /// Windows focused and waiting for their app to activate.
    pending_activations: Vec<WinID>,
    /// Windows whose app activated, shown in front at the next drain.
    activated: Vec<WinID>,
    /// Raises sent to apps in the background and not yet shown, oldest first.
    pending_raises: Vec<WinID>,
}

/// The window on top at `point`: raised windows first, then the lowest ID.
fn top_window_at(inner: &MockStateInner, point: Origin) -> Option<WinID> {
    inner
        .windows
        .iter()
        .filter(|(_, window)| window.frame.contains(point))
        .map(|(id, _)| *id)
        .max_by_key(|id| {
            let raised = inner.raised.iter().position(|raised| raised == id);
            (raised, std::cmp::Reverse(*id))
        })
}

#[derive(Clone)]
pub struct MockState {
    inner: Arc<RwLock<MockStateInner>>,
    /// Carries the window writes of apps with `queued_writes`.
    writers: Arc<WindowWriters<MockApp>>,
    holds: Arc<WriteHolds>,
}

/// Apps whose writer hangs inside its next write until released, like an
/// app busy on its main thread.
#[derive(Default)]
struct WriteHolds {
    held: Mutex<HashSet<Pid>>,
    released: Condvar,
}

impl WriteHolds {
    /// Longer than any test holds an app, yet short enough that a write made
    /// on the engine's own thread fails a test rather than hanging it.
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

    fn wait_while_held(&self, pid: Pid) {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .released
            .wait_timeout_while(held, Self::LIMIT, |held| held.contains(&pid))
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// A mock app as the engine's window writer for it sees it.
pub(crate) struct MockApp {
    pid: Pid,
    inner: Weak<RwLock<MockStateInner>>,
    holds: Arc<WriteHolds>,
}

impl MockApp {
    /// Waits while the app is held, then for the app's write delay.
    fn busy(&self, inner: &RwLock<MockStateInner>) {
        self.holds.wait_while_held(self.pid);
        let delay = inner
            .force_read()
            .apps
            .get(&self.pid)
            .map_or(std::time::Duration::ZERO, |app| app.write_delay);
        std::thread::sleep(delay);
    }
}

impl WriteSink for MockApp {
    type Window = (WinID, u32);

    fn begin(&mut self) {}

    fn set_position(&mut self, &(id, ax_generation): &(WinID, u32), origin: Origin) {
        if let Some(inner) = self.inner.upgrade() {
            self.busy(&inner);
            mock_reposition(&inner, id, ax_generation, origin);
        }
    }

    fn set_size(&mut self, &(id, ax_generation): &(WinID, u32), size: Size) {
        if let Some(inner) = self.inner.upgrade() {
            self.busy(&inner);
            mock_resize(&inner, id, ax_generation, size);
        }
    }

    fn read_frame(&mut self, &(id, ax_generation): &(WinID, u32)) -> Option<IRect> {
        let inner = self.inner.upgrade()?;
        mock_read_frame(&inner, id, ax_generation).ok()
    }

    fn finish(&mut self) {}
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Applies a size write to window `id`, as its app would.
fn mock_resize(inner: &RwLock<MockStateInner>, id: WinID, ax_generation: u32, size: Size) {
    let mut inner = inner.force_write();
    if let Some(w) = inner
        .windows
        .get_mut(&id)
        .filter(|w| w.ax_generation == ax_generation)
    {
        w.resize_writes += 1;
        if w.dropped_resizes > 0 {
            w.dropped_resizes -= 1;
            w.late_resizes.push(size);
            return;
        }
        w.frame.max = w.frame.min + w.accepted_size(size);
    }
}

/// Applies a position write to window `id`, as its app would.
fn mock_reposition(inner: &RwLock<MockStateInner>, id: WinID, ax_generation: u32, origin: Origin) {
    let mut inner = inner.force_write();
    if let Some(w) = inner
        .windows
        .get_mut(&id)
        .filter(|w| w.ax_generation == ax_generation)
    {
        w.position_writes += 1;
        if w.dropped_positions > 0 {
            w.dropped_positions -= 1;
            return;
        }
        let size = w.frame.size();
        w.frame.min = origin;
        w.frame.max = origin + size;
    }
}

/// Reads window `id`'s frame, as Accessibility would.
fn mock_read_frame(
    inner: &RwLock<MockStateInner>,
    id: WinID,
    ax_generation: u32,
) -> crate::errors::Result<IRect> {
    let mut inner = inner.force_write();
    let window = inner
        .windows
        .get_mut(&id)
        .filter(|w| w.ax_generation == ax_generation)
        .ok_or(Error::InvalidWindow)?;
    if window.frame_read_failures > 0 {
        window.frame_read_failures -= 1;
        return Err(Error::InvalidWindow);
    }
    std::thread::sleep(window.frame_read_delay);
    Ok(window.frame)
}

impl MockState {
    pub fn new() -> Self {
        let inner = Arc::new(RwLock::new(MockStateInner {
            apps: HashMap::new(),
            windows: HashMap::new(),
            displays: HashMap::new(),
            fullscreen_spaces: HashSet::new(),
            membership_failures: HashMap::new(),
            membership_reads: HashMap::new(),
            window_list_reads: HashMap::new(),
            active_display_id: 0,
            cursor_position: Origin::ZERO,
            focused_desktop: None,
            left_button_down: false,
            raised: Vec::new(),
            event_queue: VecDeque::new(),
            stale_window_ids: HashMap::new(),
            unordered_windows: HashSet::new(),
            background_tabs: HashSet::new(),
            raise_count: 0,
            raise_takes_focus: false,
            lags: false,
            pending_activations: Vec::new(),
            activated: Vec::new(),
            pending_raises: Vec::new(),
        }));
        let holds = Arc::new(WriteHolds::default());
        let writers = {
            let inner = Arc::downgrade(&inner);
            let holds = holds.clone();
            Arc::new(WindowWriters::new(
                std::time::Duration::from_millis(200),
                move |pid| MockApp {
                    pid: pid.unwrap_or_default(),
                    inner: inner.clone(),
                    holds: holds.clone(),
                },
            ))
        };
        Self {
            inner,
            writers,
            holds,
        }
    }

    #[allow(dead_code)]
    pub fn set_window_unordered(&self, window_id: WinID, unordered: bool) {
        let mut inner = self.inner.force_write();
        if unordered {
            inner.unordered_windows.insert(window_id);
        } else {
            inner.unordered_windows.remove(&window_id);
        }
    }

    /// Makes `window_id` a background native tab: on screen as far as the
    /// window server is concerned, absent from the app's accessibility window
    /// list. This is what Ghostty does with every tab but the selected one.
    #[allow(unused)]
    pub fn set_background_tab(&self, window_id: WinID, background: bool) {
        let mut inner = self.inner.force_write();
        if background {
            inner.background_tabs.insert(window_id);
        } else {
            inner.background_tabs.remove(&window_id);
        }
    }

    pub(crate) fn window_visible(&self, window_id: WinID, visible: bool) {
        let mut state = self.inner.force_write();
        let window = state.windows.get_mut(&window_id).expect("finding window");
        window.visible = visible;
    }

    // --- OS Behavior Methods ---

    pub fn spawn_app(&self, pid: Pid, bundle_id: &str, name: &str) {
        let mut inner = self.inner.force_write();
        inner.apps.insert(
            pid,
            MockAppData {
                psn: ProcessSerialNumber {
                    high: 0,
                    low: pid.cast_unsigned(),
                },
                bundle_id: bundle_id.to_string(),
                name: name.to_string(),
                focused_window_id: None,
                is_frontmost: true,
                connection: Some(0),
                queued_writes: false,
                write_delay: std::time::Duration::ZERO,
            },
        );
    }

    pub fn spawn_window(
        &self,
        pid: Pid,
        workspace_id: WorkspaceId,
        id: WinID,
        frame: IRect,
    ) -> Window {
        let mut inner = self.inner.force_write();
        inner.windows.insert(
            id,
            MockWindowData {
                id,
                pid,
                frame,
                title: format!("Window {id}"),
                workspace_id,
                ..default()
            },
        );
        drop(inner);
        self.create_window(id)
    }

    /// Moves the app's focused window without emitting the notifications a
    /// real focus change would produce.
    pub fn set_focused_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        let Some(pid) = inner.windows.get(&id).map(|window| window.pid) else {
            return;
        };
        if let Some(app) = inner.apps.get_mut(&pid) {
            app.focused_window_id = Some(id);
        }
    }

    pub fn focused_window_id(&self, pid: Pid) -> Option<WinID> {
        self.inner
            .force_read()
            .apps
            .get(&pid)
            .and_then(|app| app.focused_window_id)
    }

    pub fn set_cursor_position(&self, position: IVec2) {
        self.inner.force_write().cursor_position = position;
    }

    /// Moves a window to the front of the window order. A window of an app
    /// that is not frontmost goes just below the frontmost app's focused
    /// window, as observed with mixed-app overlapping stacks on macOS.
    pub fn raise_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        inner.raised.retain(|raised| *raised != id);
        let frontmost = |inner: &MockStateInner, id: &WinID| {
            inner
                .windows
                .get(id)
                .and_then(|window| inner.apps.get(&window.pid))
                .is_some_and(|app| app.is_frontmost)
        };
        let index = if frontmost(&inner, &id) {
            inner.raised.len()
        } else {
            inner
                .raised
                .iter()
                .position(|raised| {
                    inner.windows.get(raised).is_some_and(|window| {
                        inner.apps.get(&window.pid).is_some_and(|app| {
                            app.is_frontmost && app.focused_window_id == Some(*raised)
                        })
                    })
                })
                .unwrap_or(inner.raised.len())
        };
        inner.raised.insert(index, id);
        inner.raise_count += 1;
    }

    /// Makes `pid` the only frontmost app, and makes apps answer late.
    /// Focusing a window activates its app at the next [`Self::drain_events`],
    /// and shows the window in front at the drain after that.
    /// A raise sent to an app in the background shows once time passes, and
    /// raises sent to several apps at once can reach the window server in any
    /// order: reading the window order shows the latest waiting raise, and
    /// draining events shows the rest, newest first.
    pub fn lag_like_macos(&self, pid: Pid) {
        let mut inner = self.inner.force_write();
        inner.lags = true;
        for (app_pid, app) in &mut inner.apps {
            app.is_frontmost = *app_pid == pid;
        }
    }

    /// Makes `pid` the only frontmost app and reports the switch, as macOS
    /// does when the user activates it.
    pub fn activate_app(&self, pid: Pid) {
        let mut inner = self.inner.force_write();
        for (app_pid, app) in &mut inner.apps {
            app.is_frontmost = *app_pid == pid;
        }
        if let Some(psn) = inner.apps.get(&pid).map(|app| app.psn) {
            inner
                .event_queue
                .push_back(Event::ApplicationFrontSwitched { psn });
        }
    }

    /// How many times any window was raised.
    pub fn raise_count(&self) -> usize {
        self.inner.force_read().raise_count
    }

    /// Makes a raised window take its app's focus and report it, as `AXRaise`
    /// does for a window of the frontmost app.
    pub fn set_raise_takes_focus(&self, takes_focus: bool) {
        self.inner.force_write().raise_takes_focus = takes_focus;
    }

    fn show_latest_raise(&self) {
        let latest = self.inner.force_write().pending_raises.pop();
        if let Some(id) = latest {
            self.raise_window(id);
        }
    }

    fn show_pending_raises(&self) {
        let pending = std::mem::take(&mut self.inner.force_write().pending_raises);
        for id in pending.into_iter().rev() {
            self.raise_window(id);
        }
    }

    /// Raises a window without asking for focus, which can still move its
    /// app's focus to it. See [`Self::set_raise_takes_focus`].
    fn raise_without_focus(&self, id: WinID) {
        {
            let mut inner = self.inner.force_write();
            let background = inner
                .windows
                .get(&id)
                .and_then(|window| inner.apps.get(&window.pid))
                .is_some_and(|app| !app.is_frontmost);
            if inner.lags && background {
                inner.pending_raises.push(id);
                return;
            }
        }
        self.raise_window(id);
        let mut inner = self.inner.force_write();
        if !inner.raise_takes_focus {
            return;
        }
        let Some(pid) = inner.windows.get(&id).map(|window| window.pid) else {
            return;
        };
        if let Some(app) = inner.apps.get_mut(&pid) {
            app.focused_window_id = Some(id);
            inner
                .event_queue
                .push_back(Event::WindowFocused { window_id: id });
        }
    }

    /// `ids` back to front, as last raised. A window never raised is left out.
    pub fn window_order(&self, ids: &[WinID]) -> Vec<WinID> {
        self.inner
            .force_read()
            .raised
            .iter()
            .copied()
            .filter(|id| ids.contains(id))
            .collect()
    }

    pub fn focus_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        if inner.lags {
            inner.pending_activations.push(id);
            return;
        }
        if let Some(win) = inner.windows.get(&id) {
            let pid = win.pid;
            if let Some(app) = inner.apps.get_mut(&pid) {
                app.focused_window_id = Some(id);
                let psn = app.psn;
                inner
                    .event_queue
                    .push_back(Event::ApplicationFrontSwitched { psn });
                inner
                    .event_queue
                    .push_back(Event::WindowFocused { window_id: id });
            }
        }
    }

    /// Focuses window `id` in its app without reporting it, as when macOS
    /// reported the focus before the engine tracked the window.
    pub fn focus_window_unreported(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        if let Some(pid) = inner.windows.get(&id).map(|win| win.pid)
            && let Some(app) = inner.apps.get_mut(&pid)
        {
            app.focused_window_id = Some(id);
        }
    }

    pub fn add_display(&mut self, id: u32, bounds: IRect, workspaces: Vec<WorkspaceId>) {
        let mut inner = self.inner.force_write();
        if inner.displays.is_empty() {
            inner.active_display_id = id;
        }
        inner.displays.insert(
            id,
            MockDisplayData {
                id,
                bounds,
                workspaces,
            },
        );
    }

    /// Makes display `id` show native Space `workspace`, without telling the
    /// engine, as when it has not yet seen the switch.
    #[allow(unused)]
    pub fn show_space(&self, id: u32, workspace: WorkspaceId) {
        let mut inner = self.inner.force_write();
        let display = inner.displays.get_mut(&id).expect("finding display");
        display.workspaces.retain(|&shown| shown != workspace);
        display.workspaces.insert(0, workspace);
    }

    pub fn set_display_bounds(&self, id: u32, bounds: IRect) {
        let mut inner = self.inner.force_write();
        inner.displays.get_mut(&id).expect("finding display").bounds = bounds;
    }

    #[allow(unused)]
    pub fn remove_display(&self, id: u32) {
        let mut inner = self.inner.force_write();
        inner.displays.remove(&id);
        if inner.active_display_id == id {
            inner.active_display_id = inner.displays.keys().copied().next().unwrap_or(0);
        }
    }

    /// Makes `id` the active display and reports it, as macOS does once the
    /// focused window lands on another display.
    pub fn os_activate_display(&self, id: u32) {
        let mut inner = self.inner.force_write();
        inner.active_display_id = id;
        inner.event_queue.push_back(Event::DisplayChanged);
    }

    pub fn active_display(&self) -> CGDirectDisplayID {
        self.inner.force_read().active_display_id
    }

    pub(crate) fn activate_display(&self, id: CGDirectDisplayID) {
        let mut inner = self.inner.force_write();
        assert!(inner.displays.contains_key(&id), "finding display");
        inner.active_display_id = id;
        inner.event_queue.push_back(Event::DisplayChanged);
    }

    pub(crate) fn activate_workspace(
        &self,
        display_id: u32,
        workspace_id: WorkspaceId,
        fullscreen: bool,
    ) {
        let mut inner = self.inner.force_write();
        {
            let display = inner
                .displays
                .get_mut(&display_id)
                .expect("finding display");
            display.workspaces.retain(|id| *id != workspace_id);
            display.workspaces.insert(0, workspace_id);
        }
        if fullscreen {
            inner.fullscreen_spaces.insert(workspace_id);
        } else {
            inner.fullscreen_spaces.remove(&workspace_id);
        }
    }

    pub fn drain_events(&self) -> Vec<Event> {
        self.show_pending_raises();
        let activated = std::mem::take(&mut self.inner.force_write().activated);
        for id in activated {
            self.raise_window(id);
        }
        let pending = std::mem::take(&mut self.inner.force_write().pending_activations);
        for id in pending {
            let Some(pid) = self.inner.force_read().windows.get(&id).map(|w| w.pid) else {
                continue;
            };
            self.activate_app(pid);
            let mut inner = self.inner.force_write();
            if let Some(app) = inner.apps.get_mut(&pid) {
                app.focused_window_id = Some(id);
            }
            inner.activated.push(id);
            inner
                .event_queue
                .push_back(Event::WindowFocused { window_id: id });
        }
        let mut inner = self.inner.force_write();
        inner.event_queue.drain(..).collect()
    }

    // --- State Mutation Methods ---

    pub fn update_window<F>(&self, id: WinID, f: F)
    where
        F: FnOnce(&mut MockWindowData),
    {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            f(w);
        }
    }

    #[allow(unused)]
    pub fn update_app(&self, pid: Pid, f: impl FnOnce(&mut MockAppData)) {
        let mut inner = self.inner.force_write();
        if let Some(a) = inner.apps.get_mut(&pid) {
            f(a);
        }
    }

    // --- OS Behavior Methods ---

    #[allow(unused)]
    pub fn os_move_window(&self, id: WinID, origin: Origin) {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            let size = w.frame.size();
            w.frame.min = origin;
            w.frame.max = origin + size;
            inner
                .event_queue
                .push_back(Event::WindowMoved { window_id: id });
        }
    }

    #[allow(unused)]
    pub fn os_resize_window(&self, id: WinID, size: Size) {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            w.frame.max = w.frame.min + size;
            inner
                .event_queue
                .push_back(Event::WindowResized { window_id: id });
        }
    }

    /// Applies, in order, the size writes window `id`'s app dropped, and
    /// applies later writes as they arrive.
    pub fn os_apply_late_resizes(&self, id: WinID) {
        let sizes = self
            .inner
            .force_write()
            .windows
            .get_mut(&id)
            .map(|w| {
                w.dropped_resizes = 0;
                std::mem::take(&mut w.late_resizes)
            })
            .unwrap_or_default();
        for size in sizes {
            let size = self.inner.force_read().windows[&id].accepted_size(size);
            self.os_resize_window(id, size);
        }
    }

    #[allow(unused)]
    pub fn os_minimize_window(&self, id: WinID, minimized: bool) {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            w.minimized = minimized;
            if minimized {
                inner
                    .event_queue
                    .push_back(Event::WindowMinimized { window_id: id });
            } else {
                inner
                    .event_queue
                    .push_back(Event::WindowDeminimized { window_id: id });
            }
        }
    }

    /// Closes a window while its application keeps running, the way macOS
    /// actually reports it: the SLS space notification first, then the AX
    /// element teardown, with the AX element and the app's window list still
    /// reporting the window for a while after, as they do on real apps.
    #[allow(unused)]
    pub fn os_close_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        let Some(window) = inner.windows.remove(&id) else {
            return;
        };
        inner.stale_window_ids.insert(id, window.pid);
        inner.event_queue.push_back(Event::WindowDestroyed {
            window_id: id,
            source: DestroySource::SpaceNotification,
        });
        inner.event_queue.push_back(Event::WindowDestroyed {
            window_id: id,
            source: DestroySource::Accessibility {
                element: Some(mock_element(id, window.ax_generation)),
            },
        });
    }

    /// Moves a window to `workspace_id` the way an app that rebuilds its AX
    /// element on a display change reports it. The window server keeps the
    /// window ID ordered in on the new space, but every `Window` handle created
    /// before the move fails `role()`. The app's window list returns a fresh
    /// handle, and only the SLS space notification arrives for the old space.
    pub fn os_transfer_window(&self, id: WinID, workspace_id: WorkspaceId) {
        let mut inner = self.inner.force_write();
        let Some(window) = inner.windows.get_mut(&id) else {
            return;
        };
        window.workspace_id = workspace_id;
        window.ax_generation += 1;
        inner.event_queue.push_back(Event::WindowDestroyed {
            window_id: id,
            source: DestroySource::SpaceNotification,
        });
    }

    /// Like [`Self::os_transfer_window`], but the app lists the fresh element
    /// only once [`Self::os_list_window`] is called.
    pub fn os_transfer_window_late(&self, id: WinID, workspace_id: WorkspaceId) {
        self.os_transfer_window(id, workspace_id);
        if let Some(window) = self.inner.force_write().windows.get_mut(&id) {
            window.ax_unlisted = true;
        }
    }

    pub fn os_list_window(&self, id: WinID) {
        if let Some(window) = self.inner.force_write().windows.get_mut(&id) {
            window.ax_unlisted = false;
        }
    }

    /// Repeats the SLS space notification for a window that left a space.
    pub fn os_repeat_space_destroy(&self, id: WinID) {
        self.inner
            .force_write()
            .event_queue
            .push_back(Event::WindowDestroyed {
                window_id: id,
                source: DestroySource::SpaceNotification,
            });
    }

    /// Tears down the AX element a transferred window had before the app
    /// replaced it.
    pub fn os_tear_down_replaced_element(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        let Some(generation) = inner.windows.get(&id).map(|w| w.ax_generation) else {
            return;
        };
        inner.event_queue.push_back(Event::WindowDestroyed {
            window_id: id,
            source: DestroySource::Accessibility {
                element: Some(mock_element(id, generation - 1)),
            },
        });
    }

    /// Makes a window disappear with no notification at all, modelling a
    /// destroy event that never arrived — a lost notification, or a window
    /// closed while paneru was not running.
    #[allow(unused)]
    pub fn os_vanish_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        inner.windows.remove(&id);
        inner.unordered_windows.insert(id);
    }

    /// Lets the app's window list catch up with reality after a close.
    #[allow(unused)]
    pub fn os_settle_window_list(&self) {
        self.inner.force_write().stale_window_ids.clear();
    }

    // --- Interaction Helpers ---

    /// The window on top at `point`, as the window server would name it in
    /// a press event.
    pub fn window_at(&self, point: Origin) -> Option<WinID> {
        top_window_at(&self.inner.force_read(), point)
    }

    #[allow(unused)]
    pub fn simulate_click(&self, point: Origin) {
        let mut inner = self.inner.force_write();
        let point = CGPoint::new(point.x.into(), point.y.into());
        let window_id = top_window_at(&inner, origin_from(point));
        inner.event_queue.push_back(Event::MouseDown {
            point,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
            window_id,
            clicks: 1,
        });
        inner.event_queue.push_back(Event::MouseUp {
            point,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        });
    }

    /// Presses `point` twice in a row, as a double-click does.
    #[allow(unused)]
    pub fn simulate_double_click(&self, point: Origin) {
        let mut inner = self.inner.force_write();
        let point = CGPoint::new(point.x.into(), point.y.into());
        let window_id = top_window_at(&inner, origin_from(point));
        for clicks in 1..=2 {
            inner.event_queue.push_back(Event::MouseDown {
                point,
                modifiers: Modifiers::empty(),
                button: MouseButton::Left,
                window_id,
                clicks,
            });
            inner.event_queue.push_back(Event::MouseUp {
                point,
                modifiers: Modifiers::empty(),
                button: MouseButton::Left,
            });
        }
    }

    #[allow(unused)]
    pub fn simulate_window_click(&self, id: WinID) {
        let inner = self.inner.force_read();
        if let Some(w) = inner.windows.get(&id) {
            let center = w.frame.center();
            drop(inner);
            self.simulate_click(center);
        }
    }

    #[allow(unused)]
    pub fn simulate_drag(&self, start: Origin, end: Origin) {
        let mut inner = self.inner.force_write();
        let start_p = CGPoint::new(start.x.into(), start.y.into());
        let end_p = CGPoint::new(end.x.into(), end.y.into());
        let window_id = top_window_at(&inner, start);
        inner.event_queue.push_back(Event::MouseDown {
            point: start_p,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
            window_id,
            clicks: 1,
        });
        inner.event_queue.push_back(Event::MouseDragged {
            point: end_p,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        });
        inner.event_queue.push_back(Event::MouseUp {
            point: end_p,
            modifiers: Modifiers::empty(),
            button: MouseButton::Left,
        });
    }

    /// Makes the next `failures` native membership reads of `space` fail.
    pub fn fail_membership_reads(&self, space: WorkspaceId, failures: u16) {
        self.inner
            .force_write()
            .membership_failures
            .insert(space, failures);
    }

    /// How many native membership reads of `space` the engine made.
    pub fn membership_reads(&self, space: WorkspaceId) -> usize {
        self.inner
            .force_read()
            .membership_reads
            .get(&space)
            .copied()
            .unwrap_or(0)
    }

    /// How many times the engine asked app `pid` for its AX window list.
    pub fn window_list_reads(&self, pid: Pid) -> usize {
        self.inner
            .force_read()
            .window_list_reads
            .get(&pid)
            .copied()
            .unwrap_or(0)
    }

    /// Sets whether the window server sees the left mouse button down. Only
    /// the button state changes: no mouse event is sent.
    pub fn set_left_button_down(&self, down: bool) {
        self.inner.force_write().left_button_down = down;
    }

    pub fn cursor_position(&self) -> IVec2 {
        self.inner.force_read().cursor_position
    }

    pub fn focused_desktop(&self) -> Option<IRect> {
        self.inner.force_read().focused_desktop
    }

    // --- Mock Factory Methods ---

    fn mock_resize(&self, id: WinID, ax_generation: u32, size: Size) {
        mock_resize(&self.inner, id, ax_generation, size);
    }

    fn mock_reposition(&self, id: WinID, ax_generation: u32, origin: Origin) {
        mock_reposition(&self.inner, id, ax_generation, origin);
    }

    /// Sends the window writes of `pid`'s windows created from now on
    /// through the engine's per-app writers.
    pub fn queue_writes(&self, pid: Pid) {
        self.update_app(pid, |app| app.queued_writes = true);
    }

    /// Makes each queued write to `pid`'s windows take `delay`.
    pub fn set_write_delay(&self, pid: Pid, delay: std::time::Duration) {
        self.update_app(pid, |app| app.write_delay = delay);
    }

    /// Makes `pid`'s writer hang inside its next write until released.
    pub fn hold_writes(&self, pid: Pid) {
        self.holds
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid);
    }

    pub fn release_writes(&self, pid: Pid) {
        self.holds
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&pid);
        self.holds.released.notify_all();
    }

    /// Waits up to `timeout` for every queued write to land.
    pub fn flush_writes(&self, timeout: std::time::Duration) -> bool {
        self.writers.flush(timeout)
    }

    /// The frame window `id` has as far as its app is concerned, which can
    /// lag the frame the engine asked for.
    pub fn ax_frame(&self, id: WinID) -> IRect {
        self.inner.force_read().windows[&id].frame
    }

    /// Frame expectations for a window whose app applies each write before
    /// the call returns.
    fn expect_inline_writes(&self, mw: &mut MockWindowApi, id: WinID, ax_generation: u32) {
        let s = self.clone();
        mw.expect_frame().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.frame)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_resize()
            .returning(move |size| s.mock_resize(id, ax_generation, size));

        let s = self.clone();
        mw.expect_reposition()
            .returning(move |origin| s.mock_reposition(id, ax_generation, origin));

        // The order `WindowOS` writes a frame in: an app may limit a size at
        // the old position, so the size is set again once the window moved.
        let s = self.clone();
        mw.expect_place().returning(move |frame: IRect| {
            s.mock_resize(id, ax_generation, frame.size());
            s.mock_reposition(id, ax_generation, frame.min);
            s.mock_resize(id, ax_generation, frame.size());
        });

        let s = self.clone();
        mw.expect_update_frame()
            .returning(move || mock_read_frame(&s.inner, id, ax_generation));

        mw.expect_write_pending().return_const(false);
    }

    /// Frame expectations for a window whose app gets its writes through
    /// the engine's per-app writers, the way `WindowOS` writes them.
    fn expect_queued_writes(
        &self,
        mw: &mut MockWindowApi,
        id: WinID,
        pid: Pid,
        ax_generation: u32,
    ) {
        let writes = Arc::new(Mutex::new(
            FrameWrites::new(self.writers.clone()).starting_at(self.ax_frame(id)),
        ));
        let window = move || (id, ax_generation);

        let w = writes.clone();
        mw.expect_frame().returning(move || lock(&w).frame());
        let w = writes.clone();
        mw.expect_resize()
            .returning(move |size| lock(&w).resize(Some(pid), window, size));
        let w = writes.clone();
        mw.expect_reposition()
            .returning(move |origin| lock(&w).reposition(Some(pid), window, origin));
        let w = writes.clone();
        mw.expect_write_pending()
            .returning(move || lock(&w).write_pending(Some(pid)));
        let w = writes.clone();
        mw.expect_place()
            .returning(move |frame: IRect| lock(&w).place(Some(pid), window, frame));
        let s = self.clone();
        mw.expect_update_frame().returning(move || {
            lock(&writes).update_frame(Some(pid), || mock_read_frame(&s.inner, id, ax_generation))
        });
    }

    #[allow(clippy::too_many_lines)]
    pub fn create_window(&self, id: WinID) -> Window {
        let mut mw = MockWindowApi::new();
        let ax_generation = self
            .inner
            .force_read()
            .windows
            .get(&id)
            .map_or(0, |w| w.ax_generation);

        mw.expect_id().return_const(id);

        let s = self.clone();
        mw.expect_pid().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.pid)
                .ok_or(Error::InvalidWindow)
        });

        let queued_pid = {
            let inner = self.inner.force_read();
            inner
                .windows
                .get(&id)
                .map(|w| w.pid)
                .filter(|pid| inner.apps.get(pid).is_some_and(|app| app.queued_writes))
        };
        if let Some(pid) = queued_pid {
            self.expect_queued_writes(&mut mw, id, pid, ax_generation);
        } else {
            self.expect_inline_writes(&mut mw, id, ax_generation);
        }

        let s = self.clone();
        mw.expect_focus_with_raise().returning(move |_psn| {
            if !s.inner.force_read().lags {
                s.raise_window(id);
            }
            s.focus_window(id);
        });

        let s = self.clone();
        mw.expect_title().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.title.clone())
                .unwrap_or_default())
        });
        // The mock reads its title from shared state every time, so there's
        // nothing to invalidate — but the call still needs an expectation.
        mw.expect_invalidate_title().return_const(());

        let s = self.clone();
        mw.expect_is_minimized().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.minimized)
        });

        let s = self.clone();
        mw.expect_identifier().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.identifier.clone())
                .unwrap_or_default())
        });

        let s = self.clone();
        // Cached after the first successful read, like `WindowOS::role`, so a
        // closed window keeps reporting its role here too.
        let cached_role = Arc::new(OnceLock::<String>::new());
        mw.expect_role().returning(move || {
            if let Some(role) = cached_role.get() {
                return Ok(role.clone());
            }
            let role = s
                .inner
                .force_read()
                .windows
                .get(&id)
                .filter(|w| w.ax_generation == ax_generation)
                .map(|w| w.role.clone())
                .ok_or_else(|| crate::errors::Error::Generic(format!("window {id} not found")))?;
            if role != accessibility_sys::kAXUnknownRole {
                let _ = cached_role.set(role.clone());
            }
            Ok(role)
        });

        let s = self.clone();
        mw.expect_is_alive().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|window| window.ax_generation == ax_generation)
        });

        let s = self.clone();
        // An empty subrole models a window without an `AXSubrole` attribute,
        // which AX reports as an error rather than an empty string.
        let cached_subrole = Arc::new(OnceLock::<String>::new());
        mw.expect_subrole().returning(move || {
            if let Some(subrole) = cached_subrole.get() {
                return Ok(subrole.clone());
            }
            let subrole = s
                .inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.subrole.clone())
                .filter(|subrole| !subrole.is_empty())
                .ok_or_else(|| {
                    crate::errors::Error::Generic(format!("window {id} has no subrole"))
                })?;
            if subrole != accessibility_sys::kAXUnknownSubrole {
                let _ = cached_subrole.set(subrole.clone());
            }
            Ok(subrole)
        });

        let s = self.clone();
        mw.expect_child_role().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.child_role))
        });

        let s = self.clone();
        mw.expect_horizontal_padding().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.horizontal_padding)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_vertical_padding().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.vertical_padding)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_is_full_screen().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.is_full_screen)
        });

        let s = self.clone();
        mw.expect_is_resizable().returning(move || {
            !s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.fixed_size)
        });

        let s = self.clone();
        mw.expect_border_radius().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .and_then(|w| w.border_radius)
        });

        // Fill in remaining defaults
        mw.expect_element()
            .return_const(Some(mock_element(id, ax_generation)));
        let s = self.clone();
        mw.expect_raise_without_focus()
            .returning(move || s.raise_without_focus(id));

        let s = self.clone();
        mw.expect_top_bar_height().returning(move || {
            let mut inner = s.inner.force_write();
            let window = inner
                .windows
                .get_mut(&id)
                .ok_or_else(|| Error::NotFound(format!("window {id} is gone")))?;
            window.top_bar_reads += 1;
            if window.top_bar_unreadable {
                return Err(Error::Generic(format!("window {id} is unreadable")));
            }
            Ok(window.top_bar_height)
        });

        let s = self.clone();
        mw.expect_control_at().returning(move |point| {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.controls.iter().any(|control| control.contains(point))))
        });

        // Focusing without raising still moves the OS focus, it just leaves the
        // window order alone. Only the app's idea of its focused window changes
        // here: the notification that follows in the real thing is what
        // `focus_window` models, and callers of this already act on their own
        // request. Dropping even this much left the app reporting the previous
        // window, which made `window_focused_trigger` discard events for the
        // window that had actually been focused.
        let s = self.clone();
        mw.expect_focus_without_raise()
            .returning(move |_psn, _focused, _focused_psn| {
                s.set_focused_window(id);
            });
        mw.expect_set_padding().return_const(());

        Window::new(Box::new(mw))
    }

    pub fn create_application(&self, pid: Pid) -> Application {
        let mut ma = MockApplicationApi::new();
        let s = self.clone();

        ma.expect_pid().return_const(pid);
        ma.expect_psn()
            .returning(move || s.inner.force_read().apps.get(&pid).map(|a| a.psn).unwrap());

        let s = self.clone();
        ma.expect_focused_window_id().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .and_then(|a| a.focused_window_id)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        ma.expect_focused_window().returning(move |_config| {
            let focused_id = {
                let inner = s.inner.force_read();
                let focused_id = inner.apps.get(&pid).and_then(|a| a.focused_window_id)?;
                inner
                    .windows
                    .contains_key(&focused_id)
                    .then_some(focused_id)
            }?;
            Some(s.create_window(focused_id))
        });

        let s = self.clone();
        ma.expect_bundle_id().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .map(|a| a.bundle_id.clone())
        });

        let name = self
            .inner
            .force_read()
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap();
        ma.expect_name().return_const(name);

        let s = self.clone();
        ma.expect_is_frontmost().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .is_some_and(|a| a.is_frontmost)
        });

        let s = self.clone();
        ma.expect_connection().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .and_then(|a| a.connection)
        });

        ma.expect_observe().returning(|| Ok(true));
        ma.expect_observe_window().returning(|_| Ok(true));
        ma.expect_unobserve_window().return_const(());
        let s = self.clone();
        let window_ids = move || {
            let inner = s.inner.force_read();
            inner
                .windows
                .values()
                .filter(|w| w.pid == pid && !w.ax_unlisted)
                .map(|w| w.id)
                .chain(
                    inner
                        .stale_window_ids
                        .iter()
                        .filter(|&(_, &owner)| owner == pid)
                        .map(|(&id, _)| id),
                )
                .collect::<Vec<_>>()
        };

        let (s, ids) = (self.clone(), window_ids.clone());
        ma.expect_window_list().returning(move |_| {
            *s.inner
                .force_write()
                .window_list_reads
                .entry(pid)
                .or_default() += 1;
            ids().into_iter().map(|id| s.create_window(id)).collect()
        });

        let (s, ids) = (self.clone(), window_ids.clone());
        ma.expect_ax_window_ids().returning(move || {
            let background = s.inner.force_read().background_tabs.clone();
            ids()
                .into_iter()
                .filter(|id| !background.contains(id))
                .collect()
        });

        Application::new(Box::new(ma))
    }

    #[allow(clippy::too_many_lines)]
    pub fn create_window_manager(&self) -> MockWindowManagerApi {
        let mut wm = MockWindowManagerApi::new();

        wm.expect_dim_windows().returning(|_, _| ());

        let s = self.clone();
        wm.expect_window_ordered_in().returning(move |window_id| {
            let inner = s.inner.force_read();
            Ok(inner.windows.contains_key(&window_id)
                && !inner.unordered_windows.contains(&window_id))
        });

        let s = self.clone();
        wm.expect_active_display_id()
            .returning(move || Ok(s.inner.force_read().active_display_id));

        let s = self.clone();
        wm.expect_active_display_space().returning(move |id| {
            s.inner
                .force_read()
                .displays
                .get(&id)
                .map(|d| d.workspaces[0])
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        wm.expect_is_fullscreen_space()
            .returning(move |display_id| {
                let inner = s.inner.force_read();
                inner
                    .displays
                    .get(&display_id)
                    .and_then(|display| display.workspaces.first())
                    .is_some_and(|workspace_id| inner.fullscreen_spaces.contains(workspace_id))
            });

        let s = self.clone();
        wm.expect_present_displays().returning(move || {
            s.inner
                .force_read()
                .displays
                .values()
                .map(|d| {
                    (
                        Display::new(d.id, d.bounds, TEST_MENUBAR_HEIGHT),
                        d.workspaces.clone(),
                    )
                })
                .collect()
        });

        let s = self.clone();
        wm.expect_find_existing_application_windows()
            .returning(move |app, spaces, _config| {
                let pid = app.pid();
                let mut ids = s
                    .inner
                    .force_read()
                    .windows
                    .values()
                    .filter_map(|w| {
                        (w.pid == pid && spaces.contains(&w.workspace_id)).then_some(w.id)
                    })
                    .collect::<Vec<_>>();
                ids.sort_unstable();
                let windows = ids.into_iter().map(|id| s.create_window(id)).collect();
                Ok((windows, vec![]))
            });

        let s = self.clone();
        wm.expect_windows_in_workspace()
            .returning(move |workspace_id| {
                let mut inner = s.inner.force_write();
                *inner.membership_reads.entry(workspace_id).or_default() += 1;
                if let Some(failures) = inner.membership_failures.get_mut(&workspace_id)
                    && *failures > 0
                {
                    *failures -= 1;
                    return Err(Error::InvalidWindow);
                }
                let mut windows = inner
                    .windows
                    .values()
                    .filter_map(|w| {
                        (w.on_all_spaces || w.workspace_id == workspace_id).then_some(w.id)
                    })
                    .collect::<Vec<_>>();
                // Sort the windows to keep the tests consistent
                windows.sort_unstable();
                Ok(windows)
            });

        let s = self.clone();
        wm.expect_windows_on_screen().returning(move || {
            s.show_latest_raise();
            let inner = s.inner.force_read();
            let mut windows = inner
                .windows
                .iter()
                .filter_map(|(id, window)| window.visible.then_some(id))
                .copied()
                .collect::<Vec<_>>();
            // Front to back, as the window server lists them.
            windows.sort_by_key(|id| {
                let raised = inner.raised.iter().position(|raised| raised == id);
                std::cmp::Reverse((raised, std::cmp::Reverse(*id)))
            });
            Some(windows)
        });

        let s = self.clone();
        wm.expect_all_window_ids()
            .returning(move || s.inner.force_read().windows.keys().copied().collect());

        let s = self.clone();
        wm.expect_warp_mouse()
            .returning(move |origin| s.inner.force_write().cursor_position = origin);

        let s = self.clone();
        wm.expect_focus_desktop().returning(move |bounds| {
            s.inner.force_write().focused_desktop = Some(bounds);
            Ok(())
        });

        let s = self.clone();
        wm.expect_cursor_position()
            .returning(move || Some(origin_to(s.inner.force_read().cursor_position)));

        let s = self.clone();
        wm.expect_left_button_down()
            .returning(move || s.inner.force_read().left_button_down);

        wm.expect_get_associated_windows().return_const(vec![]);

        let s = self.clone();
        wm.expect_flush_window_writes()
            .returning(move |timeout| s.flush_writes(timeout));

        let s = self.clone();
        wm.expect_find_window_at_point().returning(move |at_point| {
            let point = origin_from(*at_point);
            top_window_at(&s.inner.force_read(), point)
                .ok_or(Error::NotFound(format!("no window found at point {point}")))
        });

        wm
    }

    pub fn create_process(&self, pid: Pid) -> MockProcessApi {
        let mut mp = MockProcessApi::new();
        let s = self.clone();

        let name = self
            .inner
            .force_read()
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap();
        mp.expect_name().return_const(name);

        mp.expect_pid().return_const(pid);
        mp.expect_psn()
            .returning(move || s.inner.force_read().apps.get(&pid).map(|a| a.psn).unwrap());
        mp.expect_is_observable().returning(|| true);
        mp.expect_application().return_const(None);
        mp.expect_ready().return_const(true);
        mp.expect_force_manage().return_const(());

        mp
    }
}

/// A stand-in AX element: one per window and element generation, equal only
/// to another for the same pair, as `CFEqual` treats real elements.
fn mock_element(id: WinID, generation: u32) -> CFRetained<AXUIWrapper> {
    let number = CFNumber::new_i64(i64::from(id) << 32 | i64::from(generation));
    unsafe { CFRetained::cast_unchecked(number) }
}
