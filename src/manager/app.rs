use accessibility_sys::{
    AXObserverRef, AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementRef,
    AXUIElementSetMessagingTimeout, kAXErrorCannotComplete, kAXErrorSuccess, kAXRoleAttribute,
};
use bevy::ecs::component::Component;
use core::ptr::NonNull;
use crossbeam_channel::Sender;
use derive_more::{DerefMut, with_trait::Deref};
use mockall::automock;
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{CFRetained, CFString, CFType, kCFRunLoopCommonModes};
use std::ffi::c_void;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock, RwLock};
use std::thread;
use stdext::sync::rw_lock::RwLockExt;

use stdext::function_name;
use tracing::{debug, error};

use super::skylight::_SLPSGetFrontProcess;
use super::window_writer::Answer;
use super::windows::ask_app;
use super::{AXUIElementCopyAttributeValue, ProcessApi, Window, WindowOS, ax_window_id};
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::{DestroySource, Event, EventSender};
use crate::platform::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, ConnID,
    Pid, ProcessSerialNumber, WinID,
};
use crate::util::{AXUIAttributes, AXUIWrapper, MacResult, add_run_loop, remove_run_loop};

/// How long any one accessibility call may wait on the app that owns the
/// element before giving up. See [`ApplicationOS::new`] for why this is set at
/// all; the default is six seconds.
const AX_MESSAGING_TIMEOUT_SEC: f32 = 0.25;

/// Applies [`AX_MESSAGING_TIMEOUT_SEC`] to the process rather than to one
/// element, and must run before any application is observed.
///
/// The call in [`ApplicationOS::new`] binds the element it is given. Per Apple's
/// `AXUIElement.h` the system-wide element is the one that carries a
/// process-global default, and an element left at zero "uses the current global
/// timeout value". Window elements arrive from `AXUIElementCopyAttributeValue`
/// and are never handed to `AXUIElementSetMessagingTimeout`, so without this
/// they wait out the six second default instead.
pub(crate) fn bound_ax_messaging_timeout() -> Result<()> {
    let system_wide = AXUIWrapper::from_retained(unsafe { AXUIElementCreateSystemWide() })?;
    unsafe { AXUIElementSetMessagingTimeout(system_wide.as_ptr(), AX_MESSAGING_TIMEOUT_SEC) }
        .to_result(function_name!())
}

/// Whether the app with `pid` answers an Accessibility question within
/// [`AX_MESSAGING_TIMEOUT_SEC`]. Blocks for up to that long, so it runs on a
/// worker thread, as `bruteforce_windows` does.
pub(crate) fn answers_accessibility(pid: Pid) -> bool {
    objc2::rc::autoreleasepool(|_| {
        let Ok(element) = AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(pid) })
        else {
            // Not a busy app, so let the launch path report the real error.
            return true;
        };
        unsafe { AXUIElementSetMessagingTimeout(element.as_ptr(), AX_MESSAGING_TIMEOUT_SEC) };
        let role = CFString::from_static_str(kAXRoleAttribute);
        let mut value: *mut CFType = null_mut();
        let result = unsafe { AXUIElementCopyAttributeValue(element.as_ptr(), &role, &mut value) };
        if let Some(value) = NonNull::new(value) {
            drop(unsafe { CFRetained::from_raw(value) });
        }
        result != kAXErrorCannotComplete
    })
}

/// A static `LazyLock` that holds a list of `AXNotification` strings to be observed for application-level events.
/// These notifications are general events related to an application's lifecycle and state changes,
/// such as a new window being created, the focused window changing, or a menu being opened/closed.
pub static AX_NOTIFICATIONS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        accessibility_sys::kAXCreatedNotification,
        accessibility_sys::kAXFocusedWindowChangedNotification,
        accessibility_sys::kAXFocusedUIElementChangedNotification,
        accessibility_sys::kAXWindowMovedNotification,
        accessibility_sys::kAXWindowResizedNotification,
        accessibility_sys::kAXMenuOpenedNotification,
        accessibility_sys::kAXMenuClosedNotification,
    ]
});

/// A static `LazyLock` that holds a list of `AXNotification` strings to be observed for window-specific events.
/// These notifications are related to individual window lifecycle events,
/// such as a window being destroyed, miniaturized (minimized), or deminiaturized (restored).
pub static AX_WINDOW_NOTIFICATIONS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        accessibility_sys::kAXUIElementDestroyedNotification,
        accessibility_sys::kAXWindowMiniaturizedNotification,
        accessibility_sys::kAXWindowDeminiaturizedNotification,
        // Observed per window, not per application: on the app observer this
        // notification's element carries no resolvable window id, which left
        // `WindowOS::title` caching a title it was never told to invalidate.
        accessibility_sys::kAXTitleChangedNotification,
    ]
});

