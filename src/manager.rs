use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use bevy::ecs::resource::Resource;
use bevy::math::{IRect, IVec2};
use core::ptr::NonNull;
use derive_more::{DerefMut, with_trait::Deref};
use mockall::automock;
use notify::{RecursiveMode, Watcher};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFMutableData, CFNumber, CFNumberType, CFRetained, CFString, CFType,
    CGPoint, CGRect, CGSize, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDirectDisplayID, CGDisplayBounds, CGEventSource,
    CGEventSourceStateID, CGGetActiveDisplayList, CGMouseButton,
    CGRectMakeWithDictionaryRepresentation, CGWarpMouseCursorPosition, CGWindowLevelForKey,
    CGWindowLevelKey, CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
    kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::NSBundle;
use std::path::Path;
use std::ptr::null_mut;
use std::slice::from_raw_parts_mut;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use stdext::function_name;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::manager::skylight::SLSSetWindowListBrightness;
use crate::platform::{ConnID, Pid, ProcessSerialNumber, WinID, WorkspaceId};
use crate::util::{AXUIWrapper, MacResult, create_array, round_px, symlink_target};
use app::ApplicationOS;
pub use app::{Application, ApplicationApi};
pub use display::Display;
pub use process::{Process, ProcessApi};
pub use skylight::AXUIElementCopyAttributeValue;
use skylight::{
    _AXUIElementCreateWithRemoteToken, SLSCopyActiveMenuBarDisplayIdentifier,
    SLSCopyAssociatedWindows, SLSCopyManagedDisplaySpaces, SLSCopyWindowsWithOptionsAndTags,
    SLSFindWindowAndOwner, SLSGetConnectionIDForPSN, SLSGetCurrentCursorLocation,
    SLSGetDisplayMenubarHeight, SLSGetSpaceManagementMode, SLSMainConnectionID,
    SLSManagedDisplayGetCurrentSpace, SLSSpaceGetType, SLSWindowIsOrderedIn,
    SLSWindowIteratorAdvance, SLSWindowIteratorGetAttributes, SLSWindowIteratorGetParentID,
    SLSWindowIteratorGetTags, SLSWindowIteratorGetWindowID, SLSWindowQueryResultCopyWindows,
    SLSWindowQueryWindows,
};
pub(crate) use windows::forget_enhanced_ui_state;
pub use windows::{
    Margins, Window, WindowApi, WindowOS, WindowPadding, ax_window_id, try_ax_window_id,
};

#[cfg(test)]
pub use process::MockProcessApi;
#[cfg(test)]
pub use windows::MockWindowApi;

pub(crate) mod app;
mod display;
mod process;
mod skylight;
pub(crate) mod window_writer;
mod windows;

pub type Origin = IVec2;
pub type Size = IVec2;

pub fn origin_from(point: CGPoint) -> Origin {
    Origin::new(round_px(point.x), round_px(point.y))
}

pub fn origin_to(point: Origin) -> CGPoint {
    CGPoint::new(point.x.into(), point.y.into())
}

pub fn size_from(size: CGSize) -> Size {
    Size::new(round_px(size.width), round_px(size.height))
}

pub fn irect_from(rect: CGRect) -> IRect {
    let origin = origin_from(rect.origin);
    IRect::from_corners(origin, origin + size_from(rect.size))
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn odd_sized_ax_frame_keeps_its_origin_and_size() {
        let frame = CGRect::new(CGPoint::new(0.0, 20.0), CGSize::new(399.0, 701.0));

        assert_eq!(irect_from(frame), IRect::new(0, 20, 399, 721));
    }
}

