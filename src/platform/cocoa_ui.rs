//! Shared AppKit/CoreGraphics coordinate transforms and overlay window helpers.

use bevy::math::IRect;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSPanel, NSScreen, NSView,
    NSViewLayerContentsRedrawPolicy, NSWindowAnimationBehavior, NSWindowCollectionBehavior,
    NSWindowLevel, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Z-ordering of Paneru's borderless overlay windows above normal app windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayLevel {
    /// Edge-slice photo panels: above tiled windows, below strip covers.
    EdgeSlice,
    /// Strip move/resize picture covers: above edge slices, below dim overlays.
    StripCover,
    /// Inactive-window dim + focus border and drag feedback overlays.
    DimOverlay,
    /// Transient flash notifications: topmost Paneru overlay.
    FlashMessage,
}

impl OverlayLevel {
    pub(crate) fn window_level(self) -> NSWindowLevel {
        match self {
            Self::EdgeSlice => NSFloatingWindowLevel - 2,
            Self::StripCover => NSFloatingWindowLevel - 1,
            Self::DimOverlay => NSFloatingWindowLevel,
            Self::FlashMessage => NSFloatingWindowLevel + 1,
        }
    }
}

/// Converts an absolute CG screen frame (origin top-left, y-down) to Cocoa
/// screen coordinates (origin bottom-left of primary screen, y-up).
pub(crate) fn cg_abs_to_cocoa(frame: NSRect, primary_screen_height: f64) -> NSRect {
    let cocoa_y = primary_screen_height - frame.origin.y - frame.size.height;
    NSRect::new(NSPoint::new(frame.origin.x, cocoa_y), frame.size)
}

/// Height of the display at the global origin (the main display, whose Cocoa
/// frame origin is `(0, 0)`). The CG-Cocoa Y-flip is anchored to this display;
/// `NSScreen::screens()[0]` is not reliably the main display.
pub(crate) fn primary_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    let mut fallback = 0.0;
    let mut first = true;
    for screen in &screens {
        let frame = screen.frame();
        if first {
            fallback = frame.size.height;
            first = false;
        }
        if frame.origin.x == 0.0 && frame.origin.y == 0.0 {
            return frame.size.height;
        }
    }
    fallback
}

/// Converts a CG global integer rectangle into Cocoa screen coordinates.
pub(crate) fn cocoa_frame(mtm: MainThreadMarker, on_screen: IRect) -> NSRect {
    cg_abs_to_cocoa(
        NSRect::new(
            NSPoint::new(f64::from(on_screen.min.x), f64::from(on_screen.min.y)),
            NSSize::new(f64::from(on_screen.width()), f64::from(on_screen.height())),
        ),
        primary_screen_height(mtm),
    )
}

/// Converts a Cocoa screen-space rectangle into a display's flipped (top-left
/// origin) local view coordinates.
pub(crate) fn cocoa_to_local_flipped(rect: NSRect, screen_frame: NSRect) -> NSRect {
    NSRect::new(
        NSPoint::new(
            rect.origin.x - screen_frame.origin.x,
            (screen_frame.origin.y + screen_frame.size.height) - (rect.origin.y + rect.size.height),
        ),
        rect.size,
    )
}

/// Returns the backing scale factor of the screen containing `on_screen`.
pub(crate) fn scale_at(mtm: MainThreadMarker, on_screen: IRect) -> f64 {
    let frame = cocoa_frame(mtm, on_screen);
    let (x, y) = (
        frame.origin.x + frame.size.width / 2.0,
        frame.origin.y + frame.size.height / 2.0,
    );
    NSScreen::screens(mtm)
        .iter()
        .find(|screen| {
            let bounds = screen.frame();
            (bounds.origin.x..bounds.origin.x + bounds.size.width).contains(&x)
                && (bounds.origin.y..bounds.origin.y + bounds.size.height).contains(&y)
        })
        .map_or(1.0, |screen| screen.backingScaleFactor())
}

/// Creates a borderless, non-activating, layer-backed `NSPanel` for photo
/// slices or strip covers.
pub(crate) fn make_layer_panel(
    mtm: MainThreadMarker,
    frame: NSRect,
    level: OverlayLevel,
    opaque: bool,
    background: &NSColor,
    disable_animation: bool,
) -> Retained<NSPanel> {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setOpaque(opaque);
    panel.setBackgroundColor(Some(background));
    panel.setHasShadow(false);
    panel.setHidesOnDeactivate(false);
    panel.setIgnoresMouseEvents(false);
    if disable_animation {
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    }
    panel.setLevel(level.window_level());
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenNone,
    );
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), frame.size),
    );
    view.setWantsLayer(true);
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
    panel.setContentView(Some(&view));
    panel
}
