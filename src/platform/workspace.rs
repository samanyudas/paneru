use core::ptr::NonNull;
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
};
use objc2_foundation::{
    NSDictionary, NSDistributedNotificationCenter, NSKeyValueChangeNewKey, NSNotification,
    NSNotificationCenter, NSNumber, NSObject, NSString,
};
use std::ffi::c_void;
use tracing::{debug, info, warn};

use crate::events::{Event, EventSender};
use crate::manager::Process;
use crate::platform::Pid;

#[derive(Debug, Clone)]
pub struct Ivars {
    events: EventSender,
}

fn notification_pid(notification: &NSNotification) -> Option<Pid> {
    let user_info = notification.userInfo()?;
    let value = unsafe { user_info.objectForKey(NSWorkspaceApplicationKey) }?;
    value
        .downcast_ref::<NSRunningApplication>()
        .map(NSRunningApplication::processIdentifier)
}

define_class!(
    // SAFETY:
    // - The superclass NSObject does not have any subclassing requirements.
    // - `Drop` only removes this observer from notification centers and does not
    //   retain or resurrect `self` during deallocation.
    #[unsafe(super(NSObject))]
    #[name = "Observer"]
    #[ivars = Ivars]
    #[derive(Debug)]
    pub struct WorkspaceObserver;

    impl WorkspaceObserver {
        #[unsafe(method(activeDisplayDidChange:))]
        fn display_changed(&self, _: &NSNotification) {
            _ = self.ivars().events.send(Event::DisplayChanged);
        }

        #[unsafe(method(activeSpaceDidChange:))]
        fn space_changed(&self, _: &NSNotification) {
            _ = self.ivars().events.send(Event::SpaceChanged);
        }

        #[unsafe(method(didHideApplication:))]
        fn application_hidden(&self, notification: &NSNotification) {
            if let Some(pid) = notification_pid(notification) {
                _ = self.ivars().events.send(Event::ApplicationHidden { pid });
            }
        }

        #[unsafe(method(didActivateApplication:))]
        fn application_activated(&self, notification: &NSNotification) {
            if let Some(pid) = notification_pid(notification) {
                _ = self.ivars().events.send(Event::ApplicationActivated { pid });
            }
        }

        #[unsafe(method(didDeactivateApplication:))]
        fn application_deactivated(&self, notification: &NSNotification) {
            if let Some(pid) = notification_pid(notification) {
                _ = self.ivars().events.send(Event::ApplicationDeactivated { pid });
            }
        }

        #[unsafe(method(didUnhideApplication:))]
        fn application_unhidden(&self, notification: &NSNotification) {
            if let Some(pid) = notification_pid(notification) {
                _ = self.ivars().events.send(Event::ApplicationVisible { pid });
            }
        }

        #[unsafe(method(didWake:))]
        fn system_woke(&self, notification: &NSObject) {
            let msg = Event::SystemWoke {
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didChangeMenuBarHiding:))]
        fn menubar_hidden(&self, notification: &NSObject) {
            let msg = Event::MenuBarHiddenChanged {
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didRestartDock:))]
        fn dock_restarted(&self, notification: &NSObject) {
            let msg = Event::DockDidRestart {
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        /// Called when a display's parameters change, including its usable
        /// area when the Dock shows, hides, moves, or resizes.
        #[unsafe(method(didChangeScreenParameters:))]
        fn screen_parameters_changed(&self, _notification: &NSObject) {
            _ = self.ivars().events.send(Event::ScreenParametersChanged);
        }

        #[unsafe(method(didChangeDockPref:))]
        fn dock_pref_changed(&self, notification: &NSObject) {
            let msg = Event::DockDidChangePref {
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didChangeTheme:))]
        fn theme_changed(&self, _: &NSNotification) {
            _ = self.ivars().events.send(Event::ThemeChanged);
        }

        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value_for_keypath(
            &self,
            key_path: &NSString,
            _object: &NSObject,
            change: &NSDictionary,
            context: *mut c_void,
        ) {
            let Some(process) =
                NonNull::new(context).map(|ptr| unsafe { ptr.cast::<Process>().as_mut() })
            else {
                warn!("null pointer passed as context");
                return;
            };

            let result = unsafe { change.objectForKey(NSKeyValueChangeNewKey) };
            let policy =
                result.and_then(|result| result.downcast_ref::<NSNumber>().map(NSNumber::intValue));

            match key_path.to_string().as_ref() {
                "finishedLaunching" => {
                    if policy.is_some_and(|value| value != 1) {
                        return;
                    }
                    process.unobserve_finished_launching();
                }
                "activationPolicy" => {
                    let Some(value) = policy else {
                        return;
                    };
                    if i32::try_from(process.policy.0).is_ok_and(|current| value == current) {
                        return;
                    }
                    process.policy = NSApplicationActivationPolicy(value as isize);
                    process.unobserve_activation_policy();
                }
                err => {
                    warn!("unknown key path {err:?}");
                    return;
                }
            }

            let msg = Event::ApplicationLaunched {
                psn: process.psn,
                observer: process.observer.clone(),
            };
            _ = self.ivars().events.send(msg);
            debug!("got {key_path:?} for {}", process.name);
        }
    }
);

