//! The covers that slide pictures of a strip's windows during a strip move,
//! and the `ScreenCaptureKit` pictures they show (miri#509).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use bevy::math::{IRect, IVec2};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSPanel, NSScreen, NSView,
    NSViewLayerContentsRedrawPolicy, NSWindowAnimationBehavior, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGColor, CGColorSpace, CGColorSpaceModel, CGDirectDisplayID, CGImage,
    CGPreflightScreenCaptureAccess,
};
use objc2_foundation::{NSArray, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CAKeyframeAnimation, CALayer, CAMediaTiming as _, CATransaction, kCAAnimationDiscrete,
};
use objc2_screen_capture_kit::{SCContentFilter, SCShareableContent, SCWindow};
use tracing::debug;

use crate::ecs::slice_photos::{Photo, PhotoError};
use crate::ecs::strip_cover::{CoverScene, CoverScreen, CoverSubject};
use crate::events::{Event, EventSender};
use crate::overlay::{cg_abs_to_cocoa, primary_screen_height};
use crate::platform::WinID;
use crate::platform::slice_tap::{SlicePanel, set_cover_panels};
use crate::slice_screen::{Filter, capture};
use crate::types::commands::Side;

/// The key of the slide animation on a cover's strip layer. A new slide
/// replaces the one before it.
const SLIDE_KEY: &str = "slide";

/// The corner radius of a window's frame on a cover while a resize crops
/// its picture or opens a strip right of it, in points. Matches modern macOS
/// window corner radii so cropped edges stay rounded and fill tucks cleanly
/// behind the right corner cutouts.
const FRAME_RADIUS: f64 = 14.0;

/// The key of the fill opacity animation on a window's fill sublayer.
const FILL_OPACITY_KEY: &str = "fill-opacity";

/// A shown cover, as the event tap needs it on the main thread: its window,
/// its viewport, its strip layer, and each window's frame container layer,
/// back to front in paint order.
struct LiveCover {
    panel: WinID,
    viewport: IRect,
    strip: Retained<CALayer>,
    layers: Vec<(WinID, Retained<CALayer>)>,
}

thread_local! {
    static LIVE_COVERS: std::cell::RefCell<Vec<LiveCover>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The global screen rectangle of a container layer with bottom-left anchor
/// `(0, 0)` inside a strip layer at `strip_pos`, over `viewport`.
fn global_rect(viewport: IRect, strip_pos: CGPoint, container: CGRect) -> IRect {
    #[allow(clippy::cast_possible_truncation)]
    let round = |v: f64| v.round() as i32;
    let min_x = viewport.min.x + round(strip_pos.x + container.origin.x);
    let max_y = viewport.max.y - round(strip_pos.y + container.origin.y);
    let width = round(container.size.width).max(0);
    let height = round(container.size.height).max(0);
    IRect::new(min_x, max_y - height, min_x + width, max_y)
}

/// The pictures of every shown cover, as panels of their windows, where
/// Core Animation shows them now, the front one first. The event tap asks on
/// the main thread when a press or a scroll arrives.
pub(crate) fn live_cover_panels(_mtm: MainThreadMarker) -> Vec<SlicePanel> {
    LIVE_COVERS.with(|live| {
        live.borrow()
            .iter()
            .flat_map(|cover| {
                // SAFETY: plain reads of the layers Core Animation shows now.
                let strip_shown = unsafe { cover.strip.presentationLayer() }
                    .unwrap_or_else(|| cover.strip.clone());
                let strip_pos = strip_shown.position();
                let frames = cover
                    .layers
                    .iter()
                    .map(|(id, container)| {
                        let shown = unsafe { container.presentationLayer() }
                            .unwrap_or_else(|| container.clone());
                        let rect = global_rect(
                            cover.viewport,
                            strip_pos,
                            CGRect::new(shown.position(), shown.bounds().size),
                        );
                        (*id, rect)
                    })
                    .collect::<Vec<_>>();
                live_panels(cover.panel, cover.viewport, &frames)
                    .into_iter()
                    .rev()
            })
            .collect()
    })
}

pub struct CoverScreenOS {
    mtm: MainThreadMarker,
    events: EventSender,
    covers: HashMap<CGDirectDisplayID, Cover>,
    /// The filter of each window's last lookup, once it arrives.
    filters: HashMap<WinID, Arc<OnceLock<Filter>>>,
}

/// One window on a cover: a frame that a resize moves and sizes, which
/// crops what it does not cover, and in it the window's picture at its own
/// size from the frame's top left corner, which never stretches.
struct Framed {
    id: WinID,
    /// The frame, with its anchor at its bottom left corner, so its position
    /// is that corner and its bounds are its size.
    container: Retained<CALayer>,
    /// The picture, a sublayer of `container`.
    picture: Retained<CALayer>,
    /// The size `picture` shows the window at, in points.
    picture_size: CGSize,
    /// Behind the picture, from a little left of its right edge: the
    /// colour of the window's surface there, which fills the strip a wider
    /// frame opens. `None` when the photo has no edge colour.
    fill: Option<Retained<CALayer>>,
}

/// Where the fill sublayer starts inside a picture of `picture_width` points:
/// tucked only behind the right rounded corner cutout, never reaching the
/// left half of a narrow picture.
fn fill_start(picture_width: f64) -> f64 {
    let overlap = FRAME_RADIUS.min(picture_width / 2.0);
    (picture_width - overlap).max(0.0)
}

/// The normalized time in `(0.0, 1.0]` when the linearly interpolated size
/// across `shares` crosses `picture_size + 0.5` on either axis, paired with
/// whether the frame is open (`true`) or closed (`false`) after the crossing.
/// `None` when the size stays on one side of the threshold throughout.
fn fill_crossing_time(
    from_size: CGSize,
    to_size: CGSize,
    picture_size: CGSize,
    shares: &[f64],
) -> Option<(f64, bool)> {
    let w_thresh = picture_size.width + 0.5;
    let h_thresh = picture_size.height + 0.5;
    let size_at = |share: f64| {
        (
            from_size.width + (to_size.width - from_size.width) * share,
            from_size.height + (to_size.height - from_size.height) * share,
        )
    };
    let axis_cross = |a0: f64, a1: f64, thresh: f64| -> Option<f64> {
        if (a0 > thresh) == (a1 > thresh) {
            return None;
        }
        let delta = a1 - a0;
        if delta.abs() < f64::EPSILON {
            Some(0.0)
        } else {
            Some(((thresh - a0) / delta).clamp(0.0, 1.0))
        }
    };
    let segments = shares.len().checked_sub(1)?;
    if segments == 0 {
        return None;
    }
    let (w_start, h_start) = size_at(shares[0]);
    let start_open = w_start > w_thresh || h_start > h_thresh;
    for (idx, pair) in shares.windows(2).enumerate() {
        let (w0, h0) = size_at(pair[0]);
        let (w1, h1) = size_at(pair[1]);
        let open1 = w1 > w_thresh || h1 > h_thresh;
        if open1 != start_open {
            let fw = axis_cross(w0, w1, w_thresh);
            let fh = axis_cross(h0, h1, h_thresh);
            let frac = match (fw, fh) {
                (Some(a), Some(b)) => {
                    if open1 {
                        a.min(b)
                    } else {
                        a.max(b)
                    }
                }
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => 0.0,
            };
            #[allow(clippy::cast_precision_loss)]
            let cross = ((idx as f64 + frac) / segments as f64).clamp(0.0, 1.0);
            return Some((cross, open1));
        }
    }
    None
}

/// How a picture of the size a frame heads to goes with the picture in the
/// frame, of size `shown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Heading {
    /// No smaller either way: it takes the frame's picture's place.
    Replaces,
    /// No larger either way: it lies over the frame's picture's top left
    /// corner.
    Overlays,
}

