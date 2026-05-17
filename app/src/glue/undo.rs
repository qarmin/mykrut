//! Undo / redo for reversible file operations (Ctrl+Z / Ctrl+Y).
//!
//! Only operations with a non-destructive inverse are tracked, so an undo can
//! never silently lose data:
//!   * **Rename** — undo renames back to the old name; redo re-applies.
//!   * **Create** (new empty file / new folder) — undo sends the created item to
//!     the trash (recoverable); redo re-creates the empty file/folder.
//!   * **Trash** — undo restores the items from the trash to their original
//!     location; redo moves them back to the trash.
//!
//! Copy / move are intentionally *not* undoable yet: reversing them correctly
//! (cross-device, collision-renamed destinations) needs more bookkeeping than
//! is safe to guess at. Each apply re-lists the active pane (and the inactive
//! split pane) afterwards so the view reflects reality.

use std::path::Path;
use std::sync::Arc;

use mykrut_core::Location;
use slint::ComponentHandle;
use tokio::runtime::Runtime;
use tracing::{info, info_span, warn};

use crate::glue::watcher::WatcherHandle;
use crate::state::{AppStateRc, UndoOp};
use crate::{Callabler, MainWindow};

/// Record a freshly-performed operation so it can be undone. Clears the redo
/// stack (standard linear-history behaviour).
pub fn record(state: &AppStateRc, op: UndoOp) {
    let mut s = state.borrow_mut();
    s.undo.undo.push(op);
    s.undo.redo.clear();
}

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher.clone();
        app.global::<Callabler>().on_undo(move || {
            let app = weak.upgrade().expect("MainWindow alive in undo");
            let Some(op) = state.borrow_mut().undo.undo.pop() else {
                info!("nothing to undo");
                return;
            };
            apply(&app, &rt, &state, &watcher, op, true);
        });
    }
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        app.global::<Callabler>().on_redo(move || {
            let app = weak.upgrade().expect("MainWindow alive in redo");
            let Some(op) = state.borrow_mut().undo.redo.pop() else {
                info!("nothing to redo");
                return;
            };
            apply(&app, &rt, &state, &watcher, op, false);
        });
    }
}

/// Run the (inverse, when `is_undo`) of `op` off the UI thread, then push it
/// onto the opposite history stack and refresh the view. On failure the op is
/// pushed back where it came from so the user can retry.
fn apply(app: &MainWindow, rt: &Arc<Runtime>, state: &AppStateRc, watcher: &WatcherHandle, op: UndoOp, is_undo: bool) {
    let span = info_span!("undo_apply", undo = is_undo, ?op);
    let _g = span.enter();

    let weak = app.as_weak();
    let rt_inner = rt.clone();
    let state_clone = state.clone();
    let watcher_clone = watcher.clone();
    let op_for_task = op.clone();

    let _ = slint::spawn_local(async move {
        let ok = rt_inner
            .spawn_blocking(move || perform(&op_for_task, is_undo))
            .await
            .unwrap_or(false);

        let Some(app) = weak.upgrade() else { return };
        {
            let mut s = state_clone.borrow_mut();
            if ok {
                // Success → the op now lives on the opposite stack.
                if is_undo {
                    s.undo.redo.push(op);
                } else {
                    s.undo.undo.push(op);
                }
            } else {
                // Failed → put it back so a later attempt can retry.
                if is_undo {
                    s.undo.undo.push(op);
                } else {
                    s.undo.redo.push(op);
                }
            }
        }
        if ok {
            if let Some(Location::Local(cur)) = state_clone.borrow().current.clone() {
                crate::glue::navigation::navigate_internal_refresh(
                    &app,
                    &rt_inner,
                    state_clone.clone(),
                    watcher_clone,
                    Location::Local(cur),
                );
            }
            crate::glue::navigation::refresh_inactive_pane(&app, &rt_inner, &state_clone);
        }
    });
}

