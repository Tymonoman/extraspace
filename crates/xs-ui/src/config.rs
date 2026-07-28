//! Persisted user settings.
//!
//! A JSON file under `$XDG_CONFIG_HOME` rather than GSettings. GSettings would be
//! the more GNOME-native choice, but it needs a compiled schema installed
//! system-wide, which means `cargo run` in a fresh clone would fail with a fairly
//! baffling error. Contributors being able to clone and run matters more here.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use xs_core::{BitrateBounds, DisplayMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Logical scale; the monitor is created panel_size/scale. 1.0 is native.
    pub scale: f64,
    /// `"extend"` or `"mirror"`.
    pub mode: String,
    /// Connector to mirror, when mode is `"mirror"`.
    pub mirror_source: Option<String>,
    pub framerate: u32,
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub camera_enabled: bool,
    pub camera_id: String,
    /// Start streaming as soon as a tablet is detected.
    pub auto_connect: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 1x is unreadable on a 10.4" panel; 1.5x is the sweet spot.
            scale: 1.5,
            mode: "extend".into(),
            mirror_source: None,
            framerate: 60,
            min_bitrate_kbps: BitrateBounds::default().min_kbps,
            max_bitrate_kbps: BitrateBounds::default().max_kbps,
            camera_enabled: false,
            camera_id: "0".into(),
            auto_connect: true,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("extraspace").join("config.json")
    }

    /// Loads settings, falling back to defaults for anything unreadable.
    ///
    /// A corrupt config must never stop the app starting -- the user would have
    /// no way to fix it from within the UI.
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(config) => {
                    debug!(path = %path.display(), "settings loaded");
                    config
                }
                Err(e) => {
                    warn!(error = %e, "settings file is malformed; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                warn!(error = %e, "could not create the settings directory");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    warn!(error = %e, "could not save settings");
                }
            }
            Err(e) => warn!(error = %e, "could not serialise settings"),
        }
    }

    pub fn bounds(&self) -> BitrateBounds {
        // Guard against a hand-edited file with the two the wrong way round.
        let min = self.min_bitrate_kbps.min(self.max_bitrate_kbps);
        let max = self.max_bitrate_kbps.max(self.min_bitrate_kbps);
        BitrateBounds {
            min_kbps: min,
            max_kbps: max,
        }
    }

    pub fn display_mode(&self) -> DisplayMode {
        match self.mode.as_str() {
            "mirror" => DisplayMode::Mirror,
            _ => DisplayMode::Extend,
        }
    }
}

/// Scale options offered in the UI, with the labels shown next to them.
pub const SCALE_OPTIONS: &[f64] = &[1.0, 1.25, 1.5, 1.75, 2.0];

/// Index of the closest available scale, so a hand-edited value still selects
/// something sensible rather than nothing.
pub fn scale_index(scale: f64) -> u32 {
    SCALE_OPTIONS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (*a - scale)
                .abs()
                .partial_cmp(&(*b - scale).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i as u32)
        .unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert!(c.scale >= 1.0 && c.scale <= 3.0);
        assert!(c.bounds().min_kbps < c.bounds().max_kbps);
        assert_eq!(c.display_mode(), DisplayMode::Extend);
    }

    #[test]
    fn swapped_bitrate_bounds_are_corrected() {
        let c = Config {
            min_bitrate_kbps: 30_000,
            max_bitrate_kbps: 6_000,
            ..Default::default()
        };
        let b = c.bounds();
        assert_eq!(b.min_kbps, 6_000);
        assert_eq!(b.max_kbps, 30_000);
    }

    #[test]
    fn unknown_mode_falls_back_to_extend() {
        let c = Config {
            mode: "nonsense".into(),
            ..Default::default()
        };
        assert_eq!(c.display_mode(), DisplayMode::Extend);
    }

    #[test]
    fn scale_index_snaps_to_the_nearest_option() {
        assert_eq!(scale_index(1.0), 0);
        assert_eq!(scale_index(1.5), 2);
        assert_eq!(scale_index(2.0), 4);
        // A hand-edited oddity still lands somewhere reasonable.
        assert_eq!(scale_index(1.6), 2);
        assert_eq!(scale_index(1.7), 3);
    }

    #[test]
    fn config_roundtrips_through_json() {
        let c = Config {
            scale: 1.75,
            mode: "mirror".into(),
            camera_enabled: true,
            ..Default::default()
        };
        let text = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.scale, 1.75);
        assert_eq!(back.display_mode(), DisplayMode::Mirror);
        assert!(back.camera_enabled);
    }

    #[test]
    fn partial_json_fills_in_defaults() {
        // serde(default) means an old config missing new fields still loads.
        let back: Config = serde_json::from_str(r#"{"scale": 2.0}"#).unwrap();
        assert_eq!(back.scale, 2.0);
        assert_eq!(back.framerate, 60);
    }
}
