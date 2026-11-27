use std::cell::{Cell, RefCell};

use bevy::math::IRect;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext, NSBackingStoreType, NSBezierPath, NSColor,
    NSCompositingOperation, NSFont, NSFontWeightSemibold, NSGraphicsContext, NSImage,
    NSImageSymbolConfiguration, NSParagraphStyle, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableCopying, NSPoint, NSRect, NSSize, NSString,
};

use crate::ecs::drag::DragOverlayApi;
use crate::platform::cocoa_ui::{
    OverlayLevel, cg_abs_to_cocoa, cocoa_frame, cocoa_to_local_flipped, primary_screen_height,
};

#[derive(Clone, Debug, PartialEq)]
pub struct BorderParams {
    pub color: (f64, f64, f64),
    pub opacity: f64,
    pub width: f64,
    pub radius: f64,
}

/// Parameters for the fullscreen dim + cutout overlay.
#[derive(Clone, Debug, PartialEq)]
pub struct DimParams {
    pub opacity: f32,
    pub color: (f64, f64, f64),
    /// The focused window rect to cut out (in the view's local flipped coordinates).
    /// `None` means dim everything (no focused window).
    pub cutout: Option<NSRect>,
    pub border: Option<BorderParams>,
}

// ── DimView: fullscreen dark overlay with a transparent cutout + border ──

#[derive(Debug)]
struct DimViewIvars {
    params: RefCell<DimParams>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruDimView"]
    #[ivars = DimViewIvars]
    #[derive(Debug)]
    struct DimView;

    impl DimView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let params = self.ivars().params.borrow();
            let bounds = self.bounds();

            let dim_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                params.color.0 as CGFloat,
                params.color.1 as CGFloat,
                params.color.2 as CGFloat,
                CGFloat::from(params.opacity),
            );
            dim_color.setFill();
            NSBezierPath::fillRect(bounds);

            if let Some(cutout_rect) = params.cutout {
                let border_width = params.border.as_ref().map_or(0.0, |b| b.width);
                let half = border_width / 2.0;
                let radius = params.border.as_ref().map_or(0.0, |b| b.radius) as CGFloat;

                // Expand the cutout by half the border width so the clear hole
                // extends just past the window edge. The border straddles the
                // window edge: outer half visible in the cutout, inner half
                // hidden behind the window.
                let cutout = NSRect::new(
                    NSPoint::new(cutout_rect.origin.x - half, cutout_rect.origin.y - half),
                    NSSize::new(
                        cutout_rect.size.width + border_width,
                        cutout_rect.size.height + border_width,
                    ),
                );

                if let Some(ctx) = NSGraphicsContext::currentContext() {
                    ctx.setCompositingOperation(NSCompositingOperation::Clear);
                    let hole = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        cutout, radius, radius,
                    );
                    hole.fill();
                    ctx.setCompositingOperation(NSCompositingOperation::SourceOver);
                }

                if let Some(border) = &params.border {
                    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        cutout_rect,
                        radius,
                        radius,
                    );
                    path.setLineWidth(border.width as CGFloat);
                    let border_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                        border.color.0 as CGFloat,
                        border.color.1 as CGFloat,
                        border.color.2 as CGFloat,
                        border.opacity as CGFloat,
                    );
                    border_color.setStroke();
                    path.stroke();
                }
            }
        }

        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl DimView {
    fn new(mtm: MainThreadMarker, frame: NSRect, params: DimParams) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DimViewIvars {
            params: RefCell::new(params),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn update_params(&self, params: DimParams) -> bool {
        let mut stored = self.ivars().params.borrow_mut();
        if *stored == params {
            return false;
        }
        *stored = params;
        true
    }
}

/// Do two Cocoa rects overlap? Used to decide which displays a focused window
/// touches (a window straddling a seam touches both).
fn rects_intersect(a: NSRect, b: NSRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

// ── Overlay window factory ──────────────────────────────────────────────

fn make_overlay_window(
    mtm: MainThreadMarker,
    cocoa_frame: NSRect,
    level: OverlayLevel,
) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cocoa_frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setIgnoresMouseEvents(true);
    window.setHasShadow(false);
    window.setLevel(level.window_level());
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenNone,
    );

    window
}

// ── OverlayManager ──────────────────────────────────────────────────────

pub struct OverlayManager {
    mtm: MainThreadMarker,
    /// One overlay window per display. macOS will not reliably let a single
    /// window span multiple displays (with "Displays have separate Spaces" it
    /// renders on only one), so each screen gets its own overlay drawn in that
    /// screen's local coordinates. Indexed in lockstep with `NSScreen::screens`.
    overlays: Vec<(Retained<NSWindow>, Retained<DimView>)>,
    hidden: bool,
}

