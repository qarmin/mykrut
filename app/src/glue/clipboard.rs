use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use mykrut_core::{CancelFlag, Conflict, Location, Op, PauseFlag, Progress};
use slint::{ComponentHandle, Timer, TimerMode};
use tokio::runtime::Runtime;
use tracing::{error, info, info_span, warn};

use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{Callabler, MainWindow, ProgressData, ProgressDialogCallbacks, Translations};

/// The user's answer to one conflict prompt, sent from the UI back to the
/// blocked worker thread.
struct ConflictResponse {
    decision: Conflict,
    apply_all: bool,
}

thread_local! {
    /// Kept alive for the app's lifetime: on X11 the clipboard contents are
    /// served by this instance's background thread, so dropping it would drop
    /// what we copied. Lazily created on first use.
    static SYS_CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };

    /// Sender the conflict dialog uses to answer the worker. Replaced at the
    /// start of each transfer; `None` between transfers.
    static CONFLICT_RESP: RefCell<Option<Sender<ConflictResponse>>> = const { RefCell::new(None) };
}

/// Put `text` on the system (cross-application) clipboard as plain text.
fn set_system_clipboard_text(text: String) {
    SYS_CLIPBOARD.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => *slot = Some(cb),
                Err(err) => {
                    warn!(?err, "system clipboard unavailable");
                    return;
                }
            }
        }
        if let Some(cb) = slot.as_mut()
            && let Err(err) = cb.set_text(text)
        {
            warn!(?err, "failed to set system clipboard");
        }
    });
}

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_clipboard_cut(move || {
            let app = weak.upgrade().expect("MainWindow alive in cut");
            put_into_clipboard(&app, &state, true);
        });
    }

    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_clipboard_copy(move || {
            let app = weak.upgrade().expect("MainWindow alive in copy");
            put_into_clipboard(&app, &state, false);
        });
    }

    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_copy_path(move || {
            let app = weak.upgrade().expect("MainWindow alive in copy-path");
            let paths = selected_paths(&app, &state);
            if paths.is_empty() {
                return;
            }
            // One path per line (the common "copy paths" convention).
            let text = paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            info!(count = paths.len(), "copy path(s) to system clipboard");
            set_system_clipboard_text(text);
        });
    }

    // Cancel/pause flags and pollable channel live for the app lifetime — paste reuses them.
    let cancel = CancelFlag::new();
    let pause = PauseFlag::new();
    let (tx, rx) = channel::<ProgressMsg>();
    install_progress_pump(app, rx);

    {
        let cancel = cancel.clone();
        let pause = pause.clone();
        app.global::<ProgressDialogCallbacks>().on_cancel_requested(move || {
            info!("user cancelled");
            cancel.cancel();
            // A cancel while paused must un-stick the worker's pause loop.
            pause.set_paused(false);
            // Also trip the shared flag so a running compress (archive.rs) stops.
            crate::glue::transfer_cancel().cancel();
        });
    }

    {
        let weak = app.as_weak();
        let pause = pause.clone();
        app.global::<ProgressDialogCallbacks>()
            .on_pause_requested(move |paused| {
                info!(paused, "pause toggled");
                pause.set_paused(paused);
                if let Some(app) = weak.upgrade() {
                    let mut pd = app.get_progress();
                    pd.paused = paused;
                    app.set_progress(pd);
                }
            });
    }

    {
        let weak = app.as_weak();
        app.global::<Callabler>()
            .on_conflict_chosen(move |decision_i, apply_all| {
                let decision = match decision_i {
                    0 => Conflict::Overwrite,
                    1 => Conflict::Skip,
                    2 => Conflict::KeepBoth,
                    _ => Conflict::Cancel,
                };
                info!(?decision, apply_all, "conflict resolved");
                CONFLICT_RESP.with(|c| {
                    if let Some(tx) = c.borrow().as_ref() {
                        let _ = tx.send(ConflictResponse { decision, apply_all });
                    }
                });
                if let Some(app) = weak.upgrade() {
                    app.global::<crate::DialogState>().set_conflict_open(false);
                }
            });
    }

    // Internal drag-and-drop shares paste's progress channel + cancel flag.
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_start_drag(move |idx| {
            let app = weak.upgrade().expect("MainWindow alive in start-drag");
            start_drag(&app, &state, idx);
        });
    }
    {
        let weak = app.as_weak();
        let rt_d = rt.clone();
        let state_d = state.clone();
        let watcher_d = watcher.clone();
        let cancel_d = cancel.clone();
        let pause_d = pause.clone();
        let tx_d = tx.clone();
        app.global::<Callabler>().on_drop_dragged(move |copy| {
            let app = weak.upgrade().expect("MainWindow alive in drop-dragged");
            drop_dragged(
                &app,
                &rt_d,
                &state_d,
                &watcher_d,
                copy,
                cancel_d.clone(),
                pause_d.clone(),
                tx_d.clone(),
            );
        });
    }
    {
        let weak = app.as_weak();
        let state_c = state.clone();
        app.global::<Callabler>().on_cancel_drag(move || {
            if let Some(app) = weak.upgrade() {
                clear_drag(&app, &state_c);
            }
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_clipboard_paste(move || {
            let app = weak.upgrade().expect("MainWindow alive in paste");
            do_paste(&app, &rt, &state, &watcher, cancel.clone(), pause.clone(), tx.clone());
        });
    }
}

/// Capture the items to drag (the multi-selection if the pressed row is part of
/// it, otherwise just that row) and switch DragState on so the ghost appears.
/// No-op while a search overlay is focused (search results aren't draggable).
fn start_drag(app: &MainWindow, state: &AppStateRc, idx: i32) {
    if crate::glue::search_focused(app, state) || idx < 0 {
        return;
    }
    let idx = idx as usize;
    let (paths, label, icon) = {
        let s = state.borrow();
        let Some(this) = s.entries.get(idx) else {
            return;
        };
        if s.selected.contains(&idx) && s.selected.len() > 1 {
            let mut sel: Vec<usize> = s.selected.iter().copied().collect();
            sel.sort_unstable();
            let paths: Vec<PathBuf> = sel
                .iter()
                .filter_map(|&i| s.entries.get(i).map(|e| e.path.clone()))
                .collect();
            let count = paths.len();
            (paths, format!("{count} items"), "file-generic".to_string())
        } else {
            (
                vec![this.path.clone()],
                this.display_name.clone(),
                mykrut_core::icon_for_entry(this).to_string(),
            )
        }
    };
    if paths.is_empty() {
        return;
    }
    let count = paths.len() as i32;
    state.borrow_mut().drag_paths = paths;

    let ds = app.global::<crate::DragState>();
    ds.set_count(count);
    ds.set_label(label.into());
    ds.set_icon(icon.into());
    ds.set_has_target(false);
    ds.set_target_path("".into());
    ds.set_target_row(-1);
    ds.set_active(true);
}

/// Released a drag: if it ended over a folder, move (or copy) the dragged items
/// into it. Guards against no-op moves and dropping a folder into its own tree.
fn drop_dragged(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: &AppStateRc,
    watcher: &WatcherHandle,
    copy: bool,
    cancel: CancelFlag,
    pause: PauseFlag,
    tx: Sender<ProgressMsg>,
) {
    let ds = app.global::<crate::DragState>();
    let active = ds.get_active();
    let has_target = ds.get_has_target();
    let target_path = ds.get_target_path().to_string();
    let target_row = ds.get_target_row();
    let target_active = ds.get_target_active_pane();
    ds.set_active(false);
    ds.set_has_target(false);
    ds.set_target_path("".into());
    ds.set_target_row(-1);

    let paths = std::mem::take(&mut state.borrow_mut().drag_paths);
    if !active || !has_target || paths.is_empty() {
        return;
    }

    // A path-based target (sidebar place / tab) wins; otherwise resolve the
    // file-row index against the right pane's entries.
    let dest = if !target_path.is_empty() {
        let p = PathBuf::from(&target_path);
        if p.is_dir() {
            p
        } else {
            return;
        }
    } else if target_row >= 0 {
        let s = state.borrow();
        let entry = if target_active {
            s.entries.get(target_row as usize).cloned()
        } else {
            s.inactive_pane
                .as_ref()
                .and_then(|p| p.entries.get(target_row as usize).cloned())
        };
        match entry {
            Some(e) if e.is_dir() => e.path,
            _ => return,
        }
    } else {
        return;
    };

    let srcs: Vec<PathBuf> = paths
        .into_iter()
        .filter(|src| {
            // Not onto itself, not into its own subtree, not a no-op (already
            // a direct child of dest).
            dest != *src && !dest.starts_with(src) && src.parent() != Some(dest.as_path())
        })
        .collect();
    if srcs.is_empty() {
        return;
    }

    let op = if copy { Op::Copy } else { Op::Move };
    info!(count = srcs.len(), ?op, dest = %dest.display(), "drop");
    run_transfer(app, rt, state, watcher, srcs, dest, op, cancel, pause, tx);
}

fn clear_drag(app: &MainWindow, state: &AppStateRc) {
    let ds = app.global::<crate::DragState>();
    ds.set_active(false);
    ds.set_has_target(false);
    ds.set_target_path("".into());
    ds.set_target_row(-1);
    state.borrow_mut().drag_paths.clear();
}

/// Messages from worker to UI.
enum ProgressMsg {
    Update(Progress),
    /// A destination already exists; the worker is blocked awaiting the user's
    /// choice (answered via [`CONFLICT_RESP`]).
    Conflict(PathBuf),
    Done,
}

/// Slint Timer polls the receiver every 50 ms and pushes updates into the UI model.
fn install_progress_pump(app: &MainWindow, rx: Receiver<ProgressMsg>) {
    let weak = app.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(app) = weak.upgrade() else { return };
        let mut latest: Option<Progress> = None;
        let mut done = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ProgressMsg::Update(p) => latest = Some(p),
                ProgressMsg::Conflict(path) => {
                    // Worker is blocked until `conflict-chosen` answers. Show the
                    // dialog; no more messages will arrive until the user picks.
                    let ds = app.global::<crate::DialogState>();
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ds.set_conflict_name(name.into());
                    ds.set_conflict_apply_all(false);
                    ds.set_conflict_open(true);
                }
                ProgressMsg::Done => done = true,
            }
        }
        if let Some(p) = latest {
            let paused = app.get_progress().paused;
            let pct = if p.bytes_total > 0 {
                p.bytes_done as f32 / p.bytes_total as f32
            } else {
                0.0
            };
            let (bd_lo, bd_hi) = pack_u64(p.bytes_done);
            let (bt_lo, bt_hi) = pack_u64(p.bytes_total);
            let op_text = match p.op {
                Op::Copy => app.global::<Translations>().get_progress_op_copy(),
                Op::Move => app.global::<Translations>().get_progress_op_move(),
            };
            let current = p
                .current
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            app.set_progress(ProgressData {
                visible: true,
                op: op_text,
                current: current.into(),
                files_done: p.files_done as i32,
                files_total: p.files_total as i32,
                percent: pct,
                bytes_done_lo: bd_lo,
                bytes_done_hi: bd_hi,
                bytes_total_lo: bt_lo,
                bytes_total_hi: bt_hi,
                bytes_text: format!(
                    "{} {} {}",
                    crate::format_util::human_size(p.bytes_done),
                    app.global::<Translations>().get_progress_files_of(),
                    crate::format_util::human_size(p.bytes_total)
                )
                .into(),
                paused,
            });
        }
        if done {
            let mut p = app.get_progress();
            p.visible = false;
            app.set_progress(p);
        }
    });
    // Keep the timer alive for the app lifetime.
    Box::leak(Box::new(timer));
}