/// Defines the interface for a window manager, abstracting OS-specific operations.
#[automock]
pub trait WindowManagerApi: Send + Sync {
    /// Creates a new `Application` instance from a given `ProcessApi`.
    ///
    /// # Arguments
    ///
    /// * `process` - A reference to the `ProcessApi` trait object representing the application's process.
    ///
    /// # Returns
    ///
    /// `Ok(Application)` if the application is successfully created, otherwise `Err(Error)`.
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application>;
    /// Retrieves a list of window IDs associated with a parent window.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The `WinID` of the parent window.
    ///
    /// # Returns
    ///
    /// A `Vec<WinID>` containing the IDs of associated child windows.
    fn get_associated_windows(&self, window_id: WinID) -> Vec<WinID>;
    /// Retrieves a list of all currently present displays.
    ///
    /// # Returns
    ///
    /// A `Vec<Display>` containing `Display` objects for all present displays.
    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)>;
    /// Retrieves the `CGDirectDisplayID` of the active menu bar display.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn active_display_id(&self) -> Result<u32>;
    /// Retrieves the ID of the current active space on a given display.
    ///
    /// # Arguments
    ///
    /// * `display_id` - The `CGDirectDisplayID` of the display.
    ///
    /// # Returns
    ///
    /// `Ok(u64)` with the space ID if successful, otherwise `Err(Error)`.
    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId>;
    /// Returns `true` if the current space on the given display is a native fullscreen space.
    fn is_fullscreen_space(&self, display_id: CGDirectDisplayID) -> bool;
    /// Centers the mouse cursor on a given window within its display bounds if it's not already within the window.
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` to center the mouse on.
    /// * `display_bounds` - The `CGRect` representing the bounds of the display the window is on.
    fn warp_mouse(&self, origin: Origin);
    /// Focuses the desktop of the display with `bounds`, which makes that
    /// display the active one without moving the pointer.
    ///
    /// # Errors
    ///
    /// Returns an `Error` when no desktop window covers the display's centre,
    /// as when Finder does not draw the desktop.
    fn focus_desktop(&self, bounds: IRect) -> Result<()>;
    /// Adds existing windows for a given application, potentially resolving unresolved windows.
    ///
    /// # Arguments
    ///
    /// * `app` - A mutable reference to the `Application` whose windows are to be added.
    /// * `spaces` - A slice of space IDs to query for windows.
    /// * `config` - The current Paneru configuration, used to evaluate window rules.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Window>)` containing the found and added windows, otherwise `Err(Error)`.
    fn find_existing_application_windows(
        &self,
        app: &mut Application,
        spaces: &[WorkspaceId],
        config: &Config,
    ) -> Result<(Vec<Window>, Vec<WinID>)>;
    /// Finds the `WinID` of a window at a given screen point.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the found window's ID if successful, otherwise `Err(Error)`.
    fn find_window_at_point(&self, point: &CGPoint) -> Result<WinID>;
    /// Returns a list of `WinID`s for all windows in a given workspace (space).
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the space to query.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<WinID>)` containing the list of window IDs, otherwise `Err(Error)`.
    fn windows_in_workspace(&self, space_id: WorkspaceId) -> Result<Vec<WinID>>;
    /// Whether the window server has the window ordered in. A failed query is
    /// an `Err`, never an answer: it is no evidence the window exists or is gone.
    fn window_ordered_in(&self, window_id: WinID) -> Result<bool>;

    /// Sends an `Event::Exit` to the event loop, signaling the application to quit.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the exit event is sent successfully, otherwise `Err(Error)`.
    fn quit(&self) -> Result<()>;

    fn setup_config_watcher(&self, path: &Path) -> Result<Box<dyn Watcher>>;

    /// Returns the current cursor position in absolute CG coordinates,
    /// or `None` if the position cannot be determined.
    fn cursor_position(&self) -> Option<CGPoint>;

    /// Whether the left mouse button is down right now, as the window
    /// server sees it, whatever events the tap did or did not deliver.
    fn left_button_down(&self) -> bool;

    fn dim_windows(&self, windows: &[WinID], level: f32);

    fn windows_on_screen(&self) -> Option<Vec<WinID>>;

    /// Every window that exists right now, on any Space, minimized or not.
    fn all_window_ids(&self) -> Vec<WinID>;

    /// Waits up to `timeout` for every window move and resize asked for so
    /// far to reach its app. Returns whether all of them did.
    fn flush_window_writes(&self, timeout: Duration) -> bool;
}

/// `WindowManager` is a Bevy resource that holds a boxed `WindowManagerApi` trait object.
/// It allows for dynamic dispatch to the OS-specific window management implementation.
#[derive(Deref, DerefMut, Resource)]
pub struct WindowManager(pub Box<dyn WindowManagerApi>);

/// `WindowManagerOS` is the macOS-specific implementation of the `WindowManagerApi` trait.
/// It directly interacts with the macOS `SkyLight` and Accessibility APIs to manage windows.
pub struct WindowManagerOS {
    main_cid: ConnID,
    event_sender: EventSender,
    resolver: app::WindowResolver,
}

impl WindowManagerOS {
    /// Creates a new `WindowManagerOS` instance.
    /// It initializes the main connection ID to the macOS `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `event_sender` - The `EventSender` to dispatch events from the window manager.
    ///
    /// # Returns
    ///
    /// A new `WindowManagerOS` instance.
    pub fn new(event_sender: EventSender) -> Self {
        let main_cid = unsafe { SLSMainConnectionID() };
        debug!("My connection id: {main_cid}");
        let resolver = app::WindowResolver::new(event_sender.clone());

        Self {
            main_cid,
            event_sender,
            resolver,
        }
    }

    /// Retrieves a list of space IDs for a given display UUID.
    /// It queries the `SkyLight` API for managed display spaces and filters by the provided UUID.
    ///
    /// # Arguments
    ///
    /// * `uuid` - A reference to the `CFString` representing the display's UUID.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<u64>)` with the list of space IDs if successful, otherwise `Err(Error)` if the spaces cannot be retrieved or the display is not found.
    fn display_space_list(&self, uuid: &CFString) -> Result<Vec<WorkspaceId>> {
        let display_spaces = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(self.main_cid) })
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or(Error::PermissionDenied(format!(
                "can not copy managed display spaces for {}.",
                self.main_cid
            )))?;
        let uuid = uuid.to_string();

        let display = display_spaces.iter().find(|display| {
            let identifier = display
                .get(&CFString::from_static_str("Display Identifier"))
                .map(|name| name.to_string());
            identifier.is_some_and(|identifier| {
                // FIXME: Sometimes the main display simply has the name 'Main'.
                identifier == "Main" || identifier == uuid
            })
        });
        let Some(display) = display else {
            return Err(Error::PermissionDenied(format!(
                "could not get any displays for {}",
                self.main_cid
            )));
        };
        debug!("found display with uuid '{uuid}'");

        let display = unsafe {
            display.cast_unchecked::<CFString, CFArray<CFDictionary<CFString, CFNumber>>>()
        };
        let Some(spaces) = display.get(&CFString::from_static_str("Spaces")) else {
            return Err(Error::PermissionDenied(format!(
                "could not get any spaces for display '{uuid}'",
            )));
        };

        let spaces = spaces
            .iter()
            .filter_map(|space| {
                space
                    .get(&CFString::from_static_str("id64"))
                    .and_then(|id| id.as_i64().and_then(|value| u64::try_from(value).ok()))
            })
            .collect::<Vec<WorkspaceId>>();
        debug!(
            "spaces [{}]",
            spaces
                .iter()
                .map(|id| format!("{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(spaces)
    }

    /// Retrieves the UUID of the active menu bar display.
    /// This typically corresponds to the display where the primary menu bar is located.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID if successful, otherwise `Err(Error)` if the active display cannot be determined.
    fn active_display_uuid(&self) -> Result<CFRetained<CFString>> {
        unsafe {
            let ptr = SLSCopyActiveMenuBarDisplayIdentifier(self.main_cid);
            let ptr = NonNull::new(ptr.cast_mut()).ok_or(Error::NotFound(format!(
                "can not find active display for connection {}.",
                self.main_cid
            )))?;
            Ok(CFRetained::from_raw(ptr))
        }
    }

    /// Returns the connection ID (`ConnID`) for a given process serial number (`PSN`).
    ///
    /// # Arguments
    ///
    /// * `psn` - The `ProcessSerialNumber` of the process.
    ///
    /// # Returns
    ///
    /// `Some(ConnID)` if the connection ID is found, otherwise `None`.
    fn connection_for_process(&self, psn: ProcessSerialNumber) -> Option<ConnID> {
        let mut connection: ConnID = 0;
        unsafe { SLSGetConnectionIDForPSN(self.main_cid, &psn, &mut connection) };
        (connection != 0).then_some(connection)
    }
}

impl WindowManagerApi for WindowManagerOS {
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application> {
        let connection = self.connection_for_process(process.psn());
        ApplicationOS::new(
            connection,
            process,
            &self.event_sender,
            self.resolver.sender(),
        )
        .map(|app| Application::new(Box::new(app)))
    }

    /// Returns child windows of the main window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn get_associated_windows(&self, window_id: WinID) -> Vec<WinID> {
        trace!("for window {window_id}");
        let windows =
            unsafe { CFRetained::from_raw(SLSCopyAssociatedWindows(self.main_cid, window_id)) };
        windows.into_iter().filter_map(|id| id.as_i32()).collect()
    }

    /// Retrieves a list of all currently present displays, along with their associated spaces.
    ///
    /// # Returns
    ///
    /// A `Vec<Self>` containing `Display` objects for all present displays.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)> {
        let mut count = 0u32;
        unsafe {
            CGGetActiveDisplayList(0, null_mut(), &raw mut count);
        }
        if count < 1 {
            return vec![];
        }
        let mut displays = Vec::with_capacity(count.try_into().unwrap());
        unsafe {
            CGGetActiveDisplayList(count, displays.as_mut_ptr(), &raw mut count);
            displays.set_len(count.try_into().unwrap());
        }
        displays
            .into_iter()
            .filter_map(|id| {
                let bounds = CGDisplayBounds(id);
                let mut menubar_height: u32 = 0;
                unsafe { SLSGetDisplayMenubarHeight(id, &raw mut menubar_height) };
                debug!("menubar height: {menubar_height}");
                let workspaces = Display::uuid_from_id(id)
                    .and_then(|uuid| self.display_space_list(uuid.as_ref()))
                    .ok()?;

                Some((
                    Display::new(id, irect_from(bounds), menubar_height.cast_signed()),
                    workspaces,
                ))
            })
            .collect()
    }

    /// Retrieves the `CGDirectDisplayID` of the active menu bar display.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    #[instrument(level = Level::TRACE, skip_all, ret)]
    fn active_display_id(&self) -> Result<u32> {
        let uuid = self.active_display_uuid()?;
        Display::id_from_uuid(&uuid)
    }

    /// Retrieves the ID of the current active space on this display.
    ///
    /// # Returns
    ///
    /// `Ok(u64)` with the space ID if successful, otherwise `Err(Error)`.
    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId> {
        Display::uuid_from_id(display_id).map(|uuid| unsafe {
            SLSManagedDisplayGetCurrentSpace(self.main_cid, &raw const *uuid)
        })
    }

    fn is_fullscreen_space(&self, display_id: CGDirectDisplayID) -> bool {
        self.active_display_space(display_id)
            .is_ok_and(|space_id| unsafe { SLSSpaceGetType(self.main_cid, space_id) } == 4)
    }

    /// Centers the mouse cursor on the window if it's not already within the window's bounds.
    #[instrument(level = Level::DEBUG, skip_all, fields(window))]
    fn warp_mouse(&self, origin: Origin) {
        // Drop the local-event suppression interval to zero so HID mouse
        // events resume immediately after the warp. The default 250ms
        // interval drops physical mouse motion in that window, making the
        // cursor feel "stuck" at the warped position.
        #[allow(deprecated)]
        objc2_core_graphics::CGSetLocalEventsSuppressionInterval(0.0);
        CGWarpMouseCursorPosition(origin_to(origin));
        // Re-associate the mouse and cursor so HID input continues to drive
        // the cursor immediately after the warp.
        CGAssociateMouseAndMouseCursorPosition(true);
    }

    fn focus_desktop(&self, bounds: IRect) -> Result<()> {
        let (window_id, pid) = desktop_window_at(bounds.center())
            .ok_or_else(|| Error::NotFound(format!("no desktop window covers {bounds:?}")))?;
        windows::focus_window(process::psn_for_pid(pid)?, window_id);
        Ok(())
    }

    /// Adds existing windows for a given application, attempting to resolve any that are not yet found.
    /// It compares the application's reported window list with the global window list and uses brute-forcing if necessary.
    ///
    /// # Arguments
    ///
    /// * `app` - A mutable reference to the `Application` whose windows are to be added.
    /// * `spaces` - A slice of space IDs to query.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Window>)` containing the found windows, otherwise `Err(Error)`.
    fn find_existing_application_windows(
        &self,
        app: &mut Application,
        spaces: &[WorkspaceId],
        config: &Config,
    ) -> Result<(Vec<Window>, Vec<WinID>)> {
        let global_window_list = existing_application_window_list(self.main_cid, app, spaces)?;
        if global_window_list.is_empty() {
            return Err(Error::InvalidInput(format!("No windows found for {app}")));
        }
        debug!("{app} has global windows: {global_window_list:?}");

        let found_windows = app.window_list(config);
        if found_windows.len() == global_window_list.len() {
            debug!("All windows for {:?} are now resolved", app.psn());
            return Ok((found_windows, vec![]));
        }

        let find_window = |window_id| found_windows.iter().find(|window| window.id() == window_id);
        let offscreen_windows = global_window_list
            .into_iter()
            .filter(|&window_id| find_window(window_id).is_none())
            .collect::<Vec<_>>();
        debug!(
            "{:?} has {} windows that are not yet resolved",
            app.psn(),
            offscreen_windows.len()
        );
        Ok((found_windows, offscreen_windows))
    }

    /// Finds a window at a given screen point using `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the found window's ID if successful, otherwise `Err(Error)`.
    fn find_window_at_point(&self, point: &CGPoint) -> Result<WinID> {
        let mut window_id: WinID = 0;
        let mut window_conn_id: ConnID = 0;
        let mut window_point = CGPoint { x: 0f64, y: 0f64 };
        unsafe {
            SLSFindWindowAndOwner(
                self.main_cid,
                0, // filter window id
                1,
                0,
                point,
                &mut window_point,
                &mut window_id,
                &mut window_conn_id,
            )
        }
        .to_result(function_name!())?;
        if self.main_cid == window_conn_id {
            unsafe {
                SLSFindWindowAndOwner(
                    self.main_cid,
                    window_id,
                    -1,
                    0,
                    point,
                    &mut window_point,
                    &mut window_id,
                    &mut window_conn_id,
                )
            }
            .to_result(function_name!())?;
        }
        if window_id == 0 {
            Err(Error::invalid_window(&format!(
                "could not find a window at {point:?}",
            )))
        } else {
            Ok(window_id)
        }
    }

    /// Returns a list of `WinID`s for all windows in a given workspace (space).
    fn windows_in_workspace(&self, space_id: WorkspaceId) -> Result<Vec<WinID>> {
        space_window_list_for_connection(self.main_cid, &[space_id], None, true)
    }

    fn window_ordered_in(&self, window_id: WinID) -> Result<bool> {
        let mut ordered_in = 0;
        unsafe { SLSWindowIsOrderedIn(self.main_cid, window_id, &mut ordered_in) }
            .to_result(function_name!())?;
        Ok(ordered_in != 0)
    }

    fn quit(&self) -> Result<()> {
        self.event_sender.send(Event::Exit)
    }

    fn cursor_position(&self) -> Option<CGPoint> {
        let mut cursor = CGPoint::default();
        unsafe { SLSGetCurrentCursorLocation(self.main_cid, &mut cursor) }
            .to_result(function_name!())
            .ok()?;
        Some(cursor)
    }

    fn left_button_down(&self) -> bool {
        CGEventSource::button_state(
            CGEventSourceStateID::CombinedSessionState,
            CGMouseButton::Left,
        )
    }

    fn setup_config_watcher(&self, path: &Path) -> Result<Box<dyn Watcher>> {
        let setup = notify::Config::default()
            .with_poll_interval(Duration::from_secs(3))
            .with_follow_symlinks(false);
        let config_handler = ConfigHandler(self.event_sender.clone());
        let symlink = symlink_target(path);

        let mut watcher = if let Some(symlink) = symlink {
            setup.with_follow_symlinks(true);
            let mut watcher = notify::PollWatcher::new(config_handler, setup)?;
            debug!("watching symlink target {} for changes.", symlink.display());
            watcher.watch(&symlink, RecursiveMode::NonRecursive)?;

            Ok::<Box<dyn Watcher>, Error>(Box::new(watcher))
        } else {
            Ok::<Box<dyn Watcher>, Error>(Box::new(notify::RecommendedWatcher::new(
                config_handler,
                setup,
            )?))
        }?;
        debug!("watching config file {} for changes.", path.display());
        watcher.watch(path, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }

    /// level: 0.0 = normal, 1.0 = bright, -1.0 = dark
    fn dim_windows(&self, windows: &[WinID], level: f32) {
        let Ok(count) = isize::try_from(windows.len()) else {
            return;
        };
        let levels = vec![level; windows.len()];

        _ = unsafe {
            SLSSetWindowListBrightness(self.main_cid, windows.as_ptr(), levels.as_ptr(), count)
        }
        .to_result(function_name!())
        .inspect_err(|err| debug!("{err}"));
    }

    fn windows_on_screen(&self) -> Option<Vec<WinID>> {
        window_ids(
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        )
    }

    fn flush_window_writes(&self, timeout: Duration) -> bool {
        windows::flush_window_writes(timeout)
    }

    fn all_window_ids(&self) -> Vec<WinID> {
        window_ids(CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements)
            .unwrap_or_default()
    }
}

