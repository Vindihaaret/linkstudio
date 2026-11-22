//! Persistent settings: saved gimbal positions, profiles and preferences,
//! stored as JSON in ~/.config/linkstudio/config.json.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// WebCamControl stores pan/tilt in degrees; V4L2 uses arc seconds.
const ARCSEC_PER_DEGREE: i32 = 3600;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub name: String,
    pub pan: i32,
    pub tilt: i32,
    pub zoom: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// V4L2 control id (hex) → value.
    pub controls: BTreeMap<String, i32>,
    pub tracking: Option<bool>,
    pub framing: Option<u8>,
    pub group_tracking: Option<bool>,
    #[serde(default)]
    pub exposure_compensation: Option<i16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub positions: Vec<Position>,
    pub profiles: Vec<Profile>,
    /// Profile applied every time the camera is connected.
    pub startup_profile: Option<String>,
    pub preview_autostart: bool,
    pub preview_mirror: bool,
    /// Rule-of-thirds lines over the preview.
    pub preview_grid: bool,
    pub preview_height: u32,
    pub imported_webcamcontrol: bool,
    /// Language code, or `None` to follow the desktop.
    pub language: Option<String>,
    /// Feed the "Link Studio Camera" virtual camera whenever the camera is connected.
    pub virtual_camera: bool,
    pub virtual_camera_height: u32,
    pub appearance: Appearance,
    /// Let Claude (through `linkstudio mcp`) read and change camera settings.
    /// Off until the user turns it on.
    pub claude_control: bool,
    /// Let Claude take pictures through the camera. Off until the user turns it on.
    pub claude_camera_view: bool,
    /// Privacy mode survives restarts, so the camera is never silently pointed
    /// back at the room. Holds where to return to (pan, tilt, zoom).
    pub privacy: bool,
    pub privacy_restore: Option<(i32, i32, i32)>,
    /// Turn privacy mode on while the screen is locked.
    pub privacy_on_lock: bool,
    /// Closing the window keeps Link Studio in the status icon instead of quitting.
    pub close_to_tray: bool,
    /// Look for a new release on GitHub once a month.
    pub auto_update_check: bool,
    /// Unix time of the last successful update check.
    pub last_update_check: i64,
    /// A release tag the user chose to skip.
    pub skipped_version: Option<String>,
    /// Write what the app does to a log file (see `debuglog`).
    pub debug_log: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            positions: Vec::new(),
            profiles: Vec::new(),
            startup_profile: None,
            preview_autostart: true,
            preview_mirror: false,
            preview_grid: false,
            preview_height: 720,
            imported_webcamcontrol: false,
            language: None,
            appearance: Appearance::System,
            virtual_camera: false,
            virtual_camera_height: 1080,
            claude_control: false,
            claude_camera_view: false,
            privacy: false,
            privacy_restore: None,
            privacy_on_lock: false,
            close_to_tray: false,
            auto_update_check: true,
            last_update_check: 0,
            skipped_version: None,
            debug_log: false,
        }
    }
}

fn config_path() -> PathBuf {
    gtk::glib::user_config_dir().join("linkstudio").join("config.json")
}

impl Config {
    /// Only the debug log switch, for `debuglog` in every process.
    pub fn read_debug_log() -> bool {
        #[derive(Deserialize)]
        struct Only {
            #[serde(default)]
            debug_log: bool,
        }
        fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_json::from_str::<Only>(&text).ok())
            .is_some_and(|only| only.debug_log)
    }

    pub fn load() -> Self {
        let path = config_path();
        let mut config: Config = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                crate::debuglog::debug!("config: {} is damaged ({err}), starting with defaults", path.display());
                // Keep a damaged file instead of overwriting it with defaults,
                // so saved positions and profiles can still be recovered.
                let _ = fs::rename(&path, path.with_extension("json.damaged"));
                Config::default()
            }),
            Err(_) => Config::default(),
        };
        if !config.imported_webcamcontrol {
            config.import_webcamcontrol();
            config.save();
        }
        config
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        // Write a new file and move it into place, so a crash or a full disk
        // in the middle never leaves a half-written file.
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let temporary = path.with_extension("json.tmp");
            if fs::write(&temporary, text).is_ok() {
                let _ = fs::rename(&temporary, &path);
            }
        }
    }

    pub fn preview_size(&self) -> (u32, u32) {
        match self.preview_height {
            2160 => (3840, 2160),
            1080 => (1920, 1080),
            _ => (1280, 720),
        }
    }

    /// Video-call programs rarely use more than 1080p, so 4K is not offered.
    pub fn virtual_camera_size(&self) -> (u32, u32) {
        match self.virtual_camera_height {
            720 => (1280, 720),
            _ => (1920, 1080),
        }
    }

    /// One-time import of positions saved in the WebCamControl Flatpak.
    fn import_webcamcontrol(&mut self) {
        self.imported_webcamcontrol = true;
        let path = gtk::glib::home_dir().join(".var/app/com.daniel15.wcc/config/webcamcontrol.json");
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Some(presets) = value.get("Presets").and_then(|p| p.as_array()) else {
            return;
        };
        for preset in presets {
            let field = |key: &str| preset.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let name = preset.get("Name").and_then(|v| v.as_str()).unwrap_or(crate::i18n::tr("Position"));
            if self.positions.iter().any(|p| p.name == name) {
                continue;
            }
            self.positions.push(Position {
                name: name.to_owned(),
                pan: (field("Pan") * ARCSEC_PER_DEGREE as f64).round() as i32,
                tilt: (field("Tilt") * ARCSEC_PER_DEGREE as f64).round() as i32,
                zoom: field("Zoom").round().max(100.0) as i32,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_get_defaults() {
        let config: Config = serde_json::from_str(r#"{"positions":[{"name":"Desk","pan":3600,"tilt":0,"zoom":150}]}"#).unwrap();
        assert_eq!(config.positions[0].name, "Desk");
        assert!(config.preview_autostart);
        assert!(config.auto_update_check);
        // Nothing that gives access or changes behaviour is on by default.
        assert!(!config.claude_control && !config.claude_camera_view);
        assert!(!config.privacy_on_lock && !config.close_to_tray && !config.virtual_camera);
    }

    #[test]
    fn survives_a_round_trip() {
        let mut config = Config { privacy: true, privacy_restore: Some((3600, -7200, 200)), ..Default::default() };
        config.profiles.push(Profile { name: "Evening".into(), exposure_compensation: Some(-128), ..Default::default() });
        let back: Config = serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(back.privacy_restore, Some((3600, -7200, 200)));
        assert_eq!(back.profiles[0].exposure_compensation, Some(-128));
    }

    #[test]
    fn sizes() {
        let mut config = Config::default();
        assert_eq!(config.preview_size(), (1280, 720));
        config.preview_height = 2160;
        assert_eq!(config.preview_size(), (3840, 2160));
        config.virtual_camera_height = 2160;
        assert_eq!(config.virtual_camera_size(), (1920, 1080));
    }
}
