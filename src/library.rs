//! The capture library: every capture is saved into one folder, and images
//! keep an editable sidecar (original pixels + annotations) under `.rega/`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    items.sort_by_key(|a| std::cmp::Reverse(a.modified));
    items
}

/// A fresh, unique path such as `Rega 2026-11-02 at 14.03.22.png`, taken
/// neither by a file nor by a copy still being saved under that name.
pub fn new_capture_path(dir: &Path, ext: &str) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    let stamp = chrono::Local::now().format("%Y-%m-%d at %H.%M.%S");
    let mut path = dir.join(format!("{APP_NAME} {stamp}.{ext}"));
    let mut n = 2;
    while path.exists() || part_path(&path).exists() {
        path = dir.join(format!("{APP_NAME} {stamp} ({n}).{ext}"));
        n += 1;
    }
    path
}

/// Where a copy for `path` is written until it's complete: `.<name>.part` in
/// the same folder (so the final rename stays on one volume), a name the
/// library doesn't list. The leading dot hides it on macOS only.
pub fn part_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.part"))
}

/// Sidecar files that make an image re-editable: (annotations json, original image).
pub fn project_paths(image: &Path) -> (PathBuf, PathBuf) {
    let dir = image.parent().unwrap_or_else(|| Path::new(".")).join(".rega");
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

/// How long a partial copy must have gone unwritten to count as left behind,
/// rather than one that another Rega, running alongside, is still writing.
const STALE_PART: Duration = Duration::from_secs(10 * 60);

/// Remove the partial copies (`.<name>.part`, see the player's export) left
/// in `dir` by saves that a crash or a forced quit cut short. Returns how
/// many went.
pub fn remove_stale_parts(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        // Only names a copy of a library item gets; other apps' files stay.
        let Some(copy) = name.to_str().and_then(|n| n.strip_prefix('.')?.strip_suffix(".part")) else { continue };
        let stale = entry.metadata().is_ok_and(|m| {
            m.is_file() && m.modified().is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > STALE_PART))
        });
        if stale && kind_of(Path::new(copy)).is_some() && !held_open(&entry.path()) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Whether another program, such as a second Rega still saving a long copy,
/// has `path` open. Only Windows needs asking: its folder listing can show a
/// file's old write time until the writer closes it, and the writer's handle
/// doesn't stop a delete. Opening the file unshared fails while it's open.
#[cfg(windows)]
fn held_open(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).share_mode(0).open(path).is_err()
}

#[cfg(not(windows))]
fn held_open(_: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{new_capture_path, part_path, remove_stale_parts};

    /// A name a copy is still being saved under is taken: a second save
    /// started the same second gets another.
    #[test]
    fn names_being_saved_are_taken() {
        let dir = std::env::temp_dir().join(format!("rega-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = new_capture_path(&dir, "mp4");
        assert_eq!(part_path(&first).file_name().unwrap().to_string_lossy(), format!(".{}.part", first.file_name().unwrap().to_string_lossy()));
        std::fs::File::create(part_path(&first)).unwrap();
        assert_ne!(new_capture_path(&dir, "mp4"), first);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Partial copies that a save cut short left behind go once they're old;
    /// fresh ones (another Rega may be writing them) and other files stay.
    #[test]
    fn removes_stale_partial_copies_only() {
        let dir = std::env::temp_dir().join(format!("rega-stale-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ago = |minutes: u64| SystemTime::now() - Duration::from_secs(minutes * 60);
        let (old, recent) = (ago(60), ago(5));
        for (name, modified) in [
            (".Rega 2026-11-05 at 10.20.30.mp4.part", old),
            (".Rega 2026-11-05 at 10.20.31.gif.part", old),
            (".Rega 2026-11-05 at 11.20.30.mp4.part", recent),
            ("Rega 2026-11-05 at 10.20.30.mp4", old),
            ("Rega 2026-11-05 at 10.20.32.mp4.part", old),
            (".notes.txt.part", old),
            (".DS_Store", old),
        ] {
            std::fs::File::create(dir.join(name)).unwrap().set_modified(modified).unwrap();
        }
        assert_eq!(remove_stale_parts(&dir), 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(
            left,
            [
                ".DS_Store",
                ".Rega 2026-11-05 at 11.20.30.mp4.part",
                ".notes.txt.part",
                "Rega 2026-11-05 at 10.20.30.mp4",
                "Rega 2026-11-05 at 10.20.32.mp4.part",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Windows: a partial copy that another program still has open (a second
    /// Rega saving a long copy) stays, however old the folder listing says
    /// it is; once it's closed, it goes.
    #[cfg(windows)]
    #[test]
    fn keeps_partial_copies_still_open() {
        let dir = std::env::temp_dir().join(format!("rega-open-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join(".Rega 2026-11-05 at 10.20.30.mp4.part");
        std::fs::File::create(&part).unwrap().set_modified(SystemTime::now() - Duration::from_secs(60 * 60)).unwrap();
        // Held open as an export holds its copy, which doesn't stop a delete.
        let writer = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
        assert_eq!(remove_stale_parts(&dir), 0, "a copy still being written stays");
        drop(writer);
        assert_eq!(remove_stale_parts(&dir), 1, "a copy left behind goes");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
