use core::ptr::NonNull;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGError,
};
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use tracing::{debug, error, info, warn};

use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};

pub(super) struct DisplayHandler {
    events: EventSender,
    _pin: PhantomPinned,
}

pub type PinnedDisplayHandler =
    ScopeGuard<Pin<Box<DisplayHandler>>, Box<dyn FnOnce(Pin<Box<DisplayHandler>>)>>;

impl DisplayHandler {
    pub(super) fn new(events: EventSender) -> Self {
        Self {
            events,
            _pin: PhantomPinned,
        }
    }

    pub(super) fn start(self) -> Result<PinnedDisplayHandler> {
        info!(" Registering display handler");
        let mut pinned = Box::pin(self);
        let this = unsafe { NonNull::new_unchecked(pinned.as_mut().get_unchecked_mut()) }.as_ptr();
        let result =
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(Self::callback), this.cast()) };
        if result != CGError::Success {
            return Err(Error::PermissionDenied(format!(
                "registering display handler callback: {result:?}",
            )));
        }
        Ok(scopeguard::guard(
            pinned,
            Box::new(|mut pin: Pin<Box<Self>>| {
                info!("Unregistering display handler");
                let this =
                    unsafe { NonNull::new_unchecked(pin.as_mut().get_unchecked_mut()) }.as_ptr();
                unsafe {
                    CGDisplayRemoveReconfigurationCallback(Some(Self::callback), this.cast())
                };
            }),
        ))
    }

    extern "C-unwind" fn callback(
        display_id: CGDirectDisplayID,
        flags: CGDisplayChangeSummaryFlags,
        context: *mut c_void,
    ) {
        if let Some(this) =
            NonNull::new(context).map(|this| unsafe { this.cast::<DisplayHandler>().as_ref() })
        {
            this.display_handler(display_id, flags);
        } else {
            error!("Zero passed to Display Handler.");
        }
    }

    fn display_handler(&self, display_id: CGDirectDisplayID, flags: CGDisplayChangeSummaryFlags) {
        debug!("display change {display_id:?}: {flags:?}");
        let event = if flags.contains(CGDisplayChangeSummaryFlags::AddFlag) {
            Event::DisplayAdded { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::RemoveFlag) {
            Event::DisplayRemoved { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::MovedFlag) {
            Event::DisplayMoved { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::DesktopShapeChangedFlag) {
            Event::DisplayResized { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
            Event::DisplayConfigured { display_id }
        } else {
            // Changes to the main display, mode or mirroring also need a full
            // scan, even when macOS does not send an add/remove/move flag.
            Event::DisplayConfigured { display_id }
        };
        _ = self
            .events
            .send(event)
            .inspect_err(|err| warn!("error sending event: {err}"));
    }
}