impl Heading {
    /// How a picture of `heading` points goes with one of `shown` points,
    /// or `None` when it is wider and shorter, or taller and narrower, so
    /// neither covers the other.
    fn of(heading: CGSize, shown: CGSize) -> Option<Self> {
        if heading.width >= shown.width - 0.5 && heading.height >= shown.height - 0.5 {
            Some(Self::Replaces)
        } else if heading.width <= shown.width + 0.5 && heading.height <= shown.height + 0.5 {
            Some(Self::Overlays)
        } else {
            None
        }
    }
}

impl Framed {
    /// Shows `photo`, a picture of the window at `size` points, in place of
    /// the frame's picture, at its own size, with the photo's edge colour
    /// behind it for a frame that opens past it.
    fn swap_picture(&mut self, photo: &Photo, size: CGSize) {
        let image: &CFType = &photo.image;
        self.drop_heading();
        unsafe { self.picture.setContents(Some(image.as_ref())) };
        self.picture_size = size;
        // Because `picture` is anchored at its top-left `(0, 1)` and its
        // `position.y` tracks `container`'s height animation, updating
        // `bounds` alone keeps the new picture pinned to the container's
        // top-left corner while the frame still moves.
        self.picture
            .setBounds(CGRect::new(CGPoint::new(0.0, 0.0), size));
        if let Some(fill) = self.fill.take() {
            fill.removeFromSuperlayer();
        }
        if let Some(color) = edge_fill(photo) {
            let fill = fill_layer(&color);
            self.container
                .insertSublayer_below(&fill, Some(&self.picture));
            self.fill = Some(fill);
        }
    }

    /// Lays `photo`, a picture of the window at `size` points, no larger
    /// than the frame's picture, over that picture's top left corner, which
    /// shows on around it until the frame has closed over it.
    fn lay_heading(&self, photo: &Photo, size: CGSize, scale: f64) {
        self.drop_heading();
        let image: &CFType = &photo.image;
        let over = CoverScreenOS::picture(
            image,
            CGRect::new(
                CGPoint::new(0.0, self.picture_size.height - size.height),
                size,
            ),
            scale,
        );
        self.picture.addSublayer(&over);
    }

    /// Dresses the frame as [`Self::dress`] does, and shows the picture of
    /// `headings` for its window at the size the frame heads to, if any,
    /// from the morph's start, in the same transaction, so the old picture
    /// and its fill never show on their own first.
    fn dress_heading(
        &mut self,
        from: CGRect,
        to: CGRect,
        shares: &[f64],
        duration: f64,
        headings: &[(WinID, IVec2, Photo)],
        scale: f64,
    ) {
        let heading = headings
            .iter()
            .filter(|(heading, ..)| *heading == self.id)
            .map(|(_, size, photo)| (CGSize::new(f64::from(size.x), f64::from(size.y)), photo))
            .find(|(size, _)| {
                (to.size.width - size.width).abs() <= 0.5
                    && (to.size.height - size.height).abs() <= 0.5
            })
            .and_then(|(size, photo)| {
                Heading::of(size, self.picture_size).map(|kind| (kind, size, photo))
            });
        let id = self.id;
        if let Some((Heading::Replaces, size, photo)) = heading {
            // Before the frame is dressed, which goes by the picture's size.
            self.swap_picture(photo, size);
            debug!(
                "cover picture of window {id} replaced at {}x{}",
                size.width, size.height
            );
        }
        self.dress(from, to, shares, duration);
        if let Some((Heading::Overlays, size, photo)) = heading {
            // After the frame is dressed, which drops any picture laid over
            // its own.
            self.lay_heading(photo, size, scale);
            debug!(
                "cover picture of window {id} headed to {}x{}",
                size.width, size.height
            );
        }
    }

    /// Puts the picture at the top left corner of a frame `height` points
    /// high. The picture's anchor is `(0, 1)` (its top left corner), so its
    /// position is `(0, height)` regardless of its own height.
    fn place_picture(&self, height: f64) {
        self.picture
            .setBounds(CGRect::new(CGPoint::new(0.0, 0.0), self.picture_size));
        self.picture.setPosition(CGPoint::new(0.0, height));
    }

    /// Takes away the picture [`CoverScreen::head_picture`] laid over this
    /// one's top left corner, if any.
    fn drop_heading(&self) {
        // SAFETY: the picture layer's only sublayers are the ones
        // `head_picture` added.
        unsafe { self.picture.setSublayers(None) };
    }

    /// Sets the frame up to move from `from` to `to` along `shares` over
    /// `duration` seconds, in the strip layer's space: a frame wider or taller
    /// than the picture shows the fill only while its presented size exceeds
    /// the picture, switching discretely at the exact interpolated crossing
    /// time, and one that crops or opens rounds its corners as a window's are.
    /// A frame of the picture's own size keeps the picture as it is.
    fn dress(&self, from: CGRect, to: CGRect, shares: &[f64], duration: f64) {
        let widest = from.size.width.max(to.size.width);
        let narrowest = from.size.width.min(to.size.width);
        let tallest = from.size.height.max(to.size.height);
        let shortest = from.size.height.min(to.size.height);
        let opens_w = widest > self.picture_size.width + 0.5;
        let opens_h = tallest > self.picture_size.height + 0.5;
        let opens = opens_w || opens_h;
        let crops =
            narrowest < self.picture_size.width - 0.5 || shortest < self.picture_size.height - 0.5;
        self.container
            .setCornerRadius(if opens || crops { FRAME_RADIUS } else { 0.0 });
        // A picture of where the last morph headed is not where this one
        // heads.
        self.drop_heading();
        self.picture.removeAllAnimations();
        self.place_picture(to.size.height);
        if duration > 0.0 && (to.size.height - from.size.height).abs() >= f64::EPSILON {
            let values = shares
                .iter()
                .map(|share| from.size.height + (to.size.height - from.size.height) * share)
                .collect::<Vec<_>>();
            self.picture.addAnimation_forKey(
                &keyframes("position.y", &values, duration),
                Some(&NSString::from_str("position.y")),
            );
        }
        if let Some(fill) = &self.fill {
            fill.removeAnimationForKey(&NSString::from_str(FILL_OPACITY_KEY));
            if !opens {
                fill.setHidden(true);
                fill.setOpacity(0.0);
                return;
            }
            let start = if opens_h {
                0.0
            } else {
                fill_start(self.picture_size.width)
            };
            fill.setFrame(CGRect::new(
                CGPoint::new(start, 0.0),
                CGSize::new((widest - start).max(0.0), tallest),
            ));
            fill.setHidden(false);
            let end_open = to.size.width > self.picture_size.width + 0.5
                || to.size.height > self.picture_size.height + 0.5;
            fill.setOpacity(if end_open { 1.0 } else { 0.0 });
            if duration > 0.0
                && let Some((cross_t, open_after)) =
                    fill_crossing_time(from.size, to.size, self.picture_size, shares)
            {
                let (before, after) = if open_after { (0.0, 1.0) } else { (1.0, 0.0) };
                fill.addAnimation_forKey(
                    &discrete_step("opacity", before, after, cross_t, duration),
                    Some(&NSString::from_str(FILL_OPACITY_KEY)),
                );
            }
        }
    }
}

