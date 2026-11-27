//! The panels that show photos of hidden windows on shared display edges,
//! and the `ScreenCaptureKit` photos they show.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use bevy::math::IVec2;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, MainThreadMarker};
use objc2_app_kit::{NSColor, NSPanel, NSWindowOcclusionState};
use objc2_core_foundation::{CFData, CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGPreflightScreenCaptureAccess,
};
use objc2_foundation::NSError;
use objc2_quartz_core::CATransaction;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCStreamErrorCode,
};
use tracing::debug;

use crate::ecs::shared_edge::EdgeSlice;
use crate::ecs::slice_photos::{Digest, Lookup, Photo, PhotoError, SliceScreen, Ticket};
use crate::events::{Event, EventSender};
use crate::platform::WinID;
use crate::platform::cocoa_ui::{OverlayLevel, cocoa_frame, make_layer_panel, scale_at};
use crate::platform::slice_tap::{SlicePanel, set_slice_panels};

/// Options controlling post-processing on a `ScreenCaptureKit` capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureOptions {
    /// Compute pixel digest (used by edge slices to detect unchanged frames).
    pub digest: bool,
    /// Sample opaque right-edge surface color (used by strip cover resize fill).
    pub sample_edge_color: bool,
}

/// Caches per-window `SCContentFilter` instances so repeated captures skip
/// `SCShareableContent` enumeration.
#[derive(Default)]
pub(crate) struct WindowFilterCache {
    filters: HashMap<WinID, Arc<OnceLock<Filter>>>,
}

impl WindowFilterCache {
    pub(crate) fn retain(&mut self, keep: impl FnMut(&WinID) -> bool) {
        let mut keep = keep;
        self.filters.retain(|id, _| keep(id));
    }

    pub(crate) fn forget(&mut self, window: Option<WinID>) {
        match window {
            Some(window) => {
                self.filters.remove(&window);
            }
            None => self.filters.clear(),
        }
    }

    /// Captures `window` at `size` points and `scale`, reusing a cached filter
    /// when `reuse_filter` is true and one is ready. `canceled` is checked
    /// after the asynchronous shareable-content lookup returns.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture_window(
        &mut self,
        window: WinID,
        size: IVec2,
        scale: f64,
        reuse_filter: bool,
        options: CaptureOptions,
        canceled: impl Fn() -> bool + 'static,
        done: impl Fn(Result<Photo, PhotoError>) + Clone + 'static,
    ) {
        if reuse_filter
            && let Some(filter) = self.filters.get(&window).and_then(|found| found.get())
        {
            capture(&filter.0, size, scale, options, done);
            return;
        }
        let filter = Arc::new(OnceLock::new());
        self.filters.insert(window, filter.clone());
        let found = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ScreenCaptureKit passes a valid content object or null.
                let Some(content) = (unsafe { content.as_ref() }) else {
                    done(Err(photo_error(error)));
                    return;
                };
                let target = unsafe { content.windows() }
                    .iter()
                    .find(|candidate| unsafe { candidate.windowID() } == window.cast_unsigned());
                let Some(target) = target else {
                    done(Err(PhotoError::WindowGone));
                    return;
                };
                if canceled() {
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
                capture(&filter.0, size, scale, options, done.clone());
            },
        );
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &found,
            );
        }
    }
}

pub struct SliceScreenOS {
    mtm: MainThreadMarker,
    events: EventSender,
    panels: HashMap<WinID, Panel>,
    /// The placed windows, their panels back to front.
    stack: Vec<WinID>,
    /// The painted panels in the order they were last brought to the front.
    fronted: Vec<WinID>,
    filters: WindowFilterCache,
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
            filters: WindowFilterCache::default(),
        }
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
                        cover: false,
                    })
                })
                .collect(),
        );
    }
}

impl SliceScreen for SliceScreenOS {
    fn may_photograph(&self) -> bool {
        CGPreflightScreenCaptureAccess()
    }

