use std::env;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub translate_lang: String,
    pub translations_visible: bool,
    pub pronunciation_visible: bool,
    pub ui_scale: f32,
    pub now_playing_panel: bool,
    pub volume: Option<f64>,
    pub window_size: Option<[f32; 2]>,
    pub window_pos: Option<[f32; 2]>,
    pub window_maximized: bool,
    pub last_playlist_uri: Option<String>,
    pub theme_source: String,
}

pub const DEFAULT_UI_SCALE: f32 = 1.25;
pub const MIN_UI_SCALE: f32 = 0.8;
pub const MAX_UI_SCALE: f32 = 2.0;
pub const UI_SCALE_STEP: f32 = 0.1;

pub fn clamp_ui_scale(scale: f32) -> f32 {
    if !scale.is_finite() {
        return DEFAULT_UI_SCALE;
    }
    ((scale / UI_SCALE_STEP).round() * UI_SCALE_STEP).clamp(MIN_UI_SCALE, MAX_UI_SCALE)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            translate_lang: "ES".to_string(),
            translations_visible: false,
            pronunciation_visible: false,
            ui_scale: DEFAULT_UI_SCALE,
            now_playing_panel: false,
            volume: None,
            window_size: None,
            window_pos: None,
            window_maximized: false,
            last_playlist_uri: None,
            theme_source: "dms".to_string(),
        }
    }
}

fn settings_path() -> String {
    let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/settings.json")
}

pub fn load() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) {
    if let Ok(json) = serde_json::to_string(settings) {
        let _ = std::fs::write(settings_path(), json);
    }
}
