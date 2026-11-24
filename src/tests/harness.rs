use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, TaskPoolBuilder};
use bevy::time::TimeUpdateStrategy;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::commands::register_commands;
use crate::config::Config;
use crate::ecs::display::DisplayEventsPlugin;
use crate::ecs::drag::DragEventsPlugin;
use crate::ecs::focus::FocusEventsPlugin;
use crate::ecs::layout::LayoutEventsPlugin;
use crate::ecs::mouse::MouseEventsPlugin;
use crate::ecs::pacing::ShownFrames;
use crate::ecs::params::FrameActivity;
use crate::ecs::scroll::ScrollEventsPlugin;
use crate::ecs::state::PaneruState;
use crate::ecs::workspace::WorkspaceEventsPlugin;
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, Initializing, LaunchWindows, MissionControlActive,
    SkipReshuffle, SpawnWindowTrigger, register_systems, register_triggers,
};
use crate::events::Event;
use crate::manager::{Window, WindowManager};
use crate::platform::{Pid, WinID, WorkspaceId};

use super::*;

/// Simulated frame length, chosen to sit just under
/// `LOOP_MAX_TIMEOUT_FRAME_ACTIVE_MS` — the interval `pump_events` actually
/// paces to once a frame is active, which is when the scroll integrator runs.
/// A coarser tick would land above that integrator's step cap and exercise
/// clamped physics production never sees.
const FRAME: Duration = Duration::from_millis(20);

/// Coarser tick used while the engine is idle (`!FrameActivity::mid_frame()` and
/// no pending mock events), staying under Bevy's default 250 ms `max_delta`
/// while avoiding dozens of no-op frames during multi-second settle windows.
const IDLE_FRAME: Duration = Duration::from_millis(200);

/// How much simulated time each command in [`TestHarness::run`] is given to
/// play out.
const COMMAND_WINDOW: Duration = Duration::from_millis(500);

type VerifierFunc = Box<dyn FnMut(&mut World, MockState)>;
pub(crate) struct TestHarness {
    pub(crate) app: App,
    pub(crate) mock_state: MockState,
    pub(crate) verifiers: HashMap<usize, VerifierFunc>,
    activity: SystemState<FrameActivity<'static, 'static>>,
}