impl OverlayManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            overlays: Vec::new(),
            hidden: false,
        }
    }

    /// Update the per-display overlays.
    /// `focused_abs_cg` is the focused window rect in absolute CG coords,
    /// or `None` if no window is focused.
    pub fn update(
        &mut self,
        dim_opacity: f32,
        dim_color: (f64, f64, f64),
        focused_abs_cg: Option<NSRect>,
        border: Option<&BorderParams>,
    ) {
        let screen_h = primary_screen_height(self.mtm);
        let screens = NSScreen::screens(self.mtm);

        // The focused window in Cocoa global coords (shared across all screens).
        let focused_cocoa = focused_abs_cg.map(|cg| cg_abs_to_cocoa(cg, screen_h));

        // A display was added/removed - tear down and rebuild from scratch.
        if self.overlays.len() != screens.len() {
            for (window, _) in self.overlays.drain(..) {
                window.orderOut(None::<&AnyObject>);
            }
        }

        for (i, screen) in (&screens).into_iter().enumerate() {
            let frame = screen.frame();

            // Cut out the focused window on every display it touches, each in
            // that display's local (flipped, top-left origin) coordinates. A
            // window straddling a seam draws on both, clipped to each.
            let cutout_local = focused_cocoa
                .filter(|wc| rects_intersect(*wc, frame))
                .map(|wc| cocoa_to_local_flipped(wc, frame));

            let params = DimParams {
                opacity: dim_opacity,
                color: dim_color,
                cutout: cutout_local,
                border: border.cloned(),
            };

            if let Some((window, view)) = self.overlays.get(i) {
                if view.update_params(params) {
                    view.setNeedsDisplay(true);
                }
                if window.frame() != frame {
                    window.setFrame_display(frame, true);
                }
                if self.hidden {
                    window.orderFront(None::<&AnyObject>);
                }
            } else {
                let window = make_overlay_window(self.mtm, frame, OverlayLevel::DimOverlay);
                let view = DimView::new(self.mtm, frame, params);
                window.setContentView(Some(&view));
                window.orderFront(None::<&AnyObject>);
                self.overlays.push((window, view));
            }
        }
        self.hidden = false;
    }

    pub fn remove_all(&mut self) {
        for (window, _) in self.overlays.drain(..) {
            window.orderOut(None::<&AnyObject>);
        }
        self.hidden = false;
    }

    pub fn hide_all(&mut self) {
        if self.hidden {
            return;
        }
        for (window, _) in &self.overlays {
            window.orderOut(None::<&AnyObject>);
        }
        self.hidden = true;
    }
}

// ── DragOverlay ─────────────────────────────────────────────────────────

/// What one display's drag overlay draws, in that display's local, flipped
/// coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DragMark {
    Line {
        rect: NSRect,
        color: (f64, f64, f64),
    },
    Refusal(NSRect),
}

#[derive(Debug)]
struct DragViewIvars {
    mark: Cell<Option<DragMark>>,
    symbol: Option<Retained<NSImage>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruDragView"]
    #[ivars = DragViewIvars]
    #[derive(Debug)]
    struct DragView;

    impl DragView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let ivars = self.ivars();
            match ivars.mark.get() {
                Some(DragMark::Line { rect, color }) => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(color.0, color.1, color.2, 1.0)
                        .setFill();
                    let radius = rect.size.width.min(rect.size.height) / 2.0;
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius)
                        .fill();
                }
                Some(DragMark::Refusal(rect)) => {
                    if let Some(symbol) = &ivars.symbol {
                        symbol.drawInRect(rect);
                    }
                }
                None => {}
            }
        }

        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl DragView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        symbol: Option<Retained<NSImage>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DragViewIvars {
            mark: Cell::new(None),
            symbol,
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// How long the refusal symbol takes to fade out after the drag ends.
const REFUSAL_FADE: f64 = 0.15;

/// The feedback drawn while a window is dragged by its title bar: an
/// insertion line where a release would put it, or the `nosign` symbol
/// beside the pointer where a release would be refused.
///
/// Like [`OverlayManager`], it keeps one click-through window per display,
/// because one window cannot reliably span displays. Each draws only the
/// part of the shape on its display.
pub struct DragOverlay {
    mtm: MainThreadMarker,
    symbol: Option<Retained<NSImage>>,
    /// Indexed in lockstep with `NSScreen::screens`.
    overlays: Vec<(Retained<NSWindow>, Retained<DragView>)>,
    /// Whether the refusal symbol shows, so hiding fades it out.
    refusing: bool,
}

impl DragOverlay {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            symbol: refusal_symbol(),
            overlays: Vec::new(),
            refusing: false,
        }
    }

    fn show(&mut self, shape: IRect, mark: impl Fn(NSRect) -> DragMark) {
        let screens = NSScreen::screens(self.mtm);
        if self.overlays.len() != screens.len() {
            for (window, _) in self.overlays.drain(..) {
                window.orderOut(None::<&AnyObject>);
            }
        }
        let cocoa = cocoa_frame(self.mtm, shape);

        for (i, screen) in (&screens).into_iter().enumerate() {
            let frame = screen.frame();
            if i == self.overlays.len() {
                let window = make_overlay_window(self.mtm, frame, OverlayLevel::DimOverlay);
                let view = DragView::new(self.mtm, frame, self.symbol.clone());
                window.setContentView(Some(&view));
                self.overlays.push((window, view));
            }
            let (window, view) = &self.overlays[i];
            if window.frame() != frame {
                window.setFrame_display(frame, false);
            }
            if !rects_intersect(cocoa, frame) {
                view.ivars().mark.set(None);
                window.orderOut(None::<&AnyObject>);
                continue;
            }
            let local = cocoa_to_local_flipped(cocoa, frame);
            view.ivars().mark.set(Some(mark(local)));
            view.setNeedsDisplay(true);
            // Stops a fade still running from the last refusal.
            fade(window, 0.0, 1.0);
            window.orderFront(None::<&AnyObject>);
        }
    }
}

