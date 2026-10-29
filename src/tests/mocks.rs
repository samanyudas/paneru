use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGDirectDisplayID;
use stdext::prelude::RwLockExt;

use crate::errors::Error;
use crate::events::{DestroySource, Event};
use crate::manager::app::MockApplicationApi;
use crate::manager::window_writer::{
    Answers, AppWriter, AxFrame, FrameTarget, FrameWrite, WindowWriters, WindowWrites,
};
use crate::manager::{
    Application, Display, MockProcessApi, MockWindowApi, MockWindowManagerApi, Origin, Size,
    Window, origin_from, origin_to,
};
use crate::platform::{Modifiers, Pid, ProcessSerialNumber, WinID, WorkspaceId};

use super::*;

/// Data for a mocked application.
pub(crate) struct MockAppData {
    pub(crate) psn: ProcessSerialNumber,
    pub(crate) bundle_id: String,
    pub(crate) name: String,
    pub(crate) focused_window_id: Option<WinID>,
    pub(crate) is_frontmost: bool,
    pub(crate) connection: Option<crate::platform::ConnID>,
}

/// Data for a mocked window.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct MockWindowData {
    pub(crate) id: WinID,
    pub(crate) pid: Pid,
    pub(crate) frame: IRect,
    pub(crate) title: String,
    pub(crate) minimized: bool,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) visible: bool,
    pub(crate) role: String,
    pub(crate) subrole: String,
    pub(crate) identifier: String,
    pub(crate) is_full_screen: bool,
    pub(crate) border_radius: Option<f64>,
    pub(crate) horizontal_padding: i32,
    pub(crate) vertical_padding: i32,
    pub(crate) child_role: bool,
    /// The narrowest width the app accepts, like an `NSWindow.minSize`.
    pub(crate) min_width: i32,
    /// The widest width the app accepts, or no limit when zero.
    pub(crate) max_width: i32,
}

impl Default for MockWindowData {
    fn default() -> Self {
        Self {
            id: 0,
            pid: 0,
            frame: IRect::default(),
            title: String::new(),
            minimized: false,
            workspace_id: 0,
            visible: true,
            role: "AXWindow".to_string(),
            subrole: "AXStandardWindow".to_string(),
            identifier: "testid".to_string(),
            is_full_screen: false,
            border_radius: None,
            horizontal_padding: 0,
            vertical_padding: 0,
            child_role: false,
            min_width: 0,
            max_width: 0,
        }
    }
}

/// Data for a mocked display.
struct MockDisplayData {
    id: u32,
    bounds: IRect,
    workspaces: Vec<WorkspaceId>,
}

/// Whether a mocked app is answering window writes, to model one that is
/// slow or stuck.
#[derive(Default)]
struct WriteHold {
    /// The app has yet to take a write.
    held: bool,
    /// The app takes each write and reports it, then keeps the writer
    /// waiting, as when the writer reads the frame back after a resize.
    read_back_held: bool,
    /// The next frame read that gives up waiting for a write releases the
    /// app, and returns once the write has landed.
    land_as_read_gives_up: bool,
    /// Holds that ran out with nobody releasing them.
    expired_holds: usize,
    /// Frame reads begun while a write to the same window was still queued.
    reads_behind_writes: usize,
}

/// A held write gives up after this long, so a test that waits on one fails
/// instead of hanging.
const WRITE_HOLD_LIMIT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct WriteHolds {
    apps: Mutex<HashMap<Pid, WriteHold>>,
    released: Condvar,
}

/// The internal state of our "Virtual macOS".
struct MockStateInner {
    apps: HashMap<Pid, MockAppData>,
    windows: HashMap<WinID, MockWindowData>,
    displays: HashMap<u32, MockDisplayData>,
    fullscreen_spaces: HashSet<WorkspaceId>,
    active_display_id: u32,
    cursor_position: Origin,
    event_queue: VecDeque<Event>,
    /// Windows that are gone but which the app's AX window list still reports,
    /// modelling the lag real apps show right after a window closes.
    stale_window_ids: HashMap<WinID, Pid>,
    unordered_windows: HashSet<WinID>,
    /// Windows the app keeps out of its accessibility window list while the
    /// window server still reports them on screen: a background native tab.
    background_tabs: HashSet<WinID>,
    /// Windows raised without focus, in order, so a test can check what ends
    /// up in front of what.
    raised: Vec<WinID>,
    /// How many times each app has been asked for its focused window or its
    /// window list, so a test can check who a lookup bothers.
    app_reads: HashMap<Pid, usize>,
}

