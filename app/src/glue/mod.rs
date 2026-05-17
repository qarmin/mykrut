use std::sync::Arc;

use slint::ComponentHandle;
use tokio::runtime::Runtime;

use crate::MainWindow;
use crate::state::AppStateRc;

pub mod archive;
pub mod clipboard;
pub mod default_app;
pub mod file_ops;
pub mod mtp;
pub mod navigation;
pub mod open_with;
pub mod places;
pub mod properties;
pub mod remote;
pub mod search;
pub mod selection;
pub mod sort_select;
pub mod spawn;
pub mod split;
pub mod tabs;
pub mod thumbnails;
pub mod udisks;
pub mod undo;
pub mod watcher;

/// True when search is active **and** the currently-focused (active) pane is
/// the pane that owns the search overlay.
///
/// Every selection / clipboard / open / archive operation targets the active
/// pane, so they must treat the visible listing as search results only under
/// this condition. When the user clicks into the *other* split pane (which
/// swaps focus, leaving the search overlay pinned to its origin side) those
/// operations should act on that pane's normal folder listing instead.
/// Process-wide cancel flag for the foreground transfer/compress operation.
/// Only one such operation runs at a time (the progress dialog is modal), so a
/// single shared flag is enough: the dialog's Cancel button trips it and both
/// the copy/move worker and the compress worker poll it.
pub fn transfer_cancel() -> mykrut_core::CancelFlag {
    use std::sync::OnceLock;
    static C: OnceLock<mykrut_core::CancelFlag> = OnceLock::new();
    C.get_or_init(mykrut_core::CancelFlag::new).clone()
}

pub fn search_focused(app: &MainWindow, state: &AppStateRc) -> bool {
    if !app.global::<crate::SearchState>().get_active() {
        return false;
    }
    let s = state.borrow();
    s.search_on_right == s.active_is_right
}

pub fn wire_all(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc) -> watcher::WatcherHandle {
    let watcher = watcher::WatcherHandle::new(rt.clone());
    let thumb_ctrl = thumbnails::install(app, state.clone());
    navigation::wire_with_thumbnails(app, rt, state.clone(), watcher.clone(), thumb_ctrl.clone());
    sort_select::wire(app, state.clone());
    selection::wire(app, state.clone());
    let _g = rt.enter();
    clipboard::wire(app, rt, state.clone(), watcher.clone());
    file_ops::wire(app, rt, state.clone(), watcher.clone());
    properties::wire(app, rt, state.clone());
    let search_ctrl = search::install(app, state.clone(), thumb_ctrl);
    search::wire(app, rt, state.clone(), search_ctrl);
    let places_ctrl = places::PlacesController::install(app);
    let udisks_ctrl = udisks::install(app, rt);
    places_ctrl.borrow_mut().set_udisks(udisks_ctrl);
    let mtp_ctrl = mtp::install(app, rt);
    places_ctrl.borrow_mut().set_mtp(mtp_ctrl);
    places::wire(app, rt, state.clone(), watcher.clone(), places_ctrl);
    tabs::wire(app, rt, state.clone(), watcher.clone());
    split::wire(app, rt, state.clone(), watcher.clone());
    undo::wire(app, rt, state.clone(), watcher.clone());
    spawn::wire(app, rt, state.clone(), watcher.clone());
    archive::wire(app, rt, state.clone(), watcher.clone());
    remote::wire(app, rt, state.clone(), watcher.clone());
    default_app::wire(app, rt, state.clone());
    open_with::wire(app, state);
    watcher::wire(app, rt, watcher.clone());
    watcher
}
