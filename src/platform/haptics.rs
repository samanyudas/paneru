use std::cell::OnceCell;
use std::ffi::c_void;

use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
    NSHapticFeedbackPerformer as _,
};

type CFTypeRef = *const c_void;
type ActuateFn = unsafe extern "C" fn(CFTypeRef, i32, u32, f32, f32) -> i32;

/// A medium trackpad actuation, which a moving hand still feels without it
/// standing out. `NSHapticFeedbackManager`'s generic pattern is faint enough
/// that three fingers sliding over the glass hide it, and actuation 6, the
/// strongest, felt too strong as a detent.
const ACTUATION: i32 = 4;

/// Every trackpad that can actuate, opened through the private
/// `MultitouchSupport` framework, and the call that pulses one.
struct Actuators {
    actuate: ActuateFn,
    devices: Vec<CFTypeRef>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
}

impl Actuators {
    /// Opens every actuating trackpad, or `None` when the framework or one of
    /// its calls is missing, as it may be on a later macOS.
    fn open() -> Option<Self> {
        // SAFETY: each symbol is cast to the signature `MultitouchSupport` has
        // exported since macOS 10.11, and checked for null first.
        unsafe {
            let handle = libc::dlopen(
                c"/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport"
                    .as_ptr(),
                libc::RTLD_NOW,
            );
            if handle.is_null() {
                return None;
            }
            let symbol = |name: &std::ffi::CStr| {
                let symbol = libc::dlsym(handle, name.as_ptr());
                (!symbol.is_null()).then_some(symbol)
            };
            let list: unsafe extern "C" fn() -> CFTypeRef =
                std::mem::transmute(symbol(c"MTDeviceCreateList")?);
            let device_id: unsafe extern "C" fn(CFTypeRef, *mut u64) -> i32 =
                std::mem::transmute(symbol(c"MTDeviceGetDeviceID")?);
            let create: unsafe extern "C" fn(u64) -> CFTypeRef =
                std::mem::transmute(symbol(c"MTActuatorCreateFromDeviceID")?);
            let open: unsafe extern "C" fn(CFTypeRef) -> i32 =
                std::mem::transmute(symbol(c"MTActuatorOpen")?);
            let actuate: ActuateFn = std::mem::transmute(symbol(c"MTActuatorActuate")?);

            let trackpads = list();
            if trackpads.is_null() {
                return None;
            }
            let devices: Vec<_> = (0..CFArrayGetCount(trackpads))
                .filter_map(|index| {
                    let mut id = 0;
                    (device_id(CFArrayGetValueAtIndex(trackpads, index), &raw mut id) == 0)
                        .then_some(id)
                })
                .map(|id| create(id))
                .filter(|actuator| !actuator.is_null() && open(*actuator) == 0)
                .collect();
            (!devices.is_empty()).then_some(Self { actuate, devices })
        }
    }
}

thread_local! {
    static ACTUATORS: OnceCell<Option<Actuators>> = const { OnceCell::new() };
}

/// Pulses the trackpad once, for a page change or a window-landing detent.
/// It falls back to the system's generic pulse when no trackpad actuator
/// opens.
pub(crate) fn page_changed(_main_thread: MainThreadMarker) {
    let actuated = ACTUATORS.with(|actuators| {
        actuators
            .get_or_init(Actuators::open)
            .as_ref()
            .is_some_and(|actuators| {
                // SAFETY: each device is an actuator `Actuators::open` opened, and
                // this thread is the only one that uses it.
                actuators
                .devices
                .iter()
                .filter(|device| unsafe { (actuators.actuate)(**device, ACTUATION, 0, 0.0, 0.0) } == 0)
                .count()
                > 0
            })
    });
    if !actuated {
        NSHapticFeedbackManager::defaultPerformer().performFeedbackPattern_performanceTime(
            NSHapticFeedbackPattern::Generic,
            NSHapticFeedbackPerformanceTime::Now,
        );
    }
}
