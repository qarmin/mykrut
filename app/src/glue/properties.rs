//! Properties dialog wiring: gather metadata synchronously, kick off a
//! recursive deep-count for directories in the background, and stream the
//! result back. Cancellation fires when the user closes the dialog.

use std::cell::RefCell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use mykrut_core::{CancelFlag, DeepCountStats, deep_count};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Runtime;
use tracing::{debug, error, info, info_span};

use crate::format_util::{human_mtime, human_size};
use crate::state::AppStateRc;
use crate::{Callabler, DialogState, MainWindow, PropKv, PropertiesData, Translations};

thread_local! {
    /// Path of the file/folder the open Properties dialog describes. Used by the
    /// permissions "Apply" action. Empty for multi-selection (chmod disabled).
    static PROPS_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc) {
    // Single shared cancellation flag — re-used across opens.
    let cancel = CancelFlag::new();

    {
        let weak = app.as_weak();
        let state = state.clone();
        let rt = rt.clone();
        let cancel = cancel.clone();
        app.global::<Callabler>().on_request_properties(move || {
            let app = weak.upgrade().expect("MainWindow alive in request-properties");
            let entries: Vec<mykrut_core::FileEntry> = {
                let s = state.borrow();
                let mut idx: Vec<usize> = s.selected.iter().copied().collect();
                idx.sort_unstable();
                idx.into_iter().filter_map(|i| s.entries.get(i).cloned()).collect()
            };
            match entries.len() {
                0 => {}
                1 => open_for(&app, &rt, cancel.clone(), &entries.into_iter().next().unwrap()),
                _ => open_for_many(&app, &rt, cancel.clone(), &entries),
            }
        });
    }

    {
        let cancel = cancel.clone();
        app.global::<Callabler>().on_properties_closed(move || {
            cancel.cancel();
            debug!("properties dialog closed — cancelled any deep scan");
        });
    }

    {
        let weak = app.as_weak();
        app.global::<Callabler>().on_apply_permissions(move |mode| {
            let app = weak.upgrade().expect("MainWindow alive in apply-permissions");
            let Some(path) = PROPS_PATH.with(|p| p.borrow().clone()) else {
                return;
            };
            if mode < 0 {
                return;
            }
            match mykrut_core::set_permissions(&path, mode as u32) {
                Ok(()) => {
                    info!(path = %path.display(), mode, "permissions applied");
                    refresh_perms(&app, &path);
                }
                Err(err) => {
                    error!(?err, path = %path.display(), "set_permissions failed");
                    let ds = app.global::<DialogState>();
                    ds.set_nav_error_title("Could not change permissions".into());
                    ds.set_nav_error_message(format!("{}\n\n{err}", path.display()).into());
                    ds.set_nav_error_open(true);
                }
            }
        });
    }

    // Right-click on empty area → "Folder properties" — describes the current dir.
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_request_folder_properties(move || {
            let app = weak.upgrade().expect("MainWindow alive in folder-props");
            let Some(cur) = current_dir_state(&state) else {
                return;
            };
            let synthetic = mykrut_core::FileEntry {
                path: cur.clone(),
                display_name: cur
                    .file_name()
                    .map_or_else(|| cur.display().to_string(), |n| n.to_string_lossy().into_owned()),
                file_type: mykrut_core::FileType::Directory,
                mime: Some("inode/directory".to_string()),
                size: 0,
                mtime: std::fs::symlink_metadata(&cur)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                permissions: Default::default(),
                is_hidden: false,
                is_symlink: false,
            };
            open_for(&app, &rt, cancel.clone(), &synthetic);
        });
    }
}

fn set_media_rows(app: &MainWindow, rows: &[(String, String)]) {
    let model: Vec<PropKv> = rows
        .iter()
        .map(|(k, v)| PropKv {
            key: k.clone().into(),
            value: v.clone().into(),
        })
        .collect();
    app.set_properties_media_rows(ModelRc::from(Rc::new(VecModel::from(model))));
}

/// Re-stat `path` after a chmod and refresh the permission bits + octal display
/// in the live Properties struct (other fields untouched).
fn refresh_perms(app: &MainWindow, path: &Path) {
    let m = extended_stat(path).mode;
    let mut p = app.get_properties();
    p.mode_octal = format!("{:04o}", m & 0o7777).into();
    p.perm_or = m & 0o400 != 0;
    p.perm_ow = m & 0o200 != 0;
    p.perm_ox = m & 0o100 != 0;
    p.perm_gr = m & 0o040 != 0;
    p.perm_gw = m & 0o020 != 0;
    p.perm_gx = m & 0o010 != 0;
    p.perm_tr = m & 0o004 != 0;
    p.perm_tw = m & 0o002 != 0;
    p.perm_tx = m & 0o001 != 0;
    app.set_properties(p);
}

fn current_dir_state(state: &AppStateRc) -> Option<PathBuf> {
    match state.borrow().current.clone()? {
        mykrut_core::Location::Local(p) => Some(p),
        mykrut_core::Location::Trash => None,
    }
}

