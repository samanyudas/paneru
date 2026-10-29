//! The panels that show photos of hidden windows on shared display edges,
//! and the `ScreenCaptureKit` photos they show.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use bevy::math::{IRect, IVec2};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSPanel, NSScreen, NSView,
    NSViewLayerContentsRedrawPolicy, NSWindowCollectionBehavior, NSWindowOcclusionState,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDataProvider, CGImage, CGPreflightScreenCaptureAccess};
use objc2_foundation::{NSError, NSPoint, NSRect, NSSize};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCStreamErrorCode,
};
use tracing::debug;

use crate::ecs::shared_edge::EdgeSlice;
use crate::ecs::slice_photos::{Digest, Lookup, Photo, PhotoError, SliceScreen, Ticket};
use crate::events::{Event, EventSender};
use crate::overlay::{cg_abs_to_cocoa, primary_screen_height};
use crate::platform::WinID;
use crate::platform::slice_tap::{SlicePanel, set_slice_panels};

pub struct SliceScreenOS {
    mtm: MainThreadMarker,
    events: EventSender,
    panels: HashMap<WinID, Panel>,
    /// The placed windows, their panels back to front.
    stack: Vec<WinID>,
    /// The painted panels in the order they were last brought to the front.
    fronted: Vec<WinID>,
    /// The filter of each placed window's last lookup, once it arrives.
    filters: HashMap<WinID, Arc<OnceLock<Filter>>>,
}

/// A filter that `ScreenCaptureKit`'s queue makes and the main thread reuses.
struct Filter(Retained<SCContentFilter>);

// SAFETY: no one changes a filter after making it, and Apple's own samples
// share one filter across queues.
unsafe impl Send for Filter {}
unsafe impl Sync for Filter {}

struct Panel {
    window: Retained<NSPanel>,
    slice: EdgeSlice,
    painted: bool,
}

impl Panel {
    /// Shows the slice's part of the photo of the whole window. The layer's
    /// unit space starts at the bottom of the photo.
    fn crop(&self) {
        let Some(layer) = self.window.contentView().and_then(|view| view.layer()) else {
            return;
        };
        let (min, max) = (
            self.slice.in_window.min.as_dvec2(),
            self.slice.in_window.max.as_dvec2(),
        );
        let size = self.slice.size.as_dvec2();
        if size.x <= 0.0 || size.y <= 0.0 {
            return;
        }
        layer.setContentsRect(CGRect::new(
            CGPoint::new(min.x / size.x, 1.0 - max.y / size.y),
            CGSize::new((max.x - min.x) / size.x, (max.y - min.y) / size.y),
        ));
    }
}

impl SliceScreenOS {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        Self {
            mtm,
            events,
            panels: HashMap::new(),
            stack: Vec::new(),
            fronted: Vec::new(),
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

    /// Brings the painted panels to the front in stack order, so each shows
    /// above the ones before it, unless they are in that order already.
    fn restack(&mut self) {
        let painted = self
            .stack
            .iter()
            .copied()
            .filter(|id| self.panels.get(id).is_some_and(|panel| panel.painted))
            .collect::<Vec<_>>();
        if painted == self.fronted {
            return;
        }
        for id in &painted {
            if let Some(panel) = self.panels.get(id) {
                panel.window.orderFrontRegardless();
            }
        }
        debug!("edge slice panels stacked: {painted:?}");
        self.fronted = painted;
    }

    /// Tells the event tap and the window lookups where the painted panels
    /// are, the front one first.
    fn publish_panels(&self) {
        set_slice_panels(
            self.stack
                .iter()
                .rev()
                .filter_map(|window| Some((window, self.panels.get(window)?)))
                .filter(|(_, panel)| panel.painted)
                .filter_map(|(window, panel)| {
                    Some(SlicePanel {
                        panel: WinID::try_from(panel.window.windowNumber()).ok()?,
                        window: *window,
                        on_screen: panel.slice.on_screen,
                        in_window: panel.slice.in_window,
                        side: panel.slice.side,
                    })
                })
                .collect(),
        );
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

    /// A borderless panel above normal windows and below the dim overlay. It
    /// takes clicks without activating the engine, so they reach no app.
    fn make_panel(&self, frame: NSRect) -> Retained<NSPanel> {
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(self.mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);
        panel.setIgnoresMouseEvents(false);
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
}

impl SliceScreen for SliceScreenOS {
    fn may_photograph(&self) -> bool {
        CGPreflightScreenCaptureAccess()
    }

    fn place(&mut self, slices: &[(WinID, EdgeSlice)]) {
        self.filters
            .retain(|id, _| slices.iter().any(|(showing, _)| showing == id));
        self.panels.retain(|id, panel| {
            let keep = slices.iter().any(|(showing, _)| showing == id);
            if !keep {
                panel.window.orderOut(None::<&AnyObject>);
                debug!("edge slice panel closed: window {id}");
            }
            keep
        });
        self.fronted.retain(|id| self.panels.contains_key(id));
        for (id, slice) in slices {
            let frame = self.cocoa_frame(slice.on_screen);
            let panel = match self.panels.get_mut(id) {
                Some(panel) if panel.slice.on_screen == slice.on_screen => {
                    panel.slice = *slice;
                    panel.crop();
                    continue;
                }
                Some(panel) => {
                    panel.window.setFrame_display(frame, panel.painted);
                    panel.slice = *slice;
                    panel
                }
                None => {
                    let window = self.make_panel(frame);
                    self.panels.entry(*id).or_insert(Panel {
                        window,
                        slice: *slice,
                        painted: false,
                    })
                }
            };
            panel.crop();
            let on = slice.on_screen;
            debug!(
                "edge slice panel placed: window {id} at ({},{})-({},{})",
                on.min.x, on.min.y, on.max.x, on.max.y
            );
        }
        self.stack = slices.iter().map(|(id, _)| *id).collect();
        self.restack();
        self.publish_panels();
    }

    fn photograph(
        &mut self,
        window: WinID,
        slice: &EdgeSlice,
        generation: u32,
        ticket: Ticket,
        lookup: Lookup,
    ) {
        let reply = Reply {
            events: self.events.clone(),
            window,
            generation,
        };
        let crop = IRect::from_corners(IVec2::ZERO, slice.size);
        let scale = self.scale_at(slice.on_screen);
        if lookup == Lookup::Reuse
            && let Some(filter) = self.filters.get(&window).and_then(|found| found.get())
        {
            capture(&filter.0, crop, scale, reply);
            return;
        }
        debug!("edge slice window looked up: window {window} generation {generation}");
        let filter = Arc::new(OnceLock::new());
        self.filters.insert(window, filter.clone());
        let found = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ScreenCaptureKit passes a valid content object or null.
                let Some(content) = (unsafe { content.as_ref() }) else {
                    reply.fail(error);
                    return;
                };
                let target = unsafe { content.windows() }
                    .iter()
                    .find(|candidate| unsafe { candidate.windowID() } == window.cast_unsigned());
                let Some(target) = target else {
                    reply.send(Err(PhotoError::WindowGone));
                    return;
                };
                if ticket.called_off() {
                    debug!("edge slice photo called off: window {window} generation {generation}");
                    return;
                }
                let filter = filter.get_or_init(|| {
                    Filter(unsafe {
                        SCContentFilter::initWithDesktopIndependentWindow(
                            SCContentFilter::alloc(),
                            &target,
                        )
                    })
                });
                capture(&filter.0, crop, scale, reply.clone());
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &found,
            );
        }
    }