impl TestHarness {
    pub(crate) fn new() -> Self {
        let pid = TEST_PROCESS_ID;
        let mut app = setup_world();
        let mut mock_state = MockState::new();

        // Setup default display
        mock_state.add_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID],
        );

        // Initialize Bevy with the mocked process and WM
        let world = app.world_mut();

        mock_state.spawn_app(pid, "test", "TestApp");

        let mock_process = mock_state.create_process(pid);
        let process_entity = world.spawn(BProcess(Box::new(mock_process))).id();

        let application = mock_state.create_application(pid);
        world.spawn((ExistingMarker, ChildOf(process_entity), application));

        let wm = mock_state.create_window_manager();
        world.insert_resource(WindowManager(Box::new(wm)));
        world.insert_resource(ShownFrames::inline(mock_state.shown_frames()));

        let activity = SystemState::new(app.world_mut());

        Self {
            app,
            mock_state,
            verifiers: HashMap::new(),
            activity,
        }
    }

    pub(crate) fn world(&mut self) -> &mut World {
        self.app.world_mut()
    }

    pub(crate) fn with_windows(mut self, count: i32) -> Self {
        let pid = TEST_PROCESS_ID;

        let windows = (0..count)
            .map(|i| {
                let win_id = i as WinID;
                let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
                self.mock_state
                    .spawn_window(pid, TEST_WORKSPACE_ID, win_id, frame)
            })
            .collect::<Vec<_>>();
        self.existed_at_launch(0..count);
        self.app.world_mut().trigger(SpawnWindowTrigger(windows));

        self
    }

    pub(crate) fn with_window<F>(mut self, id: WinID, f: F) -> Self
    where
        F: FnOnce(&mut MockWindowData),
    {
        let pid = TEST_PROCESS_ID;
        let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = self
            .mock_state
            .spawn_window(pid, TEST_WORKSPACE_ID, id, frame);
        self.mock_state.update_window(id, f);
        self.existed_at_launch([id]);
        self.app
            .world_mut()
            .trigger(SpawnWindowTrigger(vec![window]));
        self
    }

    /// Marks windows as already open when the engine started, which is what
    /// the `with_*window*` builders model.
    pub(crate) fn existed_at_launch(&mut self, ids: impl IntoIterator<Item = WinID>) {
        if let Some(mut launch) = self.app.world_mut().get_resource_mut::<LaunchWindows>() {
            launch.0.extend(ids);
        }
    }

    pub(crate) fn with_workspace_window<F>(
        mut self,
        id: WinID,
        workspace_id: WorkspaceId,
        f: F,
    ) -> Self
    where
        F: FnOnce(&mut MockWindowData),
    {
        let pid = TEST_PROCESS_ID;
        let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = self.mock_state.spawn_window(pid, workspace_id, id, frame);
        self.mock_state.update_window(id, f);
        self.existed_at_launch([id]);
        self.app
            .world_mut()
            .trigger(SpawnWindowTrigger(vec![window]));
        self
    }

    #[allow(unused)]
    pub(crate) fn with_focused_window(self, id: WinID) -> Self {
        self.mock_state.focus_window(id);
        self
    }

    pub(crate) fn with_display(
        mut self,
        id: u32,
        bounds: IRect,
        workspaces: Vec<WorkspaceId>,
    ) -> Self {
        self.mock_state.add_display(id, bounds, workspaces);
        self
    }

    #[allow(unused)]
    pub(crate) fn with_app<F>(mut self, pid: Pid, bundle_id: &str, name: &str, f: F) -> Self
    where
        F: FnOnce(&mut MockAppData),
    {
        self.mock_state.spawn_app(pid, bundle_id, name);
        self.mock_state.update_app(pid, f);

        let world = self.app.world_mut();
        let mock_process = self.mock_state.create_process(pid);
        let process_entity = world.spawn(BProcess(Box::new(mock_process))).id();

        let application = self.mock_state.create_application(pid);
        world.spawn((ExistingMarker, ChildOf(process_entity), application));

        self
    }

    pub(crate) fn with_config(mut self, config: Config) -> Self {
        self.app.world_mut().insert_resource(config);
        self
    }

    pub(crate) fn with_workspace_popup(self) -> Self {
        self.with_config(
            Config::try_from("[decorations]\nworkspace_popup_status = true\n[bindings]\n")
                .expect("config should parse"),
        )
    }

    pub(crate) fn with_state(mut self, state: PaneruState) -> Self {
        self.app.world_mut().insert_resource(state);
        self
    }

    pub(crate) fn on_iteration<F>(mut self, iteration: usize, verifier: F) -> Self
    where
        F: FnMut(&mut World, MockState) + 'static,
    {
        self.verifiers.insert(iteration, Box::new(verifier));
        self
    }

    fn is_mid_frame(&mut self) -> bool {
        self.activity
            .get(self.app.world())
            .expect("frame activity")
            .mid_frame()
    }

    /// Runs the app for `duration` of simulated time, draining the mock
    /// platform's events each frame just as [`Self::run`] does.
    ///
    /// Waits on a deadline — a grace timer expiring, an animation reaching its
    /// target — have to be expressed in time, not in a frame count: a frame is
    /// [`FRAME`] long, so a bare `for _ in 0..10 { app.update() }` silently
    /// changes how long it waits whenever that constant is retuned.
    ///
    /// While the engine has active frame work or mock events arriving, each
    /// step uses [`FRAME`] so animations and physics match production. Once two
    /// consecutive frames finish idle, remaining time steps in [`IDLE_FRAME`]
    /// chunks (still under Bevy's 250 ms `max_delta`), mirroring how production
    /// `pump_events` backs off when `!FrameActivity::mid_frame()`.
    pub(crate) fn advance(&mut self, duration: Duration) {
        const TAIL_FRAMES: u32 = 3;

        let frames = duration.as_millis().div_ceil(FRAME.as_millis());
        let base_step = match *self.app.world().resource::<TimeUpdateStrategy>() {
            TimeUpdateStrategy::ManualDuration(d) => Some(d),
            _ => None,
        };
        // Tests that explicitly override `TimeUpdateStrategy` expect every
        // frame to advance with their configured delta untouched.
        if base_step != Some(FRAME) {
            for _ in 0..frames {
                self.app.update();
                for event in self.mock_state.drain_events() {
                    self.app.world_mut().write_message::<Event>(event);
                }
            }
            return;
        }

        let tail = FRAME * TAIL_FRAMES;
        let total_ms = u64::try_from(frames * FRAME.as_millis()).expect("duration fits in u64 ms");
        let mut remaining = Duration::from_millis(total_ms);
        let mut current_step = FRAME;
        let mut idle_streak: u8 = 0;

        while remaining > Duration::ZERO {
            let step = if idle_streak >= 2 {
                remaining
                    .checked_sub(tail)
                    .filter(|excess| !excess.is_zero())
                    .map_or_else(|| remaining.min(FRAME), |excess| excess.min(IDLE_FRAME))
            } else {
                remaining.min(FRAME)
            };
            if current_step != step {
                *self.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                    TimeUpdateStrategy::ManualDuration(step);
                current_step = step;
            }

            self.app.update();

            let events = self.mock_state.drain_events();
            let had_events = !events.is_empty();
            for event in events {
                self.app.world_mut().write_message::<Event>(event);
            }

            remaining = remaining.saturating_sub(step);
            if had_events || self.is_mid_frame() {
                idle_streak = 0;
            } else {
                idle_streak = idle_streak.saturating_add(1);
            }
        }

        if current_step != FRAME {
            *self.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                TimeUpdateStrategy::ManualDuration(FRAME);
        }
    }

    pub(crate) fn run(&mut self, commands: Vec<Event>) {
        if let Some(step) = self
            .verifiers
            .keys()
            .copied()
            .filter(|step| *step >= commands.len())
            .min()
        {
            panic!(
                "verifier at iteration {step} never runs: there are only {} commands",
                commands.len()
            );
        }
        for (iteration, command) in commands.into_iter().enumerate() {
            self.app.world_mut().write_message::<Event>(command);
            self.advance(COMMAND_WINDOW);

            if let Some(mut verifier) = self.verifiers.remove(&iteration) {
                verifier(self.app.world_mut(), self.mock_state.clone());
            }
        }
    }
}

