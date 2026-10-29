use accessibility_sys::{
    AXUIElementCreateApplication, AXUIElementRef, AXValueCreate, AXValueGetValue,
    kAXChildrenAttribute, kAXCloseButtonAttribute, kAXErrorAttributeUnsupported, kAXErrorNoValue,
    kAXErrorNotImplemented, kAXErrorSuccess, kAXFloatingWindowSubrole, kAXPositionAttribute,
    kAXRaiseAction, kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXToolbarRole,
    kAXUnknownRole, kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
};
use bevy::ecs::component::Component;
use bevy::math::IRect;
use core::ptr::NonNull;
use derive_more::{DerefMut, with_trait::Deref};
use mockall::automock;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize, Type,
    kCFBooleanFalse, kCFBooleanTrue,
};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;
use stdext::function_name;
use stdext::sync::rw_lock::RwLockExt;
use tracing::{Level, debug, instrument, trace, warn};

use super::skylight::{
    _AXUIElementGetWindow, _SLPSSetFrontProcessWithOptions, AXUIElementCopyAttributeValue,
    AXUIElementPerformAction, AXUIElementSetAttributeValue, SLPSPostEventRecordTo,
    SLSWindowIteratorAdvance,
};
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::manager::{Origin, Size, irect_from};
use crate::platform::{OSStatus, Pid, ProcessSerialNumber, WinID, macos_major_version};
use crate::util::{AXUIAttributes, AXUIWrapper, MacResult};

/// The registry lock only looks up per-app locks; AX calls never hold it.
static ENHANCED_UI_STATES: LazyLock<Mutex<HashMap<Pid, Arc<Mutex<EnhancedUiState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Default)]
struct EnhancedUiState {
    active_operations: usize,
    /// Permanently cache a confirmed false value, but never an AX read error.
    absent: bool,
    /// Retained after a failed restoration so our own false value isn't cached.
    restore_pending: bool,
}

impl EnhancedUiState {
    fn acquire(
        &mut self,
        read_enabled: impl FnOnce() -> Result<bool>,
        disable: impl FnOnce(),
    ) -> Result<bool> {
        if self.absent {
            return Ok(false);
        }
        if self.active_operations > 0 {
            self.active_operations += 1;
            return Ok(true);
        }
        if !self.restore_pending && !read_enabled()? {
            self.absent = true;
            return Ok(false);
        }
        // AX setters can change app state before returning an error. Once a
        // write is attempted, owe a restoration even if it reports failure.
        self.restore_pending = true;
        disable();
        self.active_operations = 1;
        Ok(true)
    }

    fn release(&mut self, restore: impl FnOnce() -> Result<()>) -> Result<()> {
        self.active_operations -= 1;
        if self.active_operations == 0 {
            restore()?;
            self.restore_pending = false;
        }
        Ok(())
    }
}

/// Owns one acquisition, without keeping the PID lock across window operations.
struct EnhancedUiGuard<F: FnOnce() -> Result<()>> {
    state: Arc<Mutex<EnhancedUiState>>,
    restore: Option<F>,
    pid: Pid,
}

impl<F: FnOnce() -> Result<()>> Drop for EnhancedUiGuard<F> {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = state.release(restore);
            trace!(
                target: "paneru::ax_diagnostics",
                pid = self.pid,
                active_operations = state.active_operations,
                restore_pending = state.restore_pending,
                "released enhanced UI guard"
            );
            let _ = result.inspect_err(|err| {
                warn!(pid = self.pid, "error restoring enhanced UI: {err}");
            });
        }
    }
}

fn enhanced_ui_state(pid: Pid) -> Arc<Mutex<EnhancedUiState>> {
    ENHANCED_UI_STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(pid)
        .or_default()
        .clone()
}

/// Retires a terminated app's cached flag and restoration debt before PID reuse.
/// Outstanding window operations keep their own `Arc` and finish independently.
pub(crate) fn forget_enhanced_ui_state(pid: Pid) {
    ENHANCED_UI_STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&pid);
}

fn acquire_enhanced_ui<F: FnOnce() -> Result<()>>(
    state: Arc<Mutex<EnhancedUiState>>,
    pid: Pid,
    read_enabled: impl FnOnce() -> Result<bool>,
    disable: impl FnOnce() -> Result<()>,
    restore: F,
) -> Result<Option<EnhancedUiGuard<F>>> {
    let mut locked = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let acquired = locked.acquire(read_enabled, || {
        if let Err(err) = disable() {
            warn!(
                pid,
                "error disabling enhanced UI; retaining restoration guard: {err}"
            );
        }
    })?;
    trace!(
        target: "paneru::ax_diagnostics",
        pid,
        acquired,
        active_operations = locked.active_operations,
        absent = locked.absent,
        restore_pending = locked.restore_pending,
        "acquired enhanced UI guard"
    );
    drop(locked);
    Ok(acquired.then(|| EnhancedUiGuard {
        state,
        restore: Some(restore),
        pid,
    }))
}

fn decode_enhanced_ui_read(ax_error: OSStatus, value: Option<bool>) -> Result<bool> {
    if ax_error == kAXErrorSuccess {
        value.ok_or_else(|| {
            Error::InvalidInput("nullptr while getting attribute AXEnhancedUserInterface.".into())
        })
    } else if ax_error == kAXErrorAttributeUnsupported || ax_error == kAXErrorNoValue {
        Ok(false)
    } else {
        ax_error
            .to_result("read AXEnhancedUserInterface")
            .map(|()| false)
    }
}

fn read_enhanced_ui(app_element: &CFRetained<AXUIWrapper>) -> Result<bool> {
    let name = CFString::from_static_str("AXEnhancedUserInterface");
    let mut attribute: *mut CFType = null_mut();
    let ax_error =
        unsafe { AXUIElementCopyAttributeValue(app_element.as_ptr(), &name, &mut attribute) };
    let value = NonNull::new(attribute).map(|ptr| {
        let boolean: CFRetained<CFBoolean> = unsafe { CFRetained::from_raw(ptr.cast()) };
        CFBoolean::value(&boolean)
    });
    decode_enhanced_ui_read(ax_error, value)
}

fn verify_enhanced_ui_write(
    enabled: bool,
    result: Result<()>,
    read_enabled: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    if result.is_err() && read_enabled().is_ok_and(|observed| observed == enabled) {
        return Ok(());
    }
    result
}

