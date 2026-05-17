use std::path::{Path, PathBuf};
use std::sync::Arc;

use mykrut_core::Location;
use slint::{ComponentHandle, Model};
use tokio::runtime::Runtime;
use tracing::{error, info, info_span, warn};

use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{BulkRenameRow, Callabler, DialogState, MainWindow};

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    wire_open(app, state.clone());
    wire_trash(app, rt, state.clone(), watcher.clone());
    wire_empty_trash(app, rt, state.clone(), watcher.clone());
    wire_delete(app, rt, state.clone(), watcher.clone());
    wire_rename(app, rt, state.clone(), watcher.clone());
    wire_bulk_rename(app, rt, state.clone(), watcher.clone());
    wire_new_folder(app, rt, state.clone(), watcher.clone());
    wire_new_file(app, rt, state.clone(), watcher.clone());
    wire_restore(app, rt, state, watcher);
}

fn wire_bulk_rename(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_bulk_rename_preview(move |pattern| {
            let app = weak.upgrade().expect("MainWindow alive in bulk-preview");
            publish_bulk_rename_preview(&app, &state, &pattern);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_bulk_rename_apply(move |pattern| {
            let _app = weak.upgrade().expect("MainWindow alive in bulk-apply");
            let pattern = pattern.to_string();
            let originals_with_paths: Vec<(PathBuf, String)> = {
                let s = state.borrow();
                let mut v: Vec<usize> = s.selected.iter().copied().collect();
                v.sort_unstable();
                v.into_iter()
                    .filter_map(|i| s.entries.get(i))
                    .map(|e| (e.path.clone(), e.display_name.clone()))
                    .collect()
            };
            if originals_with_paths.is_empty() {
                return;
            }
            let originals: Vec<String> = originals_with_paths.iter().map(|(_, n)| n.clone()).collect();
            let new_names = mykrut_core::bulk_rename::render_batch(&pattern, &originals);

            let span = info_span!("bulk_rename", count = originals.len(), pattern = %pattern);
            let _g = span.enter();
            info!("begin");

            let weak = weak.clone();
            let state_clone = state.clone();
            let watcher_clone = watcher.clone();
            let rt_inner = rt.clone();

            let pairs: Vec<(PathBuf, String)> = originals_with_paths
                .into_iter()
                .zip(new_names)
                .map(|((src, _), new)| (src, new))
                .collect();

            let _ = slint::spawn_local(async move {
                let res = rt_inner
                    .spawn(async move {
                        let mut ok = 0u32;
                        let mut fail = 0u32;
                        for (src, new_name) in pairs {
                            match mykrut_core::rename_in_place(&src, &new_name).await {
                                Ok(_) => ok += 1,
                                Err(err) => {
                                    tracing::warn!(?err, path = %src.display(), new = %new_name, "rename failed");
                                    fail += 1;
                                }
                            }
                        }
                        (ok, fail)
                    })
                    .await
                    .expect("bulk rename task panicked");
                info!(ok = res.0, fail = res.1, "bulk rename done");
                if let Some(app) = weak.upgrade() {
                    refresh_view(&app, &rt_inner, &state_clone, &watcher_clone);
                }
            });
        });
    }
}

/// Re-compute pattern → new-name mappings and push them to the bulk-rename model.
/// Marks conflicts: duplicate within the new set, empty/invalid name, or
/// destination already exists on disk (and is not one of the inputs we're
/// about to rename away).
fn publish_bulk_rename_preview(app: &MainWindow, state: &AppStateRc, pattern: &str) {
    use std::collections::{HashMap, HashSet};

    use slint::{ModelRc, VecModel};

    let (originals, parent) = {
        let s = state.borrow();
        let mut idxs: Vec<usize> = s.selected.iter().copied().collect();
        idxs.sort_unstable();
        let entries: Vec<(PathBuf, String)> = idxs
            .into_iter()
            .filter_map(|i| s.entries.get(i))
            .map(|e| (e.path.clone(), e.display_name.clone()))
            .collect();
        let parent = entries.first().and_then(|(p, _)| p.parent().map(Path::to_path_buf));
        (entries, parent)
    };

    let names: Vec<String> = originals.iter().map(|(_, n)| n.clone()).collect();
    let new_names = mykrut_core::bulk_rename::render_batch(pattern, &names);

    // Build conflict set:
    //   1. Empty / invalid (validate_name fails)
    //   2. Duplicate within the new batch
    //   3. Hits an existing file in `parent` that's NOT one of the inputs
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let inputs: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut conflicts_count = 0i32;

    let mut rows: Vec<BulkRenameRow> = Vec::with_capacity(originals.len());
    for (i, (orig, new)) in names.iter().zip(new_names.iter()).enumerate() {
        let mut conflict = false;
        if mykrut_core::validate_name(new).is_err() {
            conflict = true;
        } else if let Some(prev) = seen.insert(new.as_str(), i) {
            conflict = true;
            // Retroactively mark the previously-seen row too.
            if let Some(prev_row) = rows.get_mut(prev)
                && !prev_row.conflict
            {
                prev_row.conflict = true;
                conflicts_count += 1;
            }
        } else if let Some(par) = parent.as_ref() {
            let dest = par.join(new);
            if dest.exists() && !inputs.contains(new.as_str()) {
                conflict = true;
            }
        }

        if conflict {
            conflicts_count += 1;
        }
        rows.push(BulkRenameRow {
            original: orig.clone().into(),
            new_name: new.clone().into(),
            conflict,
        });
    }

    let model = std::rc::Rc::new(VecModel::from(rows));
    app.set_bulk_rename_rows(ModelRc::from(model));
    app.global::<DialogState>().set_bulk_rename_conflicts(conflicts_count);
}

fn wire_restore(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    let weak = app.as_weak();
    let rt = rt.clone();
    app.global::<Callabler>().on_restore_from_trash(move || {
        let app = weak.upgrade().expect("MainWindow alive in restore-from-trash");
        let paths = selected_paths(&app, &state);
        if paths.is_empty() {
            return;
        }
        let span = info_span!("restore", count = paths.len());
        let _g = span.enter();
        info!("begin");

        let weak = weak.clone();
        let state_clone = state.clone();
        let watcher_clone = watcher.clone();
        let rt_inner = rt.clone();

        let _ = slint::spawn_local(async move {
            let res = rt_inner
                .spawn(async move {
                    let mut restored = 0u32;
                    let mut failed = 0u32;
                    for p in &paths {
                        match mykrut_core::trash_io::restore(p) {
                            Ok(_) => restored += 1,
                            Err(err) => {
                                tracing::warn!(?err, path = %p.display(), "restore failed");
                                failed += 1;
                            }
                        }
                    }
                    (restored, failed)
                })
                .await
                .expect("restore task panicked");
            info!(restored = res.0, failed = res.1, "restore done");
            if let Some(app) = weak.upgrade() {
                refresh_view(&app, &rt_inner, &state_clone, &watcher_clone);
            }
        });
    });
}

fn wire_new_file(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    // Open the prompt with a unique name suggestion in the current dir.
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_create_empty_file(move || {
            let app = weak.upgrade().expect("MainWindow alive in create-empty-file");
            let Some(parent) = current_dir(&state) else {
                return;
            };
            let initial = mykrut_core::unique_destination(&parent, "untitled.txt")
                .file_name()
                .map_or_else(|| "untitled.txt".to_string(), |n| n.to_string_lossy().into_owned());
            let ds = app.global::<DialogState>();
            ds.set_new_file_initial(initial.into());
            ds.set_new_file_open(true);
        });
    }

    // Actually create the file when the user confirms.
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_new_file_confirmed(move |name| {
            let _app = weak.upgrade().expect("MainWindow alive in new-file-confirmed");
            let name = name.to_string();
            let Some(parent) = current_dir(&state) else {
                return;
            };
            let span = info_span!("create_empty_file", parent = %parent.display(), name = %name);
            let _g = span.enter();
            info!("begin");

            let weak = weak.clone();
            let state_clone = state.clone();
            let watcher_clone = watcher.clone();
            let rt_inner = rt.clone();

            let _ = slint::spawn_local(async move {
                let res = rt_inner
                    .spawn(async move {
                        if let Err(e) = mykrut_core::validate_name(&name) {
                            return Err(std::io::Error::other(e.to_string()));
                        }
                        // Re-resolve uniqueness in case anything raced between the
                        // popup opening and the user clicking Create.
                        let path = mykrut_core::unique_destination(&parent, &name);
                        tokio::fs::File::create(&path).await.map(|_| path)
                    })
                    .await
                    .expect("create_empty_file task panicked");
                match res {
                    Ok(p) => {
                        info!(path = %p.display(), "file created");
                        crate::glue::undo::record(
                            &state_clone,
                            crate::state::UndoOp::Create { path: p, is_dir: false },
                        );
                    }
                    Err(err) => error!(?err, "create_empty_file failed"),
                }
                if let Some(app) = weak.upgrade() {
                    refresh_view(&app, &rt_inner, &state_clone, &watcher_clone);
                }
            });
        });
    }
}

