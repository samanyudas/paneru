use mockall::automock;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObjectNSKeyValueObserverRegistration, NSString,
};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use stdext::function_name;
use tracing::debug;

use crate::ecs::BProcess;
use crate::errors::Result;
use crate::platform::{OSStatus, Pid, ProcessSerialNumber, WorkspaceObserver};
use crate::util::MacResult;

unsafe extern "C" {
    /// `OSStatus CopyProcessName(const ProcessSerialNumber *psn, CFStringRef *name);`
    /// Follows Core Foundation's Create/Copy rule and returns an owned string,
    /// so callers wrap it with `CFRetained::from_raw`.
    fn CopyProcessName(psn: *const ProcessSerialNumber, name: *mut *const CFString) -> OSStatus;

    /// `OSStatus GetProcessPID(const ProcessSerialNumber *psn, pid_t *pid);`
    fn GetProcessPID(psn: *const ProcessSerialNumber, pid: *mut Pid) -> OSStatus;

    /// `OSStatus GetProcessForPID(pid_t pid, ProcessSerialNumber *psn);`
    fn GetProcessForPID(pid: Pid, psn: *mut ProcessSerialNumber) -> OSStatus;
}

pub(super) fn psn_for_pid(pid: Pid) -> Result<ProcessSerialNumber> {
    let mut psn = ProcessSerialNumber::default();
    unsafe { GetProcessForPID(pid, &raw mut psn) }.to_result(function_name!())?;
    Ok(psn)
}

#[automock]
pub trait ProcessApi: Send + Sync {
    fn is_observable(&mut self) -> bool;
    fn name(&self) -> &str;
    fn pid(&self) -> Pid;
    fn psn(&self) -> ProcessSerialNumber;
    fn bundle_id(&self) -> Option<String>;
    fn ready(&mut self) -> bool;
    /// Forces the process to be treated as manageable even if macOS reports it as
    /// unobservable (e.g. `LSUIElement` background apps that create standard windows).
    fn force_manage(&mut self, force: bool);
}

pub struct ProcessOS {
    pub inner: Pin<Box<Process>>,
}

impl ProcessApi for ProcessOS {
    fn is_observable(&mut self) -> bool {
        self.inner.is_observable()
    }

    fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    fn pid(&self) -> Pid {
        self.inner.pid
    }

    fn psn(&self) -> ProcessSerialNumber {
        self.inner.psn
    }

    fn bundle_id(&self) -> Option<String> {
        self.inner.bundle_id()
    }

    fn ready(&mut self) -> bool {
        self.inner.ready()
    }

    fn force_manage(&mut self, force: bool) {
        self.inner.force_manage(force);
    }
}

impl From<Pin<Box<Process>>> for BProcess {
    fn from(inner: Pin<Box<Process>>) -> Self {
        BProcess(Box::new(ProcessOS { inner }))
    }
}

#[repr(C)]
pub struct Process {
    pub psn: ProcessSerialNumber,
    pub pid: Pid,
    pub name: String,
    pub application: Option<Retained<NSRunningApplication>>,
    pub policy: NSApplicationActivationPolicy,
    pub observer: Retained<WorkspaceObserver>,
    observing_launched: AtomicBool,
    observing_activated: AtomicBool,
    force_manage: bool,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.unobserve_finished_launching();
        self.unobserve_activation_policy();
    }
}

fn lookup_running_application(pid: Pid) -> Option<Retained<NSRunningApplication>> {
    (pid > 0)
        .then(|| NSRunningApplication::runningApplicationWithProcessIdentifier(pid))
        .flatten()
}

impl Process {
    pub fn new(psn: &ProcessSerialNumber, observer: Retained<WorkspaceObserver>) -> Pin<Box<Self>> {
        let mut pid: Pid = 0;
        unsafe { GetProcessPID(psn, NonNull::from(&mut pid).as_ptr()) };

        let mut nameref: *const CFString = std::ptr::null();
        unsafe { CopyProcessName(psn, &raw mut nameref) };
        let name = NonNull::new(nameref.cast_mut())
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .map(|name| name.to_string())
            .unwrap_or_default();

        let apps = lookup_running_application(pid);

        Box::pin(Process {
            psn: *psn,
            name,
            pid,
            application: apps,
            policy: NSApplicationActivationPolicy::Prohibited,
            observer,
            observing_launched: AtomicBool::new(false),
            observing_activated: AtomicBool::new(false),
            force_manage: false,
        })
    }

    pub fn bundle_id(&self) -> Option<String> {
        self.application
            .as_ref()
            .and_then(|app| app.bundleIdentifier())
            .map(|id| id.to_string())
    }

    pub fn is_observable(&mut self) -> bool {
        if let Some(app) = &self.application {
            self.policy = app.activationPolicy();
            self.policy == NSApplicationActivationPolicy::Regular
        } else {
            self.policy = NSApplicationActivationPolicy::Prohibited;
            false
        }
    }

    pub fn force_manage(&mut self, force: bool) {
        self.force_manage = force;
    }