#[derive(Clone)]
pub struct MockState {
    inner: Arc<RwLock<MockStateInner>>,
    /// Kept outside `inner` so a slow write never holds the lock every other
    /// mock call needs.
    write_holds: Arc<WriteHolds>,
    /// Writer threads for apps that were ever held, as `WindowOS` has for
    /// every app.
    writers: Arc<WindowWriters<MockWriteTarget>>,
    /// Keeps each writer alive, as a `WindowOS` handle does.
    live_writers: Arc<Mutex<HashMap<Pid, Arc<AppWriter<MockWriteTarget>>>>>,
    views: Arc<Mutex<HashMap<WinID, WindowView>>>,
}

/// One window as `WindowOS` and its writer see it, from where the app had it
/// when the engine first wrote or read it.
struct WindowView {
    /// The frame last asked for, which `WindowOS` keeps until the next read.
    asked: IRect,
    answers: Arc<Answers>,
    asked_origins: Vec<Origin>,
    /// Every origin the app was given, by the engine or by a staged resize.
    taken_origins: Vec<Origin>,
}

impl WindowView {
    fn new(frame: IRect) -> Self {
        Self {
            asked: frame,
            answers: Arc::new(Answers::new(frame)),
            asked_origins: Vec::new(),
            taken_origins: Vec::new(),
        }
    }
}

/// Lands a window's queued writes on the mock app.
#[derive(Clone)]
struct MockWriteTarget {
    state: MockState,
    window_id: WinID,
}

impl FrameTarget for MockWriteTarget {
    /// Lands the write the way `AxWindow` does. An app that was ever held
    /// also reports each change back, as real apps do, and reports nothing
    /// for a write that changes nothing.
    fn write(&self, write: FrameWrite) {
        let (state, id) = (&self.state, self.window_id);
        let Some(pid) = state.inner.force_read().windows.get(&id).map(|w| w.pid) else {
            return;
        };
        let echo = state.answer(pid, |app| &mut app.held);
        let Some(answers) = state.view(id, |view| Arc::clone(&view.answers)) else {
            return;
        };
        let Ok(before) = self.read_frame() else {
            return;
        };
        answers.land(self, write);
        let Ok(after) = self.read_frame() else {
            return;
        };
        if echo {
            let mut inner = state.inner.force_write();
            if after.min != before.min {
                inner
                    .event_queue
                    .push_back(Event::WindowMoved { window_id: id });
            }
            if after.size() != before.size() {
                inner
                    .event_queue
                    .push_back(Event::WindowResized { window_id: id });
            }
            drop(inner);
            state.answer(pid, |app| &mut app.read_back_held);
        }
    }
}

impl AxFrame for MockWriteTarget {
    fn set_position(&self, origin: Origin) {
        self.state
            .view(self.window_id, |view| view.taken_origins.push(origin));
        if let Some(w) = self
            .state
            .inner
            .force_write()
            .windows
            .get_mut(&self.window_id)
        {
            let size = w.frame.size();
            w.frame.min = origin;
            w.frame.max = origin + size;
        }
    }

    fn set_size(&self, size: Size) {
        if let Some(w) = self
            .state
            .inner
            .force_write()
            .windows
            .get_mut(&self.window_id)
        {
            let max_width = if w.max_width > 0 {
                w.max_width
            } else {
                i32::MAX
            };
            let width = size.x.max(w.min_width).min(max_width);
            w.frame.max = w.frame.min + size.with_x(width);
        }
    }

    fn read_frame(&self) -> crate::errors::Result<IRect> {
        self.state.read_app_frame(self.window_id)
    }
}