/// The colour that fills the strip a resize opens right of `photo`, in the
/// photo's own colour space, so it matches the picture beside it.
fn edge_fill(photo: &Photo) -> Option<CFRetained<CGColor>> {
    let [red, green, blue, alpha] = photo.edge_color?;
    if alpha <= 0.0 {
        return None;
    }
    let space = CGImage::color_space(Some(&photo.image)).filter(|space| {
        CGColorSpace::model(Some(space)) == CGColorSpaceModel::RGB
            && CGColorSpace::number_of_components(Some(space)) == 3
    });
    let Some(space) = space else {
        return Some(CGColor::new_srgb(red, green, blue, alpha));
    };
    let components = [red, green, blue, alpha];
    // SAFETY: four components, one for each of the space's three and alpha.
    unsafe { CGColor::new(Some(&space), components.as_ptr()) }
}

/// A layer filled with `color`, hidden until a frame opens past its picture.
fn fill_layer(color: &CGColor) -> Retained<CALayer> {
    let fill = CALayer::new();
    fill.setAnchorPoint(CGPoint::new(0.0, 0.0));
    fill.setBackgroundColor(Some(color));
    fill.setHidden(true);
    fill
}

/// The four bands of a cover's mask around `hole`, in a cover of size
/// `whole`: left of it, right of it, below it, and above it. The hole is
/// held to the cover first, so a hole partly past an edge leaves no band
/// reaching past the cover or short of the hole.
fn mask_bands(whole: CGSize, hole: CGRect) -> [CGRect; 4] {
    let left = hole.origin.x.clamp(0.0, whole.width);
    let bottom = hole.origin.y.clamp(0.0, whole.height);
    let right = (hole.origin.x + hole.size.width).clamp(left, whole.width);
    let top = (hole.origin.y + hole.size.height).clamp(bottom, whole.height);
    let rect = |x: f64, y: f64, width: f64, height: f64| {
        CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
    };
    [
        rect(0.0, 0.0, left, whole.height),
        rect(right, 0.0, whole.width - right, whole.height),
        rect(left, 0.0, right - left, bottom),
        rect(left, top, right - left, whole.height - top),
    ]
}

/// The mask's bands for a hole that moves from `from` to `to` along
/// `shares`, one share at each even step of the move's time, as key times
/// from 0 to 1 with the bands at each. Core Animation moves each band in a
/// straight line between key times, and a band held to the cover bends
/// where the hole crosses an edge of it, so a step that crosses one gets a
/// key time there too.
fn mask_steps(whole: CGSize, from: CGRect, to: CGRect, shares: &[f64]) -> Vec<(f64, [CGRect; 4])> {
    let hole_at = |share: f64| {
        let mix = |a: f64, b: f64| a + (b - a) * share;
        CGRect::new(
            CGPoint::new(
                mix(from.origin.x, to.origin.x),
                mix(from.origin.y, to.origin.y),
            ),
            CGSize::new(
                mix(from.size.width, to.size.width),
                mix(from.size.height, to.size.height),
            ),
        )
    };
    // Each edge of a hole, with the cover's bounds along it.
    let edges = |hole: CGRect| {
        [
            (hole.origin.x, whole.width),
            (hole.origin.x + hole.size.width, whole.width),
            (hole.origin.y, whole.height),
            (hole.origin.y + hole.size.height, whole.height),
        ]
    };
    let Some(&first) = shares.first() else {
        return Vec::new();
    };
    #[allow(clippy::cast_precision_loss)]
    let segments = shares.len().saturating_sub(1).max(1) as f64;
    let mut steps = vec![(0.0, first)];
    for (index, pair) in shares.windows(2).enumerate() {
        let (start, end) = (pair[0], pair[1]);
        let (before, after) = (edges(hole_at(start)), edges(hole_at(end)));
        let mut crossings = before
            .iter()
            .zip(&after)
            .flat_map(|(&(a, limit), &(b, _))| {
                [0.0, limit]
                    .into_iter()
                    .filter(move |bound| (a - bound) * (b - bound) < 0.0)
                    .map(move |bound| (bound - a) / (b - a))
            })
            .collect::<Vec<_>>();
        crossings.sort_by(f64::total_cmp);
        crossings.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        #[allow(clippy::cast_precision_loss)]
        let at = |part: f64| (index as f64 + part) / segments;
        for part in crossings {
            steps.push((at(part), start + (end - start) * part));
        }
        steps.push((at(1.0), end));
    }
    steps
        .into_iter()
        .map(|(time, share)| (time, mask_bands(whole, hole_at(share))))
        .collect()
}

/// One animated number of a morph: the frame layer, which of its numbers
/// moves (an index into [`MORPH_TRACKS`]), and from where to where.
struct Lead {
    layer: Retained<CALayer>,
    track: usize,
    from: f64,
    to: f64,
}

/// The numbers of a frame layer a morph animates. Its anchor is its bottom
/// left corner, so its position is that corner and its bounds' size is its
/// size, the two parts of the frame a morph sets.
const MORPH_TRACKS: [&str; 4] = [
    "position.x",
    "position.y",
    "bounds.size.width",
    "bounds.size.height",
];

/// One display's cover: a panel over its viewport, the layer that holds the
/// strip's pictures, and the viewport it covers.
struct Cover {
    window: Retained<NSPanel>,
    strip: Retained<CALayer>,
    viewport: IRect,
    /// Each window's picture and the frame it shows from the strip origin.
    pictures: Vec<(WinID, IRect)>,
    /// Each window's frame and picture, which a resize moves on its own.
    layers: Vec<Framed>,
    /// The strip origin the cover was laid out at, which a resize keeps.
    laid: IVec2,
    /// The track of a morph that moves furthest, to tell how far it shows.
    lead: Option<Lead>,
    /// Where the strip origin shows now, as far as the engine has told.
    at: IVec2,
    /// Whether the cover is up.
    shown: bool,
    /// The windows whose frames the next morph opens onto the windows
    /// themselves.
    open: Vec<WinID>,
    /// Pictures of windows at the size their frames head to, which the next
    /// morph shows from its start: each window, its size, and its picture.
    headings: Vec<(WinID, IVec2, Photo)>,
}