    pub fn finished_launching(&self) -> bool {
        self.application
            .as_ref()
            .is_some_and(|app| app.isFinishedLaunching())
    }

    pub fn observe_finished_launching(&self) {
        if self.application.is_some() && !self.observing_launched.swap(true, Ordering::Acquire) {
            self.observe("finishedLaunching");
        }
    }

    pub fn unobserve_finished_launching(&self) {
        if self.observing_launched.swap(false, Ordering::Release) {
            self.unobserve("finishedLaunching");
        }
    }

    pub fn observe_activation_policy(&self) {
        if self.application.is_some() && !self.observing_activated.swap(true, Ordering::Acquire) {
            self.observe("activationPolicy");
        }
    }

    pub fn unobserve_activation_policy(&self) {
        if self.observing_activated.swap(false, Ordering::Release) {
            self.unobserve("activationPolicy");
        }
    }

    fn observe(&self, flavor: &str) {
        if let Some(app) = self.application.as_ref() {
            unsafe {
                let key_path = NSString::from_str(flavor);
                let options = NSKeyValueObservingOptions::New | NSKeyValueObservingOptions::Initial;
                app.addObserver_forKeyPath_options_context(
                    &self.observer,
                    key_path.as_ref(),
                    options,
                    NonNull::from(self).as_ptr().cast(),
                );
            }
            debug!("observing {flavor} for {}", &self.name);
        }
    }

    fn unobserve(&self, flavor: &str) {
        if let Some(app) = self.application.as_ref() {
            unsafe {
                let key_path = NSString::from_str(flavor);
                app.removeObserver_forKeyPath_context(
                    &self.observer,
                    key_path.as_ref(),
                    NonNull::from(self).as_ptr().cast(),
                );
            }
            debug!("removed {flavor} observers for {}", &self.name);
        }
    }

    pub fn ready(&mut self) -> bool {
        self.ready_with(lookup_running_application)
    }

    fn ready_with(
        &mut self,
        lookup: impl FnOnce(Pid) -> Option<Retained<NSRunningApplication>>,
    ) -> bool {
        // macOS can announce a launch before `NSRunningApplication` knows the
        // process, and without one the process would never look launched.
        if self.application.is_none() {
            self.application = lookup(self.pid);
            if self.application.is_none() {
                return false;
            }
        }
        if !self.finished_launching() {
            debug!(
                "{} ({}) is not finished launching, subscribing to finishedLaunching changes",
                self.name, self.pid
            );
            self.observe_finished_launching();
            return false;
        }
        self.unobserve_finished_launching();

        if !self.force_manage && !self.is_observable() {
            debug!(
                "{} ({}) is not observable, subscribing to activationPolicy changes",
                self.name, self.pid
            );
            self.observe_activation_policy();
            return false;
        }
        self.unobserve_activation_policy();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventSender;

    fn test_process(pid: Pid) -> Pin<Box<Process>> {
        let (events, _receiver) = EventSender::new();
        Box::pin(Process {
            psn: ProcessSerialNumber {
                high: 0,
                low: pid.max(0).cast_unsigned(),
            },
            pid,
            name: "TestApp".to_string(),
            application: None,
            policy: NSApplicationActivationPolicy::Prohibited,
            observer: WorkspaceObserver::new(events),
            observing_launched: AtomicBool::new(false),
            observing_activated: AtomicBool::new(false),
            force_manage: false,
        })
    }

    #[test]
    fn observe_methods_ignore_missing_application() {
        let mut process = test_process(42);
        process.observe_finished_launching();
        process.observe_activation_policy();
        assert!(!process.observing_launched.load(Ordering::Relaxed));
        assert!(!process.observing_activated.load(Ordering::Relaxed));

        // Attaching an application later and dropping must not attempt to
        // remove KVO observers that were never registered.
        process.application = Some(NSRunningApplication::currentApplication());
        drop(process);
    }

    #[test]
    fn ready_with_retries_lookup_and_caches_resolved_application() {
        let mut process = test_process(42);

        let mut queried_pid = None;
        assert!(!process.ready_with(|pid| {
            queried_pid = Some(pid);
            None
        }));
        assert_eq!(queried_pid, Some(42));
        assert!(process.application.is_none());
        assert!(!process.observing_launched.load(Ordering::Relaxed));
        assert!(!process.observing_activated.load(Ordering::Relaxed));

        let _ = process.ready_with(|pid| {
            assert_eq!(pid, 42);
            Some(NSRunningApplication::currentApplication())
        });
        assert!(process.application.is_some());

        // Once resolved, subsequent readiness checks must reuse the cached application.
        let _ = process.ready_with(|_| panic!("lookup must not rerun once application is cached"));
    }

    #[test]
    fn lookup_running_application_ignores_non_positive_pid() {
        assert!(lookup_running_application(0).is_none());
        assert!(lookup_running_application(-1).is_none());

        let mut zero_pid = test_process(0);
        assert!(!zero_pid.ready());
        assert!(zero_pid.application.is_none());

        let mut negative_pid = test_process(-1);
        assert!(!negative_pid.ready());
        assert!(negative_pid.application.is_none());
    }
}
