use accessibility_sys::{
    AXUIElementCopyElementAtPosition, AXUIElementCreateApplication, AXUIElementIsAttributeSettable,
    AXUIElementRef, AXValueCreate, AXValueGetValue, kAXChildrenAttribute, kAXCloseButtonAttribute,
    kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXErrorNotImplemented, kAXErrorSuccess,
    kAXFloatingWindowSubrole, kAXParentAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXToolbarRole, kAXUnknownRole,
    kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
};
use bevy::ecs::component::Component;
use bevy::math::{IRect, IVec2};
use core::ptr::NonNull;
use derive_more::{DerefMut, with_trait::Deref};
use mockall::automock;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize, Type, kCFBooleanFalse, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGWindowLevelForKey, CGWindowLevelKey, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGWindowLayer,
};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;
use stdext::function_name;
use stdext::sync::rw_lock::RwLockExt;
use tracing::{Level, debug, instrument, trace, warn};

use super::skylight::{
    _AXUIElementGetWindow, _SLPSGetFrontProcess, _SLPSSetFrontProcessWithOptions,
    AXUIElementCopyAttributeValue, AXUIElementPerformAction, AXUIElementSetAttributeValue,
    SLPSPostEventRecordTo, SLSWindowIteratorAdvance,
};
use super::window_writer::{FrameWrites, WindowWriters, WriteSink};
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

fn set_enhanced_ui(app_element: &CFRetained<AXUIWrapper>, enabled: bool, pid: Pid) -> Result<()> {
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
                    requested = enabled,
                    ax_error,
                    observed = ?observed,
                    "AXEnhancedUserInterface changed despite reported error"
                );
            } else {
                warn!(
                    pid,
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

/// Every window move and resize, carried to the window's app.
static WRITERS: LazyLock<Arc<WindowWriters<AxApp>>> =
    LazyLock::new(|| Arc::new(WindowWriters::new(WRITER_IDLE, AxApp::new)));

/// How long an app's writer outlives its last write.
const WRITER_IDLE: Duration = Duration::from_secs(5);

/// Waits up to `timeout` for every window move and resize asked for so far
/// to reach its app. Returns whether all of them did.
pub(super) fn flush_window_writes(timeout: Duration) -> bool {
    WRITERS.flush(timeout)
}

type RestoreEnhancedUi = Box<dyn FnOnce() -> Result<()> + Send>;

/// One app as its window writer sees it.
struct AxApp {
    pid: Option<Pid>,
    app: Option<CFRetained<AXUIWrapper>>,
    /// Held from [`WriteSink::begin`] to [`WriteSink::finish`], so the app
    /// keeps `AXEnhancedUserInterface` off for a whole batch of writes.
    enhanced_ui: Option<EnhancedUiGuard<RestoreEnhancedUi>>,
}

/// A window as its app's writer addresses it.
struct AxWindow {
    element: CFRetained<AXUIWrapper>,
    margins: Margins,
}

impl AxApp {
    fn new(pid: Option<Pid>) -> Self {
        let app = pid.and_then(|pid| {
            AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(pid) })
                .inspect_err(|err| warn!("error getting app reference: {err}"))
                .ok()
        });
        Self {
            pid,
            app,
            enhanced_ui: None,
        }
    }

    /// Suppresses app-driven move/resize animations until the guard drops.
    /// With `AXEnhancedUserInterface` on, apps like Chrome, Firefox, and Zen
    /// Browser animate every move and resize, which breaks window
    /// management. Accessibility clients such as Kindavim turn it on.
    fn disable_enhanced_ui(&self) -> Option<EnhancedUiGuard<RestoreEnhancedUi>> {
        let pid = self.pid?;
        let app_element = self.app.clone()?;
        let restore_element = app_element.clone();
        let restore: RestoreEnhancedUi =
            Box::new(move || set_enhanced_ui(&restore_element, true, pid));
        acquire_enhanced_ui(
            enhanced_ui_state(pid),
            pid,
            || {
                read_enhanced_ui(&app_element).inspect(|enabled| {
                    trace!(
                        target: "paneru::ax_diagnostics",
                        pid,
                        enabled,
                        "read AXEnhancedUserInterface"
                    );
                })
            },
            || set_enhanced_ui(&app_element, false, pid),
            restore,
        )
        .inspect_err(|err| warn!(pid, "error reading enhanced UI: {err}"))
        .ok()
        .flatten()
    }
}

