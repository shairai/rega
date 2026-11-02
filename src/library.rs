//! The capture library: every capture is saved into one folder, and images
//! keep an editable sidecar (original pixels + annotations) under `.snapquill/`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::APP_NAME;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemKind {
    Image,
    Video,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub name: String,
    pub kind: ItemKind,
    pub modified: SystemTime,
}

pub fn kind_of(path: &Path) -> Option<ItemKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => Some(ItemKind::Image),
        "mp4" | "mov" => Some(ItemKind::Video),
        _ => None,
    }
}

pub fn scan(dir: &Path) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<Item> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let kind = kind_of(&path)?;
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(Item {
                name: path.file_name()?.to_string_lossy().into_owned(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                path,
                kind,
            })
        })
        .collect();
    items.sort_by(|a, b| b.modified.cmp(&a.modified));
    items
}

/// A fresh, unique path such as `Snapquill 2026-11-02 at 14.03.22.png`.
pub fn new_capture_path(dir: &Path, ext: &str) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    let stamp = chrono::Local::now().format("%Y-%m-%d at %H.%M.%S");
    let mut path = dir.join(format!("{APP_NAME} {stamp}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{APP_NAME} {stamp} ({n}).{ext}"));
        n += 1;
    }
    path
}

/// Sidecar files that make an image re-editable: (annotations json, original image).
pub fn project_paths(image: &Path) -> (PathBuf, PathBuf) {
    let dir = image.parent().unwrap_or_else(|| Path::new(".")).join(".snapquill");
    let name = image
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (dir.join(format!("{name}.json")), dir.join(format!("{name}.orig.png")))
}

pub fn delete(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)?;
    let (json, orig) = project_paths(path);
    let _ = std::fs::remove_file(json);
    let _ = std::fs::remove_file(orig);
    Ok(())
}