fn setup_world() -> App {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        _ = tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(
                fmt::layer()
                    .with_level(true)
                    .with_line_number(true)
                    .with_file(true)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_writer(std::io::stderr)
                    .compact(),
            )
            .try_init();

        let _pool = AsyncComputeTaskPool::get_or_init(|| {
            TaskPoolBuilder::new()
                .num_threads(1) // Keep it light for tests
                .build()
        });
        assert!(AsyncComputeTaskPool::try_get().is_some());
    });
    let mut bevy_app = App::new();
    bevy_app
        .add_plugins(MinimalPlugins)
        .add_message::<Event>()
        .insert_resource(SkipReshuffle(false))
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(Config::default())
        .insert_resource(Initializing)
        .insert_resource(LaunchWindows::default())
        .add_plugins(MouseEventsPlugin)
        .add_plugins(DragEventsPlugin)
        .add_plugins(crate::ecs::maximize::MaximizePlugin)
        .add_plugins(ScrollEventsPlugin)
        .add_plugins(WorkspaceEventsPlugin)
        .add_plugins(LayoutEventsPlugin)
        .add_plugins(FocusEventsPlugin)
        .add_plugins(DisplayEventsPlugin)
        .add_plugins((register_triggers, register_systems, register_commands));

    bevy_app.insert_resource(TimeUpdateStrategy::ManualDuration(FRAME));

    crate::ecs::run_frame_schedules_inline(&mut bevy_app);

    bevy_app
}

pub(crate) fn find_window_entity(window_id: WinID, world: &mut World) -> Entity {
    let mut query = world.query::<(&Window, Entity)>();
    query
        .iter(world)
        .find(|(w, _)| w.id() == window_id)
        .map_or_else(|| panic!("window {window_id} not found"), |(_, e)| e)
}

#[macro_export]
macro_rules! assert_window_at {
    ($world:expr, $id:expr, $x:expr, $y:expr) => {{
        let mut query = $world.query::<&$crate::manager::Window>();
        let window = query
            .iter($world)
            .find(|w| w.id() == $id)
            .expect("window not found");
        assert_eq!(
            window.frame().min.x,
            $x,
            "window {} x position mismatch",
            $id
        );
        assert_eq!(
            window.frame().min.y,
            $y,
            "window {} y position mismatch",
            $id
        );
    }};
}

#[macro_export]
macro_rules! assert_window_size {
    ($world:expr, $id:expr, $w:expr, $h:expr) => {{
        let mut query = $world.query::<&$crate::manager::Window>();
        let window = query
            .iter($world)
            .find(|w| w.id() == $id)
            .expect("window not found");
        let frame = window.frame();
        assert_eq!(frame.width(), $w, "window {} width mismatch", $id);
        assert_eq!(frame.height(), $h, "window {} height mismatch", $id);
    }};
}

#[macro_export]
macro_rules! assert_focused {
    ($world:expr, $id:expr) => {{
        let mut query = $world.query::<(
            &$crate::manager::Window,
            bevy::ecs::query::Has<$crate::ecs::FocusedMarker>,
        )>();
        let (_, focused) = query
            .iter($world)
            .find(|(w, _)| w.id() == $id)
            .expect("window not found");
        assert!(focused, "window {} should be focused", $id);
    }};
}

#[macro_export]
macro_rules! assert_on_workspace {
    ($world:expr, $window_id:expr, $workspace_id:expr) => {{
        let entity = $crate::tests::harness::find_window_entity($window_id, $world);
        let mut query = $world.query::<&$crate::ecs::layout::LayoutStrip>();
        let found = query
            .iter($world)
            .any(|strip| strip.id() == $workspace_id && strip.index_of(entity).is_ok());
        assert!(
            found,
            "window {} should be on workspace {}",
            $window_id, $workspace_id
        );
    }};
}

#[macro_export]
macro_rules! assert_not_on_workspace {
    ($world:expr, $window_id:expr, $workspace_id:expr) => {{
        let entity = $crate::tests::harness::find_window_entity($window_id, $world);
        let mut query = $world.query::<&$crate::ecs::layout::LayoutStrip>();
        let found = query
            .iter($world)
            .any(|strip| strip.id() == $workspace_id && strip.index_of(entity).is_ok());
        assert!(
            !found,
            "window {} should NOT be on workspace {}",
            $window_id, $workspace_id
        );
    }};
}