fn wire_new_folder(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        app.global::<Callabler>().on_request_new_folder(move || {
            let app = weak.upgrade().expect("MainWindow alive in request-new-folder");
            app.global::<DialogState>().set_new_folder_open(true);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_new_folder_confirmed(move |name| {
            let _app = weak.upgrade().expect("MainWindow alive in new-folder-confirmed");
            let name = name.to_string();
            let Some(parent) = current_dir(&state) else {
                return;
            };
            let span = info_span!("new_folder", parent = %parent.display(), name = %name);
            let _g = span.enter();
            info!("begin");

            let weak = weak.clone();
            let state_clone = state.clone();
            let watcher_clone = watcher.clone();
            let rt_inner = rt.clone();

            let _ = slint::spawn_local(async move {
                let res = rt_inner
                    .spawn(async move { mykrut_core::create_directory(&parent, &name).await })
                    .await
                    .expect("create_directory task panicked");
                match res {
                    Ok(p) => {
                        info!(path = %p.display(), "created");
                        crate::glue::undo::record(&state_clone, crate::state::UndoOp::Create { path: p, is_dir: true });
                    }
                    Err(err) => error!(?err, "create_directory failed"),
                }
                if let Some(app) = weak.upgrade() {
                    refresh_view(&app, &rt_inner, &state_clone, &watcher_clone);
                }
            });
        });
    }
}

fn selected_paths(app: &MainWindow, state: &AppStateRc) -> Vec<PathBuf> {
    if crate::glue::search_focused(app, state) {
        // Search mode: pull paths from the parallel `search_hit_paths` vector,
        // honouring the selection flags on the search-rows model (state.selected
        // doesn't reflect the search-mode selection).
        let model = app.get_search_rows();
        let n = model.row_count();
        let s = state.borrow();
        return (0..n)
            .filter_map(|i| {
                let row = model.row_data(i)?;
                if !row.selected {
                    return None;
                }
                s.search_hit_paths.get(i).cloned()
            })
            .collect();
    }
    let s = state.borrow();
    let mut v: Vec<_> = s.selected.iter().copied().collect();
    v.sort_unstable();
    v.into_iter()
        .filter_map(|i| s.entries.get(i).map(|e| e.path.clone()))
        .collect()
}

fn current_dir(state: &AppStateRc) -> Option<PathBuf> {
    match state.borrow().current.clone()? {
        Location::Local(p) => Some(p),
        Location::Trash => None,
    }
}

/// Threshold above which "open all" prompts the user for confirmation —
/// opening 30 PDFs from a stray Ctrl+A is rarely what you meant.
const BULK_OPEN_CONFIRM_AT: usize = 5;

fn wire_open(app: &MainWindow, state: AppStateRc) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_open_default_app(move || {
            let app = weak.upgrade().expect("MainWindow alive in open-default-app");
            let paths = selected_paths(&app, &state);
            if paths.is_empty() {
                return;
            }
            if paths.len() >= BULK_OPEN_CONFIRM_AT {
                let ds = app.global::<DialogState>();
                ds.set_bulk_open_count(paths.len() as i32);
                ds.set_bulk_open_confirm_open(true);
                return;
            }
            open_all(&paths);
        });
    }
    {
        let weak = app.as_weak();
        app.global::<Callabler>().on_bulk_open_confirmed(move || {
            let app = weak.upgrade().expect("MainWindow alive in bulk-open-confirmed");
            let paths = selected_paths(&app, &state);
            open_all(&paths);
        });
    }
}

