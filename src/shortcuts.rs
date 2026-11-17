//! User-configurable global shortcuts: defaults, parsing, display, and
//! turning a key press in the settings window into a shortcut.

use std::collections::BTreeMap;

use eframe::egui;
use global_hotkey::hotkey::{HotKey, Modifiers};
use serde::{Deserialize, Serialize};

use crate::app::AppEvent;
use crate::overlay::Purpose;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum Action {
    Image,
    FullScreen,
    Video,
    Scroll,
    Guide,
    Text,
    Color,
}

impl Action {
    pub const ALL: [Action; 7] = [Action::Image, Action::FullScreen, Action::Video, Action::Scroll, Action::Guide, Action::Text, Action::Color];

    pub fn label(self) -> &'static str {
        match self {
            Action::Image => "Capture image",
            Action::FullScreen => "Capture full screen",
            Action::Video => "Record video",
            Action::Scroll => "Scrolling capture",
            Action::Guide => "Record guide",
            Action::Text => "Grab text",
            Action::Color => "Pick a color",
        }
    }

    pub fn event(self) -> AppEvent {
        match self {
            Action::Image => AppEvent::Capture(Purpose::Image),
            Action::FullScreen => AppEvent::FullScreen,
            Action::Video => AppEvent::Capture(Purpose::Video),
            Action::Scroll => AppEvent::Capture(Purpose::Scroll),
            Action::Guide => AppEvent::Capture(Purpose::Guide),
            Action::Text => AppEvent::Capture(Purpose::Text),
            Action::Color => AppEvent::Capture(Purpose::Color),
        }
    }

    pub fn default_combo(self) -> &'static str {
        match self {
            Action::Image => "control+alt+KeyC",
            Action::FullScreen => "control+alt+KeyF",
            Action::Video => "control+alt+KeyR",
            Action::Scroll => "control+alt+KeyS",
            Action::Guide => "control+alt+KeyG",
            Action::Text => "control+alt+KeyT",
            Action::Color => "control+alt+KeyP",
        }
    }
}

/// Action → shortcut string in global-hotkey syntax (e.g. `control+alt+KeyC`).
/// An empty string means "no shortcut".
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts(pub BTreeMap<Action, String>);

impl Default for Shortcuts {
    fn default() -> Self {
        Self(Action::ALL.iter().map(|a| (*a, a.default_combo().to_owned())).collect())
    }
}

impl Shortcuts {
    pub fn get(&self, action: Action) -> &str {
        self.0.get(&action).map(String::as_str).unwrap_or(action.default_combo())
    }

    pub fn set(&mut self, action: Action, combo: String) {
        self.0.insert(action, combo);
    }

    pub fn is_default(&self, action: Action) -> bool {
        self.get(action) == action.default_combo()
    }

    pub fn hotkey(&self, action: Action) -> Option<HotKey> {
        self.get(action).parse().ok()
    }

    /// Human-readable form for labels, e.g. `⌃⌥C` on macOS.
    pub fn display(&self, action: Action) -> String {
        self.hotkey(action).map(display).unwrap_or_default()
    }

    /// Which other action already uses this combination.
    pub fn conflict(&self, action: Action, hotkey: HotKey) -> Option<Action> {
        Action::ALL
            .into_iter()
            .find(|&other| other != action && self.hotkey(other).is_some_and(|h| h.id() == hotkey.id()))
    }
}

pub fn display(hotkey: HotKey) -> String {
    let key = hotkey.key.to_string();
    let key = key.strip_prefix("Key").or_else(|| key.strip_prefix("Digit")).unwrap_or(&key).to_owned();
    let m = hotkey.mods;
    if cfg!(target_os = "macos") {
        let mut s = String::new();
        for (flag, sym) in [(Modifiers::CONTROL, "⌃"), (Modifiers::ALT, "⌥"), (Modifiers::SHIFT, "⇧"), (Modifiers::META, "⌘")] {
            if m.contains(flag) {
                s.push_str(sym);
            }
        }
        s + &key
    } else {
        let mut parts = Vec::new();
        for (flag, name) in [(Modifiers::CONTROL, "Ctrl"), (Modifiers::ALT, "Alt"), (Modifiers::SHIFT, "Shift"), (Modifiers::META, "Super")] {
            if m.contains(flag) {
                parts.push(name.to_owned());
            }
        }
        parts.push(key);
        parts.join("+")
    }
}

/// Turn a key press into a shortcut string. Needs Control, Option/Alt or
/// Command so a plain letter can't hijack typing everywhere.
pub fn from_key_press(key: egui::Key, mods: egui::Modifiers) -> Option<String> {
    if !(mods.ctrl || mods.alt || mods.mac_cmd || (!cfg!(target_os = "macos") && mods.command)) {
        return None;
    }
    let name = key.name();
    let code = if name.len() == 1 && name.chars().all(|c| c.is_ascii_alphabetic()) {
        format!("Key{}", name.to_ascii_uppercase())
    } else if name.len() == 1 && name.chars().all(|c| c.is_ascii_digit()) {
        format!("Digit{name}")
    } else if name.starts_with('F') && name[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) {
        name.to_owned()
    } else {
        return None;
    };
    let mut combo = String::new();
    if mods.ctrl {
        combo.push_str("control+");
    }
    if mods.alt {
        combo.push_str("alt+");
    }
    if mods.shift {
        combo.push_str("shift+");
    }
    if mods.mac_cmd {
        combo.push_str("super+");
    }
    combo.push_str(&code);
    combo.parse::<HotKey>().ok().map(|h| h.into_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_display() {
        let s = Shortcuts::default();
        for a in Action::ALL {
            assert!(s.hotkey(a).is_some(), "{a:?}");
        }
        if cfg!(target_os = "macos") {
            assert_eq!(s.display(Action::Image), "⌃⌥C");
            assert_eq!(s.display(Action::Color), "⌃⌥P");
        }
        // No two defaults share a shortcut.
        for a in Action::ALL {
            assert_eq!(s.conflict(a, s.hotkey(a).unwrap()), None, "{a:?}");
        }
    }

    #[test]
    fn key_press_becomes_shortcut() {
        let mods = egui::Modifiers { ctrl: true, shift: true, ..Default::default() };
        let combo = from_key_press(egui::Key::K, mods).unwrap();
        let hk: HotKey = combo.parse().unwrap();
        assert!(hk.mods.contains(Modifiers::CONTROL | Modifiers::SHIFT));
        assert!(from_key_press(egui::Key::K, egui::Modifiers::default()).is_none());
        assert_eq!(from_key_press(egui::Key::Num5, egui::Modifiers { alt: true, ..Default::default() }).as_deref().map(|c| c.ends_with("Digit5")), Some(true));
    }

    #[test]
    fn detects_conflicts() {
        let mut s = Shortcuts::default();
        let image = s.hotkey(Action::Image).unwrap();
        assert_eq!(s.conflict(Action::Video, image), Some(Action::Image));
        s.set(Action::Video, String::new());
        assert!(s.hotkey(Action::Video).is_none());
    }
}
