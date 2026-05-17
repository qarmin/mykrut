//! Helpers for browsing & restoring the XDG Trash without re-implementing it.
//!
//! For listing we just call `LocalFs::list` on `$XDG_DATA_HOME/Trash/files`
//! (the on-disk content is exactly what should be shown). The added value here
//! is detecting "this file is in the trash" + restoring it to its original
//! location via the matching `.trashinfo` sidecar.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use tracing::{info, warn};

pub fn trash_root() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("Trash"))
}

pub fn trash_files_dir() -> Option<PathBuf> {
    Some(trash_root()?.join("files"))
}

pub fn trash_info_dir() -> Option<PathBuf> {
    Some(trash_root()?.join("info"))
}

pub fn is_in_trash(p: &Path) -> bool {
    let Some(files) = trash_files_dir() else {
        return false;
    };
    p.starts_with(&files)
}

/// Parse the `.trashinfo` sidecar that XDG Trash spec writes next to every
/// deleted item. Returns the original path on success.
pub fn original_path_for(trashed: &Path) -> Result<PathBuf> {
    let info_dir = trash_info_dir().ok_or_else(|| anyhow!("no XDG data dir"))?;
    let name = trashed
        .file_name()
        .ok_or_else(|| anyhow!("no file name"))?
        .to_string_lossy()
        .into_owned();
    let info_path = info_dir.join(format!("{name}.trashinfo"));

    let text = std::fs::read_to_string(&info_path).map_err(|e| anyhow!("read {}: {e}", info_path.display()))?;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Path=") {
            // Spec says URL-encoded. Most desktop apps just escape '%'.
            let decoded = url_decode(rest);
            return Ok(PathBuf::from(decoded));
        }
    }
    Err(anyhow!("no Path= line in {}", info_path.display()))
}

#[expect(
    clippy::indexing_slicing,
    reason = "every access is bounds-checked: `bytes[i]` by the `while i < len` guard, \
              `bytes[i + 1]`/`bytes[i + 2]` by the `i + 2 < len` check"
)]
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex(bytes[i + 1]);
            let lo = hex(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Permanently delete every item currently in the trash. Returns how many
/// top-level items were purged. Uses the `trash` crate's freedesktop-aware
/// listing so it covers per-volume trash cans, not just the home one.
///
/// Purges one item at a time rather than handing the whole list to
/// `purge_all`: that function aborts on the *first* error (e.g. a stale
/// `.trashinfo` sidecar whose file is already gone), which previously left
/// everything after it untouched. A bad entry here is logged and skipped so
/// the rest of the trash still gets emptied.
pub fn empty_trash() -> Result<usize> {
    let items = trash::os_limited::list().map_err(|e| anyhow!("list trash: {e}"))?;
    let mut count = 0;
    for item in items {
        let name = item.name.to_string_lossy().into_owned();
        match trash::os_limited::purge_all([item]) {
            Ok(()) => count += 1,
            Err(e) => warn!(name, ?e, "could not purge trash item, skipping"),
        }
    }
    info!(count, "emptied trash");
    Ok(count)
}

/// Restore the most-recently-trashed item whose original location was `orig`.
/// Used to undo a "move to trash": we only know the original path, so we scan
/// the trash for a match. Picks the newest if several share the path.
pub fn restore_by_original(orig: &Path) -> Result<()> {
    let items = trash::os_limited::list().map_err(|e| anyhow!("list trash: {e}"))?;
    let mut matches: Vec<trash::TrashItem> = items.into_iter().filter(|it| it.original_path() == orig).collect();
    if matches.is_empty() {
        return Err(anyhow!("not found in trash: {}", orig.display()));
    }
    matches.sort_by_key(|it| it.time_deleted);
    let newest = matches.pop().expect("non-empty checked above");
    trash::os_limited::restore_all([newest]).map_err(|e| anyhow!("restore {}: {e}", orig.display()))?;
    info!(orig = %orig.display(), "restored from trash (undo)");
    Ok(())
}

/// Restore one file from the trash. Reads its `.trashinfo` to discover the
/// original path, renames the trashed file back, deletes the sidecar.
/// If the original parent no longer exists, returns an error and leaves files alone.
pub fn restore(trashed: &Path) -> Result<PathBuf> {
    let orig = original_path_for(trashed)?;
    if let Some(parent) = orig.parent()
        && !parent.exists()
    {
        return Err(anyhow!("original parent gone: {}", parent.display()));
    }
    // `symlink_metadata` (not `exists`) so a dangling symlink at the original
    // path is still treated as occupied — restoring over it would clobber.
    if std::fs::symlink_metadata(&orig).is_ok() {
        return Err(anyhow!("destination exists: {}", orig.display()));
    }

    std::fs::rename(trashed, &orig).map_err(|e| anyhow!("rename {} → {}: {e}", trashed.display(), orig.display()))?;

    // Delete the sidecar. If it fails, log but don't fail the restore.
    if let Some(info_dir) = trash_info_dir()
        && let Some(name) = trashed.file_name()
    {
        let info_path = info_dir.join(format!("{}.trashinfo", name.to_string_lossy()));
        if let Err(e) = std::fs::remove_file(&info_path) {
            warn!(?e, info_path = %info_path.display(), "could not remove trashinfo sidecar");
        }
    }

    info!(trashed = %trashed.display(), restored = %orig.display(), "restore");
    Ok(orig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_basic() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("plain"), "plain");
        assert_eq!(
            url_decode("/home/user/Documents%2Ffile.txt"),
            "/home/user/Documents/file.txt"
        );
    }

    #[test]
    fn url_decode_unknown_escape_left_intact() {
        assert_eq!(url_decode("a%XYb"), "a%XYb");
    }
}