/// The ID and owner of the on-screen desktop-icon window under `point`.
/// Finder draws one over each display's desktop.
fn desktop_window_at(point: Origin) -> Option<(WinID, Pid)> {
    let level = CGWindowLevelForKey(CGWindowLevelKey::DesktopIconWindowLevelKey);
    let window_list_info =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)?;
    let array = unsafe { window_list_info.cast_unchecked::<CFDictionary<CFString, CFType>>() };
    array.iter().find_map(|dict| {
        let number = |key: &CFString| dict.get(key)?.downcast::<CFNumber>().ok()?.as_i32();
        if number(unsafe { kCGWindowLayer })? != level {
            return None;
        }
        let bounds = dict
            .get(unsafe { kCGWindowBounds })?
            .downcast::<CFDictionary>()
            .ok()?;
        let mut frame = CGRect::default();
        unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &raw mut frame) }
            .then_some(())?;
        if !irect_from(frame).contains(point) {
            return None;
        }
        Some((
            number(unsafe { kCGWindowNumber })?,
            number(unsafe { kCGWindowOwnerPID })?,
        ))
    })
}

/// The IDs of the windows `CGWindowListCopyWindowInfo` lists for `options`.
fn window_ids(options: CGWindowListOption) -> Option<Vec<WinID>> {
    let window_list_info = CGWindowListCopyWindowInfo(options, kCGNullWindowID);
    window_list_info.map(|window_info| {
        let array = unsafe { window_info.cast_unchecked::<CFDictionary<CFString, CFNumber>>() };
        array
            .iter()
            .filter_map(|dict| {
                dict.get(unsafe { kCGWindowNumber })
                    .and_then(|id| id.as_i32())
            })
            .collect::<Vec<_>>()
    })
}

