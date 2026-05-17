use std::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

use mykrut_core::{FileEntry, LocalFs, Location, disk_space, icon_for_entry};
use slint::{ComponentHandle, Model, SharedString};
use tokio::runtime::Runtime;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::format_util::{human_mtime, human_size, kind_text};
use crate::glue::thumbnails::ThumbnailController;
use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{
    AppState as SlintAppState, Callabler, DialogState, FileRowData, MainWindow, SearchState, Settings, SortKey,
};

thread_local! {
    /// Set during wire_with_thumbnails; navigation reads it to fire-and-forget
    /// thumbnail batches without threading the handle through every code path.
    static THUMB_CTRL: RefCell<Option<std::sync::Arc<ThumbnailController>>> = const { RefCell::new(None) };
}

pub fn wire_with_thumbnails(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    thumb_ctrl: std::sync::Arc<ThumbnailController>,
) {
    THUMB_CTRL.with(|c| *c.borrow_mut() = Some(thumb_ctrl));
    wire(app, rt, state, watcher);
}

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    let weak = app.as_weak();

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_navigate_to(move |path: SharedString| {
            let app = weak.upgrade().expect("MainWindow alive in navigate-to");
            // ssh:// / smb:// / ftp:// … are mounted via GVFS and browsed
            // through their FUSE mountpoint; everything else is a local path.
            if crate::glue::remote::is_remote_uri(path.as_str()) {
                crate::glue::remote::open(&app, &rt, state.clone(), watcher.clone(), path.as_str());
                return;
            }
            let loc = Location::Local(std::path::PathBuf::from(path.as_str()));
            navigate_to(&app, &rt, state.clone(), watcher.clone(), loc);
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_navigate_back(move || {
            let app = weak.upgrade().expect("MainWindow alive in back");
            let target = {
                let mut s = state.borrow_mut();
                let Some(prev) = s.back_stack.pop() else {
                    return;
                };
                if let Some(cur) = s.current.clone() {
                    s.forward_stack.push(cur);
                }
                prev
            };
            close_search_if_active(&app);
            navigate_internal(&app, &rt, state.clone(), watcher.clone(), target, true);
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_navigate_forward(move || {
            let app = weak.upgrade().expect("MainWindow alive in forward");
            let target = {
                let mut s = state.borrow_mut();
                let Some(next) = s.forward_stack.pop() else {
                    return;
                };
                if let Some(cur) = s.current.clone() {
                    s.back_stack.push(cur);
                }
                next
            };
            close_search_if_active(&app);
            navigate_internal(&app, &rt, state.clone(), watcher.clone(), target, true);
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_navigate_up(move || {
            let app = weak.upgrade().expect("MainWindow alive in up");
            let parent = state.borrow().current.as_ref().and_then(Location::parent);
            if let Some(p) = parent {
                navigate_to(&app, &rt, state.clone(), watcher.clone(), p);
            }
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_navigate_home(move || {
            let app = weak.upgrade().expect("MainWindow alive in home");
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
            navigate_to(&app, &rt, state.clone(), watcher.clone(), Location::Local(home));
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_refresh(move || {
            let app = weak.upgrade().expect("MainWindow alive in refresh");
            if let Some(cur) = state.borrow().current.clone() {
                navigate_internal(&app, &rt, state.clone(), watcher.clone(), cur, true);
            }
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_row_activated(move |idx| {
            let app = weak.upgrade().expect("MainWindow alive in row-activated");
            // Search mode uses a separate model + parallel `search_hit_paths`
            // vector; resolving the row through `state.entries` would land on
            // a random file from the underlying folder.
            let (path, is_dir) = if crate::glue::search_focused(&app, &state) {
                let s = state.borrow();
                let Some(p) = s.search_hit_paths.get(idx as usize).cloned() else {
                    return;
                };
                // is_dir comes from the search model so we don't re-stat.
                let is_dir = app.get_search_rows().row_data(idx as usize).is_some_and(|r| r.is_dir);
                (p, is_dir)
            } else {
                let entry = state.borrow().entries.get(idx as usize).cloned();
                let Some(entry) = entry else { return };
                let is_dir = entry.is_dir();
                (entry.path, is_dir)
            };
            if is_dir {
                let target = Location::Local(path);
                navigate_to(&app, &rt, state.clone(), watcher.clone(), target);
            } else {
                let span = info_span!("open_via_dblclick", path = %path.display());
                let _g = span.enter();
                crate::glue::spawn::open_file(&path);
            }
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_activate_selection(move || {
            let app = weak.upgrade().expect("MainWindow alive in activate-selection");
            // In search mode the underlying entries[] doesn't index against
            // search-rows; route through the model + search_hit_paths the
            // same way row-activated does.
            if crate::glue::search_focused(&app, &state) {
                let model = app.get_search_rows();
                let n = model.row_count();
                let selected: Vec<usize> = (0..n)
                    .filter(|&i| model.row_data(i).is_some_and(|r| r.selected))
                    .collect();
                if selected.is_empty() {
                    return;
                }
                if selected.len() == 1 {
                    let idx = selected[0];
                    let s = state.borrow();
                    let Some(path) = s.search_hit_paths.get(idx).cloned() else {
                        return;
                    };
                    let is_dir = model.row_data(idx).is_some_and(|r| r.is_dir);
                    drop(s);
                    if is_dir {
                        navigate_to(&app, &rt, state.clone(), watcher.clone(), Location::Local(path));
                    } else {
                        crate::glue::spawn::open_file(&path);
                    }
                } else {
                    // Multi → reuse the bulk-open dispatch logic via the
                    // existing callback. Search paths are already wired
                    // through selected_paths in file_ops.
                    app.global::<Callabler>().invoke_open_default_app();
                }
                return;
            }
            // Normal mode.
            let entries: Vec<mykrut_core::FileEntry> = {
                let s = state.borrow();
                let mut idxs: Vec<usize> = s.selected.iter().copied().collect();
                idxs.sort_unstable();
                idxs.into_iter().filter_map(|i| s.entries.get(i).cloned()).collect()
            };
            if entries.is_empty() {
                return;
            }
            if entries.len() == 1 {
                let entry = &entries[0];
                if entry.is_dir() {
                    let target = Location::Local(entry.path.clone());
                    navigate_to(&app, &rt, state.clone(), watcher.clone(), target);
                } else {
                    crate::glue::spawn::open_file(&entry.path);
                }
            } else {
                // Multi-selection → standard bulk-open (handles confirm
                // dialog above threshold).
                app.global::<Callabler>().invoke_open_default_app();
            }
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher;
        app.global::<Callabler>().on_open_containing_folder(move || {
            let app = weak.upgrade().expect("MainWindow alive in open-containing");
            // Search context menu only — otherwise the action is meaningless
            // (we'd be navigating to the folder we're already in).
            if !crate::glue::search_focused(&app, &state) {
                return;
            }
            let search_rows = app.get_search_rows();
            let n = search_rows.row_count();
            let Some(idx) = (0..n).find(|&i| search_rows.row_data(i).is_some_and(|r| r.selected)) else {
                return;
            };
            let hit_path = state.borrow().search_hit_paths.get(idx).cloned();
            let Some(hit_path) = hit_path else {
                return;
            };
            let Some(parent) = hit_path.parent().map(|p| p.to_path_buf()) else {
                warn!("open-containing: hit has no parent");
                return;
            };
            info!(parent = %parent.display(), "open-containing folder");
            // Highlight + scroll to the item once the parent's listing arrives.
            state.borrow_mut().pending_select = Some(hit_path);
            // navigate_to closes search and pushes onto history.
            navigate_to(&app, &rt, state.clone(), watcher.clone(), Location::Local(parent));
        });
    }

    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state.clone();
        app.global::<Callabler>().on_toggle_hidden(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-hidden");
            let cur = app.global::<Settings>().get_show_hidden();
            app.global::<Settings>().set_show_hidden(!cur);
            let carry = snapshot_thumbs(&state);
            rebuild_rows(&app, &state, &carry);
            refresh_inactive_pane(&app, &rt, &state);
        });
    }

    // Settings popup switches Settings.show-hidden directly via two-way binding;
    // this no-op callback only triggers the model rebuild.
    {
        let weak = weak.clone();
        let rt = rt.clone();
        let state = state;
        app.global::<Callabler>().on_toggle_hidden_noop(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-hidden-noop");
            let carry = snapshot_thumbs(&state);
            rebuild_rows(&app, &state, &carry);
            refresh_inactive_pane(&app, &rt, &state);
        });
    }

    {
        let weak = weak;
        app.global::<Callabler>().on_toggle_theme(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-theme");
            let cur = app.global::<Settings>().get_dark_theme();
            app.global::<Settings>().set_dark_theme(!cur);
        });
    }
}

pub fn navigate_to(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle, target: Location) {
    // Validate before touching history — otherwise typing a bad path would
    // poison the back stack with an entry the user can't revisit.
    if let Location::Local(p) = &target
        && let Some(msg) = preflight_local_path(p)
    {
        warn!(path = %p.display(), reason = %msg, "nav-to blocked");
        show_nav_error(app, &msg);
        return;
    }
    {
        let mut s = state.borrow_mut();
        if let Some(cur) = s.current.clone() {
            if loc_eq(&cur, &target) {
                return;
            }
            s.back_stack.push(cur);
        }
        s.forward_stack.clear();
    }
    close_search_if_active(app);
    navigate_internal(app, rt, state, watcher, target, false);
}

/// Drop search mode + clear the search model. Called from explicit user-driven
/// navigation (sidebar click, Home, back/forward, path-bar enter) so the user
/// returns to a normal listing once they leave the search context. Refresh
/// (F5) deliberately doesn't go through this path — searching + refreshing
/// the underlying folder is a valid combination.
fn close_search_if_active(app: &MainWindow) {
    if app.global::<SearchState>().get_active() {
        app.global::<Callabler>().invoke_close_search();
    }
}

/// Re-list the current directory without pushing history.
/// Used after mutating file operations (trash/delete/rename/paste).
pub fn navigate_internal_refresh(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    target: Location,
) {
    navigate_internal(app, rt, state, watcher, target, true);
}

/// Refresh only the view chrome — path bar, breadcrumbs, nav buttons,
/// disk-free, trash flag, selection count — and re-arm the directory watcher
/// for the **current active pane**, WITHOUT re-listing the directory.
///
/// Used when swapping focus between split panes (F3 split). `swap_active` has
/// already moved the now-active pane's `entries` and `rows_model` — which
/// still hold their thumbnails — into place, so a full `navigate_internal_refresh`
/// would needlessly re-list the folder and resubmit every thumbnail, making
/// the gallery flicker on each focus click.
/// Whether a location is the trash view. The places sidebar navigates to the
/// XDG trash files dir as a plain `Local` path (not the `Trash` variant), so we
/// also treat any path inside the trash as the trash view.
pub(crate) fn location_is_trash(target: &Location) -> bool {
    match target {
        Location::Trash => true,
        Location::Local(p) => mykrut_core::trash_io::is_in_trash(p),
    }
}

pub fn publish_active_pane(app: &MainWindow, rt: &Arc<Runtime>, state: &AppStateRc, watcher: &WatcherHandle) {
    let Some(target) = state.borrow().current.clone() else {
        app.global::<SlintAppState>().set_current_path("".into());
        app.global::<SlintAppState>().set_in_trash(false);
        update_nav_buttons(app, state);
        crate::glue::places::refresh_current_bookmark_flags(app, state);
        app.set_selected_count(0);
        return;
    };
    app.global::<SlintAppState>().set_current_path(target.display().into());
    app.global::<SlintAppState>().set_in_trash(location_is_trash(&target));
    push_path_segments(app, &target);
    refresh_disk_free(app, rt, &target);
    update_nav_buttons(app, state);
    crate::glue::places::refresh_current_bookmark_flags(app, state);
    app.set_selected_count(state.borrow().selected.len() as i32);
    crate::glue::watcher::on_navigated(rt, watcher, app.as_weak(), &target);
}

fn navigate_internal(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    target: Location,
    is_history_nav: bool,
) {
    let display_path = target.display();
    let span = info_span!(
        "navigate",
        path = %display_path,
        history = is_history_nav,
    );
    let guard = span.enter();

    info!("begin");

    // Pre-validate local paths so a typo in the path bar (or right-click →
    // open on a file) surfaces a clean error popup instead of silently
    // rendering an empty listing.
    if let Location::Local(p) = &target
        && let Some(msg) = preflight_local_path(p)
    {
        warn!(path = %p.display(), reason = %msg, "nav blocked");
        show_nav_error(app, &msg);
        return;
    }

    app.global::<SlintAppState>().set_current_path(display_path.into());
    app.global::<SlintAppState>().set_in_trash(location_is_trash(&target));
    push_path_segments(app, &target);
    refresh_disk_free(app, rt, &target);
    app.global::<SlintAppState>().set_loading(true);
    app.global::<SlintAppState>().set_error_text("".into());

    // Clear the visible listing immediately so the user doesn't see stale
    // rows from the previous folder while the async listing is in flight
    // (matches Nemo / Files behaviour). Skip on refresh (same target) — there
    // we'd just flash the list empty for no reason.
    let same_target = state.borrow().current.as_ref().is_some_and(|c| loc_eq(c, &target));
    if !same_target {
        {
            let mut s = state.borrow_mut();
            s.entries.clear();
            s.all_entries.clear();
            s.selected.clear();
            s.anchor = None;
        }
        state.borrow().rows_model.set_vec(Vec::new());
        app.set_selected_count(0);
    }

    let weak = app.as_weak();
    let rt = rt.clone();
    let rt_for_watcher = rt;
    let target_for_async = target.clone();
    let watcher_for_async = watcher;
    let nav_started = Instant::now();
    let parent_span = span.clone();

    drop(guard);

    let _ = slint::spawn_local(async move {
        let watcher = watcher_for_async;
        let rt = rt_for_watcher;
        let listing_span = info_span!(parent: &parent_span, "listing");
        let result = async {
            let list_start = Instant::now();
            let target_io = target_for_async.clone();
            let join = rt.spawn(async move { LocalFs::list(&target_io).await });
            let res = match join.await {
                Ok(r) => r,
                Err(err) => {
                    error!(?err, "tokio task panicked");
                    return Err(anyhow::anyhow!("tokio task panicked: {err}"));
                }
            };
            let elapsed_ms = list_start.elapsed().as_millis() as u64;
            match &res {
                Ok(entries) => info!(elapsed_ms, count = entries.len(), "listing done"),
                Err(err) => warn!(elapsed_ms, ?err, "listing failed"),
            }
            res
        }
        .instrument(listing_span)
        .await;

        let Some(app) = weak.upgrade() else {
            warn!("MainWindow gone before listing returned");
            return;
        };

        match result {
            Ok(entries) => {
                let count = entries.len();
                // Snapshot existing thumbnails (keyed by path) BEFORE swapping in
                // the fresh listing, so a same-folder refresh keeps them.
                let carry = snapshot_thumbs(&state);
                {
                    let mut s = state.borrow_mut();
                    s.current = Some(target.clone());
                    s.all_entries = entries;
                    s.selected.clear();
                    s.anchor = None;
                }
                let rebuild_start = Instant::now();
                rebuild_rows(&app, &state, &carry);
                debug!(
                    elapsed_ms = rebuild_start.elapsed().as_millis() as u64,
                    count, "rebuild_rows done"
                );
                apply_pending_select(&app, &state);
                update_nav_buttons(&app, &state);
                crate::glue::places::refresh_current_bookmark_flags(&app, &state);
                app.global::<SlintAppState>().set_loading(false);
                info!(
                    total_ms = nav_started.elapsed().as_millis() as u64,
                    count, "navigate complete"
                );

                crate::glue::watcher::on_navigated(&rt, &watcher, app.as_weak(), &target);
                // Active-tab title may have changed → refresh the strip.
                crate::glue::tabs::publish_tabs(&app, &state);
            }
            Err(err) => {
                error!(?err, "navigate failed");
                app.global::<SlintAppState>().set_loading(false);
                app.global::<SlintAppState>().set_error_text(err.to_string().into());
            }
        }
    });
}

/// Capture path → decoded thumbnail for the CURRENT (aligned) entries + model.
/// A rebuild can then carry these over for files that are still present, instead
/// of clearing every thumbnail (which makes the whole grid visibly repopulate).
/// Only valid when `entries` and `rows_model` are index-aligned (the usual case
/// at a `rebuild_rows` call); capture it *before* replacing `entries`.
pub fn snapshot_thumbs(state: &AppStateRc) -> std::collections::HashMap<std::path::PathBuf, slint::Image> {
    let s = state.borrow();
    let model = s.rows_model.clone();
    let mut map = std::collections::HashMap::new();
    for (i, e) in s.entries.iter().enumerate() {
        if let Some(row) = model.row_data(i)
            && row.has_thumbnail
        {
            map.insert(e.path.clone(), row.thumbnail.clone());
        }
    }
    map
}

/// Rebuilds the Slint row model from cached entries, applying current sort + hidden filter.
/// Single batch replacement (`set_vec`) — one repaint, no incremental remove() loops.
/// `carry` lets unchanged files keep their already-decoded thumbnail so a refresh
/// (paste, undo, sort, toggle-hidden) doesn't flash every thumbnail.
pub fn rebuild_rows(
    app: &MainWindow,
    state: &AppStateRc,
    carry: &std::collections::HashMap<std::path::PathBuf, slint::Image>,
) {
    let show_hidden = app.global::<Settings>().get_show_hidden();
    let sort_key = app.global::<Settings>().get_sort_key();
    let sort_order = app.global::<Settings>().get_sort_order();

    // Filter + sort against the full unfiltered listing, not the previous
    // (already-filtered) `entries` — otherwise toggling "show hidden" back on
    // would have nothing to re-include.
    let mut indices: Vec<usize> = {
        let s = state.borrow();
        s.all_entries
            .iter()
            .enumerate()
            .filter(|(_, e)| show_hidden || !e.is_hidden)
            .map(|(i, _)| i)
            .collect()
    };

    {
        let s = state.borrow();
        crate::glue::sort_select::sort_indices(&mut indices, &s.all_entries, sort_key, sort_order);
    }

    // Reorder backing entries so row index == entry index (lets row callbacks just `get(idx)`).
    let new_entries = {
        let s = state.borrow();
        indices.iter().map(|i| s.all_entries[*i].clone()).collect::<Vec<_>>()
    };
    {
        let mut s = state.borrow_mut();
        s.entries = new_entries;
        s.selected.clear();
        s.anchor = None;
    }

    let clipboard_paths: std::collections::HashSet<std::path::PathBuf> = {
        let s = state.borrow();
        s.clipboard.paths.iter().cloned().collect()
    };
    let is_cut = state.borrow().clipboard.cut;

    let rows: Vec<FileRowData> = {
        let s = state.borrow();
        s.entries
            .iter()
            .map(|e| {
                let mut row = file_entry_to_row(e, &clipboard_paths, is_cut, false);
                // Carry an already-decoded thumbnail over for files that survived
                // the refresh, so they don't flash blank then repopulate.
                if let Some(img) = carry.get(&e.path) {
                    row.has_thumbnail = true;
                    row.thumbnail = img.clone();
                }
                row
            })
            .collect()
    };

    state.borrow().rows_model.set_vec(rows);
    app.set_selected_count(0);
    app.global::<SlintAppState>().set_selection_summary("".into());

    // Kick off thumbnail generation for any image files in this listing.
    THUMB_CTRL.with(|c| {
        if let Some(ctrl) = c.borrow().as_ref() {
            crate::glue::thumbnails::submit_for(state, ctrl);
        }
    });
}

/// Consume `pending_select`: if a path was queued (e.g. by "open containing
/// folder"), find its row in the freshly built listing, select it, and scroll
/// it into view. No-op when nothing is queued or the path isn't in this folder.
fn apply_pending_select(app: &MainWindow, state: &AppStateRc) {
    let Some(want) = state.borrow_mut().pending_select.take() else {
        return;
    };
    let idx = {
        let s = state.borrow();
        s.entries.iter().position(|e| e.path == want)
    };
    let Some(idx) = idx else {
        return;
    };
    {
        let mut s = state.borrow_mut();
        s.selected.clear();
        s.selected.insert(idx);
        s.anchor = Some(idx);
    }
    let model = state.borrow().rows_model.clone();
    if let Some(mut row) = model.row_data(idx) {
        row.selected = true;
        model.set_row_data(idx, row);
    }
    app.global::<SlintAppState>().set_scroll_to_row(idx as i32);
    app.set_selected_count(1);
    crate::glue::selection::push_selection_summary(app, state);
}

/// Asynchronously list `target` and populate the INACTIVE split pane directly,
/// without swapping active/inactive. Used when split view is first turned on so
/// the freshly created pane fills in while the focused pane is left untouched.
///
/// The old approach (swap → navigate → swap-back) was broken: navigation is
/// async, so the swap-back ran before the listing returned and the result
/// landed in the wrong (focused) pane — leaving the new pane stuck on "Folder
/// is empty" and disturbing selection in the focused pane.
pub fn load_inactive_pane(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, target: Location) {
    let weak = app.as_weak();
    let rt2 = rt.clone();
    let target_io = target.clone();
    let _g = rt.enter();
    let _ = slint::spawn_local(async move {
        let res = match rt2.spawn(async move { LocalFs::list(&target_io).await }).await {
            Ok(r) => r,
            Err(err) => {
                error!(?err, "inactive-pane listing task panicked");
                return;
            }
        };
        let Some(app) = weak.upgrade() else { return };
        let entries = match res {
            Ok(e) => e,
            Err(err) => {
                warn!(?err, "inactive-pane listing failed");
                return;
            }
        };

        // Sort + filter exactly like the active pane, then build its rows. All
        // off the state borrow (only needs Settings + a clipboard snapshot).
        let show_hidden = app.global::<Settings>().get_show_hidden();
        let sort_key = app.global::<Settings>().get_sort_key();
        let sort_order = app.global::<Settings>().get_sort_order();
        let (clipboard_paths, is_cut) = {
            let s = state.borrow();
            let paths: std::collections::HashSet<std::path::PathBuf> = s.clipboard.paths.iter().cloned().collect();
            (paths, s.clipboard.cut)
        };

        let mut indices: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| show_hidden || !e.is_hidden)
            .map(|(i, _)| i)
            .collect();
        crate::glue::sort_select::sort_indices(&mut indices, &entries, sort_key, sort_order);
        let reordered: Vec<FileEntry> = indices.iter().map(|i| entries[*i].clone()).collect();
        let rows: Vec<FileRowData> = reordered
            .iter()
            .map(|e| file_entry_to_row(e, &clipboard_paths, is_cut, false))
            .collect();

        {
            let mut s = state.borrow_mut();
            let Some(pane) = s.inactive_pane.as_mut() else {
                return; // split turned off again before the listing returned
            };
            pane.current = Some(target.clone());
            pane.entries = reordered;
            pane.all_entries = entries;
            pane.selected.clear();
            pane.anchor = None;
            pane.rows_model.set_vec(rows);
        }
        crate::glue::split::publish_split(&app, &state);
        // Refresh both tab strips so the inactive pane's tab shows its folder.
        crate::glue::tabs::publish_tabs(&app, &state);
        info!(path = %target.display(), "inactive pane loaded");
    });
}

/// Remove the rows for `removed` paths from the active pane in place, without
/// re-listing the whole directory. Only drops entries whose file is actually
/// gone now (so a failed trash/delete leaves its row alone). Using
/// `VecModel::remove` emits per-row removal notifications, so the rest of the
/// list doesn't flash/repaint the way a full `set_vec` rebuild does.
pub fn remove_paths_from_view(app: &MainWindow, state: &AppStateRc, removed: &[std::path::PathBuf]) {
    use std::collections::HashSet;
    let set: HashSet<&std::path::Path> = removed.iter().map(std::path::PathBuf::as_path).collect();

    let model = state.borrow().rows_model.clone();
    let idxs: Vec<usize> = {
        let s = state.borrow();
        let mut v: Vec<usize> = s
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| set.contains(e.path.as_path()) && !e.path.exists())
            .map(|(i, _)| i)
            .collect();
        v.sort_unstable_by(|a, b| b.cmp(a)); // descending so earlier indices stay valid
        v
    };
    if idxs.is_empty() {
        return;
    }
    {
        let mut s = state.borrow_mut();
        for &i in &idxs {
            s.entries.remove(i);
        }
        // Keep the unfiltered master in sync (by path — its order differs).
        s.all_entries
            .retain(|e| !(set.contains(e.path.as_path()) && !e.path.exists()));
        s.selected.clear();
        s.anchor = None;
    }
    for &i in &idxs {
        model.remove(i);
    }
    app.set_selected_count(0);
}

/// Re-list the **inactive** split pane (if any) from disk. Mutating operations
/// (move/copy/trash/delete/rename) can change a folder that the inactive pane
/// is showing — most visibly, the source of a cut+paste move, whose row would
/// otherwise linger with a stale "CUT" badge after the file is already gone.
/// No-op when not split or when the inactive pane isn't a local folder.
pub fn refresh_inactive_pane(app: &MainWindow, rt: &Arc<Runtime>, state: &AppStateRc) {
    let target = {
        let s = state.borrow();
        match s.inactive_pane.as_ref().and_then(|p| p.current.clone()) {
            Some(loc @ Location::Local(_)) => loc,
            _ => return,
        }
    };
    load_inactive_pane(app, rt, state.clone(), target);
}

fn file_entry_to_row(
    e: &FileEntry,
    clipboard: &std::collections::HashSet<std::path::PathBuf>,
    clipboard_is_cut: bool,
    selected: bool,
) -> FileRowData {
    let (size_lo, size_hi) = pack_u64(e.size);
    FileRowData {
        display_name: e.display_name.clone().into(),
        icon_name: icon_for_entry(e).into(),
        is_dir: e.is_dir(),
        is_symlink: e.is_symlink,
        is_hidden: e.is_hidden,
        is_cut: clipboard_is_cut && clipboard.contains(&e.path),
        selected,
        has_thumbnail: false,
        thumbnail: slint::Image::default(),
        size_text: if e.is_dir() {
            "—".into()
        } else {
            human_size(e.size).into()
        },
        modified_text: human_mtime(e.mtime).into(),
        kind_text: kind_text(e.mime.as_deref(), e.is_dir()).into(),
        size_lo,
        size_hi,
    }
}

fn update_nav_buttons(app: &MainWindow, state: &AppStateRc) {
    let s = state.borrow();
    let st = app.global::<SlintAppState>();
    st.set_can_go_back(!s.back_stack.is_empty());
    st.set_can_go_forward(!s.forward_stack.is_empty());
    st.set_can_go_up(s.current.as_ref().and_then(Location::parent).is_some());
}

/// Recompute the `is_cut` flag on every row to reflect the clipboard state.
/// O(N) but uses `set_row_data` only for rows that actually changed.
pub fn mark_clipboard_visuals(state: &AppStateRc) {
    let cb_paths: std::collections::HashSet<std::path::PathBuf> = {
        let s = state.borrow();
        s.clipboard.paths.iter().cloned().collect()
    };
    let cut = state.borrow().clipboard.cut;

    let model = state.borrow().rows_model.clone();
    let n = model.row_count();
    for i in 0..n {
        let Some(entry) = state.borrow().entries.get(i).cloned() else {
            continue;
        };
        let want = cut && cb_paths.contains(&entry.path);
        if let Some(mut row) = model.row_data(i)
            && row.is_cut != want
        {
            row.is_cut = want;
            model.set_row_data(i, row);
        }
    }
}

/// Build clickable breadcrumb segments for the path bar.
fn push_path_segments(app: &MainWindow, target: &Location) {
    use std::rc::Rc;

    use slint::{ModelRc, VecModel};

    use crate::PathSegment;

    let segments: Vec<PathSegment> = match target {
        Location::Local(p) => {
            let mut out = Vec::new();
            out.push(PathSegment {
                label: "/".into(),
                full_path: "/".into(),
            });
            let mut acc = std::path::PathBuf::from("/");
            for component in p.components().skip(1) {
                let name = component.as_os_str().to_string_lossy().into_owned();
                if name.is_empty() {
                    continue;
                }
                acc.push(&name);
                out.push(PathSegment {
                    label: name.into(),
                    full_path: acc.display().to_string().into(),
                });
            }
            out
        }
        Location::Trash => vec![PathSegment {
            label: "Trash".into(),
            full_path: "trash:///".into(),
        }],
    };

    let model = Rc::new(VecModel::from(segments));
    app.global::<SlintAppState>().set_path_segments(ModelRc::from(model));
}

/// Verify a local path is openable as a directory before we kick off the
/// async listing. Returns a user-facing message on failure, or `None` if OK.
fn preflight_local_path(p: &std::path::Path) -> Option<String> {
    match std::fs::symlink_metadata(p) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Some(format!("Path does not exist:\n{}", p.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            Some(format!("Permission denied:\n{}", p.display()))
        }
        Err(err) => Some(format!("Cannot access path:\n{}\n\n{err}", p.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            // Re-stat through the symlink. If the link target is broken or
            // points to a file, complain accordingly.
            match std::fs::metadata(p) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    Some(format!("Broken symlink:\n{}", p.display()))
                }
                Err(err) => Some(format!("Cannot resolve symlink:\n{}\n\n{err}", p.display())),
                Ok(m) if !m.is_dir() => Some(format!("Symlink target is a file, not a folder:\n{}", p.display())),
                Ok(_) => probe_dir_readable(p),
            }
        }
        Ok(meta) if !meta.is_dir() => Some(format!("This is a file, not a folder:\n{}", p.display())),
        Ok(_) => probe_dir_readable(p),
    }
}

/// Confirmed to be a directory; verify it can actually be opened for listing.
/// `read_dir` does the `openat` immediately, so a permissions failure surfaces
/// here without enumerating any entries (cheap). Catching it up front lets us
/// block navigation with a clear error instead of entering a folder that then
/// renders empty and leaves `current` out of sync with the path bar.
fn probe_dir_readable(p: &std::path::Path) -> Option<String> {
    match std::fs::read_dir(p) {
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            Some(format!("Permission denied:\n{}", p.display()))
        }
        Err(err) => Some(format!("Cannot open this folder:\n{}\n\n{err}", p.display())),
    }
}

fn show_nav_error(app: &MainWindow, message: &str) {
    let ds = app.global::<DialogState>();
    ds.set_nav_error_title("".into()); // empty → default "Cannot open this location"
    ds.set_nav_error_message(message.into());
    ds.set_nav_error_open(true);
}

/// Nemo-style summary popup for items a copy/move could not process. Reuses the
/// generic message dialog with a custom title.
pub fn show_operation_error(app: &MainWindow, op: mykrut_core::Op, errors: &[mykrut_core::CopyError]) {
    const MAX_SHOWN: usize = 12;
    let verb = match op {
        mykrut_core::Op::Copy => "copied",
        mykrut_core::Op::Move => "moved",
    };
    let title = format!("{} item(s) could not be {}", errors.len(), verb);
    let mut body = String::new();
    for e in errors.iter().take(MAX_SHOWN) {
        body.push_str(&format!("{}\n    {}\n", e.path.display(), e.message));
    }
    if errors.len() > MAX_SHOWN {
        body.push_str(&format!("… and {} more", errors.len() - MAX_SHOWN));
    }
    let ds = app.global::<DialogState>();
    ds.set_nav_error_title(title.into());
    ds.set_nav_error_message(body.into());
    ds.set_nav_error_open(true);
}

/// Refresh the bottom-bar "X free of Y" indicator. No-op for non-local targets
/// (e.g., trash). The `statvfs` syscall can block for seconds on a slow/network
/// mount, so it runs off the UI thread.
///
/// We deliberately do NOT blank the label first: clearing it on every folder
/// change made it flicker (blank → value) once per navigation. Instead the old
/// value stays put and is overwritten in place when the new query returns, so
/// it changes smoothly. (Trash, which has no disk figure, still clears.)
fn refresh_disk_free(app: &MainWindow, rt: &Arc<Runtime>, target: &Location) {
    let path = match target {
        Location::Local(p) => p.clone(),
        Location::Trash => {
            app.global::<SlintAppState>().set_disk_free_text("".into());
            return;
        }
    };

    let weak = app.as_weak();
    let rt2 = rt.clone();
    let guard_path = path.display().to_string();
    let _g = rt.enter();
    let _ = slint::spawn_local(async move {
        let space = rt2
            .spawn_blocking(move || disk_space::query(&path))
            .await
            .ok()
            .flatten();
        let Some(app) = weak.upgrade() else { return };
        // A newer navigation may have completed while statvfs blocked — don't
        // stomp the current folder's value with a stale one.
        if app.global::<SlintAppState>().get_current_path() != guard_path.as_str() {
            return;
        }
        let text = match space {
            Some(d) if d.total > 0 => {
                format!("{} free of {}", human_size(d.free), human_size(d.total))
            }
            _ => String::new(),
        };
        app.global::<SlintAppState>().set_disk_free_text(text.into());
    });
}

fn loc_eq(a: &Location, b: &Location) -> bool {
    match (a, b) {
        (Location::Local(p), Location::Local(q)) => p == q,
        (Location::Trash, Location::Trash) => true,
        _ => false,
    }
}

fn pack_u64(v: u64) -> (i32, i32) {
    (v as i32, (v >> 32) as i32)
}