/// The windows an app listed over Accessibility.
pub struct WindowList {
    pub windows: Vec<Window>,
    /// The app answered for the list and for every window in it. When it did
    /// not, a later list can hold windows that this one missed.
    pub complete: bool,
}

#[automock]
pub trait ApplicationApi: Send + Sync {
    fn pid(&self) -> Pid;
    fn psn(&self) -> ProcessSerialNumber;
    fn connection(&self) -> Option<ConnID>;
    fn focused_window_id(&self) -> Result<WinID>;
    /// Asks for the ID of the app's focused window without waiting on the
    /// app, after the moves, resizes, and raise asked for its windows so
    /// far. A busy app can take the whole Accessibility timeout to answer,
    /// and a frame that waited on it stopped every window.
    fn ask_focused_window_id(&self) -> Answer<Result<WinID>>;
    fn focused_window(&self, config: &Config) -> Option<Window>;
    fn window_list(&self, config: &Config) -> WindowList;
    /// Returns the IDs of the windows the app currently exposes over
    /// accessibility.
    ///
    /// A background native tab is missing from this list while the window
    /// server still reports it on screen, which is what tells the two apart.
    fn ax_window_ids(&self) -> Vec<WinID>;
    /// Starts observing application-level accessibility notifications.
    ///
    /// Returns `Ok(true)` when every notification is registered, `Ok(false)`
    /// when the app did not answer in time so a later call can succeed, and
    /// `Err` when the app refused every registration.
    fn observe(&mut self) -> Result<bool>;
    fn observe_window(&mut self, window: &Window) -> Result<bool>;
    fn unobserve_window(&mut self, window: &Window);
    fn is_frontmost(&self) -> bool;
    /// Checks if the user hid the application, as with Command-H.
    fn is_hidden(&self) -> bool;
    fn bundle_id(&self) -> Option<String>;
    fn name(&self) -> &str;
}

#[derive(Component, Deref, DerefMut)]
pub struct Application(Box<dyn ApplicationApi>);

impl std::fmt::Debug for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app (pid {})", self.pid())
    }
}

impl std::fmt::Display for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app (pid {})", self.pid())
    }
}

impl Application {
    pub fn new(app: Box<dyn ApplicationApi>) -> Self {
        Application(app)
    }
}

pub struct ApplicationOS {
    element: CFRetained<AXUIWrapper>,
    psn: ProcessSerialNumber,
    pid: Pid,
    connection: Option<ConnID>,
    handler: AxObserverHandler,
    bundle_id: Option<String>,
    name: String,
}

impl Drop for ApplicationOS {
    fn drop(&mut self) {
        self.handler
            .remove_observer(&ObserverType::Application, &self.element, &AX_NOTIFICATIONS);
    }
}

impl std::fmt::Display for ApplicationOS {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app '{}' (pid {})", self.name, self.pid)
    }
}

impl ApplicationOS {
    pub fn new(
        connection: Option<ConnID>,
        process: &dyn ProcessApi,
        events: &EventSender,
        resolver: Sender<PendingNotification>,
    ) -> Result<Self> {
        let refer = unsafe {
            let ptr = AXUIElementCreateApplication(process.pid());
            // Without this, a synchronous AX call to an app that stops servicing
            // its accessibility port (beachballing, paused in a debugger) blocks
            // the main thread for the default six-second timeout, per call. This
            // binds the application element; `bound_ax_messaging_timeout` covers
            // the window elements copied out of it. A call that trips either one
            // fails with `kAXErrorCannotComplete`, treated like any other AX error.
            AXUIElementSetMessagingTimeout(ptr, AX_MESSAGING_TIMEOUT_SEC);
            AXUIWrapper::from_retained(ptr)?
        };
        let bundle_id = process
            .application()
            .as_ref()
            .and_then(|app| app.bundleIdentifier())
            .map(|id| id.to_string());
        Ok(Self {
            element: refer,
            psn: process.psn(),
            pid: process.pid(),
            connection,
            handler: AxObserverHandler::new(process.pid(), events.clone(), resolver)?,
            bundle_id,
            name: process.name().to_string(),
        })
    }
}

impl ApplicationApi for ApplicationOS {
    fn pid(&self) -> Pid {
        self.pid
    }

    fn psn(&self) -> ProcessSerialNumber {
        self.psn
    }

    fn connection(&self) -> Option<ConnID> {
        self.connection
    }

