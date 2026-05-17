use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::fs;
use tracing::{debug, info, warn};

use crate::copy_progress::CancelFlag;

/// Running totals streamed from [`deep_count`] while it walks a tree.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeepCountStats {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

#[derive(Debug, Error)]
pub enum FileOpError {
    #[error("path exists at destination: {0}")]
    DestExists(PathBuf),
    #[error("invalid name (contains '/' or empty): {0}")]
    InvalidName(String),
    #[error("source path is gone: {0}")]
    SourceGone(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("trash: {0}")]
    Trash(#[from] trash::Error),
    #[error("walkdir: {0}")]
    Walk(#[from] walkdir::Error),
}

/// Move a list of paths to the OS trash.
///
/// `trash` crate handles XDG Trash spec on Linux (separate trash per volume,
/// .trashinfo metadata) and platform equivalents on macOS/Windows.
pub fn move_to_trash<P, I>(paths: I) -> Result<usize, FileOpError>
where
    P: AsRef<Path>,
    I: IntoIterator<Item = P>,
{
    let paths: Vec<PathBuf> = paths.into_iter().map(|p| p.as_ref().to_path_buf()).collect();
    let count = paths.len();
    info!(count, "move_to_trash");
    trash::delete_all(&paths)?;
    Ok(count)
}

/// Delete a list of paths permanently. Dirs are removed recursively.
pub async fn delete_permanently<P, I>(paths: I) -> Result<usize, FileOpError>
where
    P: AsRef<Path>,
    I: IntoIterator<Item = P>,
{
    let mut count = 0;
    for p in paths {
        let p = p.as_ref();
        let meta = match fs::symlink_metadata(p).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(path = %p.display(), "already gone");
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let ft = meta.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            fs::remove_dir_all(p).await?;
        } else {
            fs::remove_file(p).await?;
        }
        info!(path = %p.display(), "deleted");
        count += 1;
    }
    Ok(count)
}

/// Validate a candidate new file name (no '/', not empty, not "." or "..").
pub fn validate_name(name: &str) -> Result<(), FileOpError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(FileOpError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Rename `src` to a new name inside the same directory.
/// Returns the new absolute path.
pub async fn rename_in_place(src: &Path, new_name: &str) -> Result<PathBuf, FileOpError> {
    validate_name(new_name)?;
    let parent = src.parent().ok_or_else(|| FileOpError::SourceGone(src.to_path_buf()))?;
    let dest = parent.join(new_name);
    if dest == src {
        return Ok(dest);
    }
    if fs::symlink_metadata(&dest).await.is_ok() {
        return Err(FileOpError::DestExists(dest));
    }
    fs::rename(src, &dest).await?;
    info!(from = %src.display(), to = %dest.display(), "rename");
    Ok(dest)
}

/// Create a new directory inside `parent`. Picks a unique name if `name` exists
/// (suffix " (1)", " (2)", …). Returns the absolute path of the created dir.
pub async fn create_directory(parent: &Path, name: &str) -> Result<PathBuf, FileOpError> {
    validate_name(name)?;
    let dest = unique_destination(parent, name);
    fs::create_dir(&dest).await?;
    info!(parent = %parent.display(), name = %name, dest = %dest.display(), "mkdir");
    Ok(dest)
}

/// Set the Unix permission bits (low 12 bits: rwx + setuid/setgid/sticky) of a
/// file or directory. Other bits of the mode are preserved.
pub fn set_permissions(path: &Path, mode: u32) -> Result<(), FileOpError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path)?;
    let mut perms = meta.permissions();
    // Keep the file-type / high bits; replace only the low 12 permission bits.
    let new_mode = (perms.mode() & !0o7777) | (mode & 0o7777);
    perms.set_mode(new_mode);
    std::fs::set_permissions(path, perms)?;
    info!(path = %path.display(), mode = format!("{:o}", mode & 0o7777), "chmod");
    Ok(())
}

/// Recursive size + file/folder count for a directory, with cancellation
/// and live progress updates.
///
/// - Symlinks are not followed.
/// - Walks in parallel using jwalk (which sits on top of rayon).
/// - Permission-denied / I/O errors on individual entries are logged and
///   skipped; only the partial counts are returned.
/// - `progress` is invoked at most ~every 120 ms with the running totals.
///
/// The root directory itself is not counted as a folder; only its descendants.
pub fn deep_count(root: &Path, cancel: &CancelFlag, progress: impl Fn(DeepCountStats) + Send + Sync) -> DeepCountStats {
    let mut stats = DeepCountStats::default();
    let mut last_emit = Instant::now();
    let emit_every = Duration::from_millis(120);

    // Skip the pseudo-filesystems mounted under `/` when summing from above
    // them: their files report bogus/huge apparent sizes (e.g. /proc/kcore is
    // ~128 TB) that would wreck a folder total. Pruning at "/" leaves them
    // intact if the user navigates straight into /proc and asks for its size.
    let walk = jwalk::WalkDir::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(|_, dir, (), children| {
            if dir == Path::new("/") {
                children.retain(|c| {
                    c.as_ref()
                        .map_or(true, |e| !matches!(e.file_name.to_str(), Some("proc" | "sys")))
                });
            }
        });

    for entry in walk {
        if cancel.is_cancelled() {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                debug!(?err, "deep-count skipped entry");
                continue;
            }
        };
        // Don't double-count the root itself.
        if entry.depth() == 0 {
            continue;
        }
        let ft = entry.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            stats.folders += 1;
        } else if ft.is_file() {
            stats.files += 1;
            match entry.metadata() {
                Ok(m) => stats.bytes += m.len(),
                Err(err) => debug!(?err, path = %entry.path().display(), "metadata read failed"),
            }
        }
        if last_emit.elapsed() >= emit_every {
            progress(stats);
            last_emit = Instant::now();
        }
    }
    stats
}

