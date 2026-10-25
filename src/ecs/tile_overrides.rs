//! Windows the user tiled with the float toggle although a floating rule
//! matches them. The choice holds while that native window stays open, across
//! engine restarts, so it is kept in a file next to the layout state. A saved
//! tiled placement alone is not an override.
//!
//! A window is known by its window number and its process, and the process by
//! its ID and start time. A later process that reuses the ID, or a window that
//! reuses the number, does not inherit the override.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use bevy::ecs::resource::Resource;
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

use crate::manager::{Application, Window};
use crate::platform::{Pid, WinID};

pub const TILE_OVERRIDES_FILE_NAME: &str = "tile-overrides.json";
const SUPPORTED_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct TiledWindow {
    bundle_id: String,
    pid: Pid,
    /// The process start time in microseconds since the epoch.
    started: u64,
    window_id: WinID,
}

impl TiledWindow {
    fn of(window: &Window, app: &Application) -> Option<Self> {
        let pid = window.pid().ok()?;
        Some(Self {
            bundle_id: app.bundle_id().unwrap_or_default(),
            pid,
            started: process_started(pid)?,
            window_id: window.id(),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedTileOverrides {
    version: u32,
    windows: Vec<TiledWindow>,
}

#[derive(Debug, Default, Resource)]
pub struct TileOverrides {
    windows: HashSet<TiledWindow>,
    /// Where changes are saved. The mock harness has none.
    path: Option<PathBuf>,
}

impl TileOverrides {
    /// The overrides a previous run saved, without those of processes that
    /// have since exited.
    #[must_use]
    pub fn load() -> Self {
        let path = xdg::BaseDirectories::with_prefix("paneru")
            .get_state_file(TILE_OVERRIDES_FILE_NAME)
            .expect("XDG state directory should be available");
        let windows = fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str::<SavedTileOverrides>(&data).ok())
            .filter(|saved| saved.version == SUPPORTED_VERSION)
            .map(|saved| saved.windows)
            .unwrap_or_default()
            .into_iter()
            .filter(|window| process_started(window.pid) == Some(window.started))
            .collect();
        Self {
            windows,
            path: Some(path),
        }
    }

    /// Whether the user tiled `window` against a floating rule.
    pub fn contains(&self, window: &Window, app: &Application) -> bool {
        !self.windows.is_empty()
            && TiledWindow::of(window, app).is_some_and(|key| self.windows.contains(&key))
    }

    /// Records that the user tiled `window` against a floating rule.
    pub fn remember(&mut self, window: &Window, app: &Application) {
        if let Some(key) = TiledWindow::of(window, app)
            && self.windows.insert(key)
        {
            self.save();
        }
    }

    /// Ends the override of the window `window_id`, which floated again or
    /// closed. The window server does not reuse a number while it runs.
    pub fn forget(&mut self, window_id: WinID) {
        let before = self.windows.len();
        self.windows.retain(|window| window.window_id != window_id);
        if self.windows.len() != before {
            self.save();
        }
    }

    fn save(&self) {
        if let Some(path) = &self.path {
            match self.write(path) {
                Ok(()) => debug!("Tile overrides saved to {}", path.display()),
                Err(err) => error!("Failed to save tile overrides to {}: {err}", path.display()),
            }
        }
    }

    fn write(&self, path: &Path) -> Result<(), std::io::Error> {
        let saved = SavedTileOverrides {
            version: SUPPORTED_VERSION,
            windows: self.windows.iter().cloned().collect(),
        };
        let json = serde_json::to_string_pretty(&saved).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, json)?;
        fs::rename(tmp_path, path)
    }
}

/// The start of the kernel's `kinfo_proc` record: `kp_proc.p_un` begins with
/// the process start time. The rest of the record is not read.
#[repr(C)]
struct KinfoProc {
    started: libc::timeval,
    rest: [u8; 632],
}

/// When the process `pid` started, in microseconds since the epoch, or `None`
/// when no such process runs.
fn process_started(pid: Pid) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<KinfoProc>::zeroed();
    let mut size = std::mem::size_of::<KinfoProc>();
    let mut name = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let result = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            4,
            info.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != std::mem::size_of::<KinfoProc>() {
        return None;
    }
    let started = unsafe { info.assume_init() }.started;
    let seconds = u64::try_from(started.tv_sec).ok()?;
    let micros = u64::try_from(started.tv_usec).ok()?;
    Some(seconds * 1_000_000 + micros)
}

#[cfg(test)]
mod tests {
    use super::process_started;

    #[test]
    fn a_running_process_has_a_stable_start_time() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let started = process_started(pid);
        assert!(started.is_some());
        assert_eq!(process_started(pid), started);
    }

    #[test]
    fn a_process_of_another_user_has_a_start_time() {
        assert!(process_started(1).is_some());
    }

    #[test]
    fn a_missing_process_has_no_start_time() {
        assert_eq!(process_started(i32::MAX), None);
    }
}
