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
use crate::platform::{Pid, WinID, process_started};
use crate::util::{write_json_atomic, xdg_state_file};

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
        let path = xdg_state_file(TILE_OVERRIDES_FILE_NAME);
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

    /// Ends the overrides of every window of the process `pid`, which quit.
    pub fn forget_process(&mut self, pid: Pid) {
        let before = self.windows.len();
        self.windows.retain(|window| window.pid != pid);
        if self.windows.len() != before {
            self.save();
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
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
        write_json_atomic(path, &saved)
    }
}