impl CoverScreenOS {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        Self {
            mtm,
            events,
            covers: HashMap::new(),
            filters: HashMap::new(),
        }
    }

    fn cocoa_frame(&self, on_screen: IRect) -> NSRect {
        cg_abs_to_cocoa(
            NSRect::new(
                NSPoint::new(f64::from(on_screen.min.x), f64::from(on_screen.min.y)),
                NSSize::new(f64::from(on_screen.width()), f64::from(on_screen.height())),
            ),
            primary_screen_height(self.mtm),
        )
    }

    /// The backing scale of the screen that shows `on_screen`.
    fn scale_at(&self, on_screen: IRect) -> f64 {
        let frame = self.cocoa_frame(on_screen);
        let (x, y) = (
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        NSScreen::screens(self.mtm)
            .iter()
            .find(|screen| {
                let bounds = screen.frame();
                (bounds.origin.x..bounds.origin.x + bounds.size.width).contains(&x)
                    && (bounds.origin.y..bounds.origin.y + bounds.size.height).contains(&y)
            })
            .map_or(1.0, |screen| screen.backingScaleFactor())
    }

    /// The bounds of `display` in global points, if it is connected.
    fn display_bounds(display: CGDirectDisplayID) -> IRect {
        let bounds = objc2_core_graphics::CGDisplayBounds(display);
        #[allow(clippy::cast_possible_truncation)]
        let point = |value: f64| value.round() as i32;
        IRect::new(
            point(bounds.origin.x),
            point(bounds.origin.y),
            point(bounds.origin.x + bounds.size.width),
            point(bounds.origin.y + bounds.size.height),
        )
    }

    /// An opaque borderless panel at the level of the edge slice panels,
    /// below floating windows and the menu bar. It never animates its own
    /// appearance, and takes clicks without activating the engine.
    fn make_panel(&self, frame: NSRect) -> Retained<NSPanel> {
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(self.mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setOpaque(true);
        panel.setBackgroundColor(Some(&NSColor::blackColor()));
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);
        // A press on a picture never reaches the panel: the event tap holds
        // it for the window. One between pictures stops here.
        panel.setIgnoresMouseEvents(false);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setLevel(NSFloatingWindowLevel - 1);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::Transient
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenNone,
        );
        let view = NSView::initWithFrame(
            NSView::alloc(self.mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), frame.size),
        );
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
        panel.setContentView(Some(&view));
        panel
    }

    /// `rect` in global points, in the layer space of a cover over
    /// `viewport`: from the viewport's bottom left corner, measured up. A
    /// window's root layer ignores `geometryFlipped`, so the turn is by hand.
    fn turned(rect: IRect, viewport: IRect) -> CGRect {
        CGRect::new(
            CGPoint::new(
                f64::from(rect.min.x - viewport.min.x),
                f64::from(viewport.max.y - rect.max.y),
            ),
            CGSize::new(f64::from(rect.width()), f64::from(rect.height())),
        )
    }

    fn picture(image: &CFType, frame: CGRect, scale: f64) -> Retained<CALayer> {
        let layer = CALayer::new();
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setFrame(frame);
        unsafe { layer.setContents(Some(image.as_ref())) };
        layer.setContentsScale(scale);
        layer
    }

    /// Makes a hole in the cover that moves from `from` to `to`, in the
    /// root layer's space, along `shares` over `duration` seconds, so the
    /// windows below the cover show through it. The root layer's mask is
    /// four bands around the hole, each of which moves and sizes with it.
    /// The panel stops being opaque while it has a hole.
    fn open_over(
        panel: &NSPanel,
        root: &CALayer,
        from: CGRect,
        to: CGRect,
        shares: &[f64],
        duration: f64,
    ) {
        let whole = root.bounds().size;
        // The bands at each step of the move, and at each moment between
        // steps where the hole crosses an edge of the cover, so the bands
        // meet the hole all the way.
        let steps = mask_steps(whole, from, to, shares);
        let times = steps.iter().map(|(time, _)| *time).collect::<Vec<_>>();
        let mask = CALayer::new();
        mask.setAnchorPoint(CGPoint::new(0.0, 0.0));
        mask.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), whole));
        let solid = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        for (index, to) in mask_bands(whole, to).into_iter().enumerate() {
            let band = CALayer::new();
            band.setAnchorPoint(CGPoint::new(0.0, 0.0));
            band.setBackgroundColor(Some(&solid));
            band.setFrame(to);
            let track = |rect: &CGRect| {
                [
                    rect.origin.x,
                    rect.origin.y,
                    rect.size.width,
                    rect.size.height,
                ]
            };
            for (number, path) in MORPH_TRACKS.into_iter().enumerate() {
                let values = steps
                    .iter()
                    .map(|(_, bands)| track(&bands[index])[number])
                    .collect::<Vec<_>>();
                let moves = values
                    .iter()
                    .any(|value| (value - track(&to)[number]).abs() >= f64::EPSILON);
                if duration <= 0.0 || !moves {
                    continue;
                }
                band.addAnimation_forKey(
                    &keyframes_at(path, &values, &times, duration),
                    Some(&NSString::from_str(path)),
                );
            }
            mask.addSublayer(&band);
        }
        // SAFETY: the mask is a new layer that no other layer holds.
        unsafe { root.setMask(Some(&mask)) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
    }

    /// Closes any hole [`Self::open_over`] made in the cover.
    fn close_over(panel: &NSPanel, root: &CALayer) {
        if root.mask().is_some() {
            // SAFETY: taking a mask away holds no layer.
            unsafe { root.setMask(None) };
        }
        if !panel.isOpaque() {
            panel.setOpaque(true);
            panel.setBackgroundColor(Some(&NSColor::blackColor()));
        }
    }

    /// Window `id`'s frame at `frame` in the strip layer's space, holding
    /// `photo` at the frame's size.
    fn framed(id: WinID, photo: &Photo, frame: CGRect, scale: f64) -> Framed {
        let container = CALayer::new();
        // Its position is its bottom left corner, so a resize moves the left
        // edge and the width on their own.
        container.setAnchorPoint(CGPoint::new(0.0, 0.0));
        container.setFrame(frame);
        container.setMasksToBounds(true);
        let fill = edge_fill(photo).map(|color| fill_layer(&color));
        if let Some(fill) = &fill {
            container.addSublayer(fill);
        }
        let image: &CFType = &photo.image;
        let picture = Self::picture(
            image,
            CGRect::new(CGPoint::new(0.0, 0.0), frame.size),
            scale,
        );
        // Top-left anchor `(0, 1)` pins the picture to `container`'s top edge
        // during height morphs and mid-flight picture swaps alike.
        picture.setAnchorPoint(CGPoint::new(0.0, 1.0));
        picture.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), frame.size));
        picture.setPosition(CGPoint::new(0.0, frame.size.height));
        container.addSublayer(&picture);
        Framed {
            id,
            container,
            picture,
            picture_size: frame.size,
            fill,
        }
    }

    /// Tells the event tap where every shown cover's pictures are: the
    /// live layers for presses and scrolls, and the last known places for
    /// the window lookups off the main thread.
    fn publish_panels(&self) {
        LIVE_COVERS.with(|live| {
            *live.borrow_mut() = self
                .covers
                .values()
                .filter(|cover| cover.shown)
                .filter_map(|cover| {
                    Some(LiveCover {
                        panel: WinID::try_from(cover.window.windowNumber()).ok()?,
                        viewport: cover.viewport,
                        strip: cover.strip.clone(),
                        layers: cover
                            .layers
                            .iter()
                            .map(|framed| (framed.id, framed.container.clone()))
                            .collect(),
                    })
                })
                .collect();
        });
        set_cover_panels(
            self.covers
                .values()
                .filter(|cover| cover.shown)
                .filter_map(|cover| {
                    let panel = WinID::try_from(cover.window.windowNumber()).ok()?;
                    Some(cover_panels(
                        panel,
                        cover.viewport,
                        cover.at,
                        &cover.pictures,
                    ))
                })
                // Pictures paint back to front, and the front one takes a
                // press where pictures overlap.
                .flat_map(|panels| panels.into_iter().rev())
                .collect(),
        );
    }

    fn reply(&self, subject: CoverSubject, generation: u32) -> Reply {
        Reply {
            events: self.events.clone(),
            subject,
            generation,
        }
    }
}

impl CoverScreen for CoverScreenOS {
    fn may_photograph(&self) -> bool {
        CGPreflightScreenCaptureAccess()
    }

