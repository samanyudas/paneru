use std::{ffi::c_void, os::raw::c_float, ptr::NonNull};

use accessibility_sys::AXUIElementRef;
use objc2_core_foundation::{
    CFArray, CFDictionary, CFMutableData, CFString, CFType, CFUUID, CGPoint, CGRect,
};
use objc2_core_graphics::{CGDirectDisplayID, CGError};

use crate::platform::{CFStringRef, ConnID, OSStatus, ProcessSerialNumber, WinID};

#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    /// `extern AXError _AXUIElementGetWindow(AXUIElementRef ref, uint32_t *wid);`
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, wid: &mut WinID) -> OSStatus;

    /// `extern int SLSMainConnectionID(void);`
    pub fn SLSMainConnectionID() -> ConnID;

    /// Opens a connection to the window server of its own, whose calls do
    /// not wait on the drawing pending on the main connection.
    ///
    /// `extern CGError SLSNewConnection(int zero, int *cid);`
    pub fn SLSNewConnection(zero: i32, cid: &mut ConnID) -> CGError;

    /// Reports whether a window is currently ordered into the window list.
    pub fn SLSWindowIsOrderedIn(cid: ConnID, window_id: WinID, ordered_in: &mut u8) -> OSStatus;

    /// `extern CFUUIDRef CGDisplayCreateUUIDFromDisplayID(uint32_t did);`
    pub fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> *mut CFUUID;

    /// `extern uint32_t CGDisplayGetDisplayIDFromUUID(CFUUIDRef uuid);`
    pub fn CGDisplayGetDisplayIDFromUUID(display: &CFUUID) -> u32;

    /// `extern uint64_t SLSManagedDisplayGetCurrentSpace(int cid, CFStringRef uuid);`
    pub fn SLSManagedDisplayGetCurrentSpace(cid: ConnID, uuid: CFStringRef) -> u64;

    /// Returns the type of a space: `0` = user/desktop, `4` = fullscreen, `2` = system.
    ///
    /// `extern int SLSSpaceGetType(int cid, uint64_t space_id);`
    pub fn SLSSpaceGetType(cid: ConnID, space_id: u64) -> i32;

    /// `extern CFStringRef SLSCopyActiveMenuBarDisplayIdentifier(int cid);`
    pub fn SLSCopyActiveMenuBarDisplayIdentifier(cid: ConnID) -> CFStringRef;

    pub fn SLSGetDisplayMenubarHeight(did: CGDirectDisplayID, height: *mut u32) -> CGError;

    /// Options: `0x2` for normal windows, `0x7` for normal + minimized; `owner == 0` queries all.
    ///
    /// `extern CFArrayRef SLSCopyWindowsWithOptionsAndTags(int cid, uint32_t owner, CFArrayRef spaces, uint32_t options, uint64_t *set_tags, uint64_t *clear_tags);`
    pub fn SLSCopyWindowsWithOptionsAndTags(
        cid: ConnID,
        owner: ConnID,
        spaces: *const CFArray,
        options: i32,
        set_tags: &mut i64,
        clear_tags: &mut i64,
    ) -> *mut CFArray;

    /// Returns `1` when "Displays have separate Spaces" is enabled, `0` otherwise.
    ///
    /// `extern int SLSGetSpaceManagementMode(int cid);`
    pub fn SLSGetSpaceManagementMode(cid: ConnID) -> i32;

    /// `extern CFArrayRef SLSCopyManagedDisplaySpaces(int cid);`
    pub fn SLSCopyManagedDisplaySpaces(
        cid: ConnID,
    ) -> *mut CFArray<CFDictionary<CFString, CFString>>;

    /// `extern CFTypeRef SLSWindowQueryWindows(int cid, CFArrayRef windows, int count);`
    pub fn SLSWindowQueryWindows(
        cid: ConnID,
        windows: *const CFArray,
        count: isize,
    ) -> NonNull<CFType>;

    /// `extern CFTypeRef SLSWindowQueryResultCopyWindows(CFTypeRef window_query);`
    pub fn SLSWindowQueryResultCopyWindows(type_ref: NonNull<CFType>) -> NonNull<CFType>;

    /// `extern bool SLSWindowIteratorAdvance(CFTypeRef iterator);`
    pub fn SLSWindowIteratorAdvance(iterator: *const CFType) -> bool;

    /// `extern uint32_t SLSWindowIteratorGetParentID(CFTypeRef iterator);`
    pub fn SLSWindowIteratorGetParentID(iterator: *const CFType) -> i32;

    /// `extern uint32_t SLSWindowIteratorGetWindowID(CFTypeRef iterator);`
    pub fn SLSWindowIteratorGetWindowID(iterator: *const CFType) -> i32;

    /// `extern CGRect SLSWindowIteratorGetBounds(CFTypeRef iterator);`
    pub fn SLSWindowIteratorGetBounds(iterator: *const CFType) -> CGRect;

    /// `extern uint64_t SLSWindowIteratorGetTags(CFTypeRef iterator);`
    pub fn SLSWindowIteratorGetTags(iterator: *const CFType) -> i64;

    /// `extern uint64_t SLSWindowIteratorGetAttributes(CFTypeRef iterator);`
    pub fn SLSWindowIteratorGetAttributes(iterator: *const CFType) -> i64;

    /// `extern OSStatus _SLPSGetFrontProcess(ProcessSerialNumber *psn);`
    pub fn _SLPSGetFrontProcess(psn: &mut ProcessSerialNumber) -> OSStatus;

    /// `extern CGError SLSGetConnectionIDForPSN(int cid, ProcessSerialNumber *psn, int *psn_cid);`
    pub fn SLSGetConnectionIDForPSN(
        cid: ConnID,
        psn: &ProcessSerialNumber,
        psn_cid: &mut ConnID,
    ) -> CGError;

    /// `extern CGError _SLPSSetFrontProcessWithOptions(ProcessSerialNumber *psn, uint32_t wid, uint32_t mode);`
    pub fn _SLPSSetFrontProcessWithOptions(
        psn: &ProcessSerialNumber,
        window_id: WinID,
        mode: u32,
    ) -> CGError;

    /// `extern CGError SLPSPostEventRecordTo(ProcessSerialNumber *psn, uint8_t *bytes);`
    pub fn SLPSPostEventRecordTo(psn: &ProcessSerialNumber, event: *const c_void) -> CGError;

    /// `extern OSStatus SLSFindWindowAndOwner(int cid, int zero, int one, int zero_again, CGPoint *screen_point, CGPoint *window_point, uint32_t *wid, int *wcid);`
    pub fn SLSFindWindowAndOwner(
        cid: ConnID,
        filter_window_id: WinID,
        _unused1: i64,
        _unused2: i64,
        point: &CGPoint,
        window_point: &mut CGPoint,
        window_id: &mut WinID,
        window_cid: &mut ConnID,
    ) -> OSStatus;

    /// `extern CGError SLSGetCurrentCursorLocation(int cid, CGPoint *point);`
    pub fn SLSGetCurrentCursorLocation(cid: ConnID, cursor: &mut CGPoint) -> CGError;

    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: &mut *mut CFType,
    ) -> OSStatus;

    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: &CFType,
    ) -> OSStatus;

    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: &CFString) -> OSStatus;

    /// Creates an `AXUIElementRef` from a remote token so windows on inactive
    /// spaces can be queried even when `kAXWindowsAttribute` omits them.
    ///
    /// `extern AXUIElementRef _AXUIElementCreateWithRemoteToken(CFDataRef data);`
    pub fn _AXUIElementCreateWithRemoteToken(data: &CFMutableData) -> AXUIElementRef;

    pub fn SLSSetWindowListBrightness(
        cid: ConnID,
        window_list: *const WinID,
        brightness_levels: *const c_float,
        count: isize,
    ) -> CGError;
}