impl WriteSink for AxApp {
    type Window = AxWindow;

    fn begin(&mut self) {
        self.enhanced_ui = self.disable_enhanced_ui();
    }

    fn set_position(&mut self, window: &AxWindow, origin: Origin) {
        set_ax_position(&window.element, origin + window.margins.min);
    }

    fn set_size(&mut self, window: &AxWindow, size: Size) {
        set_ax_size(
            &window.element,
            size - window.margins.min - window.margins.max,
        );
    }

    fn read_frame(&mut self, window: &AxWindow) -> Option<IRect> {
        let frame = ax_frame(&window.element).ok()?;
        Some(window.margins.tracked(frame))
    }

    fn finish(&mut self) {
        self.enhanced_ui = None;
    }
}

fn set_ax_position(element: &AXUIWrapper, origin: Origin) {
    let mut point = CGPoint::new(f64::from(origin.x), f64::from(origin.y));
    let position_ref = unsafe {
        AXValueCreate(
            kAXValueTypeCGPoint,
            NonNull::from(&mut point).as_ptr().cast(),
        )
    };
    if let Ok(position) = AXUIWrapper::from_retained(position_ref) {
        unsafe {
            AXUIElementSetAttributeValue(
                element.as_ptr(),
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                position.as_ref(),
            )
        };
    }
}

fn set_ax_size(element: &AXUIWrapper, size: Size) {
    let mut cgsize = CGSize::new(f64::from(size.x), f64::from(size.y));
    let size_ref = unsafe {
        AXValueCreate(
            kAXValueTypeCGSize,
            NonNull::from(&mut cgsize).as_ptr().cast(),
        )
    };
    if let Ok(size_value) = AXUIWrapper::from_retained(size_ref) {
        unsafe {
            AXUIElementSetAttributeValue(
                element.as_ptr(),
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                size_value.as_ref(),
            )
        };
    }
}

#[derive(Debug)]
pub enum WindowPadding {
    Vertical(i32),
    Horizontal(i32),
}

/// How far the frame a window shows sits inside the frame the engine tracks
/// for it, on each side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Margins {
    /// The left and top margins.
    pub min: IVec2,
    /// The right and bottom margins.
    pub max: IVec2,
}

impl Margins {
    pub fn padding(padding: IVec2) -> Self {
        Self {
            min: padding,
            max: padding,
        }
    }

    /// The margins of a window drawn over `edges`, the display's edge
    /// padding: the shown frame reaches past the tracked one by `edges`.
    pub fn filling(edges: Self) -> Self {
        Self {
            min: -edges.min,
            max: -edges.max,
        }
    }

    pub fn shown(self, frame: IRect) -> IRect {
        IRect::from_corners(frame.min + self.min, frame.max - self.max)
    }