    fn focused_window_id(&self) -> Result<WinID> {
        self.element.focused_window_id()
    }

    fn ask_focused_window_id(&self) -> Answer<Result<WinID>> {
        let element = self.element.clone();
        ask_app(self.pid, move || element.focused_window_id())
    }

    fn focused_window(&self, config: &Config) -> Option<Window> {
        let bundle_id = self.bundle_id.as_deref();
        self.element
            .focused_window_element()
            .ok()
            .and_then(|element| {
                WindowOS::new_with_config(&element, config, bundle_id)
                    .ok()
                    .map(|window| Window::new(Box::new(window)))
            })
    }

    fn window_list(&self, config: &Config) -> WindowList {
        let bundle_id = self.bundle_id.as_deref();
        let elements = match self.element.windows() {
            Ok(elements) => elements,
            Err(err) => {
                return WindowList {
                    windows: Vec::new(),
                    complete: !matches!(err, Error::CannotComplete(_)),
                };
            }
        };
        let mut complete = true;
        let windows = elements
            .into_iter()
            .filter_map(
                |element| match WindowOS::new_with_config(&element, config, bundle_id) {
                    Ok(window) => Some(Window::new(Box::new(window))),
                    Err(Error::CannotComplete(_)) => {
                        complete = false;
                        None
                    }
                    Err(_) => None,
                },
            )
            .collect();
        WindowList { windows, complete }
    }