impl DragOverlayApi for DragOverlay {
    fn show_line(&mut self, line: IRect, color: (f64, f64, f64)) {
        self.show(line, |rect| DragMark::Line { rect, color });
        self.refusing = false;
    }

    fn show_refusal(&mut self, square: IRect) {
        self.show(square, DragMark::Refusal);
        self.refusing = true;
    }

    /// The refusal symbol fades out while the window returns; a line goes
    /// at once.
    fn hide(&mut self) {
        for (window, view) in &self.overlays {
            if self.refusing && window.isVisible() {
                fade(window, REFUSAL_FADE, 0.0);
            } else {
                view.ivars().mark.set(None);
                window.orderOut(None::<&AnyObject>);
            }
        }
        self.refusing = false;
    }

    fn remove(&mut self) {
        for (window, _) in self.overlays.drain(..) {
            window.orderOut(None::<&AnyObject>);
        }
        self.refusing = false;
    }
}

/// Animates `window`'s opacity to `alpha` over `duration` seconds. A zero
/// duration replaces any animation in flight.
fn fade(window: &NSWindow, duration: f64, alpha: f64) {
    NSAnimationContext::beginGrouping();
    NSAnimationContext::currentContext().setDuration(duration);
    window.animator().setAlphaValue(alpha);
    NSAnimationContext::endGrouping();
}

/// The `nosign` SF Symbol at 22 points, in the system red.
fn refusal_symbol() -> Option<Retained<NSImage>> {
    let symbol = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str("nosign"),
        Some(&NSString::from_str("Drop refused")),
    )?;
    // SAFETY: an AppKit constant, read-only for the life of the process.
    let weight = unsafe { NSFontWeightSemibold };
    let configuration = NSImageSymbolConfiguration::configurationWithPointSize_weight(
        f64::from(crate::ecs::drag::REFUSAL_SYMBOL),
        weight,
    )
    .configurationByApplyingConfiguration(
        &NSImageSymbolConfiguration::configurationWithHierarchicalColor(&NSColor::systemRedColor()),
    );
    symbol.imageWithSymbolConfiguration(&configuration)
}

// ── FlashMessage ────────────────────────────────────────────────────────

const FLASH_TEXT_FONT_SIZE: f64 = 30.0;

#[derive(Debug, Clone)]
struct FlashState {
    opacity: f32,
    message: Retained<NSString>,
    is_badge: bool,
}

#[derive(Debug)]
struct FlashMessageViewIvars {
    state: RefCell<FlashState>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruFlashMessageView"]
    #[ivars = FlashMessageViewIvars]
    #[derive(Debug)]
    struct FlashMessageView;

    impl FlashMessageView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let state = self.ivars().state.borrow();
            let bounds = self.bounds();
            let is_badge = state.is_badge;