    pub fn tracked(self, frame: IRect) -> IRect {
        IRect {
            min: frame.min - self.min,
            max: frame.max + self.max,
        }
    }
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
    /// Whether the app lets the window's size change at all. A window that
    /// does not, such as a fixed-size panel, floats unless a rule says
    /// otherwise. A failed read counts as resizable.
    fn is_resizable(&self) -> bool;
    /// Whether its app keeps the window above ordinary windows, as for a
    /// Picture-in-Picture video. Such a window floats unless a rule says
    /// otherwise. A failed read counts as an ordinary window.
    fn is_above_ordinary_windows(&self) -> bool;
    /// Reads the window's level again and keeps the answer for
    /// [`Self::is_above_ordinary_windows`]. Chrome shows its
    /// Picture-in-Picture window at the normal level and raises it a few
    /// tens of milliseconds later, so a read on arrival can miss the raise.
    /// A failed read leaves the earlier answer.
    fn refresh_above_ordinary_windows(&self) -> bool;
    fn reposition(&mut self, origin: Origin);
    fn resize(&mut self, size: Size);
    /// Moves and resizes the window in one write: the size at the old
    /// position, then the position, then the size again if the app limited
    /// it at the old position.
    fn place(&mut self, frame: IRect);
    /// Reads the window's frame back from the OS. Waits first, up to
    /// `LANDING_TIMEOUT`, for any move or resize the engine asked for to
    /// land, and fails if it has not. A system that must never wait on an
    /// app checks [`Self::write_pending`] first.
    fn update_frame(&mut self) -> Result<IRect>;
    /// Whether a move or resize the engine asked for has yet to reach the
    /// window. Until it does, the OS reports the frame the window is
    /// leaving, and [`Self::frame`] is the frame the engine asked for.
    fn write_pending(&self) -> bool;
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
    /// Maps the frame the engine tracks to the frame the window shows. The
    /// engine writes and reads every frame through it.
    fn margins(&self) -> Margins;
    /// `Some(edges)` while the window fills a maximized column: it then shows
    /// over `edges`, the display's edge padding, instead of keeping its
    /// window padding. The frame the window shows stays where it is until
    /// the next write.
    fn set_fill(&mut self, edges: Option<Margins>);
    fn fill(&self) -> Option<Margins>;
    fn border_radius(&self) -> Option<f64>;
    /// How far the window's top bar (title bar, toolbar, or navigation bar)
    /// reaches below its top edge, read through Accessibility. `Ok(None)`
    /// when the window exposes neither a close button nor a toolbar. An error
    /// when any read fails, such as while its app is busy, even after part of
    /// the top bar was read, so the caller tries again later rather than
    /// keeping a top bar that is missing a toolbar.
    fn top_bar_height(&self) -> Result<Option<i32>>;
    /// Whether the element at `point` is a control, such as a button, tab,
    /// or text field, or lies inside one. The bar around the controls is not
    /// a control. An error when Accessibility cannot name the element.
    fn control_at(&self, point: Origin) -> Result<bool>;
}

#[derive(Component, Deref, DerefMut)]
pub struct Window(Box<dyn WindowApi>);

impl Window {
    pub fn new(window: Box<dyn WindowApi>) -> Self {
        Window(window)
    }

    /// The frame the user sees, in screen points.
    pub fn shown_frame(&self) -> IRect {
        self.margins().shown(self.frame())
    }

    /// Whether `element` is this window's AX element. A window without an
    /// element cannot tell, so it claims every element.
    pub fn is_current_element(&self, element: &AXUIWrapper) -> bool {
        self.element().is_none_or(|own| *own == *element)
    }

    /// Swaps in `fresh`, a new AX element the app made for this same window,
    /// keeping the padding applied to the old one. Returns the old handle.
    ///
    /// # Errors
    ///
    /// Fails, leaving `self` unchanged, unless `fresh` names the same window
    /// through a different element and its frame can be read.
    pub fn replace_handle(&mut self, mut fresh: Window) -> Result<Window> {
        let replaces = fresh.id() == self.id()
            && fresh
                .element()
                .is_some_and(|element| !self.is_current_element(&element));
        if !replaces {
            return Err(Error::InvalidWindow);
        }
        fresh.set_padding(WindowPadding::Horizontal(self.horizontal_padding()));
        fresh.set_padding(WindowPadding::Vertical(self.vertical_padding()));
        fresh.set_fill(self.fill());
        fresh.update_frame()?;
        Ok(std::mem::replace(self, fresh))
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

/// The window server layer of window `id`, or `None` when it lists no such
/// window.
fn window_layer(id: WinID) -> Option<i32> {
    let id = u32::try_from(id).ok()?;
    let info = CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, id)?;
    let array = unsafe { info.cast_unchecked::<CFDictionary<CFString, CFType>>() };
    let dict = array.iter().next()?;
    dict.get(unsafe { kCGWindowLayer })?
        .downcast::<CFNumber>()
        .ok()?
        .as_i32()
}

/// Reads an element's frame through Accessibility.
fn ax_frame(element: &CFRetained<AXUIWrapper>) -> Result<IRect> {
    let read = |attribute: &'static str| unsafe {
        let mut value: *mut CFType = null_mut();
        AXUIElementCopyAttributeValue(
            element.as_ptr(),
            CFString::from_static_str(attribute).as_ref(),
            &mut value,
        )
        .to_result(function_name!())?;
        AXUIWrapper::from_retained(value)
    };
    let position = read(kAXPositionAttribute)?;
    let size = read(kAXSizeAttribute)?;

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
    Ok(irect_from(frame))
}