    fn photograph_window(
        &mut self,
        window: WinID,
        size: IVec2,
        display: CGDirectDisplayID,
        generation: u32,
    ) {
        let reply = self.reply(CoverSubject::Window(window), generation);
        let scale = self.scale_at(Self::display_bounds(display));
        if let Some(filter) = self.filters.get(&window).and_then(|found| found.get()) {
            capture(&filter.0, size, scale, false, move |result| {
                reply.send(result);
            });
            return;
        }
        let filter = Arc::new(OnceLock::new());
        self.filters.insert(window, filter.clone());
        let found = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ScreenCaptureKit passes a valid content object or null.
                let Some(content) = (unsafe { content.as_ref() }) else {
                    reply.send(Err(crate::slice_screen::photo_error(error)));
                    return;
                };
                let target = unsafe { content.windows() }
                    .iter()
                    .find(|candidate| unsafe { candidate.windowID() } == window.cast_unsigned());
                let Some(target) = target else {
                    reply.send(Err(PhotoError::WindowGone));
                    return;
                };
                let filter = filter.get_or_init(|| {
                    Filter(unsafe {
                        SCContentFilter::initWithDesktopIndependentWindow(
                            SCContentFilter::alloc(),
                            &target,
                        )
                    })
                });
                let reply = reply.clone();
                capture(&filter.0, size, scale, false, move |result| {
                    reply.send(result);
                });
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &found,
            );
        }
    }

    fn photograph_desktop(
        &mut self,
        display: CGDirectDisplayID,
        excluding: &[WinID],
        generation: u32,
    ) {
        let reply = self.reply(CoverSubject::Desktop(display), generation);
        let bounds = Self::display_bounds(display);
        let scale = self.scale_at(bounds);
        let excluding = excluding.to_vec();
        let own = std::process::id();
        let found = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ScreenCaptureKit passes a valid content object or null.
                let Some(content) = (unsafe { content.as_ref() }) else {
                    reply.send(Err(crate::slice_screen::photo_error(error)));
                    return;
                };
                let Some(screen) = unsafe { content.displays() }
                    .iter()
                    .find(|candidate| unsafe { candidate.displayID() } == display)
                else {
                    reply.send(Err(PhotoError::Failed));
                    return;
                };
                // The strip's windows and the engine's own panels stay out.
                let left_out = unsafe { content.windows() }
                    .iter()
                    .filter(|window| {
                        let id = unsafe { window.windowID() };
                        excluding.iter().any(|left| left.cast_unsigned() == id)
                            || unsafe { window.owningApplication() }.is_some_and(|app| {
                                u32::try_from(unsafe { app.processID() }) == Ok(own)
                            })
                    })
                    .collect::<Vec<Retained<SCWindow>>>();
                let left_out = NSArray::from_retained_slice(&left_out);
                let filter = unsafe {
                    SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        &screen,
                        &left_out,
                    )
                };
                let reply = reply.clone();
                capture(&filter, bounds.size(), scale, false, move |result| {
                    reply.send(result);
                });
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                false, true, &found,
            );
        }
    }

    fn show(&mut self, scene: &CoverScene) {
        let frame = self.cocoa_frame(scene.viewport);
        let scale = self.scale_at(scene.viewport);
        let viewport = scene.viewport;
        if !self.covers.contains_key(&scene.display) {
            let window = self.make_panel(frame);
            let strip = CALayer::new();
            self.covers.insert(
                scene.display,
                Cover {
                    window,
                    strip,
                    viewport,
                    pictures: Vec::new(),
                    layers: Vec::new(),
                    laid: scene.strip,
                    lead: None,
                    at: scene.strip,
                    shown: false,
                    open: Vec::new(),
                    headings: Vec::new(),
                },
            );
        }
        let Some(cover) = self.covers.get_mut(&scene.display) else {
            return;
        };
        if cover.viewport != viewport {
            cover.window.setFrame_display(frame, false);
            cover.viewport = viewport;
        }
        let Some(root) = cover.window.contentView().and_then(|view| view.layer()) else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        root.setMasksToBounds(true);
        // SAFETY: no layer of the cover is in use elsewhere.
        unsafe { root.setSublayers(None) };
        Self::close_over(&cover.window, &root);
        cover.open.clear();
        cover.headings.clear();
        let desktop: &CFType = &scene.desktop.image;
        root.addSublayer(&Self::picture(
            desktop,
            Self::turned(scene.display_bounds, viewport),
            scale,
        ));
        // The strip layer spans the viewport, and moves by the strip's
        // offset. Its pictures sit at their frames from the strip's origin.
        let strip = CALayer::new();
        strip.setAnchorPoint(CGPoint::new(0.0, 0.0));
        strip.setBounds(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(f64::from(viewport.width()), f64::from(viewport.height())),
        ));
        strip.setPosition(CGPoint::new(f64::from(scene.strip.x - viewport.min.x), 0.0));
        let mut layers = Vec::with_capacity(scene.pictures.len());
        for (id, rel, photo) in &scene.pictures {
            let on_screen = IRect::from_corners(
                IVec2::new(rel.min.x + viewport.min.x, rel.min.y + scene.strip.y),
                IVec2::new(rel.max.x + viewport.min.x, rel.max.y + scene.strip.y),
            );
            let framed = Self::framed(*id, photo, Self::turned(on_screen, viewport), scale);
            strip.addSublayer(&framed.container);
            layers.push(framed);
        }
        root.addSublayer(&strip);
        CATransaction::commit();
        cover.strip = strip;
        cover.layers = layers;
        cover.laid = scene.strip;
        cover.lead = None;
        cover.pictures = scene
            .pictures
            .iter()
            .map(|(id, rel, _)| (*id, *rel))
            .collect();
        cover.at = scene.strip;
        cover.shown = true;
        cover.window.orderFrontRegardless();
        CATransaction::flush();
        self.publish_panels();
        debug!(
            "cover shown on display {}: {} pictures",
            scene.display,
            scene.pictures.len()
        );
    }

    fn slide(&mut self, display: CGDirectDisplayID, path: &[f64], step: f64) {
        let Some(cover) = self.covers.get(&display) else {
            return;
        };
        let Some(last) = path.last() else {
            return;
        };
        let origin = f64::from(cover.viewport.min.x);
        let values = path.iter().map(|x| x - origin).collect::<Vec<_>>();
        #[allow(clippy::cast_precision_loss)]
        let animation = keyframes(
            "position.x",
            &values,
            step * (path.len().saturating_sub(1)) as f64,
        );
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let position = cover.strip.position();
        cover
            .strip
            .setPosition(CGPoint::new(last - origin, position.y));
        cover
            .strip
            .addAnimation_forKey(&animation, Some(&NSString::from_str(SLIDE_KEY)));
        CATransaction::commit();
        CATransaction::flush();
    }

    fn morph(
        &mut self,
        display: CGDirectDisplayID,
        frames: &[(WinID, IRect, IRect)],
        shares: &[f64],
        step: f64,
    ) {
        let scale = if self
            .covers
            .get(&display)
            .is_some_and(|cover| !cover.headings.is_empty())
        {
            self.scale_at(Self::display_bounds(display))
        } else {
            1.0
        };
        let Some(cover) = self.covers.get_mut(&display) else {
            return;
        };
        #[allow(clippy::cast_precision_loss)]
        let duration = step * (shares.len().saturating_sub(1)) as f64;
        let (laid, viewport) = (cover.laid, cover.viewport);
        // A frame in global points, in the strip layer's space.
        let local = |rect: IRect| {
            Self::turned(
                IRect::from_corners(
                    IVec2::new(rect.min.x - laid.x + viewport.min.x, rect.min.y),
                    IVec2::new(rect.max.x - laid.x + viewport.min.x, rect.max.y),
                ),
                viewport,
            )
        };
        let mut lead = None;
        let open = std::mem::take(&mut cover.open);
        let headings = std::mem::take(&mut cover.headings);
        let root = cover.window.contentView().and_then(|view| view.layer());
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // The frame of the first open window is a hole in the cover that
        // moves as the frame does. Any hole of a morph before goes.
        let hole = frames
            .iter()
            .find(|(id, ..)| open.contains(id))
            .map(|(_, from, to)| (local(*from), local(*to)));
        if let Some(root) = &root {
            match hole {
                Some((from, to)) => {
                    let (from, to) = (
                        root.convertRect_fromLayer(from, Some(&cover.strip)),
                        root.convertRect_fromLayer(to, Some(&cover.strip)),
                    );
                    Self::open_over(&cover.window, root, from, to, shares, duration);
                }
                None => Self::close_over(&cover.window, root),
            }
        }
        for (id, from, to) in frames {
            let Some(window) = cover.layers.iter_mut().find(|window| window.id == *id) else {
                continue;
            };
            let (from, to) = (local(*from), local(*to));
            // The frame moves and sizes, and the picture in it stays put at
            // its own size: it crops, or opens onto the fill, never stretches.
            window.dress_heading(from, to, shares, duration, &headings, scale);
            let layer = &window.container;
            layer.removeAllAnimations();
            layer.setFrame(to);
            let tracks = [
                (from.origin.x, to.origin.x),
                (from.origin.y, to.origin.y),
                (from.size.width, to.size.width),
                (from.size.height, to.size.height),
            ];
            for (track, (path, (start, end))) in MORPH_TRACKS.into_iter().zip(tracks).enumerate() {
                if (end - start).abs() < f64::EPSILON {
                    continue;
                }
                if lead
                    .as_ref()
                    .is_none_or(|lead: &Lead| (end - start).abs() > (lead.to - lead.from).abs())
                {
                    lead = Some(Lead {
                        layer: layer.clone(),
                        track,
                        from: start,
                        to: end,
                    });
                }
                let values = shares
                    .iter()
                    .map(|share| start + (end - start) * share)
                    .collect::<Vec<_>>();
                layer.addAnimation_forKey(
                    &keyframes(path, &values, duration),
                    Some(&NSString::from_str(path)),
                );
            }
        }
        CATransaction::commit();
        CATransaction::flush();
        cover.lead = lead;
        // A press goes to the window where its picture rests.
        // In paint order, so a press where pictures overlap goes to the front.
        cover.pictures = cover
            .layers
            .iter()
            .filter_map(|framed| {
                let (_, _, to) = frames.iter().find(|(frame, ..)| *frame == framed.id)?;
                Some((framed.id, IRect::from_corners(to.min - laid, to.max - laid)))
            })
            .collect();
        cover.at = laid;
        self.publish_panels();
    }

    fn replace_picture(
        &mut self,
        display: CGDirectDisplayID,
        window: WinID,
        size: IVec2,
        photo: &Photo,
    ) -> bool {
        let Some(cover) = self.covers.get_mut(&display).filter(|cover| cover.shown) else {
            return false;
        };
        let Some(framed) = cover.layers.iter_mut().find(|framed| framed.id == window) else {
            return false;
        };
        let picture_size = CGSize::new(f64::from(size.x), f64::from(size.y));
        let target = framed.container.bounds().size;
        // If Core Animation's live presented size is still larger than the
        // fresh picture (e.g. a centered turn back with fractional rounding),
        // leave the old picture and fill untouched so desktop never shows in
        // the overhang; the caller retries next frame once the layer fits.
        let presented = unsafe { framed.container.presentationLayer() }
            .map_or(target, |shown| shown.bounds().size);
        if presented.width.max(target.width) > picture_size.width + 0.5
            || presented.height.max(target.height) > picture_size.height + 0.5
        {
            return false;
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // The new picture fits the live presented frame, so no fill is
        // needed now. A later morph that widens past it uses this photo's
        // edge colour.
        framed.swap_picture(photo, picture_size);
        // The frame keeps its rounded corners: while it still grows, it
        // crops the new picture's right edge.
        CATransaction::commit();
        CATransaction::flush();
        debug!(
            "cover picture of window {window} replaced at {}x{}",
            size.x, size.y
        );
        true
    }

    fn open(&mut self, display: CGDirectDisplayID, window: WinID) {
        if let Some(cover) = self.covers.get_mut(&display).filter(|cover| cover.shown) {
            cover.open.push(window);
        }
    }

    fn head_picture(
        &mut self,
        display: CGDirectDisplayID,
        window: WinID,
        size: IVec2,
        photo: &Photo,
    ) -> bool {
        let Some(cover) = self
            .covers
            .get_mut(&display)
            .filter(|cover| cover.shown && cover.layers.iter().any(|framed| framed.id == window))
        else {
            return false;
        };
        cover.headings.retain(|(id, ..)| *id != window);
        cover.headings.push((window, size, photo.clone()));
        true
    }

    fn morph_share(&self, display: CGDirectDisplayID) -> Option<f64> {
        let cover = self.covers.get(&display).filter(|cover| cover.shown)?;
        let lead = cover.lead.as_ref()?;
        // SAFETY: a plain read of the layer Core Animation shows now.
        let shown = unsafe { lead.layer.presentationLayer() }?;
        let (position, size) = (shown.position(), shown.bounds().size);
        let now = [position.x, position.y, size.width, size.height][lead.track];
        // Within half a point of the end, the morph shows landed.
        Some(if (now - lead.to).abs() < 0.5 {
            1.0
        } else {
            ((now - lead.from) / (lead.to - lead.from)).clamp(0.0, 1.0)
        })
    }

    fn track(&mut self, display: CGDirectDisplayID, x: f64) {
        let Some(cover) = self.covers.get_mut(&display) else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let x = x.round() as i32;
        if cover.at.x != x {
            cover.at.x = x;
            self.publish_panels();
        }
    }

    fn place(&mut self, display: CGDirectDisplayID, x: f64) {
        let Some(cover) = self.covers.get(&display) else {
            return;
        };
        let origin = f64::from(cover.viewport.min.x);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        cover.strip.removeAllAnimations();
        let position = cover.strip.position();
        cover
            .strip
            .setPosition(CGPoint::new(x - origin, position.y));
        CATransaction::commit();
        CATransaction::flush();
        self.track(display, x);
    }

    fn forget(&mut self, window: Option<WinID>) {
        match window {
            Some(window) => {
                self.filters.remove(&window);
            }
            None => self.filters.clear(),
        }
    }

    fn showing(&self, display: CGDirectDisplayID) -> Option<f64> {
        let cover = self.covers.get(&display).filter(|cover| cover.shown)?;
        // SAFETY: a plain read of the layer Core Animation shows now.
        let shown = unsafe { cover.strip.presentationLayer() }?;
        Some(shown.position().x + f64::from(cover.viewport.min.x))
    }

    fn shown(&self) -> Vec<CGDirectDisplayID> {
        self.covers
            .iter()
            .filter(|(_, cover)| cover.shown)
            .map(|(display, _)| *display)
            .collect()
    }

    fn hide(&mut self, display: CGDirectDisplayID) {
        let Some(cover) = self.covers.get_mut(&display) else {
            return;
        };
        cover.shown = false;
        cover.pictures.clear();
        cover.layers.clear();
        cover.lead = None;
        // The pictures go with the layer that held them.
        cover.strip = CALayer::new();
        cover.window.orderOut(None::<&AnyObject>);
        cover.open.clear();
        cover.headings.clear();
        if let Some(root) = cover.window.contentView().and_then(|view| view.layer()) {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            // SAFETY: no layer of the cover is in use elsewhere.
            unsafe { root.setSublayers(None) };
            Self::close_over(&cover.window, &root);
            CATransaction::commit();
        }
        CATransaction::flush();
        self.publish_panels();
        let id = display;
        debug!("cover hidden on display {id}");
    }
}