fn open_all(paths: &[PathBuf]) {
    for p in paths {
        crate::glue::spawn::open_file(p);
    }
}

fn wire_trash(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, _watcher: WatcherHandle) {
    let weak = app.as_weak();
    let rt = rt.clone();
    app.global::<Callabler>().on_move_to_trash(move || {
        let app = weak.upgrade().expect("MainWindow alive in move-to-trash");
        let paths = selected_paths(&app, &state);
        if paths.is_empty() {
            warn!("trash invoked with empty selection");
            return;
        }
        let span = info_span!("trash", count = paths.len());
        let _g = span.enter();
        info!("begin");

        let weak = weak.clone();
        let state_clone = state.clone();
        let rt_inner = rt.clone();
        let paths_for_after = paths.clone();

        let _ = slint::spawn_local(async move {
            let res = rt_inner
                .spawn(async move {
                    tokio::task::spawn_blocking(move || mykrut_core::move_to_trash(&paths))
                        .await
                        .expect("blocking task panicked")
                })
                .await
                .expect("tokio task panicked");

            match res {
                Ok(n) => info!(count = n, "trashed"),
                Err(err) => error!(?err, "trash failed"),
            }

            if let Some(app) = weak.upgrade() {
                // Record for undo (Ctrl+Z restores from trash).
                crate::glue::undo::record(
                    &state_clone,
                    crate::state::UndoOp::Trash {
                        originals: paths_for_after.clone(),
                    },
                );
                // Drop just the trashed rows in place (no full re-list → no flicker).
                crate::glue::navigation::remove_paths_from_view(&app, &state_clone, &paths_for_after);
                crate::glue::navigation::refresh_inactive_pane(&app, &rt_inner, &state_clone);
            }
        });
    });
}

