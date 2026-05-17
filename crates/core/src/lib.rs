//! Core types and filesystem operations for the file manager.
//!
//! No UI dependencies — this crate is pure logic, async I/O, and data model.

pub mod bulk_rename;
pub mod copy_progress;
pub mod disk_space;
pub mod file_ops;
pub mod icon_map;
pub mod local_fs;
pub mod media_meta;
pub mod thumbnails;
pub mod trash_io;
pub mod types;
pub mod uid_map;

pub use copy_progress::{
    CancelFlag, Conflict, CopyError, CopyOutcome, Op, PauseFlag, Progress, prescan, run as run_copy,
    run_with as run_copy_with,
};
pub use file_ops::{
    DeepCountStats, FileOpError, create_directory, deep_count, delete_permanently, move_to_trash, rename_in_place,
    set_permissions, unique_destination, validate_name,
};
pub use icon_map::{icon_for_entry, icon_for_mime, icon_for_xdg_user_dir};
pub use local_fs::LocalFs;
pub use media_meta::{MediaInfo, MediaKind, probe as probe_media};
pub use thumbnails::{ThumbSize, generate_image as generate_thumbnail, generate_or_fail};
pub use types::{FileEntry, FileType, Location, Permissions};