fn set_enhanced_ui(
    app_element: &CFRetained<AXUIWrapper>,
    enabled: bool,
    pid: Pid,
    window_id: WinID,
) -> Result<()> {
    let value = unsafe {
        if enabled {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    };
    let ax_error = unsafe {
        AXUIElementSetAttributeValue(
            app_element.as_ptr(),
            CFString::from_static_str("AXEnhancedUserInterface").as_ref(),
            value.unwrap(),
        )
    };
    trace!(
        target: "paneru::ax_diagnostics",
        pid,
        window_id,
        requested = enabled,
        ax_error,
        "AXEnhancedUserInterface write"
    );
    verify_enhanced_ui_write(
        enabled,
        ax_error.to_result("set AXEnhancedUserInterface"),
        || {
            let observed = read_enhanced_ui(app_element);
            if observed.as_ref().is_ok_and(|value| *value == enabled) {
                debug!(
                    pid,
                    window_id,
                    requested = enabled,
                    ax_error,
                    observed = ?observed,
                    "AXEnhancedUserInterface changed despite reported error"
                );
            } else {
                warn!(
                    pid,
                    window_id,
                    requested = enabled,
                    ax_error,
                    observed = ?observed,
                    "AXEnhancedUserInterface write failed verification"
                );
            }
            observed
        },
    )
}

/// macOS may partially apply an AX width increase when the requested right edge
/// would be far outside the display. Moving the partial result left by the
/// missing width before retrying gives `WindowServer` enough offscreen room.
///
/// Only retry when the first attempt actually grew the window. Fixed-size apps
/// otherwise look identical to this failure mode and must not be moved offscreen.
fn resize_staging_origin(
    previous_frame: IRect,
    actual_frame: IRect,
    target_width: i32,
) -> Option<Origin> {
    let actual_width = actual_frame.width();
    (actual_width > previous_frame.width() && actual_width < target_width).then(|| {
        actual_frame
            .min
            .with_x(actual_frame.min.x - (target_width - actual_width))
    })
}

#[derive(Debug)]
pub enum WindowPadding {
    Vertical(i32),
    Horizontal(i32),
}

#[automock]
pub trait WindowApi: Send + Sync {
    fn id(&self) -> WinID;
    fn frame(&self) -> IRect;
    fn element(&self) -> Option<CFRetained<AXUIWrapper>>;
    fn title(&self) -> Result<String>;
    /// Drops the cached title so the next [`Self::title`] reads it afresh.
    /// Called when the app reports the title changed.
    fn invalidate_title(&self);
    fn identifier(&self) -> Result<String>;
    fn child_role(&self) -> Result<bool>;
    /// Cached after the first successful read. Not a liveness check: use
    /// [`Self::is_alive`] for that.
    fn role(&self) -> Result<String>;
    fn subrole(&self) -> Result<String>;
    /// Whether the window's AX element still answers. Always a fresh
    /// cross-process read, never cached.
    fn is_alive(&self) -> bool;
    fn is_minimized(&self) -> bool;
    fn is_full_screen(&self) -> bool;
    fn reposition(&mut self, origin: Origin);
    fn resize(&mut self, size: Size);
    fn update_frame(&mut self) -> Result<IRect>;
    fn focus_without_raise(
        &self,
        psn: ProcessSerialNumber,
        currently_focused: &Window,
        focused_psn: ProcessSerialNumber,
    );
    fn focus_with_raise(&self, psn: ProcessSerialNumber);
    /// Raises the window in the OS z-order without changing focus. Used to
    /// shuffle the floating-vs-tiled tier order. Best-effort: AX raise can't
    /// lift a window above another app's frontmost window.
    fn raise_without_focus(&self);
    fn pid(&self) -> Result<Pid>;
    fn set_padding(&mut self, padding: WindowPadding);
    fn horizontal_padding(&self) -> i32;
    fn vertical_padding(&self) -> i32;
    fn border_radius(&self) -> Option<f64>;
    /// How far the window's top bar (title bar and toolbars) reaches below
    /// its top edge, read through Accessibility. Fails when the window shows
    /// neither a close button nor a toolbar, or when a read fails, such as
    /// while its app is busy.
    fn top_bar_height(&self) -> Result<i32>;
}

#[derive(Component, Deref, DerefMut)]
pub struct Window(Box<dyn WindowApi>);

impl Window {
    pub fn new(window: Box<dyn WindowApi>) -> Self {
        Window(window)
    }
}

/// Retrieves the window ID (`WinID`) from an `AXUIElementRef`.
///
/// # Arguments
///
/// * `element_ref` - The `AXUIElementRef` to extract the window ID from.
///
/// # Returns
///
/// `Ok(WinID)` with the window ID if successful, otherwise `Err(Error)`.
pub fn ax_window_id(element_ref: AXUIElementRef) -> Result<WinID> {
    try_ax_window_id(element_ref).ok_or_else(|| {
        Error::InvalidInput(format!(
            "{}: Unable to get window id from element {element_ref:?}.",
            function_name!()
        ))
    })
}

/// Allocation-free variant of [`ax_window_id`].
///
/// [`crate::manager::bruteforce_windows`] calls this tens of thousands of times in
/// a row and discards nearly every result, so the error path must not format a
/// message it will only drop.
pub fn try_ax_window_id(element_ref: AXUIElementRef) -> Option<WinID> {
    let ptr = NonNull::new(element_ref)?;
    let mut window_id: WinID = 0;
    if unsafe { _AXUIElementGetWindow(ptr.as_ptr(), &mut window_id) } != 0 || window_id == 0 {
        return None;
    }
    Some(window_id)
}

/// How many levels below the window the toolbar search looks. Chromium
/// browsers nest their toolbars six levels down.
const TOP_BAR_SEARCH_DEPTH: usize = 6;
/// How many elements the toolbar search inspects in total, so a window with a
/// large or wide tree costs a bounded number of Accessibility calls.
const TOP_BAR_SEARCH_BUDGET: usize = 200;
/// Elements that start further below the window top than this hold content,
/// not the top bar.
const TOP_BAR_SEARCH_REACH: i32 = 200;
/// A toolbar that starts at most this far below the top bar found so far
/// continues it.
const TOOLBAR_JOIN_SLACK: i32 = 8;
/// Roles that hold content or controls, never a toolbar.
const TOP_BAR_LEAF_ROLES: [&str; 12] = [
    "AXBrowser",
    "AXButton",
    "AXImage",
    "AXList",
    "AXMenuBar",
    "AXOutline",
    "AXScrollArea",
    "AXStaticText",
    "AXTable",
    "AXTextArea",
    "AXTextField",
    "AXWebArea",
];

/// Reads an attribute. `Ok(None)` when the element does not have it, and an
/// error when the read fails, such as while the app is busy.
fn read_ax_attribute<T: Type>(
    element: &CFRetained<AXUIWrapper>,
    attribute: &'static str,
) -> Result<Option<CFRetained<T>>> {
    let mut value: *mut CFType = null_mut();
    let code = unsafe {
        AXUIElementCopyAttributeValue(
            element.as_ptr(),
            CFString::from_static_str(attribute).as_ref(),
            &mut value,
        )
    };
    if [
        kAXErrorAttributeUnsupported,
        kAXErrorNoValue,
        kAXErrorNotImplemented,
    ]
    .contains(&code)
    {
        return Ok(None);
    }
    code.to_result(attribute)?;
    Ok(NonNull::new(value).map(|value| unsafe { CFRetained::from_raw(value.cast::<T>()) }))
}

fn read_ax_frame(element: &CFRetained<AXUIWrapper>) -> Result<Option<IRect>> {
    let position = read_ax_attribute::<AXUIWrapper>(element, kAXPositionAttribute)?;
    let size = read_ax_attribute::<AXUIWrapper>(element, kAXSizeAttribute)?;
    let Some((position, size)) = position.zip(size) else {
        return Ok(None);
    };
    let mut frame = CGRect::default();
    unsafe {
        AXValueGetValue(
            position.as_ptr(),
            kAXValueTypeCGPoint,
            NonNull::from(&mut frame.origin).as_ptr().cast(),
        );
        AXValueGetValue(
            size.as_ptr(),
            kAXValueTypeCGSize,
            NonNull::from(&mut frame.size).as_ptr().cast(),
        );
    }
    Ok(Some(irect_from(frame)))
}

/// Measures how far a window's top bar reaches below its top edge.
///
/// The title bar is twice the distance from the window top to the middle of
/// the close button, since macOS centers the window buttons in the bar that
/// holds them, whether that is a plain title bar or a unified toolbar. Each
/// `AXToolbar` near the top that starts where the bar found so far ends, such
/// as a browser's address bar and bookmarks bar under its tab strip, extends it.
///
/// Any failed read fails the measurement, so a busy app costs one timed-out
/// read rather than one per element, and a partly read top bar is never kept.
fn measure_top_bar(window: &CFRetained<AXUIWrapper>) -> Result<i32> {
    let frame = read_ax_frame(window)?.ok_or(Error::InvalidWindow)?;
    let top = frame.min.y;
    let title_bar = match read_ax_attribute::<AXUIWrapper>(window, kAXCloseButtonAttribute)? {
        Some(button) => read_ax_frame(&button)?.map(|button| button.min.y + button.max.y - 2 * top),
        None => None,
    };

    let toolbars = search_toolbars(
        window.clone(),
        |element| {
            Ok(
                read_ax_attribute::<CFArray<AXUIWrapper>>(element, kAXChildrenAttribute)?
                    .map(|children| children.to_vec())
                    .unwrap_or_default(),
            )
        },
        |child| {
            let Some(role) = read_ax_attribute::<CFString>(child, kAXRoleAttribute)? else {
                return Ok(TopBarElement::Other);
            };
            let role = role.to_string();
            if TOP_BAR_LEAF_ROLES.contains(&role.as_str()) {
                return Ok(TopBarElement::Other);
            }
            let Some(child_frame) = read_ax_frame(child)? else {
                return Ok(TopBarElement::Other);
            };
            Ok(if child_frame.min.y - top > TOP_BAR_SEARCH_REACH {
                TopBarElement::Other
            } else if role == kAXToolbarRole {
                TopBarElement::Toolbar(child_frame.min.y - top..child_frame.max.y - top)
            } else {
                TopBarElement::Container
            })
        },
    )?;
    top_bar_extent(title_bar, toolbars)
        .filter(|height| *height < frame.height())
        .ok_or_else(|| Error::NotFound("no close button or toolbar".to_string()))
}

/// What the toolbar search learns about one element.
#[derive(Debug)]
enum TopBarElement {
    /// A toolbar, given as offsets from the window top.
    Toolbar(Range<i32>),
    /// An element near the top that may hold a toolbar.
    Container,
    /// Content, a control, or an element too far down to matter.
    Other,
}

/// Walks the tree under `root` breadth first, at most
/// [`TOP_BAR_SEARCH_DEPTH`] levels deep, and returns every toolbar found.
/// Each element `inspect` classifies counts against one
/// [`TOP_BAR_SEARCH_BUDGET`], so a wide tree costs no more than a deep one.
/// A failed read from either closure ends the search with that error.
fn search_toolbars<N>(
    root: N,
    mut children: impl FnMut(&N) -> Result<Vec<N>>,
    mut inspect: impl FnMut(&N) -> Result<TopBarElement>,
) -> Result<Vec<Range<i32>>> {
    let mut toolbars = vec![];
    let mut budget = TOP_BAR_SEARCH_BUDGET;
    let mut queue = VecDeque::from([(root, 0)]);
    while let Some((element, depth)) = queue.pop_front() {
        for child in children(&element)? {
            if budget == 0 {
                return Ok(toolbars);
            }
            budget -= 1;
            match inspect(&child)? {
                TopBarElement::Toolbar(range) => toolbars.push(range),
                TopBarElement::Container if depth + 1 < TOP_BAR_SEARCH_DEPTH => {
                    queue.push_back((child, depth + 1));
                }
                TopBarElement::Container | TopBarElement::Other => {}
            }
        }
    }
    Ok(toolbars)
}

/// The top bar's reach below the window top: the title bar, extended by each
/// toolbar that starts where the bar so far ends. Toolbars are given as
/// offsets from the window top.
fn top_bar_extent(title_bar: Option<i32>, mut toolbars: Vec<Range<i32>>) -> Option<i32> {
    toolbars.sort_by_key(|toolbar| toolbar.start);
    let mut bottom = title_bar;
    for toolbar in toolbars {
        let so_far = bottom.unwrap_or(0);
        if toolbar.start <= so_far + TOOLBAR_JOIN_SLACK {
            bottom = Some(so_far.max(toolbar.end));
        }
    }
    bottom.filter(|height| *height > 0)
}

// const CPS_ALL_WINDOWS: u32 = 0x100;
const CPS_USER_GENERATED: u32 = 0x200;
// const CPS_NO_WINDOWS: u32 = 0x400;

#[derive(Debug)]
pub struct WindowOS {
    id: WinID,
    ax_element: CFRetained<AXUIWrapper>,
    frame: IRect,
    vertical_padding: i32,
    horizontal_padding: i32,
    border_radius: OnceLock<Option<f64>>,
    pid: OnceLock<Result<Pid>>,
    app_reference: OnceLock<Option<CFRetained<AXUIWrapper>>>,
    enhanced_ui_state: OnceLock<Arc<Mutex<EnhancedUiState>>>,
    /// Set once this window's app is known not to use
    /// `AXEnhancedUserInterface` (the common case), so the steady-state check
    /// in [`Self::disable_enhanced_ui`] is a relaxed atomic load instead of
    /// acquiring the PID lock from every `par_iter_mut` worker.
    enhanced_ui_absent: AtomicBool,

    /// The last title read off the element, cached because reading one is a
    /// synchronous cross-process call and many callers want it for every
    /// window at once.
    ///
    /// An `RwLock` rather than a `OnceLock` like its neighbours: a title can
    /// change, and [`Self::invalidate_title`] clears it when the app reports
    /// `kAXTitleChangedNotification`. Missing that notification is the one
    /// way this can go stale.
    title: RwLock<Option<String>>,

    /// Role and subrole, cached after the first successful read: both are
    /// cross-process calls and neither changes over a window's life. Failed
    /// reads are not cached, so a transiently busy app is asked again.
    /// Dropped with the `Window` component when the entity despawns.
    role: OnceLock<String>,
    /// Never caches `AXUnknown`: some apps report it briefly while a window
    /// is still being created.
    subrole: OnceLock<String>,
}

impl WindowOS {
    /// Creates a new `Window` instance using an empty configuration.
    /// Non-standard windows are rejected unless they match a `manage = true` rule.
    ///
    /// # Arguments
    ///
    /// * `element` - A `CFRetained<AXUIWrapper>` reference to the Accessibility UI element.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if the window is created successfully, otherwise `Err(Error)`.
    #[instrument(level = Level::TRACE, ret)]
    pub fn new(element: &CFRetained<AXUIWrapper>) -> Result<Self> {
        Self::new_with_config(element, &Config::default(), None)
    }

    /// Creates a new `Window` instance.
    ///
    /// # Arguments
    ///
    /// * `element` - A `CFRetained<AXUIWrapper>` reference to the Accessibility UI element.
    /// * `config` - The current Paneru configuration, used to evaluate window rules.
    /// * `bundle_id` - The bundle identifier of the owning application, if known.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if the window is created successfully, otherwise `Err(Error)`.
    #[instrument(level = Level::TRACE, ret)]
    pub fn new_with_config(
        element: &CFRetained<AXUIWrapper>,
        config: &Config,
        bundle_id: Option<&str>,
    ) -> Result<Self> {
        let id = ax_window_id(element.as_ptr())?;
        let window = Self {
            id,
            ax_element: element.clone(),
            frame: IRect::default(),
            vertical_padding: 0,
            horizontal_padding: 0,
            border_radius: OnceLock::new(),
            pid: OnceLock::new(),
            app_reference: OnceLock::new(),
            enhanced_ui_state: OnceLock::new(),
            enhanced_ui_absent: AtomicBool::new(false),
            title: RwLock::new(None),
            role: OnceLock::new(),
            subrole: OnceLock::new(),
        };

        let forced = window.is_forced_manage(config, bundle_id);

        if window.is_unknown() && !forced {
            return Err(Error::invalid_window(&format!(
                "Ignoring AXUnknown window, id: {}, role {}, subrole {}",
                window.id(),
                window.role().unwrap_or_default(),
                window.subrole().unwrap_or_default(),
            )));
        }

        if !window.is_real() && !forced {
            return Err(Error::invalid_window(&format!(
                "Ignoring non-real window, id: {}, role {}, subrole {}",
                window.id(),
                window.role().unwrap_or_default(),
                window.subrole().unwrap_or_default(),
            )));
        }

        trace!(
            "created {} title: {} role: {} subrole: {}",
            window.id(),
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
        );
        Ok(window)
    }

    /// Checks whether a configured window rule forces this window to be managed
    /// despite having a non-standard role/subrole.
    fn is_forced_manage(&self, config: &Config, bundle_id: Option<&str>) -> bool {
        let Ok(title) = self.title() else {
            return false;
        };
        // Same transient-AX-failure caveat as `WindowProperties::new`.
        let role = self.role().ok();
        let subrole = self.subrole().ok();
        config
            .find_window_properties(
                &title,
                bundle_id.unwrap_or_default(),
                role.as_deref(),
                subrole.as_deref(),
            )
            .iter()
            .any(|params| params.manage.is_some_and(|manage| manage))
    }

    /// Checks if the window's subrole is "`AXUnknownSubrole`".
    ///
    /// # Returns
    ///
    /// `true` if the subrole is unknown, `false` otherwise.
    fn is_unknown(&self) -> bool {
        self.subrole()
            .is_ok_and(|subrole| subrole.eq(kAXUnknownSubrole))
    }

    /// Checks if the window is a "real" window based on its role and subrole.
    /// It considers standard and floating window subroles as real.
    ///
    /// # Returns
    ///
    /// `true` if the window is real, `false` otherwise.
    fn is_real(&self) -> bool {
        let role = self.role().ok();
        let subrole = self.subrole().ok();

        subrole.as_deref() == Some(kAXStandardWindowSubrole)
            || (role.as_deref() == Some(kAXWindowRole)
                && subrole.as_deref() == Some(kAXFloatingWindowSubrole))
    }

    fn app_reference(&self) -> Option<CFRetained<AXUIWrapper>> {
        self.app_reference
            .get_or_init(|| {
                self.pid()
                    .map(|pid| unsafe { AXUIElementCreateApplication(pid) })
                    .and_then(AXUIWrapper::from_retained)
                    .inspect_err(|err| warn!("error getting app reference: {err}"))
                    .ok()
            })
            .clone()
    }

    /// Suppresses app-driven move/resize animations until the last guard drops.
    /// The PID lock covers flag/counter transitions, not the window operations.
    fn disable_enhanced_ui(&self) -> Option<EnhancedUiGuard<impl FnOnce() -> Result<()> + use<>>> {
        if self.enhanced_ui_absent.load(Ordering::Relaxed) {
            return None;
        }
        let pid = self.pid().ok()?;
        let state = self
            .enhanced_ui_state
            .get_or_init(|| enhanced_ui_state(pid))
            .clone();
        let app_element = self.app_reference()?;
        let restore_element = app_element.clone();
        let window_id = self.id;
        match acquire_enhanced_ui(
            state,
            pid,
            || {
                read_enhanced_ui(&app_element).inspect(|enabled| {
                    debug!(
                        target: "paneru::ax_diagnostics",
                        pid,
                        window_id,
                        enabled,
                        "read AXEnhancedUserInterface"
                    );
                })
            },
            || set_enhanced_ui(&app_element, false, pid, window_id),
            move || set_enhanced_ui(&restore_element, true, pid, window_id),
        ) {
            Ok(guard) => {
                if guard.is_none() {
                    debug!(
                        target: "paneru::ax_diagnostics",
                        pid,
                        window_id,
                        "permanently caching false; skipping enhanced UI workaround"
                    );
                    self.enhanced_ui_absent.store(true, Ordering::Relaxed);
                }
                guard
            }
            Err(err) => {
                warn!(pid, window_id, "error reading enhanced UI: {err}");
                None
            }
        }
    }

    fn set_ax_position(&mut self, origin: Origin) {
        let mut point = CGPoint::new(
            f64::from(origin.x + self.horizontal_padding),
            f64::from(origin.y + self.vertical_padding),
        );
        let position_ref = unsafe {
            AXValueCreate(
                kAXValueTypeCGPoint,
                NonNull::from(&mut point).as_ptr().cast(),
            )
        };
        if let Ok(position) = AXUIWrapper::from_retained(position_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXPositionAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            let size = self.frame.size();
            self.frame.min = origin;
            self.frame.max = origin + size;
        }
    }

    fn set_ax_size(&mut self, size: Size) {
        let width_padding = 2 * self.horizontal_padding;
        let height_padding = 2 * self.vertical_padding;
        let mut cgsize = CGSize::new(
            f64::from(size.x - width_padding),
            f64::from(size.y - height_padding),
        );
        let size_ref = unsafe {
            AXValueCreate(
                kAXValueTypeCGSize,
                NonNull::from(&mut cgsize).as_ptr().cast(),
            )
        };
        if let Ok(size_value) = AXUIWrapper::from_retained(size_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXSizeAttribute).as_ref(),
                    size_value.as_ref(),
                )
            };
            self.frame.max = self.frame.min + size;
        }
    }

    /// Makes the window the key window for its application by sending synthesized events.
    ///
    /// # Arguments
    ///
    /// * `psn` - The process serial number of the application.
    fn make_key_window(&self, psn: &ProcessSerialNumber) {
        // Reason: On macOS 14 (Sonoma), CGSEncodeEventRecord serializes the raw event
        // buffer via NSKeyedArchiver, misinterpreting 0xFF fill as an ObjC class pointer,
        // causing SIGABRT. See https://github.com/karinushka/paneru/issues/123
        if macos_major_version() == 14 {
            debug!("make_key_window: skipped on macOS 14 (Sonoma) to prevent crash");
            return;
        }
        let window_id = self.id();
        let mut event_bytes = [0u8; 0xf8];
        event_bytes[0x04] = 0xf8;
        event_bytes[0x3a] = 0x10;
        event_bytes[0x3c..0x40].copy_from_slice(&window_id.to_ne_bytes());
        event_bytes[0x20..0x30].fill(0xff);

        event_bytes[0x08] = 0x01;
        unsafe { SLPSPostEventRecordTo(psn, event_bytes.as_ptr().cast()) };

        event_bytes[0x08] = 0x02;
        unsafe { SLPSPostEventRecordTo(psn, event_bytes.as_ptr().cast()) };
    }
}

