use std::ptr::NonNull;

use accessibility_sys::{
    AXIsProcessTrusted, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementSetMessagingTimeout, kAXRoleAttribute,
};
use clap::Parser;
use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_foundation::NSBundle;
use serde::Serialize;

use crate::errors::Result;

#[derive(Parser)]
#[command(name = "miri-ax-probe")]
struct Probe {
    #[arg(long, value_parser = clap::value_parser!(i32).range(1..))]
    target_pid: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    schema_version: u8,
    pid: u32,
    target_pid: i32,
    trusted: bool,
    messaging_timeout_code: i32,
    read_code: i32,
    role: Option<String>,
    executable: String,
    bundle_identifier: Option<String>,
}

pub fn run() -> Result<()> {
    let probe = Probe::parse_from(std::env::args().skip(1));
    let observation = objc2::rc::autoreleasepool(|_| -> Result<Observation> {
        let element = unsafe { AXUIElementCreateApplication(probe.target_pid) };
        let element: CFRetained<CFType> = unsafe {
            CFRetained::from_raw(NonNull::new(element.cast()).expect("AX application element"))
        };
        let name = CFString::from_static_str(kAXRoleAttribute);
        let mut value = std::ptr::null();
        let messaging_timeout_code = unsafe {
            AXUIElementSetMessagingTimeout(CFRetained::as_ptr(&element).as_ptr().cast(), 1.0)
        };
        let read_code = unsafe {
            AXUIElementCopyAttributeValue(
                CFRetained::as_ptr(&element).as_ptr().cast(),
                (&raw const *name).cast(),
                &raw mut value,
            )
        };
        let role = NonNull::new(value.cast_mut()).and_then(|ptr| {
            let value: CFRetained<CFType> = unsafe { CFRetained::from_raw(ptr.cast()) };
            value
                .downcast::<CFString>()
                .ok()
                .map(|value| value.to_string())
        });
        Ok(Observation {
            schema_version: 1,
            pid: std::process::id(),
            target_pid: probe.target_pid,
            trusted: unsafe { AXIsProcessTrusted() },
            messaging_timeout_code,
            read_code,
            role,
            executable: std::env::current_exe()?.to_string_lossy().into_owned(),
            bundle_identifier: NSBundle::mainBundle()
                .bundleIdentifier()
                .map(|identifier| identifier.to_string()),
        })
    })?;
    println!("{}", serde_json::to_string(&observation)?);
    Ok(())
}