            let bezel_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                0.12,
                0.12,
                0.12,
                CGFloat::from(state.opacity * 0.88),
            );
            bezel_color.setFill();
            let radius = if is_badge {
                24.0
            } else {
                bounds.size.height / 2.0
            };
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                bounds, radius, radius,
            );
            path.fill();

            let border_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                1.0,
                1.0,
                1.0,
                CGFloat::from(state.opacity * 0.15),
            );
            border_color.setStroke();
            path.setLineWidth(1.0);
            path.stroke();

            let font = if is_badge {
                NSFont::boldSystemFontOfSize(bounds.size.height * 0.62)
            } else {
                NSFont::systemFontOfSize(FLASH_TEXT_FONT_SIZE)
            };
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(
                1.0,
                1.0,
                1.0,
                CGFloat::from(state.opacity * 0.95),
            );

            let paragraph_style = unsafe {
                let style = NSParagraphStyle::defaultParagraphStyle().mutableCopy();
                let _: () = msg_send![&style, setAlignment: 1isize]; // NSTextAlignmentCenter
                let _: () = msg_send![&style, setLineBreakMode: 4isize]; // NSLineBreakByTruncatingTail
                style
            };

            let attr_str: Retained<NSAttributedString> = unsafe {
                let font_key = NSString::from_str("NSFont");
                let color_key = NSString::from_str("NSColor");
                let para_key = NSString::from_str("NSParagraphStyle");

                let keys = [&*font_key, &*color_key, &*para_key];
                let objects = [
                    &*font as &AnyObject,
                    &*color as &AnyObject,
                    &*paragraph_style as &AnyObject,
                ];

                let attributes = NSDictionary::from_slices(&keys, &objects);
                let alloc = NSAttributedString::alloc();
                msg_send![alloc, initWithString: &*state.message, attributes: &*attributes]
            };

            let text_size: NSSize = unsafe { msg_send![&attr_str, size] };

            let text_rect = if is_badge {
                NSRect::new(
                    NSPoint::new(
                        bounds.origin.x + (bounds.size.width - text_size.width) / 2.0,
                        bounds.origin.y + (bounds.size.height - text_size.height) / 2.0,
                    ),
                    text_size,
                )
            } else {
                let h_pad = 24.0;
                let available_width = (bounds.size.width - (2.0 * h_pad)).max(1.0);
                NSRect::new(
                    NSPoint::new(
                        bounds.origin.x + h_pad,
                        bounds.origin.y + (bounds.size.height - text_size.height) / 2.0,
                    ),
                    NSSize::new(available_width, text_size.height),
                )
            };

            unsafe {
                let _: () = msg_send![&attr_str, drawInRect: text_rect];
            };
        }
    }
);

impl FlashMessageView {
    fn new(mtm: MainThreadMarker, frame: NSRect, state: FlashState) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(FlashMessageViewIvars {
            state: RefCell::new(state),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn set_state(&self, state: FlashState) {
        *self.ivars().state.borrow_mut() = state;
        self.setNeedsDisplay(true);
    }
}

pub struct FlashMessageManager {
    mtm: MainThreadMarker,
    window: Option<(Retained<NSWindow>, Retained<FlashMessageView>)>,
}

impl FlashMessageManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self { mtm, window: None }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn show(&mut self, message: &str, opacity: f32, top_right_abs_cg: NSPoint) {
        let is_badge = message.chars().count() <= 2;
        let screen_h = primary_screen_height(self.mtm);
        let msg_ns = NSString::from_str(message);

        let size = if is_badge {
            NSSize::new(150.0, 150.0)
        } else {
            let font = NSFont::systemFontOfSize(FLASH_TEXT_FONT_SIZE);
            let font_key = NSString::from_str("NSFont");
            let keys = [&*font_key];
            let objects = [&*font as &AnyObject];
            let attributes = NSDictionary::from_slices(&keys, &objects);
            let attr_str: Retained<NSAttributedString> = unsafe {
                let alloc = NSAttributedString::alloc();
                msg_send![alloc, initWithString: &*msg_ns, attributes: &*attributes]
            };
            let text_size: NSSize = unsafe { msg_send![&attr_str, size] };
            let width = (text_size.width + 48.0).clamp(140.0, 780.0);
            NSSize::new(width, 64.0)
        };

        let padding = 20.0;
        let frame = NSRect::new(
            NSPoint::new(
                top_right_abs_cg.x - size.width - padding,
                screen_h - (top_right_abs_cg.y + size.height + padding),
            ),
            size,
        );
        let state = FlashState {
            opacity,
            message: msg_ns,
            is_badge,
        };

        if let Some((window, view)) = &self.window {
            view.set_state(state);
            if window.frame() != frame {
                window.setFrame_display(frame, true);
            }
            window.orderFront(None::<&AnyObject>);
        } else {
            let window = make_overlay_window(self.mtm, frame, OverlayLevel::FlashMessage);
            let view =
                FlashMessageView::new(self.mtm, NSRect::new(NSPoint::new(0.0, 0.0), size), state);
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            self.window = Some((window, view));
        }
    }

    pub fn remove(&mut self) {
        if let Some((window, _)) = self.window.take() {
            window.orderOut(None::<&AnyObject>);
        }
    }
}