/// Retrieves a list of window IDs for specified spaces and connection, with an option to include minimized windows.
/// This function uses `SkyLight` API calls to query windows based on their space, connection, and visibility tags.
///
/// # Arguments
///
/// * `main_cid` - The main connection ID.
/// * `spaces` - A slice of space IDs to query windows from.
/// * `cid` - An optional connection ID. If `None`, the main connection ID is used.
/// * `also_minimized` - A boolean indicating whether to include minimized windows in the result.
///
/// # Returns
///
/// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
fn space_window_list_for_connection(
    main_cid: ConnID,
    spaces: &[WorkspaceId],
    cid: Option<ConnID>,
    also_minimized: bool,
) -> Result<Vec<WinID>> {
    let iterator = window_iterator_for_connection(main_cid, spaces, cid, also_minimized)?;
    let count = spaces.len();
    let mut window_list = Vec::with_capacity(count);
    while unsafe { SLSWindowIteratorAdvance(&raw const *iterator) } {
        let tags = unsafe { SLSWindowIteratorGetTags(&raw const *iterator) };
        let attributes = unsafe { SLSWindowIteratorGetAttributes(&raw const *iterator) };
        let parent_wid: WinID = unsafe { SLSWindowIteratorGetParentID(&raw const *iterator) };
        let window_id: WinID = unsafe { SLSWindowIteratorGetWindowID(&raw const *iterator) };

        trace!(
            "id: {window_id} parent: {parent_wid} tags: 0x{tags:x} attributes: 0x{attributes:x}",
        );
        if found_valid_window(parent_wid, attributes, tags) {
            window_list.push(window_id);
        }
    }
    Ok(window_list)
}