impl WindowApi for WindowOS {
    /// Returns the ID of the window.
    ///
    /// # Returns
    ///
    /// The window ID as `WinID`.
    fn id(&self) -> WinID {
        self.id
    }

    /// Returns the current frame (`CGRect`) of the window.
    ///
    /// # Returns
    ///
    /// The window's frame as `CGRect`.
    fn frame(&self) -> IRect {
        self.frame
    }

    /// Returns the accessibility element of the window.
    ///
    /// # Returns
    ///
    /// A `CFRetained<AXUIWrapper>` representing the accessibility element.
    fn element(&self) -> Option<CFRetained<AXUIWrapper>> {
        Some(self.ax_element.clone())
    }

    /// Retrieves the title of the window.
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window title if successful, otherwise `Err(Error)`.
    fn title(&self) -> Result<String> {
        if let Some(cached) = self.title.force_read().clone() {
            return Ok(cached);
        }
        let title = self.ax_element.title()?;
        *self.title.force_write() = Some(title.clone());
        Ok(title)
    }

    fn invalidate_title(&self) {
        self.title.force_write().take();
    }

    fn identifier(&self) -> Result<String> {
        self.ax_element.identifier()
    }

    /// Returns true if the window has a child role.
    fn child_role(&self) -> Result<bool> {
        let role = self.role()?;
        Ok(["AXSheet", "AXDrawer"]
            .iter()
            .any(|axrole| axrole.eq(&role)))
    }

