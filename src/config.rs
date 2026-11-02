use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const APP_NAME: &str = "Snapquill";

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum VideoFormat {
    Mp4,
    Gif,
}

impl VideoFormat {
    pub fn ext(self) -> &'static str {
        match self {
            VideoFormat::Mp4 => "mp4",
            VideoFormat::Gif => "gif",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub save_dir: PathBuf,
    pub image_format: ImageFormat,
    pub copy_after_capture: bool,
    pub open_editor_after_capture: bool,
    pub delay_secs: u32,
    pub video_format: VideoFormat,
    pub video_fps: u32,
    pub video_max_width: u32,
    pub highlight_cursor: bool,
    pub launch_at_login: bool,
    pub shortcuts: crate::shortcuts::Shortcuts,
}

impl Default for Settings {
    fn default() -> Self {
        let base = dirs::picture_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            save_dir: base.join(APP_NAME),
            image_format: ImageFormat::Png,
            copy_after_capture: true,
            open_editor_after_capture: true,
            delay_secs: 0,
            video_format: VideoFormat::Mp4,
            video_fps: 15,
            video_max_width: 1920,
            highlight_cursor: true,
            launch_at_login: true,
            shortcuts: Default::default(),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_NAME).join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        settings_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    log::warn!("failed to save settings: {e}");
                }
            }
            Err(e) => log::warn!("failed to serialize settings: {e}"),
        }
    }
}