fn window_iterator_for_connection(
    main_cid: ConnID,
    spaces: &[WorkspaceId],
    cid: Option<ConnID>,
    also_minimized: bool,
) -> Result<CFRetained<CFType>> {
    let space_list_ref = create_array(spaces, CFNumberType::SInt64Type)?;

    let mut set_tags = 0i64;
    let mut clear_tags = 0i64;
    let options = if also_minimized { 0x7 } else { 0x2 };
    let ptr = NonNull::new(unsafe {
        SLSCopyWindowsWithOptionsAndTags(
            main_cid,
            cid.unwrap_or(0),
            &raw const *space_list_ref,
            options,
            &mut set_tags,
            &mut clear_tags,
        )
    })
    .ok_or(Error::InvalidInput(format!(
        "{}: nullptr returned from SLSCopyWindowsWithOptionsAndTags.",
        function_name!()
    )))?;
    let window_list_ref = unsafe { CFRetained::from_raw(ptr) };

    let count = window_list_ref.count();
    if count == 0 {
        return Err(Error::NotFound(format!(
            "{}: zero windows returned",
            function_name!()
        )));
    }

    let query = unsafe {
        CFRetained::from_raw(SLSWindowQueryWindows(
            main_cid,
            &raw const *window_list_ref,
            count,
        ))
    };
    Ok(unsafe { CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into())) })
}

pub fn window_iterator_for_id(window_id: WinID) -> Option<CFRetained<CFType>> {
    let cid = unsafe { SLSMainConnectionID() };
    let windows = create_array(&[window_id], CFNumberType::SInt32Type).ok()?;
    let query = unsafe { CFRetained::from_raw(SLSWindowQueryWindows(cid, &raw const *windows, 1)) };
    Some(unsafe { CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into())) })
}

/// Determines if a window is valid based on its parent ID, attributes, and tags.
/// This function implements complex logic to filter out irrelevant or invalid windows.
///
/// # Arguments
///
/// * `parent_wid` - The parent window ID.
/// * `attributes` - The attributes of the window.
/// * `tags` - The tags associated with the window.
///
/// # Returns
///
/// `true` if the window is considered valid, `false` otherwise.
fn found_valid_window(parent_wid: WinID, attributes: i64, tags: i64) -> bool {
    parent_wid == 0
        && ((0 != (attributes & 0x2) || 0 != (tags & 0x0400_0000_0000_0000))
            && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x8000_0000))))
        || ((attributes == 0x0 || attributes == 0x1)
            && (0 != (tags & 0x1000_0000_0000_0000) || 0 != (tags & 0x0300_0000_0000_0000))
            && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x8000_0000))))
}