/// A discrete step animation on `path`: holds `before` for `[0, cross_t)`
/// and jumps to `after` for `[cross_t, 1]` over `seconds`, with zero linear
/// interpolation before the crossing instant.
fn discrete_step(
    path: &str,
    before: f64,
    after: f64,
    cross_t: f64,
    seconds: f64,
) -> Retained<CAKeyframeAnimation> {
    let values = [NSNumber::new_f64(before), NSNumber::new_f64(after)];
    let times = [
        NSNumber::new_f64(0.0),
        NSNumber::new_f64(cross_t.clamp(0.0, 1.0)),
        NSNumber::new_f64(1.0),
    ];
    let animation = CAKeyframeAnimation::animationWithKeyPath(Some(&NSString::from_str(path)));
    // SAFETY: an array of numbers is an array of objects.
    let values: Retained<NSArray> =
        unsafe { Retained::cast_unchecked(NSArray::from_retained_slice(&values)) };
    let times = NSArray::from_retained_slice(&times);
    unsafe {
        animation.setValues(Some(&values));
        animation.setKeyTimes(Some(&times));
        animation.setCalculationMode(kCAAnimationDiscrete);
    }
    animation.setDuration(seconds);
    animation
}

/// An animation of the number at `path` through `values`, evenly over
/// `seconds`. It holds nothing at its end: the layer's own value takes over.
fn keyframes(path: &str, values: &[f64], seconds: f64) -> Retained<CAKeyframeAnimation> {
    let values = values
        .iter()
        .map(|value| NSNumber::new_f64(*value))
        .collect::<Vec<_>>();
    let animation = CAKeyframeAnimation::animationWithKeyPath(Some(&NSString::from_str(path)));
    // SAFETY: an array of numbers is an array of objects.
    let values: Retained<NSArray> =
        unsafe { Retained::cast_unchecked(NSArray::from_retained_slice(&values)) };
    unsafe { animation.setValues(Some(&values)) };
    animation.setDuration(seconds);
    animation
}

