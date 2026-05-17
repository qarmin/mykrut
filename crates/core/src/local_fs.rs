use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use tokio::fs;
use tracing::warn;

use crate::types::{FileEntry, FileType, Location, Permissions};

pub struct LocalFs;

impl LocalFs {
    pub async fn list(loc: &Location) -> Result<Vec<FileEntry>> {
        let path = loc
            .as_path()
            .with_context(|| format!("Location {} is not local", loc.display()))?;

        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(path)
            .await
            .with_context(|| format!("read_dir({})", path.display()))?;

        while let Some(dent) = read_dir.next_entry().await? {
            match Self::entry_from_dirent(&dent).await {
                Ok(e) => entries.push(e),
                Err(err) => warn!(?err, path = %dent.path().display(), "skip entry"),
            }
        }

        Ok(entries)
    }

    async fn entry_from_dirent(dent: &tokio::fs::DirEntry) -> Result<FileEntry> {
        let path = dent.path();
        let display_name = dent.file_name().to_string_lossy().into_owned();

        // Symlink-aware metadata: stat first (follow symlink), fall back to lstat.
        let metadata = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => fs::symlink_metadata(&path).await?,
        };

        let symlink_meta = fs::symlink_metadata(&path).await.ok();
        let is_symlink = symlink_meta.as_ref().is_some_and(|m| m.file_type().is_symlink());

        let file_type = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_file() {
            FileType::Regular
        } else if is_symlink {
            FileType::Symlink
        } else {
            FileType::Special
        };

        let permissions = Self::extract_permissions(&metadata);
        let size = metadata.len();
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

        let mime = mime_for(&path, file_type);

        Ok(FileEntry {
            path,
            display_name: display_name.clone(),
            file_type,
            mime,
            size,
            mtime,
            permissions,
            is_hidden: display_name.starts_with('.'),
            is_symlink,
        })
    }

    fn extract_permissions(meta: &std::fs::Metadata) -> Permissions {
        let mode = meta.permissions().mode();
        let user = (mode >> 6) & 0o7;
        Permissions {
            mode,
            readable: user & 0o4 != 0,
            writable: user & 0o2 != 0,
            executable: user & 0o1 != 0,
        }
    }
}

fn mime_for(path: &Path, ft: FileType) -> Option<String> {
    if ft == FileType::Directory {
        return Some("inode/directory".to_string());
    }
    if let Some(m) = mime_guess::from_path(path).first() {
        return Some(m.essence_str().to_string());
    }
    // No (or unknown) extension: sniff the content's magic bytes so e.g. a file
    // named "C" that's really a zip is recognised as an archive rather than an
    // unknown blob. Only regular files; reads just the header.
    if ft == FileType::Regular
        && let Ok(Some(kind)) = infer::get_from_path(path)
    {
        return Some(kind.mime_type().to_string());
    }
    None
}