    fn place(&mut self, slices: &[(WinID, EdgeSlice)], remembered: &[WinID]) {
        self.filters.retain(|id| {
            remembered.contains(id) || slices.iter().any(|(showing, _)| showing == id)
        });
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
            let frame = cocoa_frame(self.mtm, slice.on_screen);
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
                    let window = make_layer_panel(
                        self.mtm,
                        frame,
                        OverlayLevel::EdgeSlice,
                        false,
                        &NSColor::clearColor(),
                        false,
                    );
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
        // A layer-backed panel shows a new frame only once Core Animation
        // commits, which otherwise waits for the run loop in the next frame.
        CATransaction::flush();
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
        let scale = scale_at(self.mtm, slice.on_screen);
        self.filters.capture_window(
            window,
            slice.size,
            scale,
            lookup == Lookup::Reuse,
            CaptureOptions {
                digest: true,
                sample_edge_color: false,
            },
            move || {
                let called_off = ticket.called_off();
                if called_off {
                    debug!("edge slice photo called off: window {window} generation {generation}");
                }
                called_off
            },
            move |result| reply.send(result),
        );
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
}

/// Why `ScreenCaptureKit` gave no photo, from the error it passed.
pub(crate) fn photo_error(error: *mut NSError) -> PhotoError {
    // SAFETY: ScreenCaptureKit passes a valid error or null.
    let error = unsafe { error.as_ref() };
    debug!("photo failed: {error:?}");
    if error.is_some_and(|error| error.code() == SCStreamErrorCode::UserDeclined.0) {
        PhotoError::Refused
    } else {
        PhotoError::Failed
    }
}

/// Photographs what `filter` shows, at `size` points and `scale` pixels per
/// point, and hands the photo, or why there is none, to `done`.
pub(crate) fn capture(
    filter: &SCContentFilter,
    size: IVec2,
    scale: f64,
    options: CaptureOptions,
    done: impl Fn(Result<Photo, PhotoError>) + 'static,
) {
    let configuration = configuration(size, scale);
    let taken = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let Some(image) = NonNull::new(image) else {
            done(Err(photo_error(error)));
            return;
        };
        // SAFETY: a non-null image that ScreenCaptureKit lends to this call.
        let image = unsafe { CFRetained::retain(image) };
        let data = pixels(&image);
        // SAFETY: the bytes stay put while `data` lives, and nothing writes
        // to an image ScreenCaptureKit handed over.
        let pixels = data
            .as_deref()
            .map_or(&[][..], |data| unsafe { data.as_bytes_unchecked() });
        if blank(&image, pixels) {
            debug!("photo is blank");
            done(Err(PhotoError::Blank));
            return;
        }
        let digest = if options.digest {
            digest(&image, pixels)
        } else {
            Digest(0)
        };
        let edge_color = options
            .sample_edge_color
            .then(|| sample_right_edge_color(&image, pixels))
            .flatten();
        done(Ok(Photo {
            digest,
            image,
            edge_color,
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

/// Photographs the whole window, `size` in points, at `scale` pixels per
/// point, with no cursor and no shadow. It sets no source rectangle: one
/// makes `ScreenCaptureKit` leave out the part of the window off screen, so a
/// tucked window, which shows 1 point, photographed blank (#451).
fn configuration(size: IVec2, scale: f64) -> Retained<SCStreamConfiguration> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pixels = |points: i32| (f64::from(points) * scale).round().max(1.0) as usize;
    let configuration = unsafe { SCStreamConfiguration::new() };
    unsafe {
        configuration.setWidth(pixels(size.x));
        configuration.setHeight(pixels(size.y));
        configuration.setShowsCursor(false);
        configuration.setIgnoreShadowsSingleWindow(true);
    }
    configuration
}

/// The bytes of `image`, when Core Graphics can give them.
fn pixels(image: &CGImage) -> Option<CFRetained<CFData>> {
    CGImage::data_provider(Some(image)).and_then(|provider| CGDataProvider::data(Some(&provider)))
}

/// Whether nearly all of `image` is clear. Some apps, such as Notion, stop
/// drawing while their window is off screen, and their photo then shows
/// only a 1-pixel border. The samples start inside the edges to skip it.
fn blank(image: &CGImage, pixels: &[u8]) -> bool {
    const STEP: usize = 8;
    let alpha = CGImage::alpha_info(Some(image));
    let width = CGImage::width(Some(image));
    let row = CGImage::bytes_per_row(Some(image));
    if (alpha != CGImageAlphaInfo::PremultipliedFirst
        && alpha != CGImageAlphaInfo::PremultipliedLast)
        || CGImage::bits_per_pixel(Some(image)) != 32
        || row < width * 4
    {
        return false;
    }
    let (mut clear, mut sampled) = (0_usize, 0_usize);
    for line in pixels.chunks_exact(row).skip(STEP / 2).step_by(STEP) {
        for x in (STEP / 2..width).step_by(STEP) {
            sampled += 1;
            // A premultiplied pixel with no alpha has no colour either.
            if line[x * 4..x * 4 + 4] == [0; 4] {
                clear += 1;
            }
        }
    }
    sampled > 0 && clear * 100 >= sampled * 99
}

/// How the bytes of one 32-bit pixel hold its channels: where red, green,
/// blue and alpha sit, and whether the colour is premultiplied by alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PixelFormat {
    width: usize,
    height: usize,
    row: usize,
    /// The byte of red, green and blue, and of alpha if the pixel has one.
    rgb: [usize; 3],
    alpha: Option<usize>,
    premultiplied: bool,
}

impl PixelFormat {
    /// The format of `image`, if its pixels are 8-bit channels packed in 32
    /// bits, the format `ScreenCaptureKit` gives.
    fn of(image: &CGImage) -> Option<Self> {
        if CGImage::bits_per_pixel(Some(image)) != 32
            || CGImage::bits_per_component(Some(image)) != 8
        {
            return None;
        }
        let alpha = CGImage::alpha_info(Some(image));
        // Where alpha sits in the pixel as a 32-bit word: first is its high
        // byte. A word in the default or big byte order keeps that order in
        // memory, and a little-endian one turns it around.
        let (first, premultiplied, skip) = match alpha {
            CGImageAlphaInfo::PremultipliedFirst => (true, true, false),
            CGImageAlphaInfo::PremultipliedLast => (false, true, false),
            CGImageAlphaInfo::First => (true, false, false),
            CGImageAlphaInfo::Last => (false, false, false),
            CGImageAlphaInfo::NoneSkipFirst => (true, false, true),
            CGImageAlphaInfo::NoneSkipLast => (false, false, true),
            _ => return None,
        };
        let little = match CGImage::byte_order_info(Some(image)) {
            CGImageByteOrderInfo::OrderDefault | CGImageByteOrderInfo::Order32Big => false,
            CGImageByteOrderInfo::Order32Little => true,
            _ => return None,
        };
        // The word's bytes from high to low: alpha, then red, green, blue,
        // or red, green, blue, then alpha.
        let (rgb, a) = if first {
            ([1, 2, 3], 0)
        } else {
            ([0, 1, 2], 3)
        };
        let place = |byte: usize| if little { 3 - byte } else { byte };
        Some(Self {
            width: CGImage::width(Some(image)),
            height: CGImage::height(Some(image)),
            row: CGImage::bytes_per_row(Some(image)),
            rgb: rgb.map(place),
            alpha: (!skip).then_some(place(a)),
            premultiplied,
        })
    }
}

/// The colour of the opaque surface of `image` near its right edge, as red,
/// green, blue and alpha from 0 to 1 in the image's colour space: the
/// average of 16 points from 20% to 80% of its height, on a column inside
/// the edge that skips a window's border and rounded corners. `None` when
/// no point there is opaque, or the pixels are in another format.
pub(crate) fn sample_right_edge_color(image: &CGImage, pixels: &[u8]) -> Option<[f64; 4]> {
    edge_color(PixelFormat::of(image)?, pixels)
}

/// The colour [`sample_right_edge_color`] samples, from pixels in `format`.
fn edge_color(format: PixelFormat, pixels: &[u8]) -> Option<[f64; 4]> {
    /// How many points are sampled down the column.
    const SAMPLES: usize = 16;
    /// The least alpha of a point that counts as opaque surface.
    const OPAQUE: u8 = 200;
    let PixelFormat {
        width, height, row, ..
    } = format;
    if width == 0 || height == 0 || row < width * 4 {
        return None;
    }
    let x = width.saturating_sub((width / 20).clamp(12, 48));
    let (top, span) = (height / 5, height * 3 / 5);
    let (mut sum, mut count) = ([0.0_f64; 3], 0_u32);
    for sample in 0..SAMPLES {
        let y = top + span * sample / (SAMPLES - 1);
        let start = y.min(height - 1) * row + x * 4;
        let Some(pixel) = pixels.get(start..start + 4) else {
            continue;
        };
        let alpha = format.alpha.map_or(u8::MAX, |byte| pixel[byte]);
        if alpha < OPAQUE {
            continue;
        }
        let scale = if format.premultiplied {
            f64::from(alpha)
        } else {
            f64::from(u8::MAX)
        };
        for (total, byte) in sum.iter_mut().zip(format.rgb) {
            *total += (f64::from(pixel[byte]) / scale).min(1.0);
        }
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let [r, g, b] = sum.map(|total| total / f64::from(count));
    Some([r, g, b, 1.0])
}

fn digest(image: &CGImage, pixels: &[u8]) -> Digest {
    let mut hasher = DefaultHasher::new();
    CGImage::width(Some(image)).hash(&mut hasher);
    CGImage::height(Some(image)).hash(&mut hasher);
    pixels.hash(&mut hasher);
    Digest(hasher.finish())
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
        CGImageAlphaInfo, CGImageByteOrderInfo,
    };

    use bevy::math::IVec2;
    use objc2_screen_capture_kit::SCStreamConfiguration;

    use super::{PixelFormat, blank, configuration, edge_color, pixels, sample_right_edge_color};

    /// A source rectangle makes `ScreenCaptureKit` leave out the part of a
    /// window off screen, so every photo of a tucked window, which shows 1
    /// point, came back blank (#451).
    #[test]
    fn a_photo_asks_for_the_whole_window_with_no_source_rectangle() {
        let asked = configuration(IVec2::new(2560, 1410), 2.0);
        // SAFETY: plain property reads of configurations this test owns.
        let (rect, default, width, height) = unsafe {
            (
                asked.sourceRect(),
                SCStreamConfiguration::new().sourceRect(),
                asked.width(),
                asked.height(),
            )
        };
        assert_eq!(rect, default, "the source rectangle is left at its default");
        assert_eq!(
            (width, height),
            (5120, 2820),
            "the whole window at 2 pixels per point"
        );
    }

    /// A clear 64 by 48 photo with `width` by `height` pixels drawn opaque
    /// from its corner.
    fn photo(width: f64, height: f64) -> CFRetained<CGImage> {
        let space = CGColorSpace::new_device_rgb();
        // SAFETY: a null buffer asks Core Graphics to allocate a clear one.
        let context = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                64,
                48,
                8,
                0,
                space.as_deref(),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .expect("a bitmap context");
        CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(
            Some(&context),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height)),
        );
        CGBitmapContextCreateImage(Some(&context)).expect("an image")
    }

    fn is_blank(image: &CGImage) -> bool {
        let data = pixels(image);
        // SAFETY: a test image nothing writes to.
        blank(
            image,
            data.as_deref()
                .map_or(&[][..], |data| unsafe { data.as_bytes_unchecked() }),
        )
    }

    #[test]
    fn a_photo_that_shows_only_a_border_is_blank() {
        assert!(is_blank(&photo(1.0, 48.0)));
    }

    #[test]
    fn a_photo_with_a_little_drawn_is_not_blank() {
        assert!(!is_blank(&photo(16.0, 16.0)));
    }

    /// A clear 200 by 100 photo, in the pixel layout `info` gives, with its
    /// right part from x 150 filled with `rgba`.
    fn edge_photo(info: u32, rgba: [f64; 4]) -> CFRetained<CGImage> {
        let space = CGColorSpace::new_device_rgb();
        // SAFETY: a null buffer asks Core Graphics to allocate a clear one.
        let context = unsafe {
            CGBitmapContextCreate(std::ptr::null_mut(), 200, 100, 8, 0, space.as_deref(), info)
        }
        .expect("a bitmap context");
        let [r, g, b, a] = rgba;
        CGContext::set_rgb_fill_color(Some(&context), r, g, b, a);
        CGContext::fill_rect(
            Some(&context),
            CGRect::new(CGPoint::new(150.0, 0.0), CGSize::new(50.0, 100.0)),
        );
        CGBitmapContextCreateImage(Some(&context)).expect("an image")
    }

    fn edge_of(image: &CGImage) -> Option<[f64; 4]> {
        let data = pixels(image);
        // SAFETY: a test image nothing writes to.
        sample_right_edge_color(
            image,
            data.as_deref()
                .map_or(&[][..], |data| unsafe { data.as_bytes_unchecked() }),
        )
    }

    fn assert_color(found: Option<[f64; 4]>, want: [f64; 4]) {
        let found = found.expect("an edge colour");
        assert!(
            found
                .iter()
                .zip(want)
                .all(|(found, want)| (found - want).abs() < 2.0 / 255.0),
            "found {found:?}, want {want:?}"
        );
    }

    #[test]
    fn the_edge_colour_of_an_rgba_photo() {
        let image = edge_photo(CGImageAlphaInfo::PremultipliedLast.0, [0.2, 0.4, 0.6, 1.0]);
        assert_color(edge_of(&image), [0.2, 0.4, 0.6, 1.0]);
    }

    /// `ScreenCaptureKit` hands over BGRA pixels: alpha first in a
    /// little-endian word.
    #[test]
    fn the_edge_colour_of_a_bgra_photo() {
        let image = edge_photo(
            CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
            [0.8, 0.3, 0.1, 1.0],
        );
        assert_color(edge_of(&image), [0.8, 0.3, 0.1, 1.0]);
    }

    #[test]
    fn a_photo_clear_at_its_right_edge_has_no_edge_colour() {
        let image = edge_photo(CGImageAlphaInfo::PremultipliedLast.0, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(edge_of(&image), None);
    }

    /// A translucent surface is no opaque surface to fill with.
    #[test]
    fn a_translucent_right_edge_has_no_edge_colour() {
        let image = edge_photo(CGImageAlphaInfo::PremultipliedLast.0, [1.0, 1.0, 1.0, 0.5]);
        assert_eq!(edge_of(&image), None);
    }

    fn rgba(width: usize, height: usize) -> PixelFormat {
        PixelFormat {
            width,
            height,
            row: width * 4,
            rgb: [0, 1, 2],
            alpha: Some(3),
            premultiplied: true,
        }
    }

    #[test]
    fn an_empty_or_short_photo_has_no_edge_colour() {
        assert_eq!(edge_color(rgba(0, 10), &[]), None);
        assert_eq!(edge_color(rgba(10, 0), &[]), None);
        // Fewer bytes than the format says: no sample reads past them.
        assert_eq!(edge_color(rgba(64, 64), &[255; 64]), None);
    }

    /// A photo narrower than the column inside the edge samples its left
    /// column.
    #[test]
    fn a_narrow_photo_samples_its_first_column() {
        let found = edge_color(rgba(4, 4), &[51, 102, 153, 255].repeat(16));
        assert_color(found, [0.2, 0.4, 0.6, 1.0]);
    }

    /// Pixels in a format other than 8-bit channels in 32 bits, such as 16
    /// bits a channel, give no edge colour rather than a wrong one.
    #[test]
    fn a_photo_of_another_pixel_format_has_no_edge_colour() {
        let space = CGColorSpace::new_device_rgb();
        // SAFETY: a null buffer asks Core Graphics to allocate a clear one.
        let context = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                32,
                32,
                16,
                0,
                space.as_deref(),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .expect("a bitmap context");
        CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(
            Some(&context),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(32.0, 32.0)),
        );
        let image = CGBitmapContextCreateImage(Some(&context)).expect("an image");
        assert_eq!(PixelFormat::of(&image), None);
        assert_eq!(edge_of(&image), None);
    }
}