/// How many levels below the window the top bar search looks. Chromium
/// browsers nest their toolbars six levels down.
const TOP_BAR_SEARCH_DEPTH: usize = 6;
/// How many elements the top bar search inspects in total, so a window with a
/// large or wide tree costs a bounded number of Accessibility calls. Each
/// child read counts, whether or not the search descends into it.
const TOP_BAR_SEARCH_BUDGET: usize = 200;
/// Elements that start further below the window's top edge than this hold
/// content, not the top bar.
const TOP_BAR_SEARCH_REACH: i32 = 200;
/// A toolbar starting at most this far below the top bar found so far
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

/// How many levels above the pressed element [`WindowApi::control_at`] looks
/// for a control, so text inside a tab or button counts as that control.
const CONTROL_SEARCH_DEPTH: usize = 6;
/// Roles a press in a top bar acts on, so a double-click there is the
/// control's own and does not maximize the window.
const CONTROL_ROLES: [&str; 17] = [
    "AXButton",
    "AXCheckBox",
    "AXComboBox",
    "AXDisclosureTriangle",
    "AXIncrementor",
    "AXLink",
    "AXMenuBar",
    "AXMenuBarItem",
    "AXMenuButton",
    "AXMenuItem",
    "AXPopUpButton",
    "AXRadioButton",
    "AXSlider",
    "AXSplitter",
    "AXTextArea",
    "AXTextField",
    "AXValueIndicator",
];

/// The Accessibility reads a top bar measurement makes. `Ok(None)` means the
/// element does not have the attribute. An error means the read failed, such
/// as while the app is busy, and may succeed later.
trait TopBarTree {
    type Node;
    fn frame(&self, node: &Self::Node) -> Result<Option<IRect>>;
    fn close_button(&self, node: &Self::Node) -> Result<Option<Self::Node>>;
    fn children(&self, node: &Self::Node) -> Result<Vec<Self::Node>>;
    fn role(&self, node: &Self::Node) -> Result<Option<String>>;
}

/// The live Accessibility tree of a window.
struct AxTopBarTree;

impl AxTopBarTree {
    /// Reads an attribute, telling an element that lacks it apart from a
    /// read that failed.
    fn read<T: Type>(
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
        if code == kAXErrorSuccess {
            Ok(NonNull::new(value).map(|value| unsafe { CFRetained::from_raw(value.cast::<T>()) }))
        } else if [
            kAXErrorAttributeUnsupported,
            kAXErrorNoValue,
            kAXErrorNotImplemented,
        ]
        .contains(&code)
        {
            Ok(None)
        } else {
            Err(Error::Generic(format!(
                "reading {attribute}: Accessibility error {code}"
            )))
        }
    }
}

impl TopBarTree for AxTopBarTree {
    type Node = CFRetained<AXUIWrapper>;