    /// Retrieves the role of the window (e.g., "`AXWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window role if successful, otherwise `Err(Error)`.
    fn role(&self) -> Result<String> {
        if let Some(role) = self.role.get() {
            return Ok(role.clone());
        }
        let role = self.ax_element.role()?;
        if role != kAXUnknownRole {
            let _ = self.role.set(role.clone());
        }
        Ok(role)
    }

    /// Retrieves the subrole of the window (e.g., "`AXStandardWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window subrole if successful, otherwise `Err(Error)`.
    fn subrole(&self) -> Result<String> {
        if let Some(subrole) = self.subrole.get() {
            return Ok(subrole.clone());
        }
        let subrole = self.ax_element.subrole()?;
        if subrole != kAXUnknownSubrole {
            let _ = self.subrole.set(subrole.clone());
        }
        Ok(subrole)
    }

    /// Reads the role straight from AX, deliberately bypassing the `role`
    /// cache. `Self::role` keeps returning its stored value after the window
    /// is gone, so it always looks alive; only a fresh read fails once the
    /// AX element is torn down. Don't "simplify" this to `self.role().is_ok()`.
    /// Every window has a role, so a failed read means the element is gone
    /// rather than that an attribute is missing.
    fn is_alive(&self) -> bool {
        self.ax_element.role().is_ok()
    }