/// An animation of the number at `path` through `values`, each at its key
/// time of `times`, from 0 to 1, over `seconds`. It holds nothing at its
/// end: the layer's own value takes over.
fn keyframes_at(
    path: &str,
    values: &[f64],
    times: &[f64],
    seconds: f64,
) -> Retained<CAKeyframeAnimation> {
    let animation = keyframes(path, values, seconds);
    let times = times
        .iter()
        .map(|time| NSNumber::new_f64(time.clamp(0.0, 1.0)))
        .collect::<Vec<_>>();
    let times = NSArray::from_retained_slice(&times);
    animation.setKeyTimes(Some(&times));
    animation
}

/// Where the picture of one ask goes. `ScreenCaptureKit` calls back on its
/// own queue, so the picture travels to the main thread as an [`Event`].
#[derive(Clone)]
struct Reply {
    events: EventSender,
    subject: CoverSubject,
    generation: u32,
}

impl Reply {
    fn send(&self, result: Result<Photo, PhotoError>) {
        _ = self.events.send(Event::CoverPhoto {
            subject: self.subject,
            generation: self.generation,
            result,
        });
    }
}

/// The pictures of a cover over `viewport`, given each picture's live global
/// frame in paint order (back to front), as panels of `panel`.
pub(crate) fn live_panels(
    panel: WinID,
    viewport: IRect,
    frames: &[(WinID, IRect)],
) -> Vec<SlicePanel> {
    frames
        .iter()
        .filter_map(|&(window, frame)| {
            let on_screen = frame.intersect(viewport);
            (!on_screen.is_empty()).then(|| SlicePanel {
                panel,
                window,
                on_screen,
                in_window: IRect::from_corners(
                    on_screen.min - frame.min,
                    on_screen.max - frame.min,
                ),
                side: Side::West,
                cover: true,
            })
        })
        .collect()
}

/// The pictures of a cover over `viewport`, with the strip origin at
/// `strip`, as panels of the cover window `panel`: each picture's part in
/// the viewport, and the part of its window that shows there, from the top
/// left of the window's shown frame.
pub(crate) fn cover_panels(
    panel: WinID,
    viewport: IRect,
    strip: IVec2,
    pictures: &[(WinID, IRect)],
) -> Vec<SlicePanel> {
    let frames = pictures
        .iter()
        .map(|&(window, rel)| {
            (
                window,
                IRect::from_corners(rel.min + strip, rel.max + strip),
            )
        })
        .collect::<Vec<_>>();
    live_panels(panel, viewport, &frames)
}

#[cfg(test)]
mod tests {
    use bevy::math::{IRect, IVec2};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::{
        CoverScreenOS, Heading, cover_panels, fill_crossing_time, fill_start, global_rect,
        live_panels, mask_bands, mask_steps,
    };