fn open_for(app: &MainWindow, rt: &Arc<Runtime>, cancel: CancelFlag, entry: &mykrut_core::FileEntry) {
    let path = entry.path.clone();
    let span = info_span!("properties", path = %path.display());
    let _g = span.enter();

    let tr = app.global::<Translations>();
    let computing = tr.get_props_computing().to_string();

    PROPS_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));

    // Initial snapshot — synchronous metadata.
    let mut snapshot = snapshot_for(entry, &computing);
    // Type-specific metadata (image dimensions/EXIF, audio tags). Cheap,
    // header-only reads, so done inline.
    let media = mykrut_core::probe_media(&entry.path, entry.mime.as_deref());
    snapshot.media_kind = media.kind.label().into();
    set_media_rows(app, &media.rows);
    app.set_properties(snapshot);
    app.global::<DialogState>().set_properties_open(true);

    if !entry.is_dir() {
        return;
    }

    // Background deep-count for directories.
    cancel.reset();
    let cancel_for_worker = cancel;
    let weak = app.as_weak();
    let rt = rt.clone();

    let progress_weak = weak.clone();
    let progress = move |stats: DeepCountStats| {
        let weak = progress_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            if !app.global::<DialogState>().get_properties_open() {
                return;
            }
            let mut p = app.get_properties();
            p.size_text = format_size_text(stats.bytes).into();
            p.file_count_text = format_count_text(stats.files, stats.folders).into();
            app.set_properties(p);
        });
    };

    let _ = slint::spawn_local(async move {
        let path = path.clone();
        let join = rt.spawn_blocking(move || deep_count(&path, &cancel_for_worker, progress));
        let stats = match join.await {
            Ok(s) => s,
            Err(err) => {
                error!(?err, "deep-count panic");
                return;
            }
        };
        let Some(app) = weak.upgrade() else { return };

        if !app.global::<DialogState>().get_properties_open() {
            return; // dialog already dismissed
        }

        info!(
            files = stats.files,
            folders = stats.folders,
            bytes = stats.bytes,
            "deep count done"
        );
        let mut p = app.get_properties();
        p.size_text = format_size_text(stats.bytes).into();
        p.file_count_text = format_count_text(stats.files, stats.folders).into();
        p.deep_scan_active = false;
        app.set_properties(p);
    });
}

fn format_size_text(bytes: u64) -> String {
    format!("{} ({bytes} bytes)", human_size(bytes))
}

fn format_count_text(files: u64, folders: u64) -> String {
    format!("{files} files in {folders} folders")
}

/// Multi-selection variant of [`open_for`]. Walks every selected entry,
/// summing sizes (deep-count for directories) with live progress.
fn open_for_many(app: &MainWindow, rt: &Arc<Runtime>, cancel: CancelFlag, entries: &[mykrut_core::FileEntry]) {
    let tr = app.global::<Translations>();
    let computing = tr.get_props_computing().to_string();
    let count = entries.len();
    let dir_count = entries.iter().filter(|e| e.is_dir()).count();

    let parent_path = entries
        .first()
        .and_then(|e| e.path.parent().map(|p| p.display().to_string()))
        .unwrap_or_default();

    // Multi-selection: no single path to chmod, and no media tab.
    PROPS_PATH.with(|p| *p.borrow_mut() = None);
    set_media_rows(app, &[]);

    let initial = PropertiesData {
        name: format!("{count} items").into(),
        path: parent_path.into(),
        kind: format!("{} folders, {} files", dir_count, count - dir_count).into(),
        is_dir: false,
        size_text: computing.clone().into(),
        file_count_text: computing.into(),
        modified: "—".into(),
        accessed: "—".into(),
        created: "—".into(),
        owner: "—".into(),
        group: "—".into(),
        mode_octal: "—".into(),
        deep_scan_active: true,
        can_edit_perms: false,
        perm_or: false,
        perm_ow: false,
        perm_ox: false,
        perm_gr: false,
        perm_gw: false,
        perm_gx: false,
        perm_tr: false,
        perm_tw: false,
        perm_tx: false,
        media_kind: String::new().into(),
    };
    app.set_properties(initial);
    app.global::<DialogState>().set_properties_open(true);

    cancel.reset();
    let cancel_for_worker = cancel;
    let weak = app.as_weak();
    let rt = rt.clone();

    let progress_weak = weak.clone();
    let progress = move |stats: DeepCountStats| {
        let weak = progress_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            if !app.global::<DialogState>().get_properties_open() {
                return;
            }
            let mut p = app.get_properties();
            p.size_text = format_size_text(stats.bytes).into();
            p.file_count_text = format_count_text(stats.files, stats.folders).into();
            app.set_properties(p);
        });
    };

    let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();

    let _ = slint::spawn_local(async move {
        let join = rt.spawn_blocking(move || aggregate_count(&paths, &cancel_for_worker, progress));
        let stats = match join.await {
            Ok(s) => s,
            Err(err) => {
                error!(?err, "multi deep-count panic");
                return;
            }
        };
        let Some(app) = weak.upgrade() else { return };
        if !app.global::<DialogState>().get_properties_open() {
            return;
        }
        info!(
            items = count,
            files = stats.files,
            folders = stats.folders,
            bytes = stats.bytes,
            "multi-selection properties done"
        );
        let mut p = app.get_properties();
        p.size_text = format_size_text(stats.bytes).into();
        p.file_count_text = format_count_text(stats.files, stats.folders).into();
        p.deep_scan_active = false;
        app.set_properties(p);
    });
}