    fn frame(&self, node: &Self::Node) -> Result<Option<IRect>> {
        let position = Self::read::<AXUIWrapper>(node, kAXPositionAttribute)?;
        let size = Self::read::<AXUIWrapper>(node, kAXSizeAttribute)?;
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

    fn close_button(&self, node: &Self::Node) -> Result<Option<Self::Node>> {
        Self::read::<AXUIWrapper>(node, kAXCloseButtonAttribute)
    }

    fn children(&self, node: &Self::Node) -> Result<Vec<Self::Node>> {
        Ok(
            Self::read::<CFArray<AXUIWrapper>>(node, kAXChildrenAttribute)?
                .map(|children| children.to_vec())
                .unwrap_or_default(),
        )
    }

    fn role(&self, node: &Self::Node) -> Result<Option<String>> {
        Ok(Self::read::<CFString>(node, kAXRoleAttribute)?.map(|role| role.to_string()))
    }
}

/// Measures how far a window's top bar reaches below its top edge.
///
/// The title bar is twice the distance from the window top to the middle of
/// the close button, since macOS centers the window buttons in the bar that
/// holds them, whether that is a plain title bar or a unified toolbar. Each
/// `AXToolbar` near the top that starts where the bar found so far ends, such
/// as a browser's address bar and bookmarks bar under its tab strip, extends it.
///
/// Any failed read fails the measurement, so the caller tries again later
/// instead of keeping a top bar that is missing the part it could not read.
fn measure_top_bar<T: TopBarTree>(tree: &T, window: &T::Node) -> Result<Option<i32>>
where
    T::Node: Clone,
{
    let frame = tree
        .frame(window)?
        .ok_or_else(|| Error::Generic("window has no frame".to_string()))?;
    let top = frame.min.y;
    let title_bar = match tree.close_button(window)? {
        Some(button) => tree
            .frame(&button)?
            .map(|button| button.min.y + button.max.y - 2 * top),
        None => None,
    };

    let toolbars = search_toolbars(
        window.clone(),
        |element| tree.children(element),
        |child| {
            let Some(role) = tree.role(child)? else {
                return Ok(TopBarElement::Other);
            };
            if TOP_BAR_LEAF_ROLES.contains(&role.as_str()) {
                return Ok(TopBarElement::Other);
            }
            let Some(child_frame) = tree.frame(child)? else {
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
    Ok(top_bar_extent(title_bar, toolbars).filter(|height| *height < frame.height()))
}

/// What the top bar search learns about one element.
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
/// `inspect` classifies an element, and every call counts against one
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

/// Makes `window_id` the key window of the process `psn` by sending synthesized events.
fn make_key_window(psn: &ProcessSerialNumber, window_id: WinID) {
    // Reason: On macOS 14 (Sonoma), CGSEncodeEventRecord serializes the raw event
    // buffer via NSKeyedArchiver, misinterpreting 0xFF fill as an ObjC class pointer,
    // causing SIGABRT. See https://github.com/karinushka/paneru/issues/123
    if macos_major_version() == 14 {
        debug!("make_key_window: skipped on macOS 14 (Sonoma) to prevent crash");
        return;
    }
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

/// Brings the process `psn` to the front with `window_id` as its key window,
/// without raising the window.
pub(super) fn focus_window(psn: ProcessSerialNumber, window_id: WinID) {
    unsafe {
        _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
    }
    make_key_window(&psn, window_id);
}

/// Brings the process `psn` to the front with its desktop window `window_id`
/// as the key window, as [`focus_window`] does.
///
/// macOS ignores the request when `psn` is already in front, as Finder is
/// when one of its windows has focus, so the menu bar stays on the old
/// display. Fronting this process first, which has no window, makes the
/// request a real switch.
pub(super) fn focus_desktop_window(psn: ProcessSerialNumber, window_id: WinID) -> Result<()> {
    let mut front = ProcessSerialNumber::default();
    if unsafe { _SLPSGetFrontProcess(&mut front) }
        .to_result(function_name!())
        .is_ok_and(|()| front == psn)
    {
        let own = super::process::psn_for_pid(Pid::try_from(std::process::id())?)?;
        unsafe {
            _SLPSSetFrontProcessWithOptions(&own, 0, CPS_USER_GENERATED);
        }
    }
    focus_window(psn, window_id);
    Ok(())
}

#[derive(Debug)]
pub struct WindowOS {
    id: WinID,
    ax_element: CFRetained<AXUIWrapper>,
    /// The frame last asked for or read back, and the writes on their way.
    writes: FrameWrites<AxApp>,
    vertical_padding: i32,
    horizontal_padding: i32,
    fill: Option<Margins>,
    border_radius: OnceLock<Option<f64>>,
    pid: OnceLock<Result<Pid>>,

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
    /// Whether the size can be set, cached after the first successful read:
    /// a window keeps its style for its whole life.
    resizable: OnceLock<bool>,
    /// Whether the app keeps the window above ordinary windows, cached after
    /// the first successful read. An `RwLock` because an app can raise a
    /// window just after it shows it, and
    /// [`WindowApi::refresh_above_ordinary_windows`] then reads it again.
    above_ordinary: RwLock<Option<bool>>,
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
            writes: FrameWrites::new(WRITERS.clone()),
            vertical_padding: 0,
            horizontal_padding: 0,
            fill: None,
            border_radius: OnceLock::new(),
            pid: OnceLock::new(),
            title: RwLock::new(None),
            role: OnceLock::new(),
            subrole: OnceLock::new(),
            resizable: OnceLock::new(),
            above_ordinary: RwLock::new(None),
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

    /// The window as its app's writer addresses it.
    fn ax_window(&self) -> AxWindow {
        AxWindow {
            element: self.ax_element.clone(),
            margins: self.margins(),
        }
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
        self.writes.frame()
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

    fn is_resizable(&self) -> bool {
        if let Some(resizable) = self.resizable.get() {
            return *resizable;
        }
        let attribute = CFString::from_static_str(kAXSizeAttribute);
        let mut settable = 0;
        let code = unsafe {
            AXUIElementIsAttributeSettable(
                self.ax_element.as_ptr(),
                (&raw const *attribute).cast(),
                &raw mut settable,
            )
        };
        if code != kAXErrorSuccess {
            return true;
        }
        let resizable = settable != 0;
        let _ = self.resizable.set(resizable);
        resizable
    }

    fn is_above_ordinary_windows(&self) -> bool {
        if let Some(above) = *self.above_ordinary.force_read() {
            return above;
        }
        self.refresh_above_ordinary_windows()
    }

    fn refresh_above_ordinary_windows(&self) -> bool {
        let Some(layer) = window_layer(self.id) else {
            return self.above_ordinary.force_read().unwrap_or(false);
        };
        let above = layer > CGWindowLevelForKey(CGWindowLevelKey::NormalWindowLevelKey);
        *self.above_ordinary.force_write() = Some(above);
        above
    }

    #[instrument(level = Level::TRACE)]
    fn reposition(&mut self, origin: Origin) {
        let pid = self.pid().ok();
        let window = self.ax_window();
        self.writes.reposition(pid, || window, origin);
    }

    #[instrument(level = Level::TRACE)]
    fn resize(&mut self, size: Size) {
        let pid = self.pid().ok();
        let window = self.ax_window();
        self.writes.resize(pid, || window, size);
    }

    #[instrument(level = Level::TRACE)]
    fn place(&mut self, frame: IRect) {
        let pid = self.pid().ok();
        let window = self.ax_window();
        self.writes.place(pid, || window, frame);
    }

    /// Reads the window's frame back from Accessibility, padding included.
    fn update_frame(&mut self) -> Result<IRect> {
        let pid = self.pid().ok();
        let margins = self.margins();
        let element = &self.ax_element;
        self.writes
            .update_frame(pid, || Ok(margins.tracked(ax_frame(element)?)))
    }

    fn write_pending(&self) -> bool {
        self.writes.write_pending(self.pid().ok())
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
        make_key_window(&psn, window_id);
    }

    /// Focuses the window and raises it to the front.
    #[instrument(level = Level::DEBUG)]
    fn focus_with_raise(&self, psn: ProcessSerialNumber) {
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
        }
        make_key_window(&psn, window_id);
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

    fn margins(&self) -> Margins {
        self.fill.map_or_else(
            || Margins::padding(IVec2::new(self.horizontal_padding, self.vertical_padding)),
            Margins::filling,
        )
    }

    fn set_fill(&mut self, edges: Option<Margins>) {
        let shown = self.margins().shown(self.writes.frame());
        self.fill = edges;
        self.writes.track(self.margins().tracked(shown));
    }

    fn fill(&self) -> Option<Margins> {
        self.fill
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

    fn top_bar_height(&self) -> Result<Option<i32>> {
        measure_top_bar(&AxTopBarTree, &self.ax_element)
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "screen coordinates are far below f32's exact integer range"
    )]
    fn control_at(&self, point: Origin) -> Result<bool> {
        let app = AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(self.pid()?) })?;
        let mut pressed: AXUIElementRef = null_mut();
        unsafe {
            AXUIElementCopyElementAtPosition(
                app.as_ptr(),
                point.x as f32,
                point.y as f32,
                &raw mut pressed,
            )
        }
        .to_result(function_name!())?;
        let mut element = AXUIWrapper::from_retained(pressed)?;
        for _ in 0..CONTROL_SEARCH_DEPTH {
            match AxTopBarTree.role(&element)?.as_deref() {
                Some(role) if CONTROL_ROLES.contains(&role) => return Ok(true),
                Some(role) if role == kAXWindowRole => return Ok(false),
                _ => {}
            }
            let Some(parent) = AxTopBarTree::read::<AXUIWrapper>(&element, kAXParentAttribute)?
            else {
                return Ok(false);
            };
            element = parent;
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::platform::ProcessSerialNumber;
    use crate::tests::TestHarness;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

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

    /// Offsets read from Helium, a Chromium browser, on macOS 27: the close
    /// button centers in the 34 point tab strip, and the address bar and
    /// bookmarks bar toolbars follow it.
    #[test]
    fn a_top_bar_extends_through_the_toolbars_under_the_title_bar() {
        assert_eq!(top_bar_extent(Some(34), vec![65..96, 31..65]), Some(96));
    }

    #[test]
    fn a_top_bar_is_the_title_bar_without_toolbars() {
        assert_eq!(top_bar_extent(Some(28), vec![]), Some(28));
    }

    /// A toolbar lower in the window, such as one at its bottom, is not part
    /// of the top bar.
    #[test]
    fn a_toolbar_apart_from_the_top_bar_is_ignored() {
        assert_eq!(top_bar_extent(Some(28), vec![120..150, 180..200]), Some(28));
        assert_eq!(top_bar_extent(None, vec![0..52, 180..200]), Some(52));
    }

    /// A wide tree, where every element has 1000 children, costs at most the
    /// budget in inspected elements, not the budget in parents read.
    #[test]
    fn the_top_bar_search_inspects_at_most_its_budget_in_a_wide_tree() {
        let mut inspected = 0;
        let toolbars = search_toolbars(
            0_usize,
            |depth| Ok(vec![depth + 1; 1000]),
            |_| {
                inspected += 1;
                Ok(TopBarElement::Container)
            },
        );
        assert!(toolbars.is_ok_and(|toolbars| toolbars.is_empty()));
        assert_eq!(inspected, TOP_BAR_SEARCH_BUDGET);
    }

    /// The search finds a toolbar nested under containers, and stops
    /// descending at its depth limit.
    #[test]
    fn the_top_bar_search_finds_nested_toolbars_within_its_depth() {
        let toolbar_at = |depth: usize| {
            search_toolbars(
                0_usize,
                |level| Ok(if *level < 10 { vec![level + 1] } else { vec![] }),
                |level| {
                    Ok(if *level == depth {
                        TopBarElement::Toolbar(31..65)
                    } else {
                        TopBarElement::Container
                    })
                },
            )
            .expect("no read fails")
        };
        assert_eq!(toolbar_at(TOP_BAR_SEARCH_DEPTH), vec![31..65]);
        assert_eq!(toolbar_at(TOP_BAR_SEARCH_DEPTH + 1), vec![]);
    }

    /// An Accessibility read in a [`FakeTree`].
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Read {
        Frame,
        CloseButton,
        Children,
        Role,
    }

    #[derive(Default)]
    struct FakeNode {
        frame: Option<IRect>,
        close_button: Option<usize>,
        children: Vec<usize>,
        role: Option<&'static str>,
    }

    /// A window's Accessibility tree, where one read can fail.
    #[derive(Default)]
    struct FakeTree {
        nodes: HashMap<usize, FakeNode>,
        failing: Option<(usize, Read)>,
    }

    impl FakeTree {
        fn check(&self, node: usize, read: Read) -> Result<&FakeNode> {
            if self.failing == Some((node, read)) {
                return Err(Error::Generic(format!("{read:?} of {node} timed out")));
            }
            Ok(&self.nodes[&node])
        }
    }

    impl TopBarTree for FakeTree {
        type Node = usize;

        fn frame(&self, node: &usize) -> Result<Option<IRect>> {
            Ok(self.check(*node, Read::Frame)?.frame)
        }

        fn close_button(&self, node: &usize) -> Result<Option<usize>> {
            Ok(self.check(*node, Read::CloseButton)?.close_button)
        }

        fn children(&self, node: &usize) -> Result<Vec<usize>> {
            Ok(self.check(*node, Read::Children)?.children.clone())
        }

        fn role(&self, node: &usize) -> Result<Option<String>> {
            Ok(self.check(*node, Read::Role)?.role.map(str::to_string))
        }
    }

    /// A browser window 100 points down the screen, shaped like Helium's: the
    /// close button centers in a 34 point tab strip, and the address bar and
    /// bookmarks bar toolbars follow it at different depths, next to a web
    /// area with no role or frame.
    fn browser_tree() -> FakeTree {
        let rect = |top, bottom| Some(IRect::new(0, top, 800, bottom));
        let nodes = [
            (
                0,
                FakeNode {
                    frame: rect(100, 900),
                    close_button: Some(1),
                    children: vec![2, 3],
                    role: Some("AXWindow"),
                },
            ),
            (
                1,
                FakeNode {
                    frame: Some(IRect::new(8, 110, 22, 124)),
                    role: Some("AXButton"),
                    ..FakeNode::default()
                },
            ),
            (
                2,
                FakeNode {
                    frame: rect(100, 300),
                    children: vec![4, 5],
                    role: Some("AXGroup"),
                    ..FakeNode::default()
                },
            ),
            (3, FakeNode::default()),
            (
                4,
                FakeNode {
                    frame: rect(131, 165),
                    role: Some(kAXToolbarRole),
                    ..FakeNode::default()
                },
            ),
            (
                5,
                FakeNode {
                    frame: rect(165, 200),
                    children: vec![6],
                    role: Some("AXGroup"),
                    ..FakeNode::default()
                },
            ),
            (
                6,
                FakeNode {
                    frame: rect(165, 196),
                    role: Some(kAXToolbarRole),
                    ..FakeNode::default()
                },
            ),
        ];
        FakeTree {
            nodes: nodes.into_iter().collect(),
            failing: None,
        }
    }

    /// Attributes an element does not have leave it out of the top bar, but
    /// do not fail the measurement.
    #[test]
    fn a_browser_top_bar_reaches_through_its_toolbars() {
        assert_eq!(measure_top_bar(&browser_tree(), &0).ok(), Some(Some(96)));
    }

    /// A read that fails partway, such as a busy browser timing out on its
    /// toolbars, fails the whole measurement so the engine tries again. It
    /// must not measure the title bar alone and keep 34 points.
    #[test]
    fn a_read_that_fails_partway_fails_the_top_bar_measurement() {
        for failing in [
            (0, Read::Frame),
            (0, Read::CloseButton),
            (1, Read::Frame),
            (0, Read::Children),
            (2, Read::Role),
            (2, Read::Frame),
            (5, Read::Children),
            (6, Read::Role),
            (6, Read::Frame),
        ] {
            let tree = FakeTree {
                failing: Some(failing),
                ..browser_tree()
            };
            let measured = measure_top_bar(&tree, &0);
            assert!(
                measured.is_err(),
                "failing {failing:?} measured {measured:?}"
            );
        }
    }

    #[test]
    fn a_window_without_buttons_or_toolbars_has_no_measured_top_bar() {
        assert_eq!(top_bar_extent(None, vec![]), None);
        assert_eq!(top_bar_extent(None, vec![60..90, 120..150]), None);
    }
}