/// Execute the filesystem side of an undo/redo. Returns `true` on success.
/// Heavily guarded with existence checks so a stale history entry is skipped
/// rather than clobbering an unrelated file.
fn perform(op: &UndoOp, is_undo: bool) -> bool {
    match op {
        UndoOp::Rename { from, to } => {
            // undo: to → from ; redo: from → to.
            let (src, dst) = if is_undo { (to, from) } else { (from, to) };
            rename(src, dst)
        }
        UndoOp::Create { path, is_dir } => {
            if is_undo {
                // Send the just-created item to the trash (recoverable).
                match mykrut_core::move_to_trash(std::slice::from_ref(path)) {
                    Ok(_) => {
                        info!(path = %path.display(), "undo create → trashed");
                        true
                    }
                    Err(err) => {
                        warn!(?err, path = %path.display(), "undo create failed");
                        false
                    }
                }
            } else {
                recreate(path, *is_dir)
            }
        }
        UndoOp::Trash { originals } => {
            if is_undo {
                // Restore each item from the trash back to its original path.
                let mut all_ok = true;
                for orig in originals {
                    if let Err(err) = mykrut_core::trash_io::restore_by_original(orig) {
                        warn!(?err, path = %orig.display(), "undo trash (restore) failed");
                        all_ok = false;
                    }
                }
                all_ok
            } else {
                match mykrut_core::move_to_trash(originals) {
                    Ok(_) => true,
                    Err(err) => {
                        warn!(?err, "redo trash failed");
                        false
                    }
                }
            }
        }
        UndoOp::Copy { dests } => {
            if is_undo {
                // Trash the copies (recoverable). Only act on ones still present.
                let present: Vec<_> = dests.iter().filter(|p| p.exists()).cloned().collect();
                if present.is_empty() {
                    return true;
                }
                match mykrut_core::move_to_trash(&present) {
                    Ok(_) => true,
                    Err(err) => {
                        warn!(?err, "undo copy (trash) failed");
                        false
                    }
                }
            } else {
                // Redo: bring the trashed copies back to where they were.
                let mut all_ok = true;
                for d in dests {
                    if let Err(err) = mykrut_core::trash_io::restore_by_original(d) {
                        warn!(?err, path = %d.display(), "redo copy (restore) failed");
                        all_ok = false;
                    }
                }
                all_ok
            }
        }
        UndoOp::Move { moves } => {
            // undo: dest → src ; redo: src → dest.
            let mut all_ok = true;
            for (src, dest) in moves {
                let (from, to) = if is_undo { (dest, src) } else { (src, dest) };
                if !rename(from, to) {
                    all_ok = false;
                }
            }
            all_ok
        }
    }
}

fn rename(src: &Path, dst: &Path) -> bool {
    if !src.exists() {
        warn!(src = %src.display(), "rename undo/redo: source missing");
        return false;
    }
    if dst.exists() {
        warn!(dst = %dst.display(), "rename undo/redo: destination already exists");
        return false;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => {
            info!(src = %src.display(), dst = %dst.display(), "renamed");
            true
        }
        Err(err) => {
            warn!(?err, "rename undo/redo failed");
            false
        }
    }
}

fn recreate(path: &Path, is_dir: bool) -> bool {
    if path.exists() {
        // Already present (perhaps user recreated it) — treat as done.
        return true;
    }
    let res = if is_dir {
        std::fs::create_dir(path)
    } else {
        std::fs::File::create(path).map(|_| ())
    };
    match res {
        Ok(()) => {
            info!(path = %path.display(), is_dir, "recreated");
            true
        }
        Err(err) => {
            warn!(?err, path = %path.display(), "recreate failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::state::UndoOp;

    fn tmp() -> std::path::PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("fm-test-undo-{n:x}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn move_undo_then_redo_round_trips() {
        let dir = tmp();
        let src = dir.join("a.txt");
        let dest = dir.join("sub").join("a.txt");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(&dest, b"x").unwrap(); // simulate the file already moved to dest

        let op = UndoOp::Move {
            moves: vec![(src.clone(), dest.clone())],
        };
        // Undo: dest → src.
        assert!(super::perform(&op, true));
        assert!(src.exists() && !dest.exists());
        // Redo: src → dest.
        assert!(super::perform(&op, false));
        assert!(dest.exists() && !src.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