impl WorkspaceObserver {
    pub(crate) fn new(events: EventSender) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { events });
        unsafe { msg_send![super(this), init] }
    }

    fn register_notifications(&self, center: &NSNotificationCenter, methods: &[(Sel, &str)]) {
        for (sel, name) in methods {
            debug!("registering {} with {name}", *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                center.addObserver_selector_name_object(self, *sel, Some(&notification_type), None);
            }
        }
    }

    pub(super) fn start(&self) {
        self.register_notifications(
            &NSWorkspace::sharedWorkspace().notificationCenter(),
            &[
                (
                    sel!(activeDisplayDidChange:),
                    "NSWorkspaceActiveDisplayDidChangeNotification",
                ),
                (
                    sel!(activeSpaceDidChange:),
                    "NSWorkspaceActiveSpaceDidChangeNotification",
                ),
                (
                    sel!(didActivateApplication:),
                    "NSWorkspaceDidActivateApplicationNotification",
                ),
                (
                    sel!(didDeactivateApplication:),
                    "NSWorkspaceDidDeactivateApplicationNotification",
                ),
                (
                    sel!(didHideApplication:),
                    "NSWorkspaceDidHideApplicationNotification",
                ),
                (
                    sel!(didUnhideApplication:),
                    "NSWorkspaceDidUnhideApplicationNotification",
                ),
                (sel!(didWake:), "NSWorkspaceDidWakeNotification"),
            ],
        );

        self.register_notifications(
            &NSDistributedNotificationCenter::defaultCenter(),
            &[
                (
                    sel!(didChangeMenuBarHiding:),
                    "AppleInterfaceMenuBarHidingChangedNotification",
                ),
                (
                    sel!(didChangeTheme:),
                    "AppleInterfaceThemeChangedNotification",
                ),
                (sel!(didChangeDockPref:), "com.apple.dock.prefchanged"),
            ],
        );

        self.register_notifications(
            &NSNotificationCenter::defaultCenter(),
            &[
                (
                    sel!(didRestartDock:),
                    "NSApplicationDockDidRestartNotification",
                ),
                (
                    sel!(didChangeScreenParameters:),
                    "NSApplicationDidChangeScreenParametersNotification",
                ),
            ],
        );
    }
}

impl Drop for WorkspaceObserver {
    fn drop(&mut self) {
        info!("deregistering callbacks.");
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .removeObserver(self);
            NSNotificationCenter::defaultCenter().removeObserver(self);
            NSDistributedNotificationCenter::defaultCenter().removeObserver(self);
        }
    }
}

#[cfg(test)]
mod delayed_readiness_tests {
    use super::*;
    use crate::ecs::BProcess;
    use crate::manager::{Application, WindowManager};
    use crate::tests::{TEST_WORKSPACE_ID, TestHarness};
    use std::time::Duration;