fn selected_paths(app: &MainWindow, state: &AppStateRc) -> Vec<PathBuf> {
    use slint::Model;
    if crate::glue::search_focused(app, state) {
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

fn put_into_clipboard(app: &MainWindow, state: &AppStateRc, cut: bool) {
    let paths = selected_paths(app, state);
    let count = paths.len();
    let _g = info_span!("clipboard", op = if cut { "cut" } else { "copy" }, count).entered();
    if paths.is_empty() {
        info!("nothing selected");
        return;
    }
    {
        let mut s = state.borrow_mut();
        s.clipboard.paths = paths;
        s.clipboard.cut = cut;
    }
    app.set_clipboard_has_items(true);
    info!("stored");
    crate::glue::navigation::mark_clipboard_visuals(state);
}

#[expect(clippy::too_many_arguments)]
fn do_paste(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: &AppStateRc,
    watcher: &WatcherHandle,
    cancel: CancelFlag,
    pause: PauseFlag,
    tx: Sender<ProgressMsg>,
) {
    let (paths, cut) = {
        let s = state.borrow();
        (s.clipboard.paths.clone(), s.clipboard.cut)
    };
    if paths.is_empty() {
        return;
    }
    let Some(dest_dir) = paste_target(state, &paths) else {
        return;
    };
    let op = if cut { Op::Move } else { Op::Copy };
    let span = info_span!("paste", count = paths.len(), op = ?op, dest = %dest_dir.display());
    let _g = span.enter();
    info!("begin");

    // A cut is consumed by the paste: drop it from the clipboard now (the paths
    // are already handed to the worker). The view refresh at the end rebuilds
    // rows with an empty clipboard, clearing every "CUT" badge.
    if cut {
        let mut s = state.borrow_mut();
        s.clipboard.paths.clear();
        s.clipboard.cut = false;
        drop(s);
        app.set_clipboard_has_items(false);
    }

    run_transfer(app, rt, state, watcher, paths, dest_dir, op, cancel, pause, tx);
}

/// Copy or move `paths` into `dest_dir` with the shared progress dialog, then
/// refresh both panes. Used by clipboard paste and by drag-and-drop.
#[expect(clippy::too_many_arguments)]
fn run_transfer(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: &AppStateRc,
    watcher: &WatcherHandle,
    paths: Vec<PathBuf>,
    dest_dir: PathBuf,
    op: Op,
    cancel: CancelFlag,
    pause: PauseFlag,
    tx: Sender<ProgressMsg>,
) {
    // Show dialog immediately with a placeholder so user gets instant feedback.
    let initial = ProgressData {
        visible: true,
        op: match op {
            Op::Copy => app.global::<Translations>().get_progress_op_copy(),
            Op::Move => app.global::<Translations>().get_progress_op_move(),
        },
        current: "".into(),
        files_done: 0,
        files_total: 0,
        percent: 0.0,
        bytes_done_lo: 0,
        bytes_done_hi: 0,
        bytes_total_lo: 0,
        bytes_total_hi: 0,
        bytes_text: "".into(),
        paused: false,
    };
    app.set_progress(initial);

    cancel.reset();
    pause.set_paused(false);
    // Per-transfer channel the conflict dialog answers on. The worker blocks on
    // the receiver; the UI's `conflict-chosen` callback sends through the stashed
    // sender. Set before the worker starts so an early conflict has somewhere to
    // reply to.
    let (resp_tx, resp_rx) = channel::<ConflictResponse>();
    CONFLICT_RESP.with(|c| *c.borrow_mut() = Some(resp_tx));

    let weak = app.as_weak();
    let state_clone = state.clone();
    let watcher_clone = watcher.clone();
    let rt_inner = rt.clone();

    let _ = slint::spawn_local(async move {
        let cancel_for_worker = cancel.clone();
        let pause_for_worker = pause.clone();
        let tx_for_worker = tx.clone();
        let tx_conf = tx.clone();
        let join = rt_inner.spawn_blocking(move || {
            // Resolver runs on the worker thread: ask the UI (and block) on the
            // first conflict, caching the answer if "apply to all" was checked.
            let mut apply_all: Option<mykrut_core::Conflict> = None;
            let resolver = move |existing: &std::path::Path| -> mykrut_core::Conflict {
                if let Some(d) = apply_all {
                    return d;
                }
                let _ = tx_conf.send(ProgressMsg::Conflict(existing.to_path_buf()));
                match resp_rx.recv() {
                    Ok(r) => {
                        if r.apply_all {
                            apply_all = Some(r.decision);
                        }
                        r.decision
                    }
                    Err(_) => mykrut_core::Conflict::Cancel,
                }
            };
            mykrut_core::run_copy_with(
                &paths,
                &dest_dir,
                op,
                &cancel_for_worker,
                &pause_for_worker,
                resolver,
                move |p| {
                    let _ = tx_for_worker.send(ProgressMsg::Update(p));
                },
            )
        });

        let outcome = match join.await {
            Ok(o) => o,
            Err(err) => {
                // The blocking worker itself panicked: surface it rather than
                // leaving the progress dialog stuck, and stop here.
                error!(?err, "transfer worker panicked");
                let _ = tx.send(ProgressMsg::Done);
                if let Some(app) = weak.upgrade() {
                    let mut pd = app.get_progress();
                    pd.visible = false;
                    app.set_progress(pd);
                }
                return;
            }
        };
        let _ = tx.send(ProgressMsg::Done);

        info!(
            succeeded = outcome.succeeded,
            errors = outcome.errors.len(),
            cancelled = outcome.cancelled,
            "transfer done"
        );

        let Some(app) = weak.upgrade() else {
            return;
        };

        // Nemo-style summary: if some items could not be copied/moved, tell the
        // user which ones instead of silently dropping them.
        if !outcome.errors.is_empty() {
            crate::glue::navigation::show_operation_error(&app, op, &outcome.errors);
        }

        // Record for undo (Ctrl+Z). Skip if nothing actually transferred.
        if !outcome.dests.is_empty() {
            let undo_op = match op {
                Op::Copy => crate::state::UndoOp::Copy {
                    dests: outcome.dests.into_iter().map(|(_, d)| d).collect(),
                },
                Op::Move => crate::state::UndoOp::Move { moves: outcome.dests },
            };
            crate::glue::undo::record(&state_clone, undo_op);
        }

        // Force-hide the progress dialog (the pump's Done message also does this,
        // but there can be a race if the user closed the window quickly).
        let mut pd = app.get_progress();
        pd.visible = false;
        app.set_progress(pd);

        if let Some(cur) = current_dir(&state_clone) {
            crate::glue::navigation::navigate_internal_refresh(
                &app,
                &rt_inner,
                state_clone.clone(),
                watcher_clone,
                Location::Local(cur),
            );
        }
        // A move's source folder may be displayed in the other split pane; its
        // row would otherwise hang around with a stale "CUT" badge. Re-list it
        // so moved-away files disappear and the badge clears.
        crate::glue::navigation::refresh_inactive_pane(&app, &rt_inner, &state_clone);
    });
}

fn current_dir(state: &AppStateRc) -> Option<PathBuf> {
    match state.borrow().current.clone()? {
        Location::Local(p) => Some(p),
        Location::Trash => None,
    }
}

/// Resolve the paste destination: if exactly one directory is selected,
/// paste *into* it; otherwise fall back to the current directory.
///
/// Refuses to use the selected directory if it's also one of the clipboard
/// sources (would mean pasting a folder into itself).
fn paste_target(state: &AppStateRc, clipboard_paths: &[PathBuf]) -> Option<PathBuf> {
    let selected_dir = {
        let s = state.borrow();
        if s.selected.len() == 1 {
            s.selected
                .iter()
                .next()
                .and_then(|&i| s.entries.get(i))
                .filter(|e| e.is_dir() && !e.is_symlink)
                .map(|e| e.path.clone())
        } else {
            None
        }
    };
    if let Some(dir) = selected_dir
        && !clipboard_paths.iter().any(|p| p == &dir)
    {
        return Some(dir);
    }
    current_dir(state)
}

fn pack_u64(v: u64) -> (i32, i32) {
    (v as i32, (v >> 32) as i32)
}