    #[instrument(level = Level::DEBUG, ret)]
    fn is_minimized(&self) -> bool {
        self.ax_element.minimized().is_ok_and(|minimized| minimized)
    }

    fn is_full_screen(&self) -> bool {
        self.ax_element.full_screen().unwrap_or(false)
    }

    #[instrument(level = Level::TRACE)]
    fn reposition(&mut self, origin: Origin) {
        if self.frame.min == origin {
            trace!("already in position.");
            return;
        }
        let _enhanced_ui = self.disable_enhanced_ui();
        self.set_ax_position(origin);
    }

    #[instrument(level = Level::TRACE)]
    fn resize(&mut self, size: Size) {
        if self.frame.size() == size {
            trace!("already correct size.");
            return;
        }
        let previous_frame = self.frame;
        let target_origin = previous_frame.min;
        let _enhanced_ui = self.disable_enhanced_ui();
        self.set_ax_size(size);

        let mut previous_observed_frame = previous_frame;
        let mut staged = false;
        for attempt in 1..=3 {
            let Ok(actual_frame) = self.update_frame() else {
                break;
            };
            let Some(staging_origin) =
                resize_staging_origin(previous_observed_frame, actual_frame, size.x)
            else {
                break;
            };
            debug!(
                attempt,
                requested_width = size.x,
                actual_width = actual_frame.width(),
                staging_x = staging_origin.x,
                "retrying partially constrained AX resize from an offscreen origin"
            );
            staged = true;
            previous_observed_frame = actual_frame;
            self.set_ax_position(staging_origin);
            self.set_ax_size(size);
        }

        if staged {
            if let Ok(final_frame) = self.update_frame() {
                debug!(
                    requested_width = size.x,
                    actual_width = final_frame.width(),
                    "completed staged AX resize"
                );
            }
            self.set_ax_position(target_origin);
        }
    }

