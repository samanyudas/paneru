use bevy::{ecs::component::Component, math::IRect};
use core::ptr::NonNull;
use objc2_core_foundation::{CFRetained, CFString, CFUUID};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;
use tracing::{Level, instrument};

use super::skylight::{CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID};
use crate::{
    config::Config,
    ecs::DockPosition,
    errors::{Error, Result},
};

/// The Dock as the Dock itself shows it, read apart from the macOS work area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockFrame {
    /// Auto-hide is on, so the Dock reserves no space.
    AutoHidden,
    /// The frame of the Dock's icon list, in global points.
    Shown(IRect),
}

impl DockFrame {
    /// The Dock shown at `frame`, or `None` for an empty frame, which the
    /// icon list reports right after the Dock restarts.
    pub fn shown(frame: IRect) -> Option<Self> {
        (!frame.is_empty()).then_some(Self::Shown(frame))
    }
}

/// `Display` represents a physical monitor and manages its associated workspaces and window panes.
/// Each display has a unique ID, bounds, and a collection of `LayoutStrip`s for different spaces.
#[derive(Component, Debug)]
pub struct Display {
    /// The unique identifier for this display provided by Core Graphics.
    id: CGDirectDisplayID,
    /// The physical bounds (origin and size) of the display.
    bounds: IRect,
    /// The height of the menubar on this display (from the system).
    menubar_height: i32,
    /// Optional config override for the menubar height.
    menubar_height_override: Option<i32>,
    notch_height: i32,
}

impl Display {
    /// Creates a new `Display` instance.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` of the display.
    /// * `spaces` - A vector of space IDs associated with this display.
    /// * `bounds` - The `CGRect` representing the bounds of the display.
    /// * `menubar_height` - The height of the menubar on this display.
    ///
    /// # Returns
    ///
    /// A new `Display` instance.
    pub fn new(id: CGDirectDisplayID, bounds: IRect, menubar_height: i32) -> Self {
        Self {
            id,
            bounds,
            menubar_height,
            menubar_height_override: None,
            notch_height: 0,
        }
    }

    /// Converts a `CGDirectDisplayID` to a `CFUUID` string.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` to convert.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID string if successful, otherwise `Err(Error)`.
    pub fn uuid_from_id(id: CGDirectDisplayID) -> Result<CFRetained<CFString>> {
        unsafe {
            let uuid = NonNull::new(CGDisplayCreateUUIDFromDisplayID(id))
                .map(|ptr| CFRetained::from_raw(ptr))
                .ok_or(Error::InvalidInput(format!(
                    "{}: can not create uuid from {id}.",
                    function_name!()
                )))?;
            CFUUID::new_string(None, Some(&uuid)).ok_or(Error::InvalidInput(format!(
                "{}: can not create string from {uuid:?}.",
                function_name!()
            )))
        }
    }

    /// Converts a `CFUUID` string to a `CGDirectDisplayID`.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The `CFRetained<CFString>` representing the UUID.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the `CGDirectDisplayID` if successful, otherwise `Err(Error)`.
    pub fn id_from_uuid(uuid: &CFRetained<CFString>) -> Result<u32> {
        unsafe {
            let id = CFUUID::from_string(None, Some(uuid)).ok_or(Error::NotFound(format!(
                "{}: can not convert from {uuid}.",
                function_name!()
            )))?;
            Ok(CGDisplayGetDisplayIDFromUUID(&id))
        }
    }

    /// Returns the `CGDirectDisplayID` of the display.
    ///
    /// # Returns
    ///
    /// The `CGDirectDisplayID` of the display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.id
    }

    /// Where the Dock sits on this display by the Dock's own frame. A shown
    /// Dock lies inside one display, 10 points from the edge it holds to,
    /// and macOS reserves the space up to 4 points short of its inner edge,
    /// as measured on macOS 27 at two sizes on each side. An auto-hidden
    /// Dock, or a frame that does not hold to an edge of this display, as
    /// while the Dock slides, reserves nothing here.
    pub fn dock_from_frame(&self, dock: DockFrame) -> DockPosition {
        const INSET: i32 = 4;
        const MAX_EDGE_GAP: i32 = 24;
        let DockFrame::Shown(list) = dock else {
            return DockPosition::Hidden;
        };
        let bounds = self.bounds;
        if list.intersect(bounds) != list {
            return DockPosition::Hidden;
        }
        if list.width() >= list.height() {
            if bounds.max.y - list.max.y <= MAX_EDGE_GAP {
                return DockPosition::Bottom(bounds.max.y - list.min.y - INSET);
            }
        } else if list.min.x - bounds.min.x <= MAX_EDGE_GAP {
            return DockPosition::Left(list.max.x - bounds.min.x - INSET);
        } else if bounds.max.x - list.max.x <= MAX_EDGE_GAP {
            return DockPosition::Right(bounds.max.x - list.min.x - INSET);
        }
        DockPosition::Hidden
    }

    /// Where the Dock sits, from the macOS work area. The work area leaves
    /// out the real menu bar, so a configured menu bar height does not count.
    pub fn locate_dock(&self, visible_frame: &IRect) -> DockPosition {
        let menubar = self.menubar_height.max(self.notch_height);
        if self.bounds.min.x < visible_frame.min.x {
            DockPosition::Left(visible_frame.min.x - self.bounds.min.x)
        } else if visible_frame.width() < self.bounds.width() {
            DockPosition::Right(self.bounds.max.x - visible_frame.max.x)
        } else if visible_frame.height() < self.bounds.height() - menubar {
            DockPosition::Bottom(self.bounds.height() - visible_frame.height() - menubar)
        } else {
            DockPosition::Hidden
        }
    }

    pub fn bounds(&self) -> IRect {
        let mut bounds = self.bounds;
        bounds.min.y += self.menubar_height();
        bounds
    }

    pub(crate) fn physical_bounds(&self) -> IRect {
        self.bounds
    }

    pub fn menubar_height(&self) -> i32 {
        self.menubar_height_override
            .unwrap_or(self.menubar_height)
            .max(self.notch_height)
    }

    pub fn set_menubar_height_override(&mut self, height: Option<i32>) {
        self.menubar_height_override = height;
    }

    pub fn set_notch_height(&mut self, height: i32) {
        self.notch_height = height;
    }

    /// The macOS work area, without the engine's configured edge padding.
    pub fn visible_bounds(&self, dock: Option<&DockPosition>) -> IRect {
        let mut visible = self.bounds();
        match dock {
            Some(DockPosition::Bottom(size)) => visible.max.y -= size,
            Some(DockPosition::Left(size)) => visible.min.x += size,
            Some(DockPosition::Right(size)) => visible.max.x -= size,
            _ => {}
        }
        visible
    }

    #[instrument(level = Level::TRACE, skip_all, ret)]
    pub fn actual_display_bounds(&self, dock: Option<&DockPosition>, config: &Config) -> IRect {
        let (pad_top, pad_right, pad_bottom, pad_left) = config.edge_padding();
        let mut viewport = self.visible_bounds(dock);
        viewport.min.x += pad_left;
        viewport.min.y += pad_top;
        viewport.max.x -= pad_right;
        viewport.max.y -= pad_bottom;

        viewport
    }
}