/// Retrieves a list of existing application window IDs for a given application.
/// It queries windows across all active displays and spaces associated with the application's connection.
///
/// # Arguments
///
/// * `cid` - The main connection ID.
/// * `app` - A reference to the `Application` for which to retrieve window IDs.
/// * `spaces` - A slice of space IDs to query.
///
/// # Returns
///
/// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
fn existing_application_window_list(
    cid: ConnID,
    app: &Application,
    spaces: &[WorkspaceId],
) -> Result<Vec<WinID>> {
    if spaces.is_empty() {
        return Err(Error::NotFound(format!(
            "{}: no spaces returned",
            function_name!()
        )));
    }
    space_window_list_for_connection(cid, spaces, app.connection(), true)
}

/// Wall-clock ceiling on a single application's brute-force scan. A scan
/// normally ends sooner, once it passes the app's newest element, so this only
/// stops an app with an unusually large accessibility tree from holding up
/// initialisation, which waits on these tasks.
const BRUTEFORCE_BUDGET: Duration = Duration::from_secs(2);

/// How many element ids in a row may hold no window's element before the scan
/// decides it has passed the app's newest element. Closed tabs leave gaps: a
/// Helium with about 70 tabs had a gap of 8768 ids below its newest window.
const BRUTEFORCE_TRAILING_GAP: u64 = 0x4000;

/// How many applications may run [`bruteforce_windows`] at the same time.
///
/// Each loop iteration is a synchronous, private-API AX round trip. Letting
/// every newly discovered app brute-force in parallel is bounded only by the
/// async compute pool's thread count (one per core), and a burst of that
/// many concurrent AX round trips at startup has been enough to pin tccd and
/// wedge `WindowServer`'s main thread past the watchdog's timeout. Capping
/// concurrency keeps the aggregate call rate bounded no matter how many apps
/// need a scan.
const MAX_CONCURRENT_BRUTEFORCE: usize = 2;

/// Blocking counting semaphore gating concurrent [`bruteforce_windows`] scans.
struct BruteforceGate {
    available: Mutex<usize>,
    freed: Condvar,
}

impl BruteforceGate {
    fn get() -> &'static BruteforceGate {
        static GATE: OnceLock<BruteforceGate> = OnceLock::new();
        GATE.get_or_init(|| BruteforceGate {
            available: Mutex::new(MAX_CONCURRENT_BRUTEFORCE),
            freed: Condvar::new(),
        })
    }

    /// Blocks until a scan slot is free, then reserves it until the returned
    /// guard is dropped.
    fn acquire(&self) -> BruteforcePermit<'_> {
        let mut available = self.available.lock().expect("gate mutex poisoned");
        while *available == 0 {
            available = self.freed.wait(available).expect("gate mutex poisoned");
        }
        *available -= 1;
        BruteforcePermit(self)
    }
}

struct BruteforcePermit<'a>(&'a BruteforceGate);

impl Drop for BruteforcePermit<'_> {
    fn drop(&mut self) {
        *self.0.available.lock().expect("gate mutex poisoned") += 1;
        self.0.freed.notify_one();
    }
}

/// Attempts to find and add unresolved windows for a given application by brute-forcing `element_id` values.
/// This is a workaround for macOS API limitations that do not return `AXUIElementRef` for windows on inactive spaces.
///
/// # Arguments
///
/// * `pid` - The process ID of the application whose windows are to be brute-forced.
/// * `bundle_id` - The bundle identifier of the application, if known.
/// * `window_list` - A mutable vector of `WinID`s representing the expected global window list; found windows are removed from this list.
/// * `config` - The current Paneru configuration, used to evaluate window rules.
pub fn bruteforce_windows(
    pid: Pid,
    bundle_id: Option<&str>,
    mut window_list: Vec<WinID>,
    config: &Config,
) -> Vec<Window> {
    const MAGIC: u32 = 0x636f_636f;
    const BUFSIZE: isize = 0x14;
    debug!("{pid} has unresolved window on other desktops, bruteforcing them.");

    // Blocks until fewer than `MAX_CONCURRENT_BRUTEFORCE` apps are scanning.
    let _permit = BruteforceGate::get().acquire();

    //
    // NOTE: MacOS API does not return AXUIElementRef of windows on inactive spaces. However,
    // we can just brute-force the element_id and create the AXUIElementRef ourselves.
    //  https://github.com/decodism
    //  https://github.com/lwouis/alt-tab-macos/issues/1324#issuecomment-2631035482
    //

    let Some(data_ref) = CFMutableData::new(None, BUFSIZE) else {
        error!("error creating mutable data");
        return Vec::new();
    };
    CFMutableData::increase_length(data_ref.deref().into(), BUFSIZE);

    let data = unsafe {
        from_raw_parts_mut(
            CFMutableData::mutable_byte_ptr(data_ref.deref().into()),
            BUFSIZE as usize,
        )
    };
    let bytes = pid.to_ne_bytes();
    data[0x0..bytes.len()].copy_from_slice(&bytes);
    let bytes = MAGIC.to_ne_bytes();
    data[0x8..0x8 + bytes.len()].copy_from_slice(&bytes);

    let deadline = Instant::now() + BRUTEFORCE_BUDGET;

    // `bruteforce_windows` runs on `AsyncComputeTaskPool` worker threads, which
    // have no CFRunLoop to drain a thread-local `NSAutoreleasePool`.
    objc2::rc::autoreleasepool(|_| {
        let (windows, end) = scan_elements(
            &mut window_list,
            deadline,
            |element_id| {
                // Every call is a synchronous cross-process AX round trip.
                let bytes = element_id.to_ne_bytes();
                data[0xc..0xc + bytes.len()].copy_from_slice(&bytes);
                let element_ref = AXUIWrapper::from_retained(unsafe {
                    _AXUIElementCreateWithRemoteToken(data_ref.as_ref())
                })
                .ok()?;
                let window_id = try_ax_window_id(element_ref.as_ptr())?;
                Some((window_id, element_ref))
            },
            |element_ref| {
                WindowOS::new_with_config(&element_ref, config, bundle_id)
                    .inspect_err(|err| warn!("{err}"))
                    .ok()
                    .map(|window| Window::new(Box::new(window)))
            },
        );
        if end != ScanEnd::Resolved {
            warn!(
                "{pid}: giving up the brute-force scan ({end:?}) with {} window(s) unresolved: \
                 {window_list:?}",
                window_list.len()
            );
        }
        windows
    })
}

