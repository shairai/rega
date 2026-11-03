use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const APP_NAME: &str = "Rega";

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
    /// Bring Rega to the front on the new capture when it's done.
    pub focus_after_capture: bool,
    pub delay_secs: u32,
    pub video_format: VideoFormat,
    pub video_fps: u32,
    pub video_max_width: u32,
    pub highlight_cursor: bool,
    pub launch_at_login: bool,
    /// The first-launch permissions popup has been shown once.
    pub permissions_prompt_shown: bool,
    /// macOS's direct-screen-access confirmation has been triggered once.
    pub screen_access_probed: bool,
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
            focus_after_capture: true,
            delay_secs: 0,
            video_format: VideoFormat::Mp4,
            video_fps: 15,
            video_max_width: 1920,
            highlight_cursor: true,
            launch_at_login: true,
            permissions_prompt_shown: false,
            screen_access_probed: false,
            shortcuts: Default::default(),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_NAME).join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = settings_path() else { return Self::default() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(e) => {
                // Keep the unreadable file: the next save would otherwise wipe the
                // user's shortcuts and folders for good.
                log::warn!("settings unreadable ({e}); starting from defaults, old file kept as settings.json.bak");
                let _ = std::fs::rename(&path, path.with_extension("json.bak"));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                // Write then rename, so a crash mid-write can't leave a truncated file.
                let tmp = path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
                    log::warn!("failed to save settings: {e}");
                }
            }
            Err(e) => log::warn!("failed to serialize settings: {e}"),
        }
    }
}
