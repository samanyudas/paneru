use core::ptr::NonNull;
use objc2::rc::Retained;
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use stdext::function_name;
use tracing::{debug, error, info};

use super::workspace::WorkspaceObserver;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::{OSStatus, Pid};
use crate::util::MacResult;
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use serde::{Deserialize, Serialize};

/// Returns the PID of the first running application matching `bundle_id`.
pub(crate) fn pid_for_bundle_id(bundle_id: &NSString) -> Option<Pid> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(bundle_id)
        .iter()
        .next()
        .map(|app| app.processIdentifier())
}

/// Represents a process serial number (PSN), a unique identifier for a running process on macOS.
/// It is used by the Carbon APIs to identify applications.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C)]
pub struct ProcessSerialNumber {
    /// The high-order 32 bits of the process serial number.
    pub high: u32,
    /// The low-order 32 bits of the process serial number.
    pub low: u32,
}

/// Type alias for the callback function signature used by `InstallEventHandler` for process events.
type ProcessCallbackFn = extern "C-unwind" fn(
    this: *mut c_void,
    event: *const ProcessEvent,
    context: *const c_void,
) -> OSStatus;

unsafe extern "C" {
    /// Retrieves the application event target.
    /// This function returns an `EventTargetRef` that represents the application's event queue.
    ///
    /// # Returns
    ///
    /// A raw pointer to an `EventTargetRef` for the application.
    ///
    /// # Original signature
    /// extern `EventTargetRef` GetApplicationEventTarget(void)
    fn GetApplicationEventTarget() -> *const ProcessEventTarget;

    /// Installs an event handler for a specific event target and event types.
    /// This function sets up a callback to be invoked when specified Carbon events occur.
    ///
    /// # Arguments
    ///
    /// * `target` - The `EventTargetRef` to install the handler on, typically obtained from `GetApplicationEventTarget`.
    /// * `handler` - The `ProcessCallbackFn` to be called when events match.
    /// * `event_len` - The number of event types in `events`.
    /// * `events` - A raw pointer to an array of `EventTypeSpec` defining the events to handle.
    /// * `user_data` - A raw pointer to user-defined data to pass to the handler's `context` parameter.
    /// * `handler_ref` - A mutable raw pointer to an `EventHandlerRef` where the installed handler
    ///   reference will be stored.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    /// extern `OSStatus`
    /// `InstallEventHandler`(
    ///   `EventTargetRef`         inTarget,
    ///   `EventHandlerUPP`        inHandler,
    ///   `ItemCount`              inNumTypes,
    ///   const `EventTypeSpec` *  inList,
    ///   void *                 inUserData,
    ///   `EventHandlerRef` *      outRef)
    fn InstallEventHandler(
        target: *const ProcessEventTarget,
        handler: ProcessCallbackFn,
        event_len: u32,
        events: *const EventTypeSpec,
        user_data: *const c_void,
        handler_ref: *mut *const ProcessEventHandler,
    ) -> OSStatus;

    /// Removes a previously installed event handler.
    ///
    /// # Arguments
    ///
    /// * `handler_ref` - A raw pointer to the `EventHandlerRef` to remove.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    /// extern `OSStatus` RemoveEventHandler(EventHandlerRef inHandlerRef)
    fn RemoveEventHandler(handler_ref: *const ProcessEventHandler) -> OSStatus;

    /// Gets a piece of data from the given event, if it exists.
    /// The Carbon Event Manager will automatically use `AppleEvent` coercion handlers to convert
    /// the data in the event into the desired type, if possible. You may also pass `typeWildCard`
    /// to request that the data be returned in its original format.
    ///
    /// # Mac OS X threading
    /// Not thread safe.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to get the parameter from.
    /// * `param_name` - The symbolic name of the parameter (e.g., `kEventParamProcessID`).
    /// * `param_type` - The desired type of the parameter (e.g., `typeProcessSerialNumber`).
    /// * `actual_type` - A mutable pointer to `u32` to store the actual type of the parameter, or `NULL`.
    /// * `size` - The size of the output buffer specified by `data`. Pass zero and
    ///   `NULL` for `data` if data is not desired.
    /// * `actual_size` - A mutable pointer to `u32` to store the actual size of the
    ///   data, or `NULL`.
    /// * `data` - A mutable pointer to the buffer which will receive the parameter data, or `NULL`.
    ///
    /// # Returns
    ///
    /// An operating system result code (`OSStatus`).
    ///
    /// # Original signature
    /// extern `OSStatus`
    /// `GetEventParameter`(
    ///   `EventRef`          inEvent,
    ///   `EventParamName`    inName,
    ///   `EventParamType`    inDesiredType,
    ///   `EventParamType` *  outActualType,       /* can be NULL */
    ///   `ByteCount`         inBufferSize,
    ///   `ByteCount` *       outActualSize,       /* can be NULL */
    ///   void *            outData)             /* can be NULL */
    fn GetEventParameter(
        event: *const ProcessEvent,
        param_name: u32,
        param_type: u32,
        actual_type: *mut u32,
        size: u32,
        actual_size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;

    /// Returns the kind of the given event (e.g., mousedown).
    /// Event kinds overlap between event classes (e.g., `kEventMouseDown` and `kEventAppActivated`
    /// have the same value). The combination of class and kind determines an event signature.
    ///
    /// # Mac OS X threading
    /// Thread safe.
    ///
    /// # Arguments
    ///
    /// * `event` - The event in question.
    ///
    /// # Returns
    ///
    /// The kind of the event (`UInt32`).
    ///
    /// # Original signature
    /// extern `UInt32` GetEventKind(EventRef inEvent)
    fn GetEventKind(event: *const ProcessEvent) -> u32;

    /// Retrieves the next available process's serial number.
    /// This function iterates through all running processes and returns their PSN.
    ///
    /// # Arguments
    ///
    /// * `psn` - A mutable pointer to a `ProcessSerialNumber` structure. On the first call, pass a
    ///   PSN with `kNoProcess` for `highLongOfPSN` and `lowLongOfPSN`. On subsequent calls, pass
    ///   the PSN returned by the previous call.
    ///
    /// # Returns
    ///
    /// An `OSStatus` code. `noErr` (0) if a process was found, otherwise an error code.
    ///
    /// # Original signature
    /// GetNextProcess(ProcessSerialNumber * pPSN)
    fn GetNextProcess(psn: *mut ProcessSerialNumber) -> OSStatus;
}

/*
 *  EventTypeSpec
 *
 *  Discussion:
 *    This structure is used to specify an event. Typically, a static
 *    array of EventTypeSpecs are passed into functions such as
 *    InstallEventHandler, as well as routines such as
 *    FlushEventsMatchingListFromQueue.
 */
// struct EventTypeSpec {
//   OSType              eventClass;
//   UInt32              eventKind;
// };
/// Specifies a Carbon event by its class and kind.
/// Used for registering and matching events with event handlers.
#[repr(C)]
struct EventTypeSpec {
    /// The event class (e.g., `kEventClassApplication`).
    event_class: u32,
    /// The event kind within its class (e.g., `kEventAppLaunched`).
    event_kind: u32,
}

/// An opaque type representing a Carbon event handler reference.
#[repr(C)]
struct ProcessEventHandler {
    _opaque: [u8; 0],
}

/// An opaque type representing a Carbon event target reference.
#[repr(C)]
struct ProcessEventTarget {
    _opaque: [u8; 0],
}

/// An opaque type representing a Carbon event.
#[repr(C)]
struct ProcessEvent {
    _opaque: [u8; 0],
}

/// `ProcessHandler` is a Rust-side representation of the handler for Carbon process events.
/// It receives raw Carbon events and dispatches them as higher-level `Event`s through an `EventSender`.
#[repr(C)]
pub(super) struct ProcessHandler {
    /// The sender for dispatching processed events.
    events: EventSender,
    /// A retained reference to the `WorkspaceObserver`, used for `ApplicationLaunched` events.
    observer: Retained<WorkspaceObserver>,
    // Prevents from being Unpin automatically
    _pin: PhantomPinned,
}

pub type PinnedProcessHandler =
    ScopeGuard<Pin<Box<ProcessHandler>>, Box<dyn FnOnce(Pin<Box<ProcessHandler>>)>>;

/// Carbon `kEventClassApplication` event kinds registered by [`ProcessHandler`].
const EVENT_APP_LAUNCHED: u32 = 5;
const EVENT_APP_TERMINATED: u32 = 6;
const EVENT_APP_FRONT_SWITCHED: u32 = 7;

impl ProcessHandler {
    /// Creates a new `ProcessHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send process-related events.
    /// * `observer` - A `Retained<WorkspaceObserver>` to pass to `ApplicationLaunched` events.
    ///
    /// # Returns
    ///
    /// A new `ProcessHandler`.
    pub(super) fn new(events: EventSender, observer: Retained<WorkspaceObserver>) -> Self {
        ProcessHandler {
            events,
            observer,
            _pin: PhantomPinned,
        }
    }

    /// Starts the process handler by registering a C-callback with the underlying private API.
    /// It also sends initial `ApplicationLaunched` events for already running processes.
    ///
    /// # Side Effects
    ///
    /// - Registers a Carbon event handler, which will be unregistered when `cleanup` is dropped.
    /// - Iterates through existing processes and dispatches `ApplicationLaunched` events for them.
    pub(super) fn start(self) -> Result<PinnedProcessHandler> {
        const APPL_CLASS: &str = "appl";

        info!("Registering process_handler");

        // Fake launch the existing processes.
        let mut psn = ProcessSerialNumber::default();
        while unsafe { GetNextProcess(&raw mut psn) }
            .to_result(function_name!())
            .is_ok()
        {
            self.process_handler(psn, EVENT_APP_LAUNCHED);
        }

        let target = unsafe { GetApplicationEventTarget() };
        let event_class = u32::from_be_bytes(APPL_CLASS.as_bytes().try_into()?);
        let events = [
            EventTypeSpec {
                event_class,
                event_kind: EVENT_APP_LAUNCHED,
            },
            EventTypeSpec {
                event_class,
                event_kind: EVENT_APP_TERMINATED,
            },
            EventTypeSpec {
                event_class,
                event_kind: EVENT_APP_FRONT_SWITCHED,
            },
        ];

        let mut pinned = Box::pin(self);
        let this = unsafe { NonNull::new_unchecked(pinned.as_mut().get_unchecked_mut()) }.as_ptr();
        let mut handler: *const ProcessEventHandler = std::ptr::null();
        let result = unsafe {
            InstallEventHandler(
                target,
                Self::callback,
                events.len().try_into()?,
                events.as_ptr(),
                this.cast(),
                &raw mut handler,
            )
        };
        if result != 0 || handler.is_null() {
            return Err(Error::PermissionDenied(format!(
                "{}: Error registering process event handler.",
                function_name!()
            )));
        }
        debug!("Registered process_handler (result = {result}): {handler:x?}");

        Ok(scopeguard::guard(
            pinned,
            Box::new(move |_: Pin<Box<Self>>| {
                info!("Unregistering process_handler: {handler:?}");
                unsafe { RemoveEventHandler(handler) };
            }),
        ))
    }

    extern "C-unwind" fn callback(
        _: *mut c_void,
        event: *const ProcessEvent,
        this: *const c_void,
    ) -> OSStatus {
        if let Some(this) = NonNull::new(this.cast_mut())
            .map(|this| unsafe { this.cast::<ProcessHandler>().as_ref() })
        {
            const PARAM: &str = "psn "; // kEventParamProcessID and typeProcessSerialNumber
            let param_name = u32::from_be_bytes(PARAM.as_bytes().try_into().unwrap());
            let param_type = param_name; // Uses the same FourCharCode as param_name

            let mut psn = ProcessSerialNumber::default();

            let res = unsafe {
                GetEventParameter(
                    event,
                    param_name,
                    param_type,
                    std::ptr::null_mut(),
                    std::mem::size_of::<ProcessSerialNumber>()
                        .try_into()
                        .unwrap(),
                    std::ptr::null_mut(),
                    NonNull::from(&mut psn).as_ptr().cast(),
                )
            };
            if res == 0 {
                let kind = unsafe { GetEventKind(event) };
                this.process_handler(psn, kind);
            }
        } else {
            error!("Zero passed to Process Handler.");
        }
        0
    }

    fn process_handler(&self, psn: ProcessSerialNumber, event_kind: u32) {
        let _ = match event_kind {
            EVENT_APP_LAUNCHED => self.events.send(Event::ApplicationLaunched {
                psn,
                observer: self.observer.clone(),
            }),
            EVENT_APP_TERMINATED => self.events.send(Event::ApplicationTerminated { psn }),
            EVENT_APP_FRONT_SWITCHED => self.events.send(Event::ApplicationFrontSwitched { psn }),
            other => {
                error!("Unknown process event: {other}");
                Ok(())
            }
        }
        .inspect_err(|err| error!("error sending event: {err}"));
    }
}

/// The start of the kernel's `kinfo_proc` record: `kp_proc.p_un` begins with
/// the process start time. The rest of the record is not read.
#[repr(C)]
struct KinfoProc {
    started: libc::timeval,
    rest: [u8; 632],
}

/// Returns when the process `pid` started, in microseconds since the epoch,
/// or `None` when no such process runs.
pub(crate) fn process_started(pid: Pid) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<KinfoProc>::zeroed();
    let mut size = std::mem::size_of::<KinfoProc>();
    let mut name = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let result = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            4,
            info.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != std::mem::size_of::<KinfoProc>() {
        return None;
    }
    let started = unsafe { info.assume_init() }.started;
    let seconds = u64::try_from(started.tv_sec).ok()?;
    let micros = u64::try_from(started.tv_usec).ok()?;
    Some(seconds * 1_000_000 + micros)
}

#[cfg(test)]
mod tests {
    use super::process_started;

    #[test]
    fn a_running_process_has_a_stable_start_time() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let started = process_started(pid);
        assert!(started.is_some());
        assert_eq!(process_started(pid), started);
    }

    #[test]
    fn a_process_of_another_user_has_a_start_time() {
        assert!(process_started(1).is_some());
    }

    #[test]
    fn a_missing_process_has_no_start_time() {
        assert_eq!(process_started(i32::MAX), None);
    }
}
