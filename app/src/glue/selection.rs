use std::collections::HashSet;

use slint::{ComponentHandle, Model};
use tracing::{debug, info_span};

use crate::state::AppStateRc;
use crate::{AppState as SlintAppState, Callabler, MainWindow, NavKey, SelectMode};

/// PageUp/PageDown row jump distance. Fixed because Slint doesn't surface the
/// list-view's visible row count back to Rust.
const PAGE_SIZE: usize = 15;

pub fn wire(app: &MainWindow, state: AppStateRc) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_select_row(move |idx_i32, mode| {
            let app = weak.upgrade().expect("MainWindow alive in select-row");
            if idx_i32 < 0 {
                return;
            }
            let idx = idx_i32 as usize;
            if crate::glue::search_focused(&app, &state) {
                apply_search_selection(&app, &state, idx, mode);
                push_search_selected_count(&app);
            } else {
                apply_selection(&state, idx, mode);
                sync_selection_to_model(&state);
                push_selected_count(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_select_all(move || {
            let app = weak.upgrade().expect("MainWindow alive in select-all");
            if crate::glue::search_focused(&app, &state) {
                set_all_search_selected(&app, true);
                state.borrow_mut().search_anchor = Some(0);
                push_search_selected_count(&app);
            } else {
                let len = state.borrow().entries.len();
                {
                    let mut s = state.borrow_mut();
                    s.selected = (0..len).collect();
                    s.anchor = Some(0);
                }
                sync_selection_to_model(&state);
                push_selected_count(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state_clone = state.clone();
        app.global::<Callabler>().on_select_none(move || {
            let app = weak.upgrade().expect("MainWindow alive in select-none");
            if crate::glue::search_focused(&app, &state_clone) {
                set_all_search_selected(&app, false);
                state_clone.borrow_mut().search_anchor = None;
                push_search_selected_count(&app);
            } else {
                {
                    let mut s = state_clone.borrow_mut();
                    s.selected.clear();
                    s.anchor = None;
                }
                sync_selection_to_model(&state_clone);
                push_selected_count(&app, &state_clone);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_nav_selection(move |key, shift| {
            let app = weak.upgrade().expect("MainWindow alive in nav-selection");
            apply_nav(&state, key, shift);
            // Tell the list view to scroll the new cursor into view.
            if let Some(target) = state.borrow().anchor {
                app.global::<SlintAppState>().set_scroll_to_row(target as i32);
            }
            sync_selection_to_model(&state);
            push_selected_count(&app, &state);
        });
    }

    // Rubber-band: snapshot the selection when a drag begins so Ctrl-drag can
    // toggle against it.
    {
        let state = state.clone();
        app.global::<Callabler>().on_rubberband_begin(move || {
            let mut s = state.borrow_mut();
            s.rubberband_base = s.selected.clone();
        });
    }

    // Rubber-band: x1,y1,x2,y2 in grid origin px, plus geometry to reverse-map.
    // `ctrl` → toggle the intersected tiles against the drag-start snapshot
    // (selected ↔ unselected), Nemo-style; otherwise replace the selection.
    {
        let weak = app.as_weak();
        app.global::<Callabler>().on_rubberband_select(
            move |x1, y1, x2, y2, tile_w, tile_h, cell_w, cols, gutter, ctrl| {
                let app = weak.upgrade().expect("MainWindow alive in rubberband");
                let n = state.borrow().entries.len();
                let cols = cols.max(1) as usize;
                let cell_w = cell_w.max(1.0);
                let tile_h = tile_h.max(1.0);
                let tile_w = tile_w.max(1.0);
                let g = gutter;

                let lo_x = x1.min(x2);
                let hi_x = x1.max(x2);
                let lo_y = y1.min(y2);
                let hi_y = y1.max(y2);

                let mut hits = std::collections::HashSet::new();
                for idx in 0..n {
                    let col = (idx % cols) as f32;
                    let row = (idx / cols) as f32;
                    let tile_x = g + col * cell_w + (cell_w - tile_w) / 2.0;
                    let tile_y = g + row * tile_h;
                    let tile_x2 = tile_x + tile_w;
                    let tile_y2 = tile_y + tile_h;
                    let intersects = tile_x < hi_x && tile_x2 > lo_x && tile_y < hi_y && tile_y2 > lo_y;
                    if intersects {
                        hits.insert(idx);
                    }
                }

                {
                    let mut s = state.borrow_mut();
                    if ctrl {
                        // base XOR hits: items in the band flip relative to the
                        // selection when the drag started; the rest is preserved.
                        let mut result = s.rubberband_base.clone();
                        for idx in hits {
                            if !result.remove(&idx) {
                                result.insert(idx);
                            }
                        }
                        s.selected = result;
                    } else {
                        s.selected = hits;
                    }
                    s.anchor = None;
                }
                sync_selection_to_model(&state);
                push_selected_count(&app, &state);
            },
        );
    }
}

fn apply_nav(state: &AppStateRc, key: NavKey, shift: bool) {
    let mut s = state.borrow_mut();
    let n = s.entries.len();
    if n == 0 {
        return;
    }
    let cur = s.anchor.unwrap_or(0);
    let new_cur = match key {
        NavKey::Up => cur.saturating_sub(1),
        NavKey::Down => (cur + 1).min(n - 1),
        NavKey::PageUp => cur.saturating_sub(PAGE_SIZE),
        NavKey::PageDown => (cur + PAGE_SIZE).min(n - 1),
        NavKey::Home => 0,
        NavKey::End => n - 1,
    };
    if !shift {
        // Non-additive: replace the selection. (Shift keeps prior selections.)
        s.selected.clear();
    }
    s.selected.insert(new_cur);
    s.anchor = Some(new_cur);
    debug!(?key, shift, new_cur, "nav");
}

fn apply_selection(state: &AppStateRc, idx: usize, mode: SelectMode) {
    let _span = info_span!("select", idx, ?mode).entered();
    let mut s = state.borrow_mut();
    let n = s.entries.len();
    if idx >= n {
        return;
    }
    match mode {
        SelectMode::Single => {
            s.selected.clear();
            s.selected.insert(idx);
            s.anchor = Some(idx);
        }
        SelectMode::Toggle => {
            if !s.selected.remove(&idx) {
                s.selected.insert(idx);
            }
            s.anchor = Some(idx);
        }
        SelectMode::Range => {
            // Additive shift-click: extend the existing selection with the
            // range from anchor (or current click if no anchor) to the clicked
            // index. Doesn't clear what was selected before — repeated
            // shift-clicks accumulate.
            let from = s.anchor.unwrap_or(idx);
            let (lo, hi) = if from <= idx { (from, idx) } else { (idx, from) };
            for i in lo..=hi {
                s.selected.insert(i);
            }
            s.anchor = Some(idx);
        }
    }
    debug!(count = s.selected.len(), "new selection size");
}

/// Search-mode selection: operates directly on the `search-rows` model. The
/// regular `entries` / `selected` set isn't touched because rows in search
/// results don't share its index space (they come from arbitrary subdirs).
fn apply_search_selection(app: &MainWindow, state: &AppStateRc, idx: usize, mode: SelectMode) {
    let model = app.get_search_rows();
    let n = model.row_count();
    if idx >= n {
        return;
    }
    match mode {
        SelectMode::Single => {
            for i in 0..n {
                if let Some(mut row) = model.row_data(i) {
                    let want = i == idx;
                    if row.selected != want {
                        row.selected = want;
                        model.set_row_data(i, row);
                    }
                }
            }
            state.borrow_mut().search_anchor = Some(idx);
        }
        SelectMode::Toggle => {
            if let Some(mut row) = model.row_data(idx) {
                row.selected = !row.selected;
                model.set_row_data(idx, row);
            }
            state.borrow_mut().search_anchor = Some(idx);
        }
        SelectMode::Range => {
            let from = state.borrow().search_anchor.unwrap_or(idx);
            let (lo, hi) = if from <= idx { (from, idx) } else { (idx, from) };
            for i in lo..=hi {
                if let Some(mut row) = model.row_data(i)
                    && !row.selected
                {
                    row.selected = true;
                    model.set_row_data(i, row);
                }
            }
            state.borrow_mut().search_anchor = Some(idx);
        }
    }
}

fn set_all_search_selected(app: &MainWindow, want: bool) {
    let model = app.get_search_rows();
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i)
            && row.selected != want
        {
            row.selected = want;
            model.set_row_data(i, row);
        }
    }
}

fn push_search_selected_count(app: &MainWindow) {
    let model = app.get_search_rows();
    let n = model.row_count();
    let mut count: i32 = 0;
    let mut any_archive = false;
    for i in 0..n {
        if let Some(row) = model.row_data(i)
            && row.selected
        {
            count += 1;
            if !row.is_dir {
                // We don't have the full path here; use the display
                // name's extension as a fast approximation (the actual
                // path is looked up via search_hit_paths in archive.rs
                // when "Extract" is invoked).
                let name = row.display_name.as_str();
                if crate::glue::archive::is_archive_path(std::path::Path::new(name)) {
                    any_archive = true;
                }
            }
        }
    }
    app.set_selected_count(count);
    // Search-result selection isn't summarised; fall back to the item count.
    app.global::<SlintAppState>().set_selection_summary("".into());
    // Bulk ops on search results aren't wired yet — keep the menu defaults
    // conservative so users don't trigger unsupported actions.
    app.set_selection_is_dir(false);
    app.set_selection_in_trash(false);
    app.global::<SlintAppState>().set_selection_dir_count(0);
    app.global::<SlintAppState>().set_selection_has_archive(any_archive);
    // Bookmarking search results isn't wired — keep the toggle inert.
    app.global::<SlintAppState>().set_selection_is_bookmarked(false);
    app.global::<SlintAppState>().set_selection_can_bookmark(false);
}

/// Apply current selection set to the rows VecModel without rebuilding everything.
/// Computes diffs against previous `selected` bools using row_data + set_row_data.
pub fn sync_selection_to_model(state: &AppStateRc) {
    let s = state.borrow();
    let model = s.rows_model.clone();
    let total = model.row_count();
    for i in 0..total {
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        let want = s.selected.contains(&i);
        if row.selected != want {
            row.selected = want;
            drop(row.clone());
            model.set_row_data(i, row);
        }
    }
}

#[expect(dead_code)] // Used by Phase 2.2 clipboard module.
pub fn selected_indices(state: &AppStateRc) -> Vec<usize> {
    let mut v: Vec<usize> = state.borrow().selected.iter().copied().collect();
    v.sort_unstable();
    v
}

/// Past this many selected folders we skip the per-folder "direct items" count
/// (each needs a `read_dir`), so select-all / rubber-band stay responsive.
const FOLDER_STAT_CAP: usize = 50;

/// Build the status-bar selection summary. Returns "" for an empty selection so
/// the bar falls back to the plain item count.
///
/// Examples:
/// * single file:   `report.pdf selected (2.4 MB)`
/// * single folder: `src selected (containing 12 direct items)`
/// * many files:    `3 files selected (5.1 MB)`
/// * mixed:         `2 folders selected (containing 30 direct items), 3 files (5.1 MB)`
fn selection_summary(entries: &[mykrut_core::FileEntry], selected: &HashSet<usize>) -> String {
    if selected.is_empty() {
        return String::new();
    }
    let mut folders: Vec<&std::path::Path> = Vec::new();
    let mut file_count = 0usize;
    let mut file_bytes = 0u64;
    let mut single_name: Option<&str> = None;
    for &i in selected {
        let Some(e) = entries.get(i) else { continue };
        if e.is_dir() {
            folders.push(&e.path);
        } else {
            file_count += 1;
            file_bytes += e.size;
        }
        if selected.len() == 1 {
            single_name = Some(&e.display_name);
        }
    }
    let folder_count = folders.len();
    if folder_count == 0 && file_count == 0 {
        return String::new();
    }

    // Direct-item counts are only computed for a modest number of folders.
    let direct_items: Option<usize> = if folder_count > 0 && folder_count <= FOLDER_STAT_CAP {
        Some(
            folders
                .iter()
                .map(|p| std::fs::read_dir(p).map_or(0, Iterator::count))
                .sum(),
        )
    } else {
        None
    };

    let items_phrase = |n: usize| -> String {
        let unit = if n == 1 { "item" } else { "items" };
        format!("{n} direct {unit}")
    };

    // Single item: lead with its name.
    if selected.len() == 1 {
        let name = single_name.unwrap_or_default();
        return if folder_count == 1 {
            match direct_items {
                Some(c) => format!("{name} selected (containing {})", items_phrase(c)),
                None => format!("{name} selected"),
            }
        } else {
            format!("{name} selected ({})", crate::format_util::human_size(file_bytes))
        };
    }

    // "selected" attaches to the first (leading) group; trailing groups append
    // just their own detail, matching: "2 folders selected (...), 3 files (...)".
    let file_tail = || {
        let unit = if file_count == 1 { "file" } else { "files" };
        format!("{file_count} {unit} ({})", crate::format_util::human_size(file_bytes))
    };
    if folder_count > 0 {
        let unit = if folder_count == 1 { "folder" } else { "folders" };
        let detail = match direct_items {
            Some(c) => format!(" (containing total {})", items_phrase(c)),
            None => String::new(),
        };
        let mut out = format!("{folder_count} {unit} selected{detail}");
        if file_count > 0 {
            out.push_str(&format!(", {}", file_tail()));
        }
        out
    } else {
        let unit = if file_count == 1 { "file" } else { "files" };
        format!(
            "{file_count} {unit} selected ({})",
            crate::format_util::human_size(file_bytes)
        )
    }
}

/// Recompute just the status-bar selection summary from the current active-pane
/// selection. Used by navigation flows (e.g. reveal-an-item) that change the
/// selection without going through `push_selected_count`.
pub(crate) fn push_selection_summary(app: &MainWindow, state: &AppStateRc) {
    let s = state.borrow();
    app.global::<SlintAppState>()
        .set_selection_summary(selection_summary(&s.entries, &s.selected).into());
}

fn push_selected_count(app: &MainWindow, state: &AppStateRc) {
    let s = state.borrow();
    let n = s.selected.len() as i32;
    app.set_selected_count(n);
    app.global::<SlintAppState>()
        .set_selection_summary(selection_summary(&s.entries, &s.selected).into());

    // For the context menu's "Add bookmark" item — only valid for a single dir.
    let single_dir = if n == 1 {
        s.selected
            .iter()
            .next()
            .and_then(|&i| s.entries.get(i))
            .is_some_and(|e| e.is_dir())
    } else {
        false
    };
    app.set_selection_is_dir(single_dir);

    // Number of selected directories — drives the "Open in new tab(s)" item,
    // which works for any number of folders.
    let dir_count = s
        .selected
        .iter()
        .filter_map(|&i| s.entries.get(i))
        .filter(|e| e.is_dir())
        .count() as i32;
    app.global::<SlintAppState>().set_selection_dir_count(dir_count);

    // "Restore" menu item: visible only when at least one selected entry lives
    // inside the XDG trash files dir.
    let any_in_trash = s
        .selected
        .iter()
        .filter_map(|&i| s.entries.get(i))
        .any(|e| mykrut_core::trash_io::is_in_trash(&e.path));
    app.set_selection_in_trash(any_in_trash);
    // "Extract here" gating — visible when any selected entry is an archive
    // we know how to drive through 7z.
    let any_archive = s
        .selected
        .iter()
        .filter_map(|&i| s.entries.get(i))
        .any(crate::glue::archive::is_archive_entry);
    drop(s);
    app.global::<SlintAppState>().set_selection_has_archive(any_archive);

    // Refresh the default-app label whenever the selection changes; the
    // helper deals with the empty/multi cases by clearing the field.
    crate::glue::default_app::refresh(app, state);

    // Keep the context-menu "Add ↔ Remove bookmark" toggle in sync with the
    // selection.
    crate::glue::places::refresh_selection_bookmark_flags(app, state);
}


#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use mykrut_core::{FileEntry, FileType, Permissions};

    use super::*;

    fn make(n: &str) -> FileEntry {
        FileEntry {
            path: n.into(),
            display_name: n.into(),
            file_type: FileType::Regular,
            mime: None,
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            permissions: Permissions::default(),
            is_hidden: false,
            is_symlink: false,
        }
    }

    fn state_with(n: usize) -> AppStateRc {
        let model = std::rc::Rc::new(slint::VecModel::default());
        let state = AppStateRc::new(model);
        state.borrow_mut().entries = (0..n).map(|i| make(&format!("f{i}"))).collect();
        state
    }

    fn file_entry(name: &str, size: u64) -> FileEntry {
        FileEntry { size, ..make(name) }
    }

    #[test]
    fn summary_empty_when_nothing_selected() {
        assert_eq!(selection_summary(&[], &HashSet::new()), "");
    }

    #[test]
    fn summary_single_file_shows_name_and_size() {
        let entries = vec![file_entry("report.pdf", 2_400_000)];
        let sel: HashSet<usize> = [0].into_iter().collect();
        let s = selection_summary(&entries, &sel);
        assert!(s.starts_with("report.pdf selected ("), "got: {s}");
    }

    #[test]
    fn summary_many_files_counts_and_sums() {
        let entries = vec![file_entry("a", 1000), file_entry("b", 2000), file_entry("c", 3000)];
        let sel: HashSet<usize> = [0, 1, 2].into_iter().collect();
        let s = selection_summary(&entries, &sel);
        assert!(s.starts_with("3 files selected ("), "got: {s}");
    }

    #[test]
    fn single_replaces() {
        let s = state_with(5);
        apply_selection(&s, 1, SelectMode::Single);
        apply_selection(&s, 3, SelectMode::Single);
        let v: Vec<_> = s.borrow().selected.iter().copied().collect();
        assert_eq!(v, vec![3]);
        assert_eq!(s.borrow().anchor, Some(3));
    }

    #[test]
    fn toggle_adds_and_removes() {
        let s = state_with(5);
        apply_selection(&s, 1, SelectMode::Toggle);
        apply_selection(&s, 3, SelectMode::Toggle);
        let mut v: Vec<_> = s.borrow().selected.iter().copied().collect();
        v.sort_unstable();
        assert_eq!(v, vec![1, 3]);
        apply_selection(&s, 1, SelectMode::Toggle);
        let v: Vec<_> = s.borrow().selected.iter().copied().collect();
        assert_eq!(v, vec![3]);
    }

    #[test]
    fn range_extends_existing_selection() {
        let s = state_with(10);
        apply_selection(&s, 1, SelectMode::Toggle);
        apply_selection(&s, 8, SelectMode::Toggle);
        // Now {1, 8} selected. Anchor = 8.
        apply_selection(&s, 5, SelectMode::Range);
        // Should add range 5..=8 to existing selection.
        let mut v: Vec<_> = s.borrow().selected.iter().copied().collect();
        v.sort_unstable();
        assert_eq!(v, vec![1, 5, 6, 7, 8]);
    }

    #[test]
    fn range_uses_anchor() {
        let s = state_with(10);
        apply_selection(&s, 2, SelectMode::Single);
        apply_selection(&s, 6, SelectMode::Range);
        let mut v: Vec<_> = s.borrow().selected.iter().copied().collect();
        v.sort_unstable();
        assert_eq!(v, vec![2, 3, 4, 5, 6]);
    }

    #[test]
    fn nav_down_moves_cursor_and_replaces() {
        let s = state_with(10);
        apply_selection(&s, 3, SelectMode::Single);
        apply_nav(&s, NavKey::Down, false);
        let v: Vec<_> = s.borrow().selected.iter().copied().collect();
        assert_eq!(v, vec![4]);
        assert_eq!(s.borrow().anchor, Some(4));
    }

    #[test]
    fn nav_home_end() {
        let s = state_with(10);
        apply_selection(&s, 5, SelectMode::Single);
        apply_nav(&s, NavKey::End, false);
        assert_eq!(s.borrow().anchor, Some(9));
        apply_nav(&s, NavKey::Home, false);
        assert_eq!(s.borrow().anchor, Some(0));
    }

    #[test]
    fn nav_with_shift_extends_additively() {
        let s = state_with(10);
        apply_selection(&s, 3, SelectMode::Single);
        apply_nav(&s, NavKey::Down, true);
        apply_nav(&s, NavKey::Down, true);
        let mut v: Vec<_> = s.borrow().selected.iter().copied().collect();
        v.sort_unstable();
        assert_eq!(v, vec![3, 4, 5]);
    }

    #[test]
    fn nav_page_clamps_at_edges() {
        let s = state_with(5);
        apply_selection(&s, 2, SelectMode::Single);
        apply_nav(&s, NavKey::PageDown, false);
        assert_eq!(s.borrow().anchor, Some(4));
        apply_nav(&s, NavKey::PageUp, false);
        assert_eq!(s.borrow().anchor, Some(0));
    }

    #[test]
    fn range_reverse() {
        let s = state_with(10);
        apply_selection(&s, 7, SelectMode::Single);
        apply_selection(&s, 3, SelectMode::Range);
        let mut v: Vec<_> = s.borrow().selected.iter().copied().collect();
        v.sort_unstable();
        assert_eq!(v, vec![3, 4, 5, 6, 7]);
    }
}