    fn ax_window_ids(&self) -> Vec<WinID> {
        self.element
            .windows()
            .map(|windows| {
                windows
                    .iter()
                    .filter_map(|element| ax_window_id(element.as_ptr()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn observe(&mut self) -> Result<bool> {
        self.handler
            .add_observer(&self.element, &AX_NOTIFICATIONS, ObserverType::Application)
            .map(|retry| retry.is_empty())
    }

    fn observe_window(&mut self, window: &Window) -> Result<bool> {
        if let Some(element) = window.element() {
            self.handler
                .add_observer(
                    &element,
                    &AX_WINDOW_NOTIFICATIONS,
                    ObserverType::Window(window.id()),
                )
                .map(|retry| retry.is_empty())
        } else {
            Err(Error::InvalidWindow)
        }
    }

    fn unobserve_window(&mut self, window: &Window) {
        if let Some(element) = window.element() {
            self.handler.remove_observer(
                &ObserverType::Window(window.id()),
                &element,
                &AX_WINDOW_NOTIFICATIONS,
            );
        }
    }

    fn is_frontmost(&self) -> bool {
        let mut psn = ProcessSerialNumber::default();
        unsafe { _SLPSGetFrontProcess(&mut psn) }
            .to_result(function_name!())
            .is_ok()
            && self.psn == psn
    }

    fn is_hidden(&self) -> bool {
        NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            .is_some_and(|app| app.isHidden())
    }

    fn bundle_id(&self) -> Option<String> {
        self.bundle_id.clone()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ObserverType {
    Application,
    Window(WinID),
}

/// An application notification whose window is not named yet.
#[derive(Clone, Copy, Debug)]
enum WindowNotification {
    Focused,
    Moved,
    Resized,
    MenuOpened,
    MenuClosed,
}

impl WindowNotification {
    fn from_name(notification: &str) -> Option<Self> {
        match notification {
            accessibility_sys::kAXFocusedWindowChangedNotification
            | accessibility_sys::kAXFocusedUIElementChangedNotification => Some(Self::Focused),
            accessibility_sys::kAXWindowMovedNotification => Some(Self::Moved),
            accessibility_sys::kAXWindowResizedNotification => Some(Self::Resized),
            accessibility_sys::kAXMenuOpenedNotification => Some(Self::MenuOpened),
            accessibility_sys::kAXMenuClosedNotification => Some(Self::MenuClosed),
            _ => None,
        }
    }

    fn into_event(self, window_id: WinID) -> Event {
        match self {
            Self::Focused => Event::WindowFocused { window_id },
            Self::Moved => Event::WindowMoved { window_id },
            Self::Resized => Event::WindowResized { window_id },
            Self::MenuOpened => Event::MenuOpened { window_id },
            Self::MenuClosed => Event::MenuClosed { window_id },
        }
    }
}

pub(crate) struct PendingNotification {
    kind: WindowNotification,
    element: CFRetained<AXUIWrapper>,
}

/// Names the window behind each pending notification, off the run loop and in
/// arrival order.
///
/// `_AXUIElementGetWindow` blocks on a round trip into the app that sent the
/// notification. Called straight from the observer callback it holds the run
/// loop, and the `CGEventTap` callback shares that run loop, so a slow app turns
/// every lookup into keystroke latency.
///
/// One thread rather than a task pool: two focus notifications resolving out of
/// order would leave paneru chasing the wrong window.
#[derive(Clone)]
pub(crate) struct WindowResolver {
    sender: Sender<PendingNotification>,
}

impl WindowResolver {
    pub(crate) fn new(events: EventSender) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<PendingNotification>();
        thread::Builder::new()
            .name("ax-window-resolver".into())
            .spawn(move || {
                for pending in rx {
                    match ax_window_id(pending.element.as_ptr()) {
                        Ok(window_id) => {
                            _ = events.send(pending.kind.into_event(window_id));
                        }
                        Err(err) => {
                            debug!("naming window for {:?}: {err}", pending.kind);
                            // A move or resize is still matched to its window
                            // by element, so a tile is not missed.
                            let resized = match pending.kind {
                                WindowNotification::Moved => false,
                                WindowNotification::Resized => true,
                                _ => continue,
                            };
                            _ = events.send(Event::WindowChangedUnnamed {
                                element: pending.element,
                                resized,
                            });
                        }
                    }
                }
            })
            .expect("spawning the AX window resolver should succeed");
        Self { sender: tx }
    }

    pub(crate) fn sender(&self) -> Sender<PendingNotification> {
        self.sender.clone()
    }
}

struct ObserverContext {
    events: EventSender,
    resolver: Sender<PendingNotification>,
    which: ObserverType,
}

impl ObserverContext {
    fn notify(&self, notification: &str, element: AXUIElementRef) {
        match self.which {
            ObserverType::Application => self.notify_app(notification, element),
            ObserverType::Window(id) => self.notify_window(notification, id, element),
        }
    }

    fn notify_app(&self, notification: &str, element: AXUIElementRef) {
        // Handled before the window-id lookup below: a creation notification is
        // the one case whose element is not yet a window paneru knows about.
        if notification == accessibility_sys::kAXCreatedNotification {
            let Ok(element) = AXUIWrapper::retain(element).inspect_err(|err| {
                error!("invalid element {element:?}: {err}");
            }) else {
                return;
            };
            _ = self.events.send(Event::WindowCreated { element });
            return;
        }

        let Some(kind) = WindowNotification::from_name(notification) else {
            error!("unhandled application notification: {notification:?}");
            return;
        };
        let Ok(element) = AXUIWrapper::retain(element).inspect_err(|err| {
            error!("invalid element {element:?}: {err}");
        }) else {
            return;
        };

        _ = self.resolver.send(PendingNotification { kind, element });
    }

    fn notify_window(&self, notification: &str, window_id: WinID, element: AXUIElementRef) {
        let event = match notification {
            accessibility_sys::kAXWindowMiniaturizedNotification => {
                Event::WindowMinimized { window_id }
            }
            accessibility_sys::kAXWindowDeminiaturizedNotification => {
                Event::WindowDeminimized { window_id }
            }
            accessibility_sys::kAXUIElementDestroyedNotification => Event::WindowDestroyed {
                window_id,
                source: DestroySource::Accessibility {
                    element: AXUIWrapper::retain(element).ok(),
                },
            },
            accessibility_sys::kAXTitleChangedNotification => {
                Event::WindowTitleChanged { window_id }
            }

            _ => {
                error!("unhandled window notification: {notification:?}");
                return;
            }
        };
        _ = self.events.send(event);
    }
}

struct AxObserverHandler {
    observer: CFRetained<AXUIWrapper>,
    events: EventSender,
    resolver: Sender<PendingNotification>,
    contexts: Arc<RwLock<Vec<Pin<Box<ObserverContext>>>>>,
}

impl Drop for AxObserverHandler {
    fn drop(&mut self) {
        remove_run_loop(&self.observer);
    }
}

impl AxObserverHandler {
    fn new(pid: Pid, events: EventSender, resolver: Sender<PendingNotification>) -> Result<Self> {
        let observer = unsafe {
            let mut observer_ref: AXObserverRef = null_mut();
            if kAXErrorSuccess == AXObserverCreate(pid, Self::callback, &mut observer_ref) {
                AXUIWrapper::from_retained(observer_ref)?
            } else {
                return Err(Error::PermissionDenied(format!(
                    "{}: error creating observer.",
                    function_name!()
                )));
            }
        };

        unsafe { add_run_loop(&observer, kCFRunLoopCommonModes)? };
        Ok(Self {
            observer,
            events,
            resolver,
            contexts: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Returns the notifications the app did not answer in time so a later call
    /// can retry them, or `Err` when the app refused every notification.
    pub fn add_observer(
        &mut self,
        element: &AXUIWrapper,
        notifications: &[&'static str],
        which: ObserverType,
    ) -> Result<Vec<&str>> {
        let observer: AXObserverRef = self.observer.as_ptr();
        let context_ptr = self.get_or_insert_context(which).as_ptr();

        // TODO: retry re-registering these.
        let mut retry = vec![];
        let added = notifications
            .iter()
            .filter_map(|name| {
                debug!("adding {name} {element:x?} {observer:?}");
                let notification = CFString::from_static_str(name);
                match unsafe {
                    AXObserverAddNotification(
                        observer,
                        element.as_ptr(),
                        &notification,
                        context_ptr.cast(),
                    )
                } {
                    accessibility_sys::kAXErrorSuccess
                    | accessibility_sys::kAXErrorNotificationAlreadyRegistered => Some(*name),
                    accessibility_sys::kAXErrorCannotComplete => {
                        retry.push(*name);
                        None
                    }
                    result => {
                        error!("error adding {name} {element:x?} {observer:?}: {result}");
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        if added.is_empty() && retry.is_empty() {
            Err(Error::PermissionDenied(format!(
                "{}: unable to register any observers!",
                function_name!()
            )))
        } else {
            Ok(retry)
        }
    }

    pub fn remove_observer(
        &mut self,
        which: &ObserverType,
        element: &AXUIWrapper,
        notifications: &[&'static str],
    ) {
        if self.get_context(*which).is_none() {
            debug!("{which:?} ({element}) already un-observed, skipping!");
            return;
        }

        for name in notifications {
            let observer: AXObserverRef = self.observer.deref().as_ptr();
            let notification = CFString::from_static_str(name);
            debug!("removing {name} {element:x?} {observer:?}");
            let result =
                unsafe { AXObserverRemoveNotification(observer, element.as_ptr(), &notification) };
            if result != kAXErrorSuccess {
                debug!("error removing {name} {element:x?} {observer:?}: {result}");
            }
        }
        // AX may already have queued callbacks with this context pointer. Keep
        // every refcon alive until `drop` removes the observer's run-loop source.
    }

    extern "C" fn callback(
        _observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let notification = NonNull::new(notification.cast_mut())
            .map(|ptr| unsafe { ptr.as_ref() })
            .map(CFString::to_string);
        let context =
            NonNull::new(context.cast::<ObserverContext>()).map(|ptr| unsafe { ptr.as_ref() });
        let Some((notification, context)) = notification.zip(context) else {
            return;
        };

        context.notify(&notification, element);
    }

    fn get_context(&self, which: ObserverType) -> Option<NonNull<ObserverContext>> {
        let contexts = self.contexts.force_read();
        contexts
            .iter()
            .find(|context| context.which == which)
            .map(|context| NonNull::from_ref(context.as_ref().get_ref()))
    }

    fn get_or_insert_context(
        &self,
        // contexts: &mut Vec<Pin<Box<ObserverContext>>>,
        which: ObserverType,
    ) -> NonNull<ObserverContext> {
        if let Some(context) = self.get_context(which) {
            return context;
        }
        self.contexts.force_write().push(Box::pin(ObserverContext {
            events: self.events.clone(),
            resolver: self.resolver.clone(),
            which,
        }));
        self.get_context(which)
            .expect("inserted observer context must be present")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        mem::ManuallyDrop,
        ptr::NonNull,
        sync::{Arc, RwLock},
    };
    use stdext::sync::rw_lock::RwLockExt as _;

    use super::ObserverType;
    use crate::{events::EventSender, manager::app::AxObserverHandler, util::AXUIWrapper};

    #[test]
    fn same_observer_type_reuses_stable_context_pointer() {
        let fake_ptr = NonNull::<AXUIWrapper>::dangling().as_ptr();
        let observer = AXUIWrapper::from_retained(fake_ptr).unwrap();
        let (events, _receiver) = EventSender::new();
        let (resolver, _rx) = crossbeam_channel::unbounded();
        let handler = ManuallyDrop::new(AxObserverHandler {
            observer,
            events,
            resolver,
            contexts: Arc::new(RwLock::new(Vec::new())),
        });

        let first = handler.get_or_insert_context(ObserverType::Window(42));
        let reused = handler.get_or_insert_context(ObserverType::Window(42));

        assert_eq!(reused, first);
        assert_eq!(handler.contexts.as_ref().force_read().len(), 1);
    }
}