/// Why [`scan_elements`] stopped.
#[derive(Debug, PartialEq, Eq)]
enum ScanEnd {
    /// Every listed window resolved.
    Resolved,
    /// The scan passed the app's newest element before reaching this id.
    PastLastElement(u64),
    /// [`BRUTEFORCE_BUDGET`] ran out at this id.
    Deadline(u64),
}

/// Walks element ids upward from 0 and claims each element that `accept` takes
/// for a window still in `window_list`.
///
/// An app numbers its elements in creation order, so a window opened late can
/// sit far above the app's other windows, and above 0x7fff. The walk ends when
/// every window resolves, or [`BRUTEFORCE_TRAILING_GAP`] ids after the last
/// element that belongs to any window. A window `SkyLight` lists but that never
/// resolves to an AX element (a panel, helper, anything non-AX) so costs only
/// the app's own elements plus the gap.
fn scan_elements<E, T>(
    window_list: &mut Vec<WinID>,
    deadline: Instant,
    mut element_at: impl FnMut(u64) -> Option<(WinID, E)>,
    mut accept: impl FnMut(E) -> Option<T>,
) -> (Vec<T>, ScanEnd) {
    let mut found = Vec::new();
    let mut last_live = 0;
    let mut element_id = 0;
    let end = loop {
        if window_list.is_empty() {
            break ScanEnd::Resolved;
        }
        if element_id - last_live > BRUTEFORCE_TRAILING_GAP {
            break ScanEnd::PastLastElement(element_id);
        }
        // Checked periodically only: `Instant::now` can itself be a syscall.
        if element_id.is_multiple_of(256) && Instant::now() >= deadline {
            break ScanEnd::Deadline(element_id);
        }
        if let Some((window_id, element)) = element_at(element_id) {
            last_live = element_id;
            if let Some(window) =
                claim_unresolved_window(window_list, window_id, || accept(element))
            {
                debug!("Found window {window_id:?}");
                found.push(window);
            }
        }
        element_id += 1;
    };
    (found, end)
}

/// Removes `window_id` from `window_list` only once `accept` takes the element,
/// because an app can list another element with the window's id before the
/// window itself, as Notes does with an outline.
fn claim_unresolved_window<T>(
    window_list: &mut Vec<WinID>,
    window_id: WinID,
    accept: impl FnOnce() -> Option<T>,
) -> Option<T> {
    let index = window_list.iter().position(|&id| id == window_id)?;
    let accepted = accept()?;
    window_list.remove(index);
    Some(accepted)
}

/// Checks if the application has Accessibility privileges without showing UI.
///
/// # Returns
///
/// `true` if Accessibility privileges are granted, `false` otherwise.
pub fn check_ax_privilege() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Requests Accessibility privileges once through the native macOS prompt.
///
/// Subsequent permission polling must use [`check_ax_privilege`] so the system
/// dialog is not requested repeatedly while the application waits.
pub fn request_ax_privilege() -> bool {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt
            .cast::<CFString>()
            .as_ref()
            .unwrap()];
        let values = [kCFBooleanTrue.unwrap()];
        let opts = CFDictionary::from_slices(&keys, &values);
        AXIsProcessTrustedWithOptions((&raw const *opts).cast())
    }
}

fn accessibility_reset_command(bundle_id: Option<&str>) -> Option<std::process::Command> {
    let bundle_id = bundle_id.filter(|bundle_id| !bundle_id.is_empty())?;
    let mut command = std::process::Command::new("/usr/bin/tccutil");
    command.args(["reset", "Accessibility", bundle_id]);
    Some(command)
}

/// Resets the TCC Accessibility entry for the running application's bundle identity.
///
/// Rebuilding the binary or switching between Homebrew and Cargo changes the
/// ad-hoc code-signing hash (`cdhash`), leaving a stale TCC entry that
/// prevents macOS from honoring the existing toggle or re-displaying the
/// system prompt until the entry is cleared.
pub fn reset_ax_privilege() -> bool {
    let bundle_id = NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|bundle_id| bundle_id.to_string());
    let Some(mut command) = accessibility_reset_command(bundle_id.as_deref()) else {
        warn!("unable to reset Accessibility entry without a bundle identity");
        return false;
    };
    match command.output() {
        Ok(output) if output.status.success() => {
            info!(
                bundle_id = bundle_id.as_deref(),
                "reset TCC Accessibility entry"
            );
            true
        }
        Ok(output) => {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "tccutil reset Accessibility failed"
            );
            false
        }
        Err(error) => {
            warn!(%error, "unable to run tccutil to reset Accessibility entry");
            false
        }
    }
}