fn wire_empty_trash(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        app.global::<Callabler>().on_empty_trash(move || {
            let app = weak.upgrade().expect("MainWindow alive in empty-trash");
            // Destructive — confirm first.
            app.global::<DialogState>().set_empty_trash_confirm_open(true);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_empty_trash_confirmed(move || {
            let _app = weak.upgrade().expect("MainWindow alive in empty-trash-confirmed");
            let span = info_span!("empty_trash");
            let _g = span.enter();
            info!("begin");

            let weak = weak.clone();
            let state_clone = state.clone();
            let watcher_clone = watcher.clone();
            let rt_inner = rt.clone();

            let _ = slint::spawn_local(async move {
                let res = rt_inner
                    .spawn(async move {
                        tokio::task::spawn_blocking(mykrut_core::trash_io::empty_trash)
                            .await
                            .expect("blocking task panicked")
                    })
                    .await
                    .expect("tokio task panicked");
                match res {
                    Ok(n) => info!(count = n, "trash emptied"),
                    Err(err) => error!(?err, "empty trash failed"),
                }
                if let Some(app) = weak.upgrade() {
                    refresh_view(&app, &rt_inner, &state_clone, &watcher_clone);
                }
            });
        });
    }
}

fn wire_delete(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, _watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_request_delete_permanently(move || {
            let app = weak.upgrade().expect("MainWindow alive in request-delete");
            let count = state.borrow().selected.len();
            if count == 0 {
                return;
            }
            let ds = app.global::<DialogState>();
            ds.set_delete_target_count(count as i32);
            ds.set_delete_confirm_open(true);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_delete_confirmed(move || {
            let app = weak.upgrade().expect("MainWindow alive in delete-confirmed");
            let paths = selected_paths(&app, &state);
            if paths.is_empty() {
                return;
            }
            let span = info_span!("delete", count = paths.len());
            let _g = span.enter();
            info!("begin");

            let weak = weak.clone();
            let state_clone = state.clone();
            let rt_inner = rt.clone();
            let paths_for_after = paths.clone();

            let _ = slint::spawn_local(async move {
                let res = rt_inner
                    .spawn(async move { mykrut_core::delete_permanently(paths).await })
                    .await
                    .expect("tokio task panicked");
                match res {
                    Ok(n) => info!(count = n, "deleted"),
                    Err(err) => error!(?err, "delete failed"),
                }
                if let Some(app) = weak.upgrade() {
                    // Drop just the deleted rows in place (no flicker).
                    crate::glue::navigation::remove_paths_from_view(&app, &state_clone, &paths_for_after);
                    crate::glue::navigation::refresh_inactive_pane(&app, &rt_inner, &state_clone);
                }
            });
        });
    }
}

fn wire_rename(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_request_rename(move || {
            let app = weak.upgrade().expect("MainWindow alive in request-rename");
            let count = state.borrow().selected.len();
            if count == 0 {
                return;
            }
            if count >= 2 {
                // Open bulk-rename dialog and seed the preview model.
                publish_bulk_rename_preview(&app, &state, &app.global::<DialogState>().get_bulk_rename_pattern());
                app.global::<DialogState>().set_bulk_rename_open(true);
                return;
            }
            // Single-file path → existing inline-rename dialog.
            let s = state.borrow();
            let Some(&idx) = s.selected.iter().next() else {
                return;
            };
            let Some(entry) = s.entries.get(idx) else {
                return;
            };
            let initial = entry.display_name.clone();
            drop(s);
            let ds = app.global::<DialogState>();
            ds.set_rename_initial(initial.into());
            ds.set_rename_open(true);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_rename_confirmed(move |new_name| {
            let _app = weak.upgrade().expect("MainWindow alive in rename-confirmed");
            let new_name = new_name.to_string();
            let path = {
                let s = state.borrow();
                s.selected
                    .iter()
                    .next()
                    .and_then(|&i| s.entries.get(i).map(|e| e.path.clone()))
            };
            let Some(src) = path else {
                return;
            };
            let span = info_span!("rename", path = %src.display(), new_name = %new_name);
            let _g = span.enter();
            info!("begin");

            let weak = weak.clone();
            let state_clone = state.clone();
            let watcher_clone = watcher.clone();
            let rt_inner = rt.clone();
            let src_for_undo = src.clone();

            let _ = slint::spawn_local(async move {
                let res = rt_inner
                    .spawn(async move { mykrut_core::rename_in_place(&src, &new_name).await })
                    .await
                    .expect("tokio task panicked");
                match res {
                    Ok(new) => {
                        info!(new = %new.display(), "renamed");
                        crate::glue::undo::record(
                            &state_clone,
                            crate::state::UndoOp::Rename {
                                from: src_for_undo,
                                to: new,
                            },
                        );
                    }
                    Err(err) => error!(?err, "rename failed"),
                }
                if let Some(app) = weak.upgrade() {
                    refresh_view(&app, &rt_inner, &state_clone, &watcher_clone);
                }
            });
        });
    }
}

/// Re-list the current directory after a mutating op. Caller is on the UI thread.
fn refresh_view(app: &MainWindow, rt: &Arc<Runtime>, state: &AppStateRc, watcher: &WatcherHandle) {
    let Some(cur) = current_dir(state) else {
        // Even when the active pane isn't a local folder (e.g. Trash), the
        // inactive split pane might still need refreshing.
        crate::glue::navigation::refresh_inactive_pane(app, rt, state);
        return;
    };
    crate::glue::navigation::navigate_internal_refresh(app, rt, state.clone(), watcher.clone(), Location::Local(cur));
    // The op may also have touched whatever the inactive pane is showing
    // (notably the source folder of a cut+paste move).
    crate::glue::navigation::refresh_inactive_pane(app, rt, state);
}
