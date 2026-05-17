//! Tab management glue: handles new/close/switch callbacks + keeps the
//! Slint `tabs` model in sync with the Rust `AppState.tabs` snapshots.

use std::rc::Rc;
use std::sync::Arc;

use mykrut_core::Location;
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Runtime;
use tracing::{debug, info};

use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{Callabler, MainWindow, TabInfo};

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    // Initial publish so the (single) starting tab shows up.
    publish_tabs(app, &state);

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_new_tab(move || {
            let app = weak.upgrade().expect("MainWindow alive in new-tab");
            let new_idx = state.borrow_mut().new_tab();
            // Switch to the freshly created tab.
            let model = state.borrow_mut().switch_tab(new_idx);
            if let Some(m) = model {
                app.set_rows(ModelRc::from(m));
            }
            publish_tabs(&app, &state);
            // Start at $HOME for the new tab.
            let target = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
            info!(idx = new_idx, "new tab opened");
            crate::glue::navigation::navigate_to(&app, &rt, state.clone(), watcher.clone(), Location::Local(target));
        });
    }

    // Context-menu "Open in new tab(s)": open every selected folder in its own
    // new tab of the active pane and switch to the first.
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_open_in_new_tab(move || {
            let app = weak.upgrade().expect("MainWindow alive in open-in-new-tab");
            // Every selected directory (sorted by row) gets its own tab.
            let dirs: Vec<std::path::PathBuf> = {
                let s = state.borrow();
                let mut idxs: Vec<usize> = s.selected.iter().copied().collect();
                idxs.sort_unstable();
                idxs.iter()
                    .filter_map(|&i| s.entries.get(i))
                    .filter(|e| e.is_dir())
                    .map(|e| e.path.clone())
                    .collect()
            };
            if dirs.is_empty() {
                return;
            }
            // Create one tab per folder, pre-seeding each tab's location. Only
            // the first is navigated now; the rest load lazily when the user
            // switches to them (switch-tab refreshes the restored `current`),
            // which avoids racing several async listings against each other.
            let first = {
                let mut s = state.borrow_mut();
                let mut first = None;
                for dir in &dirs {
                    let idx = s.new_tab();
                    if first.is_none() {
                        first = Some(idx);
                    }
                    let loc = Location::Local(dir.clone());
                    if let Some(t) = s.tabs.get_mut(idx) {
                        t.title = crate::state::tab_title(Some(&loc));
                        t.current = Some(loc);
                    }
                }
                first
            };
            let Some(first) = first else { return };
            let model = state.borrow_mut().switch_tab(first);
            if let Some(m) = model {
                app.set_rows(ModelRc::from(m));
            }
            publish_tabs(&app, &state);
            info!(count = dirs.len(), "open in new tab(s)");
            // The first tab's `current` was pre-seeded above, so `navigate_to`
            // would early-return on the equal target and leave it empty with a
            // stale path bar. List it the same way switching to a lazily-loaded
            // tab does: a refresh that populates rows + updates breadcrumbs.
            if let Some(cur) = state.borrow().current.clone() {
                crate::glue::navigation::navigate_internal_refresh(&app, &rt, state.clone(), watcher.clone(), cur);
            }
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_close_tab(move |idx| {
            let app = weak.upgrade().expect("MainWindow alive in close-tab");
            let want = if idx < 0 {
                state.borrow().active_tab_idx
            } else {
                idx as usize
            };

            let was_active = want == state.borrow().active_tab_idx;
            let removed = state.borrow_mut().close_tab(want);
            if removed.is_none() {
                debug!(idx = want, "close-tab refused (last tab)");
                return;
            }

            if was_active {
                // After remove, active_idx points to whichever tab we landed on.
                let new_active = state.borrow().active_tab_idx;
                let model = state.borrow_mut().restore_after_close(new_active);
                if let Some(m) = model {
                    app.set_rows(ModelRc::from(m));
                }
                // Refresh nav buttons + path bar for the now-active tab.
                if let Some(cur) = state.borrow().current.clone() {
                    crate::glue::navigation::navigate_internal_refresh(&app, &rt, state.clone(), watcher.clone(), cur);
                }
            }
            publish_tabs(&app, &state);
            info!(idx = want, "tab closed");
        });
    }

    // ── Pane-aware tab strips (split view) ───────────────────────────────
    // Each pane renders its own strip. Acting on the *inactive* pane's strip
    // first focuses that pane (swap), then runs the ordinary active-pane op, so
    // all the tab logic above is reused unchanged.
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_tab_switch_in(move |is_right, idx| {
            let app = weak.upgrade().expect("MainWindow alive in tab-switch-in");
            ensure_side_active(&app, &rt, &state, &watcher, is_right);
            app.global::<Callabler>().invoke_switch_tab(idx);
        });
    }
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_tab_new_in(move |is_right| {
            let app = weak.upgrade().expect("MainWindow alive in tab-new-in");
            ensure_side_active(&app, &rt, &state, &watcher, is_right);
            app.global::<Callabler>().invoke_new_tab();
        });
    }
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_tab_close_in(move |is_right, idx| {
            let app = weak.upgrade().expect("MainWindow alive in tab-close-in");
            ensure_side_active(&app, &rt, &state, &watcher, is_right);
            app.global::<Callabler>().invoke_close_tab(idx);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state;
        let watcher = watcher;
        app.global::<Callabler>().on_switch_tab(move |idx| {
            let app = weak.upgrade().expect("MainWindow alive in switch-tab");
            let target = idx as usize;
            let model = state.borrow_mut().switch_tab(target);
            let Some(model) = model else { return };
            app.set_rows(ModelRc::from(model));
            publish_tabs(&app, &state);

            // Re-fetch the directory so the watcher attaches to the right path
            // and breadcrumbs/back buttons update.
            if let Some(cur) = state.borrow().current.clone() {
                crate::glue::navigation::navigate_internal_refresh(&app, &rt, state.clone(), watcher.clone(), cur);
            }
            info!(idx = target, "switched to tab");
        });
    }
}

