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
/// its picture or opens a strip right of it, in points.
const FRAME_RADIUS: f64 = 10.0;

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

/// The normalized time in `(0.0, 1.0]` when the linearly interpolated width
/// across `shares` crosses `picture_width + 0.5`, paired with whether the
/// frame is open (`true`) or closed (`false`) after the crossing. `None` when
/// the width stays on one side of the threshold throughout.
fn fill_crossing_time(
    from_width: f64,
    to_width: f64,
    picture_width: f64,
    shares: &[f64],
) -> Option<(f64, bool)> {
    let threshold = picture_width + 0.5;
    let width_at = |share: f64| from_width + (to_width - from_width) * share;
    let segments = shares.len().checked_sub(1)?;
    if segments == 0 {
        return None;
    }
    let start_open = width_at(shares[0]) > threshold;
    for (idx, pair) in shares.windows(2).enumerate() {
        let w0 = width_at(pair[0]);
        let w1 = width_at(pair[1]);
        let open1 = w1 > threshold;
        if open1 != start_open {
            let delta = w1 - w0;
            let frac = if delta.abs() < f64::EPSILON {
                0.0
            } else {
                ((threshold - w0) / delta).clamp(0.0, 1.0)
            };
            #[allow(clippy::cast_precision_loss)]
            let cross = ((idx as f64 + frac) / segments as f64).clamp(0.0, 1.0);
            return Some((cross, open1));
        }
    }
    None
}

impl Framed {
    /// Puts the picture at the top left corner of a frame `height` points
    /// high. A layer's space runs up from its bottom left corner.
    fn place_picture(&self, height: f64) {
        self.picture.setFrame(CGRect::new(
            CGPoint::new(0.0, height - self.picture_size.height),
            self.picture_size,
        ));
    }

    /// Sets the frame up to move from `from` to `to` along `shares` over
    /// `duration` seconds, in the strip layer's space: a frame wider than the
    /// picture shows the fill only while its presented width exceeds the
    /// picture, switching discretely at the exact interpolated crossing time,
    /// and one that crops or opens rounds its corners as a window's are. A
    /// frame of the picture's own size keeps the picture as it is.
    fn dress(&self, from: CGRect, to: CGRect, shares: &[f64], duration: f64) {
        let widest = from.size.width.max(to.size.width);
        let narrowest = from.size.width.min(to.size.width);
        let opens = widest > self.picture_size.width + 0.5;
        let crops = narrowest < self.picture_size.width - 0.5;
        self.container
            .setCornerRadius(if opens || crops { FRAME_RADIUS } else { 0.0 });
        self.place_picture(to.size.height);
        if let Some(fill) = &self.fill {
            fill.removeAnimationForKey(&NSString::from_str(FILL_OPACITY_KEY));
            if !opens {
                fill.setHidden(true);
                fill.setOpacity(0.0);
                return;
            }
            let start = fill_start(self.picture_size.width);
            fill.setFrame(CGRect::new(
                CGPoint::new(start, 0.0),
                CGSize::new((widest - start).max(0.0), to.size.height),
            ));
            fill.setHidden(false);
            let end_open = to.size.width > self.picture_size.width + 0.5;
            fill.setOpacity(if end_open { 1.0 } else { 0.0 });
            if duration > 0.0
                && let Some((cross_t, open_after)) = fill_crossing_time(
                    from.size.width,
                    to.size.width,
                    self.picture_size.width,
                    shares,
                )
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
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (id, from, to) in frames {
            let Some(window) = cover.layers.iter().find(|window| window.id == *id) else {
                continue;
            };
            let (from, to) = (local(*from), local(*to));
            // The frame moves and sizes, and the picture in it stays put at
            // its own size: it crops, or opens onto the fill, never stretches.
            let layer = &window.container;
            window.dress(from, to, shares, duration);
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
    ) {
        let Some(cover) = self.covers.get_mut(&display).filter(|cover| cover.shown) else {
            return;
        };
        let Some(framed) = cover.layers.iter_mut().find(|framed| framed.id == window) else {
            return;
        };
        let picture_size = CGSize::new(f64::from(size.x), f64::from(size.y));
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let image: &CFType = &photo.image;
        unsafe { framed.picture.setContents(Some(image.as_ref())) };
        framed.picture_size = picture_size;
        // The frame's own bounds are where the morph heads: the picture
        // goes at their top left corner, while the frame still moves.
        let target = framed.container.bounds().size;
        framed.place_picture(target.height);
        // The new picture fills its target frame, so normally no fill shows.
        // If Core Animation's live presented width is still wider than the new
        // picture (for instance after a turn back from a wider frame), keep
        // the new fill covering the overhang so desktop never shows through.
        let presented_width = unsafe { framed.container.presentationLayer() }
            .map_or(target.width, |shown| shown.bounds().size.width);
        let overhang = presented_width > picture_size.width + 0.5;
        if let Some(fill) = framed.fill.take() {
            fill.removeFromSuperlayer();
        }
        if let Some(color) = edge_fill(photo) {
            let fill = fill_layer(&color);
            if overhang {
                let start = fill_start(picture_size.width);
                fill.setFrame(CGRect::new(
                    CGPoint::new(start, 0.0),
                    CGSize::new((presented_width - start).max(0.0), target.height),
                ));
                fill.setHidden(false);
                fill.setOpacity(1.0);
            }
            framed
                .container
                .insertSublayer_below(&fill, Some(&framed.picture));
            framed.fill = Some(fill);
        }
        // The frame keeps its rounded corners: while it still grows, it
        // crops the new picture's right edge.
        CATransaction::commit();
        CATransaction::flush();
        debug!(
            "cover picture of window {window} replaced at {}x{}",
            size.x, size.y
        );
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
        if let Some(root) = cover.window.contentView().and_then(|view| view.layer()) {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            // SAFETY: no layer of the cover is in use elsewhere.
            unsafe { root.setSublayers(None) };
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
        CoverScreenOS, cover_panels, fill_crossing_time, fill_start, global_rect, live_panels,
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

    #[test]
    fn fill_stays_confined_to_right_corner_and_gates_on_presented_width() {
        // A narrow picture never lets fill reach its left half.
        assert_eq!(fill_start(16.0), 8.0);
        assert_eq!(fill_start(400.0), 390.0);

        // Segment [198, 206] for a 200-pt picture (threshold 200.5) crosses
        // at 2.5/8 = 0.3125 of segment 1 of 3 -> (1 + 0.3125)/3 = 0.4375.
        let cross = fill_crossing_time(180.0, 220.0, 200.0, &[0.0, 0.45, 0.65, 1.0]);
        let (t, open_after) = cross.expect("crosses threshold");
        assert!(open_after);
        assert!((t - (1.0 + 2.5 / 8.0) / 3.0).abs() < 1e-9, "got {t}");
        let _ = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0));
    }
}
