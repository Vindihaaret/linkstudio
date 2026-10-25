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
    /// Closing the window keeps Link Studio in the status icon instead of quitting.
    pub close_to_tray: bool,
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
            close_to_tray: false,
        }
    }
}

fn config_path() -> PathBuf {
    gtk::glib::user_config_dir().join("linkstudio").join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let mut config: Config = fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
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
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, text);
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