    /// A press on a picture lands on its window at the same point in the
    /// window, wherever the strip shows, and a picture cut by the
    /// viewport's edge maps only its shown part.
    #[test]
    fn each_picture_maps_its_shown_part_to_its_window() {
        let viewport = IRect::new(0, 25, 1000, 800);
        let pictures = [
            (7, IRect::new(0, 0, 600, 700)),
            (8, IRect::new(600, 0, 1200, 700)),
        ];
        let panels = cover_panels(99, viewport, IVec2::new(-300, 30), &pictures);
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].window, 7);
        assert_eq!(panels[0].on_screen, IRect::new(0, 30, 300, 730));
        assert_eq!(panels[0].in_window, IRect::new(300, 0, 600, 700));
        assert_eq!(panels[1].on_screen, IRect::new(300, 30, 900, 730));
        assert_eq!(panels[1].in_window, IRect::new(0, 0, 600, 700));
        assert!(panels.iter().all(|panel| panel.panel == 99));
    }

    #[test]
    fn a_picture_out_of_the_viewport_gets_no_panel() {
        let viewport = IRect::new(0, 0, 1000, 800);
        let pictures = [(7, IRect::new(0, 0, 600, 700))];
        assert_eq!(
            cover_panels(99, viewport, IVec2::new(-700, 0), &pictures),
            []
        );
    }

    /// `global_rect` is the inverse of `CoverScreenOS::turned` plus strip
    /// offset, so hit-testing reads each container's exact live frame.
    #[test]
    fn global_rect_inverts_turned_during_morph_and_slide() {
        let viewport = IRect::new(100, 30, 1100, 830);
        let laid = IVec2::new(120, 38);
        let live_global = IRect::new(250, 38, 700, 800);
        let local = CoverScreenOS::turned(
            IRect::from_corners(
                IVec2::new(
                    live_global.min.x - laid.x + viewport.min.x,
                    live_global.min.y,
                ),
                IVec2::new(
                    live_global.max.x - laid.x + viewport.min.x,
                    live_global.max.y,
                ),
            ),
            viewport,
        );
        let strip_pos = CGPoint::new(f64::from(laid.x - viewport.min.x), 0.0);
        assert_eq!(global_rect(viewport, strip_pos, local), live_global);

        // During a morph where A grows into B's old space, live frames still
        // map a click on B's visible picture to B, not A.
        let frames = [
            (1, IRect::new(100, 38, 450, 800)),
            (2, IRect::new(458, 38, 900, 800)),
        ];
        let panels = live_panels(99, viewport, &frames);
        let click_on_b = CGPoint::new(500.0, 200.0);
        let hit = panels.iter().rev().find(|p| p.contains(click_on_b));
        assert_eq!(hit.map(|p| p.window), Some(2));
    }

    /// The bands around a hole inside the cover meet it on every side and
    /// cover the rest of the cover once.
    #[test]
    fn mask_bands_meet_a_hole_inside_the_cover() {
        let whole = CGSize::new(1000.0, 800.0);
        let hole = CGRect::new(CGPoint::new(100.0, 50.0), CGSize::new(300.0, 400.0));
        let [left, right, below, above] = mask_bands(whole, hole);
        assert_eq!(left, rect(0.0, 0.0, 100.0, 800.0));
        assert_eq!(right, rect(400.0, 0.0, 600.0, 800.0));
        assert_eq!(below, rect(100.0, 0.0, 300.0, 50.0));
        assert_eq!(above, rect(100.0, 450.0, 300.0, 350.0));
    }

    /// A hole that starts past the cover's left edge, as a window that
    /// opens from off screen does, leaves no band past the edge: one would
    /// reach under the window's own place, or leave a seam beside it.
    #[test]
    fn mask_bands_hold_a_hole_past_an_edge_to_the_cover() {
        let whole = CGSize::new(1000.0, 800.0);
        let hole = CGRect::new(CGPoint::new(-200.0, -30.0), CGSize::new(500.0, 900.0));
        let [left, right, below, above] = mask_bands(whole, hole);
        assert_eq!(left, rect(0.0, 0.0, 0.0, 800.0), "nothing left of it");
        assert_eq!(right, rect(300.0, 0.0, 700.0, 800.0));
        assert_eq!(below, rect(0.0, 0.0, 300.0, 0.0), "nothing below it");
        assert_eq!(above, rect(0.0, 800.0, 300.0, 0.0), "nothing above it");

        // A hole wholly past the right edge leaves the cover whole.
        let gone = CGRect::new(CGPoint::new(1200.0, 0.0), CGSize::new(300.0, 800.0));
        let [left, right, ..] = mask_bands(whole, gone);
        assert_eq!(left, rect(0.0, 0.0, 1000.0, 800.0));
        assert_eq!(right.size.width, 0.0);
    }

    /// A hole that crosses the cover's left edge between two steps gets a
    /// key time where it crosses, so the left band, held at nothing until
    /// then, does not open early on a straight line between the steps.
    #[test]
    fn mask_steps_add_a_key_time_where_the_hole_crosses_an_edge() {
        let whole = CGSize::new(1000.0, 800.0);
        let from = rect(-10.0, 0.0, 100.0, 800.0);
        let to = rect(10.0, 0.0, 100.0, 800.0);
        let steps = mask_steps(whole, from, to, &[0.0, 1.0]);
        let times = steps.iter().map(|(time, _)| *time).collect::<Vec<_>>();
        assert_eq!(times, vec![0.0, 0.5, 1.0], "a key time at the crossing");
        let left = steps
            .iter()
            .map(|(_, bands)| bands[0].size.width)
            .collect::<Vec<_>>();
        assert_eq!(left, vec![0.0, 0.0, 10.0]);

        // Between key times, a straight line through the bands gives the
        // bands of the hole there.
        for quarter in 1..4 {
            let time = f64::from(quarter) / 4.0;
            let (low, high) = steps
                .windows(2)
                .map(|pair| (pair[0], pair[1]))
                .find(|((start, _), (end, _))| (*start..=*end).contains(&time))
                .expect("a step around it");
            let part = (time - low.0) / (high.0 - low.0);
            let width = low.1[0].size.width + (high.1[0].size.width - low.1[0].size.width) * part;
            let hole = rect(-10.0 + 20.0 * time, 0.0, 100.0, 800.0);
            let exact = mask_bands(whole, hole)[0].size.width;
            assert!(
                (width - exact).abs() < 1e-9,
                "at {time}: {width} vs {exact}"
            );
        }
    }

    /// Steps that cross no edge keep their even key times.
    #[test]
    fn mask_steps_keep_even_key_times_inside_the_cover() {
        let whole = CGSize::new(1000.0, 800.0);
        let steps = mask_steps(
            whole,
            rect(100.0, 0.0, 300.0, 800.0),
            rect(200.0, 0.0, 300.0, 800.0),
            &[0.0, 0.4, 1.0],
        );
        let times = steps.iter().map(|(time, _)| *time).collect::<Vec<_>>();
        assert_eq!(times, vec![0.0, 0.5, 1.0]);
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
    }

    /// A picture of the size a frame heads to takes the frame's picture's
    /// place when no smaller, lies over its corner when no larger, and goes
    /// nowhere when neither covers the other.
    #[test]
    fn a_heading_replaces_or_overlays_by_its_size() {
        let size = |w, h| CGSize::new(w, h);
        let shown = size(700.0, 600.0);
        assert_eq!(
            Heading::of(size(1000.0, 700.0), shown),
            Some(Heading::Replaces)
        );
        assert_eq!(
            Heading::of(size(700.2, 599.8), shown),
            Some(Heading::Replaces),
            "the same size, to within rounding"
        );
        assert_eq!(
            Heading::of(size(500.0, 600.0), shown),
            Some(Heading::Overlays)
        );
        assert_eq!(Heading::of(size(1000.0, 500.0), shown), None);
        assert_eq!(Heading::of(size(500.0, 700.0), shown), None);
    }

    #[test]
    fn fill_stays_confined_to_right_corner_and_gates_on_presented_width() {
        // A narrow picture never lets fill reach its left half.
        assert_eq!(fill_start(16.0), 8.0);
        assert_eq!(fill_start(400.0), 386.0);

        // Segment [198, 206] for a 200-pt picture (threshold 200.5) crosses
        // at 2.5/8 = 0.3125 of segment 1 of 3 -> (1 + 0.3125)/3 = 0.4375.
        let cross = fill_crossing_time(
            CGSize::new(180.0, 500.0),
            CGSize::new(220.0, 500.0),
            CGSize::new(200.0, 500.0),
            &[0.0, 0.45, 0.65, 1.0],
        );
        let (t, open_after) = cross.expect("crosses threshold");
        assert!(open_after);
        assert!((t - (1.0 + 2.5 / 8.0) / 3.0).abs() < 1e-9, "got {t}");

        // Height-only growth crosses on the vertical threshold too.
        let cross_h = fill_crossing_time(
            CGSize::new(200.0, 480.0),
            CGSize::new(200.0, 520.0),
            CGSize::new(200.0, 500.0),
            &[0.0, 0.45, 0.65, 1.0],
        );
        let (th, open_after_h) = cross_h.expect("crosses vertical threshold");
        assert!(open_after_h);
        assert!((th - (1.0 + 2.5 / 8.0) / 3.0).abs() < 1e-9, "got {th}");
        let _ = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0));
    }
}
