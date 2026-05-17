//! Crash-safe file writes for small config files.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

/// Write `contents` to `path` atomically: stream into a sibling temp file,
/// `fsync` it, then `rename` over the target. A crash (or kill, or full disk)
/// mid-write leaves either the previous file intact or the complete new one —
/// never a half-written, unparseable config.
///
/// The temp file is a sibling of `path` so the final `rename` stays on one
/// filesystem (where it is atomic). The temp name embeds the PID so two
/// instances saving concurrently don't clobber each other's temp file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "config path has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "config path has no file name"))?;

    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = parent.join(tmp_name);

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_contents() {
        let dir = std::env::temp_dir().join(format!("fm-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("cfg.toml");

        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");

        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");

        // No leftover temp files in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file was not cleaned up");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
