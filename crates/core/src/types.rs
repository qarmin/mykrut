use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Special,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Permissions {
    pub mode: u32,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

#[derive(Debug, Clone)]
pub enum Location {
    Local(PathBuf),
    Trash,
}

impl Location {
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::Local(p) => Some(p),
            Self::Trash => None,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Local(p) => p.display().to_string(),
            Self::Trash => "trash:///".to_string(),
        }
    }

    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Local(p) => p.parent().map(|p| Self::Local(p.to_path_buf())),
            Self::Trash => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub file_type: FileType,
    pub mime: Option<String>,
    pub size: u64,
    pub mtime: SystemTime,
    pub permissions: Permissions,
    pub is_hidden: bool,
    pub is_symlink: bool,
}

impl FileEntry {
    pub fn name(&self) -> &str {
        &self.display_name
    }

    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Directory
    }
}