impl MockState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MockStateInner {
                apps: HashMap::new(),
                windows: HashMap::new(),
                displays: HashMap::new(),
                fullscreen_spaces: HashSet::new(),
                active_display_id: 0,
                cursor_position: Origin::ZERO,
                event_queue: VecDeque::new(),
                stale_window_ids: HashMap::new(),
                unordered_windows: HashSet::new(),
                background_tabs: HashSet::new(),
                raised: Vec::new(),
                app_reads: HashMap::new(),
            })),
            write_holds: Arc::default(),
            writers: Arc::default(),
            live_writers: Arc::default(),
            views: Arc::default(),
        }
    }

    fn view<R>(&self, id: WinID, f: impl FnOnce(&mut WindowView) -> R) -> Option<R> {
        let frame = self.read_app_frame(id).ok()?;
        let mut views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        Some(f(views.entry(id).or_insert_with(|| WindowView::new(frame))))
    }

    fn read_app_frame(&self, id: WinID) -> crate::errors::Result<IRect> {
        self.inner
            .force_read()
            .windows
            .get(&id)
            .map(|w| w.frame)
            .ok_or(Error::InvalidWindow)
    }

    /// Origins the app was given for the window that the engine never asked
    /// for, such as a staged resize's offscreen one.
    pub fn origins_not_asked(&self, id: WinID) -> Vec<Origin> {
        self.view(id, |view| {
            view.taken_origins
                .iter()
                .filter(|origin| !view.asked_origins.contains(origin))
                .copied()
                .collect()
        })
        .unwrap_or_default()
    }

    /// Marks `pid` as ever held, which threads its writes from then on.
    fn write_hold<R>(&self, pid: Pid, f: impl FnOnce(&mut WriteHold) -> R) -> R {
        f(self
            .write_holds
            .apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(pid)
            .or_default())
    }

    /// Makes `pid`'s window writes wait until [`Self::release_writes`], or
    /// until [`WRITE_HOLD_LIMIT`] runs out.
    pub fn hold_writes(&self, pid: Pid) {
        self.write_hold(pid, |app| app.held = true);
    }

    /// Makes `pid` take each window write and report it, then keep the
    /// writer waiting until [`Self::release_writes`].
    pub fn hold_read_back(&self, pid: Pid) {
        self.write_hold(pid, |app| app.read_back_held = true);
    }

    /// Makes the next read that gives up on one of `pid`'s writes release
    /// the app and let the write land before the read returns.
    pub fn land_as_read_gives_up(&self, pid: Pid) {
        self.write_hold(pid, |app| app.land_as_read_gives_up = true);
    }

    pub fn release_writes(&self, pid: Pid) {
        if let Some(app) = self
            .write_holds
            .apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&pid)
        {
            app.held = false;
            app.read_back_held = false;
        }
        self.write_holds.released.notify_all();
    }

    /// How many of `pid`'s held writes waited out [`WRITE_HOLD_LIMIT`].
    pub fn expired_write_holds(&self, pid: Pid) -> usize {
        self.write_hold_count(pid, |app| app.expired_holds)
    }

    /// How many frame reads of `pid`'s windows began while a write to the
    /// same window was still queued. Each of those waits on the app.
    pub fn reads_behind_writes(&self, pid: Pid) -> usize {
        self.write_hold_count(pid, |app| app.reads_behind_writes)
    }

    fn write_hold_count(&self, pid: Pid, count: fn(&WriteHold) -> usize) -> usize {
        self.write_holds
            .apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pid)
            .map_or(0, count)
    }

    /// Waits up to [`WRITE_HOLD_LIMIT`] for every queued write to land.
    pub fn wait_for_writes(&self) {
        assert!(
            self.writers.flush(WRITE_HOLD_LIMIT),
            "queued window writes never landed"
        );
    }

    /// The window's writes, threaded once its app was ever held, the way
    /// `WindowOS` threads every app's.
    fn window_writes(&self, id: WinID) -> Option<WindowWrites<MockWriteTarget>> {
        let pid = self.inner.force_read().windows.get(&id).map(|w| w.pid)?;
        let ever_held = self
            .write_holds
            .apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&pid);
        let writer = ever_held.then(|| {
            let writer = self.writers.for_app(pid);
            self.live_writers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(pid, Arc::clone(&writer));
            writer
        });
        let target = MockWriteTarget {
            state: self.clone(),
            window_id: id,
        };
        Some(WindowWrites::new(id, target, writer))
    }

    /// Waits for the window's queued writes the way `WindowOS::update_frame`
    /// does.
    fn wait_before_read(&self, id: WinID) -> crate::errors::Result<()> {
        let Some(writes) = self.window_writes(id) else {
            return Ok(());
        };
        let Some(pid) = self.inner.force_read().windows.get(&id).map(|w| w.pid) else {
            return Ok(());
        };
        if writes.pending() {
            self.write_hold(pid, |app| app.reads_behind_writes += 1);
        }
        let landed = writes.wait_for_landing();
        if landed.is_err()
            && self.write_hold(pid, |app| std::mem::take(&mut app.land_as_read_gives_up))
        {
            self.release_writes(pid);
            assert!(writes.wait_for_landing().is_ok(), "released write landed");
        }
        landed
    }

    /// Where the app has the window, which lags what the engine asked for
    /// while a write is still on its way.
    pub fn window_frame(&self, window_id: WinID) -> IRect {
        self.inner
            .force_read()
            .windows
            .get(&window_id)
            .map(|w| w.frame)
            .expect("finding window")
    }

    /// Blocks while `held` says `pid` is held, and returns whether `pid` was
    /// ever held.
    fn answer(&self, pid: Pid, held: fn(&mut WriteHold) -> &mut bool) -> bool {
        let holds = &self.write_holds;
        let mut apps = holds.apps.lock().unwrap_or_else(PoisonError::into_inner);
        if !apps.contains_key(&pid) {
            return false;
        }
        let deadline = Instant::now() + WRITE_HOLD_LIMIT;
        while let Some(app) = apps.get_mut(&pid)
            && *held(app)
        {
            let now = Instant::now();
            if now >= deadline {
                *held(app) = false;
                app.expired_holds += 1;
                break;
            }
            apps = holds
                .released
                .wait_timeout(apps, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Queues a write the way `WindowOS` does.
    fn write_frame(&self, id: WinID, origin: Option<Origin>, size: Option<Size>) {
        let Some(origin_before) = self.view(id, |view| {
            let before = view.asked;
            if let Some(origin) = origin {
                view.asked_origins.push(origin);
                view.asked = IRect::from_corners(origin, origin + before.size());
            }
            if let Some(size) = size {
                view.asked.max = view.asked.min + size;
            }
            before.min
        }) else {
            return;
        };
        if let Some(writes) = self.window_writes(id) {
            writes.push(FrameWrite {
                origin,
                size,
                origin_before,
            });
        }
    }

    /// The windows raised since this was last called, in the order they were.
    pub fn take_raised(&self) -> Vec<WinID> {
        std::mem::take(&mut self.inner.force_write().raised)
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

    /// How many times `pid` has been asked for its focused window or window
    /// list since this was last called.
    pub fn take_app_reads(&self, pid: Pid) -> usize {
        self.inner.force_write().app_reads.remove(&pid).unwrap_or(0)
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

    pub fn focus_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
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
            source: DestroySource::Accessibility,
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

    #[allow(unused)]
    pub fn simulate_click(&self, point: Origin) {
        let mut inner = self.inner.force_write();
        let point = CGPoint::new(point.x.into(), point.y.into());
        inner.event_queue.push_back(Event::MouseDown {
            point,
            modifiers: Modifiers::empty(),
        });
        inner.event_queue.push_back(Event::MouseUp {
            point,
            modifiers: Modifiers::empty(),
        });
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
        inner.event_queue.push_back(Event::MouseDown {
            point: start_p,
            modifiers: Modifiers::empty(),
        });
        inner.event_queue.push_back(Event::MouseDragged {
            point: end_p,
            modifiers: Modifiers::empty(),
        });
        inner.event_queue.push_back(Event::MouseUp {
            point: end_p,
            modifiers: Modifiers::empty(),
        });
    }

    pub fn cursor_position(&self) -> IVec2 {
        self.inner.force_read().cursor_position
    }

    // --- Mock Factory Methods ---

    #[allow(clippy::too_many_lines)]
    pub fn create_window(&self, id: WinID) -> Window {
        let mut mw = MockWindowApi::new();

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
            .returning(move |size| s.write_frame(id, None, Some(size)));

        let s = self.clone();
        mw.expect_reposition()
            .returning(move |origin| s.write_frame(id, Some(origin), None));

        let s = self.clone();
        mw.expect_focus_with_raise().returning(move |_psn| {
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
        mw.expect_update_frame().returning(move || {
            s.wait_before_read(id)?;
            let frame = s.read_app_frame(id)?;
            s.view(id, |view| {
                view.asked = frame;
                view.answers.record(frame);
            });
            Ok(frame)
        });

        let s = self.clone();
        mw.expect_write_pending()
            .returning(move || s.window_writes(id).is_some_and(|writes| writes.pending()));

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
                .map(|w| w.role.clone())
                .ok_or_else(|| crate::errors::Error::Generic(format!("window {id} not found")))?;
            if role != accessibility_sys::kAXUnknownRole {
                let _ = cached_role.set(role.clone());
            }
            Ok(role)
        });

        let s = self.clone();
        mw.expect_is_alive()
            .returning(move || s.inner.force_read().windows.contains_key(&id));

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
        mw.expect_border_radius().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .and_then(|w| w.border_radius)
        });

        // Fill in remaining defaults
        mw.expect_element().return_const(None);
        let s = self.clone();
        mw.expect_raise_without_focus()
            .returning(move || s.inner.force_write().raised.push(id));

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
        self.create_application_with_observer(pid, || Ok(true))
    }

    pub fn create_application_with_observer(
        &self,
        pid: Pid,
        observe: impl FnMut() -> crate::errors::Result<bool> + Send + 'static,
    ) -> Application {
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
            *s.inner.force_write().app_reads.entry(pid).or_default() += 1;
            let inner = s.inner.force_read();
            let focused_id = inner.apps.get(&pid).and_then(|a| a.focused_window_id)?;
            if inner.windows.contains_key(&focused_id) {
                Some(s.create_window(focused_id))
            } else {
                None
            }
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

        ma.expect_observe().returning(observe);
        ma.expect_observe_window().returning(|_| Ok(true));
        ma.expect_unobserve_window().return_const(());
        let s = self.clone();
        let window_ids = move || {
            let inner = s.inner.force_read();
            inner
                .windows
                .values()
                .filter(|w| w.pid == pid)
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
            *s.inner.force_write().app_reads.entry(pid).or_default() += 1;
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

        let s = self.clone();
        wm.expect_flush_window_writes().returning(move |timeout| {
            s.writers.flush(timeout);
        });

        let s = self.clone();
        wm.expect_window_is_unordered().returning(move |window_id| {
            let inner = s.inner.force_read();
            inner.unordered_windows.contains(&window_id) || !inner.windows.contains_key(&window_id)
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
                let mut windows = s
                    .inner
                    .force_read()
                    .windows
                    .values()
                    .filter_map(|w| {
                        (w.pid == pid && spaces.contains(&w.workspace_id))
                            .then_some(s.create_window(w.id))
                    })
                    .collect::<Vec<_>>();
                windows.sort_unstable_by_key(|window| window.id());
                Ok((windows, vec![]))
            });

        let s = self.clone();
        wm.expect_windows_in_workspace()
            .returning(move |workspace_id| {
                let mut windows = s
                    .inner
                    .force_read()
                    .windows
                    .values()
                    .filter_map(|w| (w.workspace_id == workspace_id).then_some(w.id))
                    .collect::<Vec<_>>();
                // Sort the windows to keep the tests consistent
                windows.sort_unstable();
                Ok(windows)
            });

        let s = self.clone();
        wm.expect_windows_on_screen().returning(move || {
            let windows = s
                .inner
                .force_read()
                .windows
                .iter()
                .filter_map(|(id, window)| window.visible.then_some(id))
                .copied()
                .collect::<Vec<_>>();
            Some(windows)
        });

        let s = self.clone();
        wm.expect_window_owner().returning(move |window_id| {
            let inner = s.inner.force_read();
            inner
                .windows
                .get(&window_id)
                .map(|w| w.pid)
                .or_else(|| inner.stale_window_ids.get(&window_id).copied())
        });

        let s = self.clone();
        wm.expect_warp_mouse()
            .returning(move |origin| s.inner.force_write().cursor_position = origin);

        let s = self.clone();
        wm.expect_cursor_position()
            .returning(move || Some(origin_to(s.inner.force_read().cursor_position)));

        wm.expect_get_associated_windows().return_const(vec![]);

        let s = self.clone();
        wm.expect_find_window_at_point().returning(move |at_point| {
            let point = origin_from(*at_point);
            s.inner
                .force_read()
                .windows
                .iter()
                .find_map(|(id, window)| window.frame.contains(point).then_some(id))
                .ok_or(Error::NotFound(format!("no window found at point {point}")))
                .copied()
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