    #[test]
    fn readiness_notification_resumes_retained_process() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid = 43;
        harness.mock_state.spawn_app(pid, "org.gnu.Emacs", "Emacs");
        let process = harness.mock_state.create_process(pid);
        let psn = crate::manager::ProcessApi::psn(&process);
        // Initial readiness polling has expired, leaving its KVO observer alive.
        harness.world().spawn(BProcess(Box::new(process)));
        let mut wm = harness.mock_state.create_window_manager();
        let state = harness.mock_state.clone();
        wm.expect_new_application()
            .returning(move |process| Ok(state.create_application(process.pid())));
        harness.world().insert_resource(WindowManager(Box::new(wm)));
        harness.mock_state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            43,
            bevy::math::IRect::new(0, 0, 400, 600),
        );
        let (events, _receiver) = crate::events::EventSender::new();
        let observer = WorkspaceObserver::new(events);
        harness
            .world()
            .write_message(crate::events::Event::ApplicationLaunched { psn, observer });
        harness.advance(Duration::from_secs(2));
        let world = harness.world();
        assert!(
            world
                .query::<&Application>()
                .iter(world)
                .any(|app| app.pid() == pid),
            "late readiness must resume an existing process"
        );
        crate::tests::find_window_entity(43, harness.world());
    }

    /// An app that hid its Dock icon (Bitwarden in the menu bar) when Paneru
    /// started is kept and watched, and adopted once it turns Regular.
    #[test]
    fn app_without_dock_icon_at_startup_is_adopted_when_it_turns_regular() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut harness = TestHarness::new().with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid = 44;
        harness
            .mock_state
            .spawn_app(pid, "com.bitwarden.desktop", "Bitwarden");
        let regular = Arc::new(AtomicBool::new(false));
        let mut process = crate::manager::MockProcessApi::new();
        process.expect_pid().return_const(pid);
        let psn = crate::platform::ProcessSerialNumber {
            high: 0,
            low: pid.cast_unsigned(),
        };
        process.expect_psn().return_const(psn);
        process.expect_name().return_const("Bitwarden".to_string());
        process.expect_bundle_id().return_const(None);
        let flag = regular.clone();
        process
            .expect_is_observable()
            .returning(move || flag.load(Ordering::SeqCst));
        let flag = regular.clone();
        process
            .expect_ready()
            .returning(move || flag.load(Ordering::SeqCst));

        {
            let world = harness.world();
            let mut commands = world.commands();
            crate::ecs::systems::spawn_initial_process(
                BProcess(Box::new(process)),
                false,
                &mut commands,
            );
            world.flush();
        }
        let mut wm = harness.mock_state.create_window_manager();
        let state = harness.mock_state.clone();
        wm.expect_new_application()
            .returning(move |process| Ok(state.create_application(process.pid())));
        harness.world().insert_resource(WindowManager(Box::new(wm)));
        harness.mock_state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            44,
            bevy::math::IRect::new(0, 0, 400, 600),
        );
        harness.advance(Duration::from_secs(70));
        let world = harness.world();
        assert!(
            world
                .query::<&BProcess>()
                .iter(world)
                .any(|p| p.pid() == pid),
            "the process must be kept while it waits to turn Regular"
        );

        // The activation policy KVO fires as the app shows its Dock icon.
        regular.store(true, Ordering::SeqCst);
        let (events, _receiver) = crate::events::EventSender::new();
        let observer = WorkspaceObserver::new(events);
        harness
            .world()
            .write_message(crate::events::Event::ApplicationLaunched { psn, observer });
        harness.advance(Duration::from_secs(2));
        let world = harness.world();
        assert!(
            world
                .query::<&Application>()
                .iter(world)
                .any(|app| app.pid() == pid),
            "turning Regular must adopt the app"
        );
        crate::tests::find_window_entity(44, harness.world());
    }

    #[test]
    fn headless_process_is_despawned_after_recovery_window() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid: i32 = 45;
        let mut process = crate::manager::MockProcessApi::new();
        process.expect_pid().return_const(pid);
        process
            .expect_psn()
            .return_const(crate::platform::ProcessSerialNumber {
                high: 0,
                low: pid.cast_unsigned(),
            });
        process
            .expect_name()
            .return_const("headless-helper".to_string());
        process.expect_ready().returning(|| false);
        harness.world().spawn((
            BProcess(Box::new(process)),
            crate::ecs::systems::PendingWindowRecovery::default(),
        ));

        harness.advance(Duration::from_secs(10));
        let world = harness.world();
        assert!(
            world
                .query::<&BProcess>()
                .iter(world)
                .any(|p| p.pid() == pid),
            "process must remain retained during the recovery window"
        );

        harness.advance(Duration::from_secs(55));
        let world = harness.world();
        assert!(
            !world
                .query::<&BProcess>()
                .iter(world)
                .any(|p| p.pid() == pid),
            "headless process must be despawned once the recovery window expires"
        );
    }

    #[test]
    fn force_managed_app_missing_at_launch_is_adopted_when_application_appears() {
        use crate::config::{Config, MainOptions, WindowParams};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let mut rule = WindowParams::new(".*", Some("com.example.LateAccessory".to_string()));
        rule.manage = Some(true);
        let config: Config = (MainOptions::default(), vec![rule]).into();

        let mut harness = TestHarness::new().with_config(config).with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid: i32 = 47;
        harness
            .mock_state
            .spawn_app(pid, "com.example.LateAccessory", "LateAccessory");
        let mut wm = harness.mock_state.create_window_manager();
        let state = harness.mock_state.clone();
        wm.expect_new_application()
            .returning(move |process| Ok(state.create_application(process.pid())));
        harness.world().insert_resource(WindowManager(Box::new(wm)));

        // Simulate an unobservable (LSUIElement/Accessory) app whose NSRunningApplication
        // is initially missing at launch, so `bundle_id()` returns `None` until the first
        // `ready()` call resolves it.
        let resolved = Arc::new(AtomicBool::new(false));
        let forced = Arc::new(AtomicBool::new(false));
        let ready_calls = Arc::new(AtomicUsize::new(0));

        let mut process = crate::manager::MockProcessApi::new();
        process.expect_pid().return_const(pid);
        process
            .expect_psn()
            .return_const(crate::platform::ProcessSerialNumber {
                high: 0,
                low: pid.cast_unsigned(),
            });
        process
            .expect_name()
            .return_const("LateAccessory".to_string());
        let resolved_for_bundle = resolved.clone();
        process.expect_bundle_id().returning(move || {
            resolved_for_bundle
                .load(Ordering::SeqCst)
                .then(|| "com.example.LateAccessory".to_string())
        });
        let forced_for_setter = forced.clone();
        process.expect_force_manage().returning(move |value| {
            forced_for_setter.store(value, Ordering::SeqCst);
        });
        let resolved_for_ready = resolved.clone();
        let forced_for_ready = forced.clone();
        let calls_for_ready = ready_calls.clone();
        process.expect_ready().returning(move || {
            calls_for_ready.fetch_add(1, Ordering::SeqCst);
            resolved_for_ready.store(true, Ordering::SeqCst);
            // Accessory app is only ready once `force_manage(true)` has been applied.
            forced_for_ready.load(Ordering::SeqCst)
        });

        harness
            .world()
            .spawn((crate::ecs::FreshMarker, BProcess(Box::new(process))));

        // Advance only until the very first `add_launched_process` poll runs so
        // a later 200 ms poll cannot mask a missing same-tick re-check.
        for _ in 0..50 {
            harness.app.update();
            if ready_calls.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        let world = harness.world();
        assert!(
            forced.load(Ordering::SeqCst),
            "manage = true rule must re-run on the same poll after ready() resolves the application"
        );
        assert_eq!(
            ready_calls.load(Ordering::SeqCst),
            2,
            "accessory app must be adopted on the same poll tick once force_manage engages"
        );
        assert!(
            world
                .query::<&Application>()
                .iter(world)
                .any(|app| app.pid() == pid),
            "force-managed app whose NSRunningApplication appeared late must be adopted on that poll"
        );
    }
}
