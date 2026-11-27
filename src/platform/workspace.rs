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