/// Ensure the pane on the given physical side (`is_right`) is the active one,
/// swapping the split if needed. No-op when there is no split or that side is
/// already active.
fn ensure_side_active(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: &AppStateRc,
    watcher: &WatcherHandle,
    is_right: bool,
) {
    let need_swap = {
        let s = state.borrow();
        s.split_active() && is_right != s.active_is_right
    };
    if need_swap {
        crate::glue::split::do_swap_active(app, rt, state, watcher);
    }
}

pub fn publish_tabs(app: &MainWindow, state: &AppStateRc) {
    let s = state.borrow();

    // Active pane: the active tab's title/location come from the live flattened
    // fields (`current`), since snapshots are only written on tab switch.
    let active_tabs = build_tab_infos(&s.tabs, s.active_tab_idx, s.current.as_ref());
    app.set_tabs(ModelRc::from(Rc::new(VecModel::from(active_tabs))));

    // Inactive pane (split view): its own tab list, built from its snapshot. Its
    // active tab's live location is `p.current`.
    let inactive = s.inactive_pane.as_ref().map_or_else(Vec::new, |p| {
        build_tab_infos(&p.tabs, p.active_tab_idx, p.current.as_ref())
    });
    app.set_inactive_tabs(ModelRc::from(Rc::new(VecModel::from(inactive))));
}

/// Build the `TabInfo` row list for one pane. `live_current` is the pane's live
/// location, used for the active tab (whose snapshot may lag the live value).
fn build_tab_infos(
    tabs: &[crate::state::TabSnapshot],
    active: usize,
    live_current: Option<&mykrut_core::Location>,
) -> Vec<TabInfo> {
    let mut out: Vec<TabInfo> = Vec::with_capacity(tabs.len());
    for (i, t) in tabs.iter().enumerate() {
        let loc = if i == active { live_current } else { t.current.as_ref() };
        let title = if i == active {
            crate::state::tab_title(live_current)
        } else if !t.title.is_empty() {
            t.title.clone()
        } else {
            crate::state::tab_title(t.current.as_ref())
        };
        let path = match loc {
            Some(mykrut_core::Location::Local(p)) => p.display().to_string(),
            _ => String::new(),
        };
        out.push(TabInfo {
            title: title.into(),
            is_active: i == active,
            path: path.into(),
        });
    }
    out
}
