use core::ptr::NonNull;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use stdext::function_name;
use tracing::{Level, debug, error, instrument};

use crate::errors::Result;
use crate::events::{DestroySource, Event, EventSender};
use crate::platform::{ConnID, OSStatus, WinID, WorkspaceId};
use crate::util::MacResult;

unsafe extern "C" {
    pub fn SLSMainConnectionID() -> ConnID;
    pub fn SLSSpaceGetType(cid: ConnID, sid: WorkspaceId) -> libc::c_int;
    pub fn SLSRegisterConnectionNotifyProc(
        cid: ConnID,
        callback: extern "C-unwind" fn(u32, *mut c_void, usize, *mut c_void, ConnID),
        event: u32,
        data: *mut c_void,
    ) -> OSStatus;
}

pub(super) struct NotifyHandler {
    events: EventSender,
    conn: ConnID,
    _pin: PhantomPinned,
}

pub(super) type PinnedNotifyHandler =
    ScopeGuard<Pin<Box<NotifyHandler>>, Box<dyn FnOnce(Pin<Box<NotifyHandler>>)>>;

impl NotifyHandler {
    pub(super) fn new(events: EventSender) -> Self {
        Self {
            events,
            conn: unsafe { SLSMainConnectionID() },
            _pin: PhantomPinned,
        }
    }

    pub(super) fn start(self) -> Result<PinnedNotifyHandler> {
        debug!("Registering notify handler");
        let cid = self.conn;
        let mut pinned = Box::pin(self);
        let this = unsafe { NonNull::new_unchecked(pinned.as_mut().get_unchecked_mut()) }.as_ptr();
        let events = [
            SpaceEvent::Created,
            SpaceEvent::CurrentChanged,
            SpaceEvent::Destroyed,
            SpaceEvent::WindowDestroyed,
        ];
        for event in events {
            unsafe {
                SLSRegisterConnectionNotifyProc(cid, Self::callback, event.into(), this.cast())
            }
            .to_result(function_name!())?;
        }

        Ok(scopeguard::guard(
            pinned,
            Box::new(|_pin: Pin<Box<Self>>| {
                debug!("Unregistering notify handler");
            }),
        ))
    }

    extern "C-unwind" fn callback(
        event_id: u32,
        data: *mut c_void,
        len: usize,
        context: *mut c_void,
        cid: ConnID,
    ) {
        if let Some(this) =
            NonNull::new(context).map(|this| unsafe { this.cast::<NotifyHandler>().as_ref() })
        {
            this.notify_handler(event_id, data, len, cid);
        } else {
            error!("Zero passed to Notify Handler.");
        }
    }

    #[instrument(level = Level::DEBUG, skip_all, fields(event_id, len))]
    fn notify_handler(&self, event_id: u32, data: *mut c_void, len: usize, _cid: ConnID) {
        let Ok(event) = SpaceEvent::try_from(event_id) else {
            debug!("Unknown event received: {event_id}");
            return;
        };

        match event {
            SpaceEvent::Created | SpaceEvent::Destroyed | SpaceEvent::CurrentChanged => {
                if let Some(space_id) = from_bytes::<WorkspaceId>(data, len) {
                    if matches!(event, SpaceEvent::Destroyed) {
                        _ = self.events.send(Event::SpaceDestroyed { space_id });
                    } else {
                        let space_type = unsafe { SLSSpaceGetType(self.conn, space_id) };
                        if space_type == 0 && matches!(event, SpaceEvent::Created) {
                            _ = self.events.send(Event::SpaceCreated { space_id });
                        } else {
                            debug!("{event:?} space = {space_id}, space_type = {space_type}");
                        }
                    }
                }
            }

            SpaceEvent::WindowDestroyed => {
                let offset = std::mem::size_of::<u64>();
                if let Some(space) = from_bytes::<WorkspaceId>(data, len)
                    && let Some(window_id) = from_bytes::<WinID>(
                        unsafe { data.byte_add(offset) },
                        len.saturating_sub(offset),
                    )
                {
                    debug!("{event:?} space = {space}, window_id = {window_id}");
                    _ = self.events.send(Event::WindowDestroyed {
                        window_id,
                        source: DestroySource::SpaceNotification,
                    });
                }
            }
        }
    }
}

fn from_bytes<T>(data: *const c_void, len: usize) -> Option<T> {
    let size = std::mem::size_of::<T>();
    (!data.is_null() && len >= size).then(|| unsafe { std::ptr::read_unaligned(data.cast::<T>()) })
}

// Credits for reverse-engineered SkyLight CGS notification IDs:
// - https://github.com/asmagill/hs._asm.undocumented.spaces/blob/master/CGSSpace.h
// - https://github.com/koekeishiya/yabai/blob/d55a647913ab72d8d8b348bee2d3e59e52ce4a5d/src/misc/extern.h
// - https://github.com/acsandmann/rift
// Full catalog of observed IDs and payload layouts: docs/skylight-notifications.md.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, IntoPrimitive, TryFromPrimitive)]
enum SpaceEvent {
    WindowDestroyed = 1326,
    Created = 1327,
    Destroyed = 1328,
    CurrentChanged = 1329,
}
