use accessibility_sys::{
    AXUIElementCreateApplication, AXUIElementRef, AXValueCreate, AXValueGetValue,
    kAXChildrenAttribute, kAXCloseButtonAttribute, kAXErrorAttributeUnsupported, kAXErrorNoValue,
    kAXErrorNotImplemented, kAXErrorSuccess, kAXFloatingWindowSubrole, kAXPositionAttribute,
    kAXRaiseAction, kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXToolbarRole,
    kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};
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
use crate::platform::{Pid, ProcessSerialNumber, WinID, macos_major_version};
use crate::util::{AXUIAttributes, AXUIWrapper, MacResult};

/// Per-PID ref-count for the `AXEnhancedUserInterface` workaround. Tracks how many
/// concurrent window operations are in-flight for each app so the attribute is only
/// re-enabled after the last one completes (safe under `par_iter_mut`).
static ENHANCED_UI_REFCOUNT: LazyLock<Mutex<HashMap<Pid, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Apps observed not to have `AXEnhancedUserInterface` set, so the workaround
/// above can skip them without asking again.
///
/// An `RwLock` rather than a `Mutex`: entries are written once and read by
/// many concurrent `par_iter_mut` workers afterwards, so readers must not
/// exclude each other. Entries die with the process, so a relaunch (new pid)
/// is asked afresh.
static ENHANCED_UI_ABSENT: LazyLock<RwLock<HashSet<Pid>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

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
    fn role(&self) -> Result<String>;
    fn subrole(&self) -> Result<String>;
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
    /// How far the window's top bar (title bar, toolbar, or navigation bar)
    /// reaches below its top edge, read through Accessibility. `Ok(None)`
    /// when the window exposes neither a close button nor a toolbar. An error
    /// when any read fails, such as while its app is busy, even after part of
    /// the top bar was read, so the caller tries again later rather than
    /// keeping a top bar that is missing a toolbar.
    fn top_bar_height(&self) -> Result<Option<i32>>;
}

#[derive(Component, Deref, DerefMut)]
pub struct Window(Box<dyn WindowApi>);

impl Window {
    pub fn new(window: Box<dyn WindowApi>) -> Self {
        Window(window)
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
    /// Set once this window's app is known not to use
    /// `AXEnhancedUserInterface` (the common case), so the steady-state check
    /// in [`Self::disable_enhanced_ui`] is a relaxed atomic load instead of
    /// contending for the global mutex from every `par_iter_mut` worker.
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
            enhanced_ui_absent: AtomicBool::new(false),
            title: RwLock::new(None),
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
        config
            .find_window_properties(&title, bundle_id.unwrap_or_default())
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

    /// Disables `AXEnhancedUserInterface` on this window's app if it is currently enabled.
    ///
    /// Uses a per-PID ref-count so that concurrent operations on windows of the same app
    /// (via `par_iter_mut`) keep the attribute disabled until the last caller re-enables it.
    ///
    /// This avoids animated move/resize that breaks window management for apps like Chrome,
    /// Firefox, and Zen Browser when accessibility clients (e.g. Kindavim) enable enhanced UI.
    fn disable_enhanced_ui(&self) {
        // Nothing to disable, and nothing to lock or ask: this window's app has
        // already been found not to use the attribute.
        if self.enhanced_ui_absent.load(Ordering::Relaxed) {
            return;
        }
        let Ok(pid) = self.pid() else { return };
        // Another window of the same app may have answered the question already.
        // Taken before the ref-count mutex, since the answer is usually "absent"
        // and that path should not touch the ref-count at all.
        if ENHANCED_UI_ABSENT
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&pid)
        {
            self.enhanced_ui_absent.store(true, Ordering::Relaxed);
            return;
        }
        // Scoped so the lock isn't held across the accessibility calls below:
        // each is a synchronous round-trip into another process, and holding a
        // global mutex across them would serialize every `par_iter_mut` worker
        // behind the slowest app.
        {
            let mut counts = ENHANCED_UI_REFCOUNT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(count) = counts.get_mut(&pid) {
                *count += 1;
                return;
            }
        }
        let Some(app_element) = self.app_reference() else {
            return;
        };
        let attr = CFString::from_static_str("AXEnhancedUserInterface");
        let enabled = app_element
            .get_attribute::<CFBoolean>(&attr)
            .is_ok_and(|v| CFBoolean::value(&v));
        if enabled {
            unsafe {
                AXUIElementSetAttributeValue(
                    app_element.as_ptr(),
                    attr.as_ref(),
                    kCFBooleanFalse.unwrap(),
                );
            }
            // Incremented rather than set: two windows of the same app can race
            // here and both owe a matching `reenable_enhanced_ui`; setting 1
            // would let the second one decrement past zero.
            *ENHANCED_UI_REFCOUNT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(pid)
                .or_insert(0) += 1;
        } else {
            ENHANCED_UI_ABSENT
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(pid);
            self.enhanced_ui_absent.store(true, Ordering::Relaxed);
        }
    }

    /// Re-enables `AXEnhancedUserInterface` on this window's app once the last concurrent
    /// caller has finished. Pairs with [`disable_enhanced_ui`].
    fn reenable_enhanced_ui(&self) {
        // Nothing was disabled, so there is no ref-count entry to find and no
        // reason to take the lock looking for one.
        if self.enhanced_ui_absent.load(Ordering::Relaxed) {
            return;
        }
        let Ok(pid) = self.pid() else { return };
        let mut counts = ENHANCED_UI_REFCOUNT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(count) = counts.get_mut(&pid) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count > 0 {
            return;
        }
        counts.remove(&pid);
        drop(counts);
        if let Some(app_element) = self.app_reference() {
            let attr = CFString::from_static_str("AXEnhancedUserInterface");
            unsafe {
                AXUIElementSetAttributeValue(
                    app_element.as_ptr(),
                    attr.as_ref(),
                    kCFBooleanTrue.unwrap(),
                );
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
        self.ax_element.role()
    }

    /// Retrieves the subrole of the window (e.g., "`AXStandardWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window subrole if successful, otherwise `Err(Error)`.
    fn subrole(&self) -> Result<String> {
        self.ax_element.subrole()
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
        self.disable_enhanced_ui();
        self.set_ax_position(origin);
        self.reenable_enhanced_ui();
    }

    #[instrument(level = Level::TRACE)]
    fn resize(&mut self, size: Size) {
        if self.frame.size() == size {
            trace!("already correct size.");
            return;
        }
        let previous_frame = self.frame;
        let target_origin = previous_frame.min;
        self.disable_enhanced_ui();
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
        self.reenable_enhanced_ui();
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
        self.frame = ax_frame(&self.ax_element)?;

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

    fn top_bar_height(&self) -> Result<Option<i32>> {
        measure_top_bar(&AxTopBarTree, &self.ax_element)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