    /// Updates the internal `frame` of the window by querying its current position and size from the Accessibility API.
    /// It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - An optional `CGRect` representing the bounds of the display the window is on.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the frame is updated successfully, otherwise `Err(Error)`.
    fn update_frame(&mut self) -> Result<IRect> {
        let window_ref = self.ax_element.as_ptr();

        let position = unsafe {
            let mut position_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                &mut position_ref,
            )
            .to_result(function_name!())?;
            AXUIWrapper::from_retained(position_ref)?
        };
        let size = unsafe {
            let mut size_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                &mut size_ref,
            )
            .to_result(function_name!())?;
            AXUIWrapper::from_retained(size_ref)?
        };

        let mut frame = CGRect::default();
        unsafe {
            AXValueGetValue(
                position.as_ptr(),
                kAXValueTypeCGPoint,
                NonNull::from(&mut frame.origin).as_ptr().cast(),
            );
            AXValueGetValue(
                size.as_ptr(),
                kAXValueTypeCGSize,
                NonNull::from(&mut frame.size).as_ptr().cast(),
            );
        }
        // if (CGRectEqualToRect(new_frame, window->frame)) {
        //     debug("%s:DEBOUNCED %s %d\n", __FUNCTION__, window->application->name, window->id);
        // }
        self.frame = irect_from(frame);

        self.frame.min.x -= self.horizontal_padding;
        self.frame.min.y -= self.vertical_padding;
        self.frame.max.x += self.horizontal_padding;
        self.frame.max.y += self.vertical_padding;

        Ok(self.frame)
    }

    /// Focuses the window without raising it. This involves sending specific events to the process.
    ///
    /// # Arguments
    ///
    /// * `currently_focused` - A reference to the currently focused window.
    #[instrument(level = Level::DEBUG, skip(currently_focused))]
    fn focus_without_raise(
        &self,
        psn: ProcessSerialNumber,
        currently_focused: &Window,
        focused_psn: ProcessSerialNumber,
    ) {
        let window_id = self.id();
        debug!("{window_id}");
        if focused_psn == psn {
            let mut event_bytes = [0u8; 0xf8];
            event_bytes[0x04] = 0xf8;
            event_bytes[0x08] = 0x0d;

            event_bytes[0x8a] = 0x02;
            event_bytes[0x3c..0x40].copy_from_slice(&currently_focused.id().to_ne_bytes());
            unsafe {
                SLPSPostEventRecordTo(&focused_psn, event_bytes.as_ptr().cast());
            }

            // Artificially delay the activation. This is necessary because some
            // applications appear to be confused if both of the events appear instantaneously.
            thread::sleep(Duration::from_millis(20));

            event_bytes[0x8a] = 0x01;
            event_bytes[0x3c..0x40].copy_from_slice(&window_id.to_ne_bytes());
            unsafe {
                SLPSPostEventRecordTo(&psn, event_bytes.as_ptr().cast());
            }
        }

        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
    }

    /// Focuses the window and raises it to the front.
    #[instrument(level = Level::DEBUG)]
    fn focus_with_raise(&self, psn: ProcessSerialNumber) {
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
        let element_ref = self.ax_element.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    #[instrument(level = Level::DEBUG)]
    fn raise_without_focus(&self) {
        let element_ref = self.ax_element.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    fn pid(&self) -> Result<Pid> {
        self.pid
            .get_or_init(|| {
                let pid: Pid = unsafe {
                    NonNull::new_unchecked(self.ax_element.as_ptr::<Pid>())
                        .byte_add(0x10)
                        .read()
                };
                (pid != 0).then_some(pid).ok_or(Error::InvalidInput(format!(
                    "can not get pid from {:?}.",
                    self.ax_element
                )))
            })
            .clone()
    }

    fn set_padding(&mut self, padding: WindowPadding) {
        match padding {
            WindowPadding::Vertical(padding) => self.vertical_padding = padding,
            WindowPadding::Horizontal(padding) => self.horizontal_padding = padding,
        }
    }

    fn horizontal_padding(&self) -> i32 {
        self.horizontal_padding
    }

    fn vertical_padding(&self) -> i32 {
        self.vertical_padding
    }

    // Based on:
    // - https://github.com/y3owk1n/rift/blob/cca067145f0282b532e848bb63d26a38c61f3c14/src/sys/window_server.rs#L175
    // - https://github.com/FelixKratz/JankyBorders/blob/a56a76a8a6ed77325f03655b23fcf525144d120b/src/windows.c#L67
    #[allow(clippy::cast_precision_loss)]
    fn border_radius(&self) -> Option<f64> {
        *self.border_radius.get_or_init(|| {
            let iterator = super::window_iterator_for_id(self.id)?;
            if !unsafe { SLSWindowIteratorAdvance(&raw const *iterator) } {
                return None;
            }

            let radii_ref = unsafe {
                // Load the function dynamicaly, because it exists only on macOS 26.x
                let s = c"SLSWindowIteratorGetCornerRadii";
                let p = libc::dlsym(libc::RTLD_DEFAULT, s.as_ptr());
                if p.is_null() {
                    return None;
                }
                let f: unsafe extern "C" fn(*const CFType) -> *mut CFArray<CFNumber> =
                    std::mem::transmute(p);
                f(&raw const *iterator)
            };
            let radii: CFRetained<CFArray<CFNumber>> =
                unsafe { CFRetained::from_raw(NonNull::new(radii_ref)?) };
            if radii.is_empty() {
                return None;
            }
            // Get first corner radius (usually all corners are the same)
            radii.get(0)?.as_i64().map(|v| v as f64)
        })
    }

    #[instrument(level = Level::DEBUG)]
    fn top_bar_height(&self) -> Result<i32> {
        measure_top_bar(&self.ax_element)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::platform::ProcessSerialNumber;
    use crate::tests::TestHarness;
    use std::sync::{atomic::AtomicUsize, mpsc};

    #[test]
    fn enhanced_ui_termination_discards_failed_restoration_before_pid_reuse() {
        let pid: Pid = 175_003;
        let psn = ProcessSerialNumber {
            high: 0,
            low: pid.cast_unsigned(),
        };
        let guard = acquire_enhanced_ui(
            enhanced_ui_state(pid),
            pid,
            || Ok(true),
            || Ok(()),
            || Err(Error::Generic("process exited before restoration".into())),
        )
        .unwrap()
        .unwrap();
        drop(guard);

        TestHarness::new()
            .with_app(pid, "terminated", "TerminatedApp", |_| {})
            .on_iteration(0, move |world, _| {
                assert!(
                    world
                        .query::<&crate::ecs::BProcess>()
                        .iter(world)
                        .all(|process| process.psn() != psn)
                );
                let enabled = AtomicBool::new(false);
                let reads = AtomicUsize::new(0);
                let replacement = acquire_enhanced_ui(
                    enhanced_ui_state(pid),
                    pid,
                    || {
                        reads.fetch_add(1, Ordering::Relaxed);
                        Ok(enabled.load(Ordering::Relaxed))
                    },
                    || {
                        enabled.store(false, Ordering::Relaxed);
                        Ok(())
                    },
                    || {
                        enabled.store(true, Ordering::Relaxed);
                        Ok(())
                    },
                )
                .unwrap();
                drop(replacement);
                assert_eq!(reads.load(Ordering::Relaxed), 1);
                assert!(!enabled.load(Ordering::Relaxed));
            })
            .run(vec![Event::ApplicationTerminated { psn }]);
    }

    #[test]
    fn enhanced_ui_termination_discards_cached_false_before_pid_reuse() {
        let pid: Pid = 175_004;
        let guard = acquire_enhanced_ui(
            enhanced_ui_state(pid),
            pid,
            || Ok(false),
            || panic!("an initially false flag must not be disabled"),
            || panic!("an initially false flag must not be restored"),
        )
        .unwrap();
        assert!(guard.is_none());

        TestHarness::new()
            .with_app(pid, "terminated", "TerminatedApp", |_| {})
            .on_iteration(0, move |_, _| {
                let enabled = AtomicBool::new(true);
                let replacement = acquire_enhanced_ui(
                    enhanced_ui_state(pid),
                    pid,
                    || Ok(enabled.load(Ordering::Relaxed)),
                    || {
                        enabled.store(false, Ordering::Relaxed);
                        Ok(())
                    },
                    || {
                        enabled.store(true, Ordering::Relaxed);
                        Ok(())
                    },
                )
                .unwrap();
                assert!(!enabled.load(Ordering::Relaxed));
                drop(replacement);
                assert!(enabled.load(Ordering::Relaxed));
            })
            .run(vec![Event::ApplicationTerminated {
                psn: ProcessSerialNumber {
                    high: 0,
                    low: pid.cast_unsigned(),
                },
            }]);
    }

    #[test]
    fn enhanced_ui_termination_preserves_outstanding_guards_without_sharing_with_reused_pid() {
        let pid: Pid = 175_005;
        let old_restorations = Arc::new(AtomicUsize::new(0));
        let restore_count = old_restorations.clone();
        let old_guard = acquire_enhanced_ui(
            enhanced_ui_state(pid),
            pid,
            || Ok(true),
            || Ok(()),
            move || {
                restore_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        let mut old_guard = Some(old_guard);

        TestHarness::new()
            .with_app(pid, "terminated", "TerminatedApp", |_| {})
            .on_iteration(0, move |_, _| {
                let enabled = AtomicBool::new(true);
                let restorations = AtomicUsize::new(0);
                let replacement = acquire_enhanced_ui(
                    enhanced_ui_state(pid),
                    pid,
                    || Ok(enabled.load(Ordering::Relaxed)),
                    || {
                        enabled.store(false, Ordering::Relaxed);
                        Ok(())
                    },
                    || {
                        restorations.fetch_add(1, Ordering::Relaxed);
                        enabled.store(true, Ordering::Relaxed);
                        Ok(())
                    },
                )
                .unwrap()
                .unwrap();
                assert!(!enabled.load(Ordering::Relaxed));
                drop(old_guard.take());
                assert_eq!(old_restorations.load(Ordering::Relaxed), 1);
                assert_eq!(restorations.load(Ordering::Relaxed), 0);
                assert!(!enabled.load(Ordering::Relaxed));
                drop(replacement);
                assert_eq!(restorations.load(Ordering::Relaxed), 1);
                assert!(enabled.load(Ordering::Relaxed));
            })
            .run(vec![Event::ApplicationTerminated {
                psn: ProcessSerialNumber {
                    high: 0,
                    low: pid.cast_unsigned(),
                },
            }]);
    }

    #[test]
    fn enhanced_ui_overlapping_operations_restore_only_after_last_guard() {
        let state = Arc::new(Mutex::new(EnhancedUiState::default()));
        let enabled = AtomicBool::new(true);
        let restorations = AtomicUsize::new(0);
        let restore = || {
            restorations.fetch_add(1, Ordering::Relaxed);
            enabled.store(true, Ordering::Relaxed);
            Ok(())
        };
        let first = acquire_enhanced_ui(
            state.clone(),
            1,
            || Ok(enabled.load(Ordering::Relaxed)),
            || {
                enabled.store(false, Ordering::Relaxed);
                Ok(())
            },
            restore,
        )
        .unwrap()
        .unwrap();
        assert!(
            state.try_lock().is_ok(),
            "window operations must not hold the lock"
        );

        thread::scope(|scope| {
            let second = scope
                .spawn(|| {
                    acquire_enhanced_ui(
                        state.clone(),
                        1,
                        || panic!("must not read another operation's temporary false"),
                        || panic!("the app is already disabled"),
                        restore,
                    )
                    .unwrap()
                    .unwrap()
                })
                .join()
                .unwrap();
            assert_eq!(state.lock().unwrap().active_operations, 2);
            drop(first);
            assert!(!enabled.load(Ordering::Relaxed));
            assert_eq!(restorations.load(Ordering::Relaxed), 0);
            drop(second);
        });

        assert!(enabled.load(Ordering::Relaxed));
        assert_eq!(restorations.load(Ordering::Relaxed), 1);
        let state = state.lock().unwrap();
        assert_eq!(state.active_operations, 0);
        assert!(!state.absent);
        assert!(!state.restore_pending);
    }

    #[test]
    fn enhanced_ui_flag_transitions_hold_pid_lock() {
        let state = Arc::new(Mutex::new(EnhancedUiState::default()));
        let enabled = AtomicBool::new(true);
        let guard = acquire_enhanced_ui(
            state.clone(),
            1,
            || {
                assert!(state.try_lock().is_err());
                Ok(enabled.load(Ordering::Relaxed))
            },
            || {
                enabled.store(false, Ordering::Relaxed);
                // Another window must not read false before the count is published.
                assert!(state.try_lock().is_err());
                Ok(())
            },
            || {
                // Nor after the count reaches zero but before true is restored.
                assert!(state.try_lock().is_err());
                enabled.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        drop(guard);
        assert!(enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn enhanced_ui_different_pids_can_transition_independently() {
        let first_state = enhanced_ui_state(-175_001);
        assert!(Arc::ptr_eq(&first_state, &enhanced_ui_state(-175_001)));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        thread::scope(|scope| {
            let first = scope.spawn(move || {
                let guard = acquire_enhanced_ui(
                    first_state,
                    -175_001,
                    || Ok(true),
                    || {
                        entered_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        Ok(())
                    },
                    || Ok(()),
                )
                .unwrap()
                .unwrap();
                drop(guard);
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let second = acquire_enhanced_ui(
                enhanced_ui_state(-175_002),
                -175_002,
                || Ok(true),
                || Ok(()),
                || Ok(()),
            )
            .unwrap()
            .unwrap();
            drop(second);
            resume_tx.send(()).unwrap();
            first.join().unwrap();
        });
    }

    #[test]
    fn enhanced_ui_confirmed_false_is_permanently_cached() {
        let state = Arc::new(Mutex::new(EnhancedUiState::default()));
        let guard = acquire_enhanced_ui(
            state.clone(),
            1,
            || Ok(false),
            || panic!("must not disable an already false flag"),
            || panic!("must not restore a flag we didn't disable"),
        )
        .unwrap();
        assert!(guard.is_none());
        drop(guard);
        let guard = acquire_enhanced_ui(
            state.clone(),
            1,
            || panic!("another window of the same app must use the cached false"),
            || panic!("cached false must not be disabled"),
            || panic!("cached false must not be restored"),
        )
        .unwrap();
        assert!(guard.is_none());
        let state = state.lock().unwrap();
        assert!(state.absent);
        assert_eq!(state.active_operations, 0);
    }

    #[test]
    fn enhanced_ui_unsupported_and_no_value_reads_are_cached_as_absent() {
        assert!(!decode_enhanced_ui_read(kAXErrorSuccess, Some(false)).unwrap());
        assert!(decode_enhanced_ui_read(kAXErrorSuccess, Some(true)).unwrap());
        assert!(decode_enhanced_ui_read(kAXErrorSuccess, None).is_err());
        assert!(decode_enhanced_ui_read(accessibility_sys::kAXErrorCannotComplete, None).is_err());

        for code in [kAXErrorAttributeUnsupported, kAXErrorNoValue] {
            let state = Arc::new(Mutex::new(EnhancedUiState::default()));
            let guard = acquire_enhanced_ui(
                state.clone(),
                1,
                || decode_enhanced_ui_read(code, None),
                || panic!("unsupported attribute must not be disabled"),
                || panic!("unsupported attribute must not be restored"),
            )
            .unwrap();
            assert!(guard.is_none());
            assert!(state.lock().unwrap().absent);
        }
    }

    #[test]
    fn enhanced_ui_read_errors_are_retried_without_acquiring() {
        let state = Arc::new(Mutex::new(EnhancedUiState::default()));
        let read_error = acquire_enhanced_ui(
            state.clone(),
            1,
            || Err(Error::Generic("transient AX read failure".into())),
            || panic!("a failed read must not disable the flag"),
            || panic!("a failed acquisition must not restore the flag"),
        );
        assert!(read_error.is_err());
        {
            let state = state.lock().unwrap();
            assert!(!state.absent);
            assert!(!state.restore_pending);
            assert_eq!(state.active_operations, 0);
        }
        let restorations = AtomicUsize::new(0);
        let guard = acquire_enhanced_ui(
            state.clone(),
            1,
            || Ok(true),
            || Ok(()),
            || {
                restorations.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        drop(guard);
        assert_eq!(restorations.load(Ordering::Relaxed), 1);
        assert_eq!(state.lock().unwrap().active_operations, 0);
    }

    #[test]
    fn enhanced_ui_failed_disable_still_restores_possible_side_effects() {
        let state = Arc::new(Mutex::new(EnhancedUiState::default()));
        let enabled = AtomicBool::new(true);
        let guard = acquire_enhanced_ui(
            state.clone(),
            1,
            || Ok(enabled.load(Ordering::Relaxed)),
            || {
                enabled.store(false, Ordering::Relaxed);
                Err(Error::Generic(
                    "setter changed state but returned -25208".into(),
                ))
            },
            || {
                enabled.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        assert!(!enabled.load(Ordering::Relaxed));
        assert_eq!(state.lock().unwrap().active_operations, 1);
        drop(guard);
        assert!(enabled.load(Ordering::Relaxed));
        let state = state.lock().unwrap();
        assert_eq!(state.active_operations, 0);
        assert!(!state.absent);
        assert!(!state.restore_pending);
    }

    #[test]
    fn enhanced_ui_write_errors_use_readback_to_verify_actual_state() {
        for requested in [false, true] {
            assert!(
                verify_enhanced_ui_write(
                    requested,
                    Err(Error::Generic("setter returned -25208".into())),
                    || Ok(requested),
                )
                .is_ok()
            );
            assert!(
                verify_enhanced_ui_write(
                    requested,
                    Err(Error::Generic("write rejected".into())),
                    || Ok(!requested),
                )
                .is_err()
            );
            assert!(
                verify_enhanced_ui_write(
                    requested,
                    Err(Error::Generic("write failed".into())),
                    || Err(Error::Generic("read also failed".into())),
                )
                .is_err()
            );
            assert!(
                verify_enhanced_ui_write(requested, Ok(()), || {
                    panic!("successful writes must not incur a readback round-trip")
                })
                .is_ok()
            );
        }
    }

    #[test]
    fn enhanced_ui_failed_restore_is_retried_without_caching_our_false() {
        let state = Arc::new(Mutex::new(EnhancedUiState::default()));
        let enabled = AtomicBool::new(true);
        let first = acquire_enhanced_ui(
            state.clone(),
            1,
            || Ok(true),
            || {
                enabled.store(false, Ordering::Relaxed);
                Ok(())
            },
            || Err(Error::Generic("transient AX restore failure".into())),
        )
        .unwrap()
        .unwrap();
        drop(first);
        {
            let state = state.lock().unwrap();
            assert_eq!(state.active_operations, 0);
            assert!(state.restore_pending);
            assert!(!state.absent);
        }
        let retry = acquire_enhanced_ui(
            state.clone(),
            1,
            || panic!("our own unrestored false must not be read or cached"),
            || Ok(()),
            || {
                enabled.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        drop(retry);
        assert!(enabled.load(Ordering::Relaxed));
        let state = state.lock().unwrap();
        assert!(!state.absent);
        assert!(!state.restore_pending);
        assert_eq!(state.active_operations, 0);
    }

    #[test]
    fn stages_partially_applied_width_growth() {
        let previous = IRect::new(-400, 40, 400, 640);
        let actual = IRect::new(-400, 40, 2416, 640);

        assert_eq!(
            resize_staging_origin(previous, actual, 4112),
            Some(Origin::new(-1696, 40))
        );

        let nearly_complete = IRect::new(-2056, 40, 2016, 640);
        assert_eq!(
            resize_staging_origin(actual, nearly_complete, 4112),
            Some(Origin::new(-2096, 40))
        );
    }

    #[test]
    fn does_not_stage_fixed_size_or_completed_resizes() {
        let fixed = IRect::new(0, 40, 230, 448);
        assert_eq!(resize_staging_origin(fixed, fixed, 4112), None);

        let previous = IRect::new(0, 40, 800, 640);
        let completed = IRect::new(0, 40, 4112, 640);
        assert_eq!(resize_staging_origin(previous, completed, 4112), None);
    }

    /// A Chromium browser's window buttons center in a 34 px tab strip, and
    /// its address bar and bookmarks bar toolbars continue the top bar to
    /// 96 px. A toolbar further down, such as a find bar, does not.
    #[test]
    fn a_top_bar_reaches_through_the_toolbars_that_continue_it() {
        assert_eq!(
            top_bar_extent(Some(34), vec![300..330, 62..96, 34..62]),
            Some(96)
        );
        assert_eq!(top_bar_extent(None, vec![0..52, 300..330]), Some(52));
        assert_eq!(top_bar_extent(Some(28), vec![]), Some(28));
        assert_eq!(top_bar_extent(None, vec![]), None);
    }

    /// Each element is either a container holding the next ones or a leaf.
    /// `None` children for a leaf.
    fn search(tree: &[Option<Vec<usize>>], toolbars: &[usize]) -> (Vec<Range<i32>>, usize) {
        let mut inspected = 0;
        let found = search_toolbars(
            0,
            |node| Ok(tree[*node].clone().unwrap_or_default()),
            |node| {
                inspected += 1;
                Ok(if toolbars.contains(node) {
                    TopBarElement::Toolbar(0..40)
                } else if tree[*node].is_some() {
                    TopBarElement::Container
                } else {
                    TopBarElement::Other
                })
            },
        )
        .unwrap();
        (found, inspected)
    }

    #[test]
    fn the_toolbar_search_inspects_at_most_its_budget() {
        // The root and its first child each hold 1000 containers.
        let mut tree = vec![Some((1..=1000).collect::<Vec<_>>())];
        tree.push(Some((1001..=2000).collect()));
        tree.extend((2..=2000).map(|_| Some(vec![])));
        let (_, inspected) = search(&tree, &[]);
        assert_eq!(inspected, TOP_BAR_SEARCH_BUDGET);
    }

    #[test]
    fn the_toolbar_search_finds_a_toolbar_within_its_depth() {
        // A chain of containers, each holding the next.
        let chain = |levels: usize| {
            let mut tree = (0..levels)
                .map(|node| Some(vec![node + 1]))
                .collect::<Vec<_>>();
            tree.push(None);
            tree
        };
        let (found, _) = search(&chain(TOP_BAR_SEARCH_DEPTH), &[TOP_BAR_SEARCH_DEPTH]);
        assert_eq!(found, vec![0..40]);
        let deeper = TOP_BAR_SEARCH_DEPTH + 1;
        let (found, _) = search(&chain(deeper), &[deeper]);
        assert_eq!(found, Vec::<Range<i32>>::new());
    }

    #[test]
    fn a_failed_read_fails_the_toolbar_search() {
        let failed = search_toolbars(
            0,
            |node| Ok(if *node == 0 { vec![1, 2] } else { vec![] }),
            |node| {
                if *node == 2 {
                    Err(Error::Generic("busy".to_string()))
                } else {
                    Ok(TopBarElement::Toolbar(0..40))
                }
            },
        );
        assert!(failed.is_err());
    }
}