    fn paint(&mut self, window: WinID, photo: &Photo) {
        let Some(panel) = self.panels.get_mut(&window) else {
            return;
        };
        let Some(layer) = panel.window.contentView().and_then(|view| view.layer()) else {
            return;
        };
        let image: &CFType = &photo.image;
        unsafe { layer.setContents(Some(image.as_ref())) };
        panel.crop();
        if !panel.painted {
            panel.painted = true;
            debug!("edge slice panel shown: window {window}");
            self.restack();
            self.publish_panels();
        }
    }

    fn seen(&self, window: WinID) -> bool {
        self.panels.get(&window).is_some_and(|panel| {
            panel
                .window
                .occlusionState()
                .contains(NSWindowOcclusionState::Visible)
        })
    }
}

/// Where the photo of one ask goes. `ScreenCaptureKit` calls back on its own
/// queue, so the photo travels to the main thread as an [`Event`].
#[derive(Clone)]
struct Reply {
    events: EventSender,
    window: WinID,
    generation: u32,
}

impl Reply {
    fn send(&self, result: Result<Photo, PhotoError>) {
        _ = self.events.send(Event::SlicePhoto {
            window_id: self.window,
            generation: self.generation,
            result,
        });
    }

    fn fail(&self, error: *mut NSError) {
        // SAFETY: ScreenCaptureKit passes a valid error or null.
        let error = unsafe { error.as_ref() };
        debug!(
            "edge slice photo of window {} failed: {error:?}",
            self.window
        );
        let refused = error.is_some_and(|error| error.code() == SCStreamErrorCode::UserDeclined.0);
        self.send(Err(if refused {
            PhotoError::Refused
        } else {
            PhotoError::Failed
        }));
    }
}

/// Photographs the window of `filter`, sending the photo to `reply`.
fn capture(filter: &SCContentFilter, crop: IRect, scale: f64, reply: Reply) {
    let configuration = configuration(crop, scale);
    let taken = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let Some(image) = NonNull::new(image) else {
            reply.fail(error);
            return;
        };
        // SAFETY: a non-null image that ScreenCaptureKit lends to this call.
        let image = unsafe { CFRetained::retain(image) };
        reply.send(Ok(Photo {
            digest: digest(&image),
            image,
        }));
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            &configuration,
            Some(&taken),
        );
    }
}

/// Photographs `crop`, in the window's points, at `scale` pixels per point,
/// with no cursor and no shadow.
fn configuration(crop: IRect, scale: f64) -> Retained<SCStreamConfiguration> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pixels = |points: i32| (f64::from(points) * scale).round().max(1.0) as usize;
    let configuration = unsafe { SCStreamConfiguration::new() };
    unsafe {
        configuration.setSourceRect(CGRect::new(
            CGPoint::new(f64::from(crop.min.x), f64::from(crop.min.y)),
            CGSize::new(f64::from(crop.width()), f64::from(crop.height())),
        ));
        configuration.setWidth(pixels(crop.width()));
        configuration.setHeight(pixels(crop.height()));
        configuration.setShowsCursor(false);
        configuration.setIgnoreShadowsSingleWindow(true);
    }
    configuration
}

fn digest(image: &CGImage) -> Digest {
    let mut hasher = DefaultHasher::new();
    CGImage::width(Some(image)).hash(&mut hasher);
    CGImage::height(Some(image)).hash(&mut hasher);
    if let Some(data) = CGImage::data_provider(Some(image))
        .and_then(|provider| CGDataProvider::data(Some(&provider)))
    {
        data.to_vec().hash(&mut hasher);
    }
    Digest(hasher.finish())
}
