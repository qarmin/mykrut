//! F3 split-view: create/destroy the secondary pane and swap which one is active.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc};
use tokio::runtime::Runtime;
use tracing::{debug, info};

use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{Callabler, MainWindow};

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        app.global::<Callabler>().on_toggle_split(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-split");
            let was_split = state.borrow().split_active();
            let seed = state.borrow().current.clone();

            // If we're collapsing the split, decide what happens to an active
            // search overlay *before* the pane it lived on disappears.
            let search_active = app.global::<crate::SearchState>().get_active();
            let search_on_dropped_pane = was_split && search_active && {
                let s = state.borrow();
                // Dropped pane is the inactive one; its side is !active_is_right.
                s.search_on_right != s.active_is_right
            };

            state.borrow_mut().toggle_split(seed.clone());

            if was_split && search_active {
                if search_on_dropped_pane {
                    // The search results were on the pane we just removed —
                    // close search so we don't leave a dangling overlay.
                    app.global::<Callabler>().invoke_close_search();
                } else {
                    // Search stays, but there's now a single (left) pane.
                    state.borrow_mut().search_on_right = false;
                }
            }

            publish_split(&app, &state);
            // Refresh both strips: turning split on reveals the new pane's tab,
            // turning it off drops the second strip.
            crate::glue::tabs::publish_tabs(&app, &state);

            if !was_split {
                // We just turned on split — load the freshly created (inactive)
                // pane directly so it shows its contents, without touching the
                // focused pane.
                if let Some(loc) = seed {
                    info!(seed = %loc.display(), "split on — load inactive pane");
                    crate::glue::navigation::load_inactive_pane(&app, &rt, state.clone(), loc);
                }
            } else {
                info!("split off");
            }
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state;
        let watcher = watcher;
        app.global::<Callabler>().on_swap_active_pane(move || {
            let app = weak.upgrade().expect("MainWindow alive in swap-active-pane");
            do_swap_active(&app, &rt, &state, &watcher);
        });
    }
}

/// Make the currently-inactive pane active and refresh all view chrome.
///
/// `swap_active` already moved this pane's row model (with its thumbnails) into
/// place, so we deliberately do NOT re-list the folder here: a full navigation
/// would clear and resubmit every thumbnail, flickering the gallery on each
/// swap. We only refresh the path bar, nav buttons, disk-free, watcher and the
/// tab strips.
///
/// Returns `true` when a swap happened (a split was active), `false` otherwise.
pub fn do_swap_active(app: &MainWindow, rt: &Arc<Runtime>, state: &AppStateRc, watcher: &WatcherHandle) -> bool {
    let model = state.borrow_mut().swap_active();
    let Some(model) = model else {
        debug!("swap-active-pane requested but no split is active");
        return false;
    };
    app.set_rows(ModelRc::from(model));
    publish_split(app, state);
    crate::glue::navigation::publish_active_pane(app, rt, state, watcher);
    crate::glue::tabs::publish_tabs(app, state);
    true
}

/// Push split-related view properties down to Slint.
pub fn publish_split(app: &MainWindow, state: &AppStateRc) {
    let s = state.borrow();
    let split = s.split_active();
    app.set_split_active(split);
    app.set_active_is_right(s.active_is_right);
    app.set_search_on_right(s.search_on_right);

    if let Some(p) = &s.inactive_pane {
        app.set_inactive_rows(ModelRc::from(p.rows_model.clone()));
        app.set_inactive_path(p.current.as_ref().map(|l| l.display()).unwrap_or_default().into());
        app.set_inactive_in_trash(
            p.current
                .as_ref()
                .is_some_and(crate::glue::navigation::location_is_trash),
        );
    } else {
        // Empty model so Slint stops trying to render an old one after split off.
        app.set_inactive_rows(ModelRc::from(std::rc::Rc::new(
            slint::VecModel::<crate::FileRowData>::default(),
        )));
        app.set_inactive_path("".into());
        app.set_inactive_in_trash(false);
    }
}