/// Checks if the macOS "Displays have separate Spaces" option is enabled.
/// This is crucial for the window manager's functionality, as Paneru relies on independent spaces per display.
///
/// # Returns
///
/// `true` if separate spaces are enabled, `false` otherwise.
pub fn check_separate_spaces() -> bool {
    unsafe {
        let cid = SLSMainConnectionID();
        SLSGetSpaceManagementMode(cid) == 1
    }
}

/// `ConfigHandler` is an implementation of `notify::EventHandler` that reloads the application configuration
/// when the configuration file changes. It also dispatches a `ConfigRefresh` event.
struct ConfigHandler(EventSender);

impl notify::EventHandler for ConfigHandler {
    /// Handles file system events for the configuration file. When the content changes, it reloads the configuration.
    /// Specifically, it responds to `ModifyKind::Data(DataChange::Content)` events.
    ///
    /// # Arguments
    ///
    /// * `event` - The result of a file system event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        if let Ok(event) = event {
            _ = self.0.send(Event::ConfigRefresh(event)).inspect_err(|err| {
                warn!("error sending config refresh: {err}");
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::NSBundle;

    use std::time::{Duration, Instant};

    use super::{
        BRUTEFORCE_TRAILING_GAP, ScanEnd, accessibility_reset_command, claim_unresolved_window,
        irect_from, scan_elements,
    };
    use crate::platform::WinID;

    #[test]
    fn accessibility_reset_targets_embedded_bundle_identity() {
        let bundle_id = NSBundle::mainBundle()
            .bundleIdentifier()
            .expect("the engine test binary embeds its bundle identity")
            .to_string();
        #[cfg(not(feature = "miri-developer"))]
        let embedded_plist = include_str!("../assets/Info.plist");
        #[cfg(feature = "miri-developer")]
        let embedded_plist = include_str!("../assets/Info.developer.plist");
        assert!(embedded_plist.contains(&format!("<string>{bundle_id}</string>")));

        #[cfg(not(feature = "miri-developer"))]
        assert_eq!(bundle_id, "io.github.samanyudas.miri.engine");
        #[cfg(feature = "miri-developer")]
        assert_eq!(bundle_id, "io.github.samanyudas.miri.dev.engine");

        let command = accessibility_reset_command(Some(&bundle_id))
            .expect("the runtime bundle identity permits a scoped reset");
        assert_eq!(command.get_program(), "/usr/bin/tccutil");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["reset", "Accessibility", bundle_id.as_str()]
        );
    }

    #[test]
    fn rejected_element_leaves_window_unresolved_for_a_later_element() {
        let mut window_list = vec![40966, 100];

        let outline: Option<&str> = claim_unresolved_window(&mut window_list, 40966, || None);
        assert!(outline.is_none());
        assert_eq!(window_list, vec![40966, 100]);

        let window = claim_unresolved_window(&mut window_list, 40966, || Some("AXWindow"));
        assert_eq!(window, Some("AXWindow"));
        assert_eq!(window_list, vec![100]);
    }

    /// Element ids and window ids from a Helium with about 70 tabs on
    /// 27 October 2026, where the newest window sat above 0x7fff.
    fn helium_element(element_id: u64) -> Option<(WinID, WinID)> {
        let window_id = match element_id {
            44 => 339,
            45..=19077 if element_id.is_multiple_of(97) => 339,
            27845 => 55561,
            32890 => 62288,
            _ => return None,
        };
        Some((window_id, window_id))
    }

    #[test]
    fn scan_finds_windows_past_a_gap_and_above_0x7fff() {
        let mut window_list = vec![62288, 55561];

        let (found, end) = scan_elements(&mut window_list, far_deadline(), helium_element, Some);

        assert_eq!(found, vec![55561, 62288]);
        assert_eq!(end, ScanEnd::Resolved);
        assert!(window_list.is_empty());
    }

    #[test]
    fn scan_of_a_window_that_never_resolves_stops_after_the_apps_last_element() {
        let mut window_list = vec![41068];
        let mut calls = 0;

        let (found, end) = scan_elements(
            &mut window_list,
            far_deadline(),
            |element_id| {
                calls += 1;
                (element_id <= 58).then_some((41068, ()))
            },
            |()| None::<()>,
        );

        assert!(found.is_empty());
        assert_eq!(
            end,
            ScanEnd::PastLastElement(58 + BRUTEFORCE_TRAILING_GAP + 1)
        );
        assert_eq!(calls, 58 + BRUTEFORCE_TRAILING_GAP + 1);
        assert_eq!(window_list, vec![41068]);
    }

    #[test]
    fn scan_stops_at_the_deadline() {
        let mut window_list = vec![62288];

        let (found, end) = scan_elements(&mut window_list, Instant::now(), helium_element, Some);

        assert!(found.is_empty());
        assert_eq!(end, ScanEnd::Deadline(0));
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn accessibility_reset_rejects_missing_bundle_identity() {
        assert!(accessibility_reset_command(None).is_none());
    }

    #[test]
    fn accessibility_reset_rejects_empty_bundle_identity() {
        assert!(accessibility_reset_command(Some("")).is_none());
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
    }

    #[test]
    fn irect_from_keeps_odd_sizes() {
        let frame = irect_from(rect(8.0, 30.0, 629.0, 401.0));
        assert_eq!((frame.min.x, frame.min.y), (8, 30));
        assert_eq!((frame.width(), frame.height()), (629, 401));
    }

    #[test]
    fn irect_from_keeps_odd_sizes_at_negative_origins() {
        let frame = irect_from(rect(-643.0, -1003.0, 629.0, 401.0));
        assert_eq!((frame.min.x, frame.min.y), (-643, -1003));
        assert_eq!((frame.width(), frame.height()), (629, 401));
    }
}
