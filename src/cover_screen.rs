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
use objc2_core_foundation::{CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDirectDisplayID, CGPreflightScreenCaptureAccess};
use objc2_foundation::{NSArray, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CAKeyframeAnimation, CALayer, CAMediaTiming as _, CATransaction};
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

/// The key of the fade animation on a cover's root layer.
const FADE_KEY: &str = "fade";

/// A shown cover, as the event tap needs it on the main thread: its window,
/// its viewport, its strip layer, and each picture's frame from the strip
/// origin, back to front.
struct LiveCover {
    panel: WinID,
    viewport: IRect,
    strip_y: i32,
    strip: Retained<CALayer>,
    pictures: Vec<(WinID, IRect)>,
}

thread_local! {
    static LIVE_COVERS: std::cell::RefCell<Vec<LiveCover>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The pictures of every shown cover, as panels of their windows, where
/// Core Animation shows them now, the front one first. The event tap asks on
/// the main thread when a press or a scroll arrives.
pub(crate) fn live_cover_panels(_mtm: MainThreadMarker) -> Vec<SlicePanel> {
    LIVE_COVERS.with(|live| {
        live.borrow()
            .iter()
            .flat_map(|cover| {
                // SAFETY: a plain read of the layer Core Animation shows now.
                let shown = unsafe { cover.strip.presentationLayer() }
                    .unwrap_or_else(|| cover.strip.clone());
                #[allow(clippy::cast_possible_truncation)]
                let x = (shown.position().x + f64::from(cover.viewport.min.x)).round() as i32;
                cover_panels(
                    cover.panel,
                    cover.viewport,
                    IVec2::new(x, cover.strip_y),
                    &cover.pictures,
                )
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

/// One display's cover: a panel over its viewport, the layer that holds the
/// strip's pictures, and the viewport it covers.
struct Cover {
    window: Retained<NSPanel>,
    strip: Retained<CALayer>,
    viewport: IRect,
    /// Each window's picture and the frame it shows from the strip origin.
    pictures: Vec<(WinID, IRect)>,
    /// Each window's picture layer, which a resize moves on its own.
    layers: Vec<(WinID, Retained<CALayer>)>,
    /// The strip origin the cover was laid out at, which a resize keeps.
    laid: IVec2,
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
        // The desktop picture fills the cover, and the cover fades out over
        // the real windows after a resize, so the panel lets them through.
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
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
        // Its position is its bottom left corner, so a resize moves the left
        // edge and the width on their own.
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setFrame(frame);
        unsafe { layer.setContents(Some(image.as_ref())) };
        layer.setContentsScale(scale);
        layer
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
                        strip_y: cover.at.y,
                        strip: cover.strip.clone(),
                        pictures: cover.pictures.clone(),
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
        root.removeAnimationForKey(&NSString::from_str(FADE_KEY));
        root.setOpacity(1.0);
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
            let image: &CFType = &photo.image;
            let layer = Self::picture(image, Self::turned(on_screen, viewport), scale);
            strip.addSublayer(&layer);
            layers.push((*id, layer));
        }
        root.addSublayer(&strip);
        CATransaction::commit();
        cover.strip = strip;
        cover.layers = layers;
        cover.laid = scene.strip;
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
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if let Some(root) = cover.window.contentView().and_then(|view| view.layer()) {
            root.removeAnimationForKey(&NSString::from_str(FADE_KEY));
            root.setOpacity(1.0);
        }
        for (id, from, to) in frames {
            let Some((_, layer)) = cover.layers.iter().find(|(layer_id, _)| layer_id == id) else {
                continue;
            };
            let (from, to) = (local(*from), local(*to));
            layer.removeAllAnimations();
            layer.setFrame(to);
            let tracks: [(&str, f64, f64); 4] = [
                ("position.x", from.origin.x, to.origin.x),
                ("position.y", from.origin.y, to.origin.y),
                ("bounds.size.width", from.size.width, to.size.width),
                ("bounds.size.height", from.size.height, to.size.height),
            ];
            for (path, start, end) in tracks {
                if (end - start).abs() < f64::EPSILON {
                    continue;
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
        // A press goes to the window where its picture rests.
        cover.pictures = frames
            .iter()
            .map(|(id, _, to)| (*id, IRect::from_corners(to.min - laid, to.max - laid)))
            .collect();
        cover.at = laid;
        self.publish_panels();
    }

    fn fade(&mut self, display: CGDirectDisplayID, seconds: f64) {
        let Some(root) = self
            .covers
            .get(&display)
            .filter(|cover| cover.shown)
            .and_then(|cover| cover.window.contentView())
            .and_then(|view| view.layer())
        else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        root.setOpacity(0.0);
        root.addAnimation_forKey(
            &keyframes("opacity", &[1.0, 0.0], seconds),
            Some(&NSString::from_str(FADE_KEY)),
        );
        CATransaction::commit();
        CATransaction::flush();
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
    pictures
        .iter()
        .filter_map(|(window, rel)| {
            let frame = IRect::from_corners(rel.min + strip, rel.max + strip);
            let on_screen = frame.intersect(viewport);
            (!on_screen.is_empty()).then(|| SlicePanel {
                panel,
                window: *window,
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

#[cfg(test)]
mod tests {
    use bevy::math::{IRect, IVec2};

    use super::cover_panels;

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
}