/// Produce a unique destination filename inside `dest_dir` for `src_name`.
/// Strategy: `name.ext`, `name (1).ext`, `name (2).ext`, ...
pub fn unique_destination(dest_dir: &Path, src_name: &str) -> PathBuf {
    let direct = dest_dir.join(src_name);
    if !path_occupied(&direct) {
        return direct;
    }

    let (stem, ext) = split_name(src_name);
    for n in 1..1_000_000 {
        let candidate = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        let p = dest_dir.join(candidate);
        if !path_occupied(&p) {
            return p;
        }
    }
    // Pathological fallback — caller will see the dest exists and bail.
    direct
}

/// Is something already at `p`? Uses `symlink_metadata` (not `Path::exists`)
/// so a *dangling* symlink also counts as occupied — otherwise we'd pick its
/// name and a later `File::create`/`rename` would silently write *through* the
/// link to wherever it points (possibly outside `dest_dir`).
fn path_occupied(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

#[expect(
    clippy::string_slice,
    reason = "`idx` comes from `rfind('.')`, so it is a valid char boundary and `idx + 1 <= len`"
)]
fn split_name(name: &str) -> (&str, &str) {
    if let Some(idx) = name.rfind('.')
        && idx > 0
    {
        return (&name[..idx], &name[idx + 1..]);
    }
    (name, "")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("fm-test-{}", uuid_like()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{n:x}")
    }

    #[test]
    fn validate_name_rejects_bad() {
        assert!(validate_name("").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        validate_name("hello.txt").unwrap();
    }

    #[test]
    fn split_name_handles_dotfile() {
        assert_eq!(split_name("file.txt"), ("file", "txt"));
        assert_eq!(split_name("nofile"), ("nofile", ""));
        assert_eq!(split_name(".hidden"), (".hidden", ""));
        assert_eq!(split_name("a.b.c"), ("a.b", "c"));
    }

    #[test]
    fn unique_destination_skips_existing() {
        let dir = tmp();
        fs::write(dir.join("hello.txt"), b"x").unwrap();
        let d = unique_destination(&dir, "hello.txt");
        assert_eq!(d.file_name().unwrap(), "hello (1).txt");

        fs::write(dir.join("hello (1).txt"), b"x").unwrap();
        let d = unique_destination(&dir, "hello.txt");
        assert_eq!(d.file_name().unwrap(), "hello (2).txt");

        let d = unique_destination(&dir, "fresh.txt");
        assert_eq!(d.file_name().unwrap(), "fresh.txt");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_permanently_works() {
        let dir = tmp();
        let target = dir.join("doomed.txt");
        fs::write(&target, b"bye").unwrap();
        let n = delete_permanently([&target]).await.unwrap();
        assert_eq!(n, 1);
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_directory_picks_unique_name() {
        let dir = tmp();
        let a = create_directory(&dir, "newfolder").await.unwrap();
        assert!(a.exists() && a.is_dir());
        assert_eq!(a.file_name().unwrap(), "newfolder");

        let b = create_directory(&dir, "newfolder").await.unwrap();
        assert!(b.exists());
        assert_eq!(b.file_name().unwrap(), "newfolder (1)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rename_in_place_works() {
        let dir = tmp();
        let a = dir.join("a.txt");
        fs::write(&a, b"x").unwrap();
        let b = rename_in_place(&a, "b.txt").await.unwrap();
        assert!(b.exists() && !a.exists());
        assert_eq!(b.file_name().unwrap(), "b.txt");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rename_rejects_invalid() {
        let dir = tmp();
        let a = dir.join("a.txt");
        fs::write(&a, b"x").unwrap();
        let err = rename_in_place(&a, "..").await.unwrap_err();
        assert!(matches!(err, FileOpError::InvalidName(_)));
        let err = rename_in_place(&a, "x/y").await.unwrap_err();
        assert!(matches!(err, FileOpError::InvalidName(_)));
        let _ = fs::remove_dir_all(&dir);
    }
}
