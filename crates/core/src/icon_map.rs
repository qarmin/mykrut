use std::path::Path;

use crate::types::{FileEntry, FileType};

/// Resolve a logical icon name (one of our embedded SVGs) for a file entry.
///
/// Priority: special folders (XDG user-dirs) > directory > MIME group > generic.
pub fn icon_for_entry(entry: &FileEntry) -> &'static str {
    if entry.file_type == FileType::Directory {
        if let Some(name) = icon_for_xdg_user_dir(&entry.path) {
            return name;
        }
        return "folder";
    }

    if let Some(mime) = entry.mime.as_deref() {
        return icon_for_mime(mime);
    }

    "file-generic"
}

pub fn icon_for_mime(mime: &str) -> &'static str {
    match mime {
        m if m.starts_with("image/") => "image",
        m if m.starts_with("video/") => "video",
        m if m.starts_with("audio/") => "audio",
        "application/pdf" => "pdf",
        m if m.starts_with("text/") => "text",
        "application/zip"
        | "application/x-tar"
        | "application/gzip"
        | "application/x-bzip2"
        | "application/x-xz"
        | "application/x-7z-compressed"
        | "application/vnd.rar"
        | "application/x-rar" => "archive",
        "application/x-executable" | "application/x-sharedlib" => "executable",
        "inode/directory" => "folder",
        _ => "file-generic",
    }
}

/// Map well-known XDG user directories to their dedicated folder icons.
pub fn icon_for_xdg_user_dir(path: &Path) -> Option<&'static str> {
    let home = dirs::home_dir()?;
    if path == home {
        return Some("folder-home");
    }

    let checks: &[(Option<std::path::PathBuf>, &'static str)] = &[
        (dirs::desktop_dir(), "folder-desktop"),
        (dirs::document_dir(), "folder-documents"),
        (dirs::download_dir(), "folder-downloads"),
        (dirs::picture_dir(), "folder-pictures"),
        (dirs::audio_dir(), "folder-music"),
        (dirs::video_dir(), "folder-videos"),
        (dirs::template_dir(), "folder-templates"),
        (dirs::public_dir(), "folder-public"),
    ];

    for (candidate, icon) in checks {
        if let Some(p) = candidate
            && path == p.as_path()
        {
            return Some(icon);
        }
    }
    None
}