/// Walk every path in `paths`, accumulating file/folder counts and total bytes.
/// Top-level files contribute their own size; directories are deep-counted.
fn aggregate_count(
    paths: &[PathBuf],
    cancel: &CancelFlag,
    progress: impl Fn(DeepCountStats) + Send + Sync + Clone + 'static,
) -> DeepCountStats {
    use std::sync::Mutex;
    let totals = Arc::new(Mutex::new(DeepCountStats::default()));

    for p in paths {
        if cancel.is_cancelled() {
            break;
        }
        let meta = std::fs::symlink_metadata(p);
        let is_dir = meta
            .as_ref()
            .is_ok_and(|m| m.file_type().is_dir() && !m.file_type().is_symlink());
        if is_dir {
            let totals_for_progress = totals.clone();
            let progress_for_dir = progress.clone();
            let stats = mykrut_core::deep_count(p, cancel, move |partial| {
                // Emit (already-accumulated other paths) + partial of current.
                let base = *totals_for_progress.lock().unwrap();
                progress_for_dir(DeepCountStats {
                    files: base.files + partial.files,
                    folders: base.folders + partial.folders + 1, // +1 for the dir itself
                    bytes: base.bytes + partial.bytes,
                });
            });
            let mut t = totals.lock().unwrap();
            t.files += stats.files;
            t.folders += stats.folders + 1; // the top-level dir itself
            t.bytes += stats.bytes;
        } else if let Ok(m) = meta {
            let mut t = totals.lock().unwrap();
            t.files += 1;
            t.bytes += m.len();
        }
    }
    let final_stats = *totals.lock().unwrap();
    progress(final_stats);
    final_stats
}

fn snapshot_for(entry: &mykrut_core::FileEntry, computing_label: &str) -> PropertiesData {
    let kind = if entry.is_dir() {
        "Folder".to_string()
    } else {
        entry.mime.clone().unwrap_or_else(|| "File".to_string())
    };

    let size_text = if entry.is_dir() {
        computing_label.to_string()
    } else {
        format!("{} ({} bytes)", human_size(entry.size), entry.size)
    };

    let file_count_text = if entry.is_dir() {
        computing_label.to_string()
    } else {
        String::new()
    };

    // Extra stat fields the basic FileEntry doesn't cache yet.
    let stat = extended_stat(&entry.path);
    let m = stat.mode;

    PropertiesData {
        name: entry.display_name.clone().into(),
        path: entry.path.display().to_string().into(),
        kind: kind.into(),
        is_dir: entry.is_dir(),
        size_text: size_text.into(),
        file_count_text: file_count_text.into(),
        modified: human_mtime(entry.mtime).into(),
        accessed: human_mtime(stat.atime).into(),
        created: human_mtime(stat.ctime).into(),
        owner: stat.owner.into(),
        group: stat.group.into(),
        mode_octal: format!("{:04o}", m & 0o7777).into(),
        deep_scan_active: entry.is_dir(),
        // Single, existing item → editable. The chmod itself surfaces a clear
        // error if the user doesn't own the file.
        can_edit_perms: true,
        perm_or: m & 0o400 != 0,
        perm_ow: m & 0o200 != 0,
        perm_ox: m & 0o100 != 0,
        perm_gr: m & 0o040 != 0,
        perm_gw: m & 0o020 != 0,
        perm_gx: m & 0o010 != 0,
        perm_tr: m & 0o004 != 0,
        perm_tw: m & 0o002 != 0,
        perm_tx: m & 0o001 != 0,
        media_kind: String::new().into(), // filled by the caller after probing
    }
}

struct ExtStat {
    atime: SystemTime,
    ctime: SystemTime,
    owner: String,
    group: String,
    mode: u32,
}

fn extended_stat(path: &Path) -> ExtStat {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return ExtStat {
            atime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            owner: String::new(),
            group: String::new(),
            mode: 0,
        };
    };
    ExtStat {
        atime: meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH),
        ctime: meta.created().unwrap_or(SystemTime::UNIX_EPOCH),
        owner: mykrut_core::uid_map::format_user(meta.uid()),
        group: mykrut_core::uid_map::format_group(meta.gid()),
        mode: meta.permissions().mode(),
    }
}

