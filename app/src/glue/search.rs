//! Live search across the current directory subtree.
//!
//! Pipeline:
//! 1. `open-search` sets `SearchState.active = true`. UI flips to the search-rows model.
//! 2. Each `search-query-changed(text)` bumps a generation counter and schedules a
//!    250 ms debounce. The debounce delay is dropped if a newer query arrives.
//! 3. After debounce, a `tokio::task::spawn_blocking` job walks the subtree with
//!    `walkdir` and pushes batches into a channel. The UI `Timer` pump appends
//!    matching rows to `search_rows_model` if the generation still matches.
//! 4. `close-search` clears the model + cancels any in-flight job.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use mykrut_core::{CancelFlag, FileEntry, FileType, Location, icon_for_entry};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tracing::{debug, info, info_span};

use crate::format_util::{human_mtime, human_size, kind_text};
use crate::glue::thumbnails::ThumbnailController;
use crate::state::AppStateRc;
use crate::{Callabler, FileRowData, MainWindow, SearchState};

const DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_RESULTS: usize = 5_000;

pub struct SearchController {
    generation: Arc<AtomicU64>,
    cancel: CancelFlag,
    tx: Sender<SearchMsg>,
    #[expect(dead_code)] // kept so the model's Rc isn't dropped while Slint holds a weak ref
    pub model: Rc<VecModel<FileRowData>>,
}

/// Worker-thread variant of FileRowData: no slint::Image (which is !Send).
/// Converted to FileRowData on the UI thread when applying.
pub struct SearchHit {
    pub path: std::path::PathBuf,
    pub display_name: String,
    pub icon_name: &'static str,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_hidden: bool,
    pub size: u64,
    pub mtime: std::time::SystemTime,
    pub kind: String,
}

enum SearchMsg {
    Generation(u64),
    Batch { generation: u64, hits: Vec<SearchHit> },
    Done { generation: u64 },
    Cleared,
}

pub fn install(app: &MainWindow, state: AppStateRc, thumb_ctrl: Arc<ThumbnailController>) -> Rc<SearchController> {
    let model = Rc::new(VecModel::<FileRowData>::default());
    app.set_search_rows(ModelRc::from(model.clone()));

    let (tx, rx) = channel::<SearchMsg>();
    let generation = Arc::new(AtomicU64::new(0));
    let cancel = CancelFlag::new();

    install_pump(app, state, thumb_ctrl, model.clone(), generation.clone(), rx);

    Rc::new(SearchController {
        generation,
        cancel,
        tx,
        model,
    })
}

fn install_pump(
    app: &MainWindow,
    state: AppStateRc,
    thumb_ctrl: Arc<ThumbnailController>,
    model: Rc<VecModel<FileRowData>>,
    generation: Arc<AtomicU64>,
    rx: Receiver<SearchMsg>,
) {
    let weak = app.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(60), move || {
        let Some(app) = weak.upgrade() else { return };
        let cur_gen = generation.load(Ordering::Acquire);
        let mut current_count = model.row_count();

        while let Ok(msg) = rx.try_recv() {
            match msg {
                SearchMsg::Generation(g) => {
                    // Newer search started — reset model for that generation.
                    if g == cur_gen {
                        model.set_vec(Vec::new());
                        state.borrow_mut().search_hit_paths.clear();
                        current_count = 0;
                        app.global::<SearchState>().set_searching(true);
                        app.global::<SearchState>().set_result_count(0);
                        // Drop any leftover thumbnail work (folder, or a
                        // previous query's batches) so workers immediately
                        // start on this query's hits.
                        crate::glue::thumbnails::cancel_all(&thumb_ctrl);
                    }
                }
                SearchMsg::Batch { generation: g, hits } => {
                    if g != cur_gen {
                        continue;
                    }
                    // Collect thumbnailable paths from this batch in their
                    // arrival order. They'll be enqueued at the current
                    // thumb generation so the gallery fills top-to-bottom.
                    let mut thumb_paths: Vec<std::path::PathBuf> = Vec::with_capacity(hits.len());
                    for h in hits {
                        if current_count >= MAX_RESULTS {
                            break;
                        }
                        // Mirror hit paths into AppState so row_activated /
                        // open-default-app can resolve clicked row → path.
                        state.borrow_mut().search_hit_paths.push(h.path.clone());
                        if !h.is_dir && crate::glue::thumbnails::is_thumbnailable_path(&h.path, None) {
                            thumb_paths.push(h.path.clone());
                        }
                        model.push(hit_to_row(h));
                        current_count += 1;
                    }
                    crate::glue::thumbnails::enqueue_paths(thumb_paths, &thumb_ctrl);
                    app.global::<SearchState>().set_result_count(current_count as i32);
                }
                SearchMsg::Done { generation: g } => {
                    if g == cur_gen {
                        app.global::<SearchState>().set_searching(false);
                    }
                }
                SearchMsg::Cleared => {
                    model.set_vec(Vec::new());
                    state.borrow_mut().search_hit_paths.clear();
                    app.global::<SearchState>().set_result_count(0);
                    app.global::<SearchState>().set_searching(false);
                    // Drop any in-flight search thumbnails, then re-submit
                    // the underlying folder — the user is back to looking
                    // at it and would otherwise see missing thumbnails
                    // until they navigate again.
                    crate::glue::thumbnails::cancel_all(&thumb_ctrl);
                    crate::glue::thumbnails::submit_for(&state, &thumb_ctrl);
                }
            }
        }
    });
    Box::leak(Box::new(timer));
}

pub fn wire(app: &MainWindow, rt: &Arc<tokio::runtime::Runtime>, state: AppStateRc, ctrl: Rc<SearchController>) {
    {
        let weak = app.as_weak();
        let state_for_open = state.clone();
        app.global::<Callabler>().on_open_search(move || {
            let app = weak.upgrade().expect("MainWindow alive in open-search");
            app.global::<SearchState>().set_active(true);
            app.global::<SearchState>().set_query("".into());
            app.global::<SearchState>().set_result_count(0);
            // Pin the overlay to the pane that's focused right now, and remember
            // the folder it's rooted at. Both stay put when focus later moves to
            // the other split pane (see glue::search_focused).
            let on_right = {
                let mut s = state_for_open.borrow_mut();
                s.search_on_right = s.active_is_right;
                s.search_root = s.current.clone();
                s.search_on_right
            };
            app.set_search_on_right(on_right);
        });
    }

    {
        let weak = app.as_weak();
        let ctrl = ctrl.clone();
        let state_for_close = state.clone();
        app.global::<Callabler>().on_close_search(move || {
            let app = weak.upgrade().expect("MainWindow alive in close-search");
            ctrl.cancel.cancel();
            app.global::<SearchState>().set_active(false);
            app.global::<SearchState>().set_query("".into());
            let _ = ctrl.tx.send(SearchMsg::Cleared);
            // Drop the search anchor + root so the next search starts with a
            // clean selection state.
            {
                let mut s = state_for_close.borrow_mut();
                s.search_anchor = None;
                s.search_root = None;
            }
            // After leaving search, the selected-count badge has to fall back
            // to the underlying folder's selection (which is preserved).
            app.set_selected_count(state_for_close.borrow().selected.len() as i32);
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let ctrl = ctrl;
        app.global::<Callabler>()
            .on_search_query_changed(move |q: SharedString| {
                let app = weak.upgrade().expect("MainWindow alive in search-query-changed");
                let q = q.to_string();
                app.global::<SearchState>().set_query(q.clone().into());

                // Stale anchor + count are meaningless once the result set is
                // replaced — reset them so the next click anchors cleanly.
                state.borrow_mut().search_anchor = None;
                app.set_selected_count(0);

                if q.trim().is_empty() {
                    ctrl.cancel.cancel();
                    let _ = ctrl.tx.send(SearchMsg::Cleared);
                    return;
                }

                // Root at the folder captured when search opened — not
                // `current`, which may now point at the other split pane if the
                // user clicked into it while the search overlay stayed pinned.
                let Some(Location::Local(root)) = state.borrow().search_root.clone() else {
                    return;
                };

                // Cancel previous worker and bump generation.
                ctrl.cancel.cancel();
                ctrl.cancel.reset();
                let generation = ctrl.generation.fetch_add(1, Ordering::AcqRel) + 1;
                let cancel = ctrl.cancel.clone();
                let tx = ctrl.tx.clone();

                // Debounce → execute.
                rt.spawn(async move {
                    tokio::time::sleep(DEBOUNCE).await;
                    if cancel.is_cancelled() {
                        return;
                    }
                    let _ = tx.send(SearchMsg::Generation(generation));
                    let q_lower = q.to_lowercase();
                    let span = info_span!("search", q = %q, root = %root.display(), generation);
                    let _g = span.enter();
                    info!("walking");
                    let _ =
                        tokio::task::spawn_blocking(move || walker(&root, &q_lower, &cancel, generation, &tx)).await;
                });
            });
    }
}

/// Walk the tree under `root`, pushing matching entries in 64-row batches.
fn walker(root: &std::path::Path, query_lower: &str, cancel: &CancelFlag, generation: u64, tx: &Sender<SearchMsg>) {
    let mut batch: Vec<SearchHit> = Vec::with_capacity(64);
    let mut total = 0usize;

    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        if cancel.is_cancelled() {
            break;
        }
        if total >= MAX_RESULTS {
            break;
        }
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if !name.contains(query_lower) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };

        let file_type = if meta.is_dir() {
            FileType::Directory
        } else if meta.file_type().is_symlink() {
            FileType::Symlink
        } else if meta.is_file() {
            FileType::Regular
        } else {
            FileType::Special
        };

        let path = entry.into_path();
        let mime = mime_for(&path, file_type);
        let display = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        let fe = FileEntry {
            path: path.clone(),
            display_name: display.clone(),
            file_type,
            mime: mime.clone(),
            size: meta.len(),
            mtime,
            permissions: Default::default(),
            is_hidden: display.starts_with('.'),
            is_symlink: meta.file_type().is_symlink(),
        };

        batch.push(SearchHit {
            path: path.clone(),
            display_name: display.clone(),
            icon_name: icon_for_entry(&fe),
            is_dir: fe.is_dir(),
            is_symlink: fe.is_symlink,
            is_hidden: fe.is_hidden,
            size: meta.len(),
            mtime,
            kind: kind_text(mime.as_deref(), fe.is_dir()),
        });
        total += 1;

        if batch.len() >= 64
            && tx
                .send(SearchMsg::Batch {
                    generation,
                    hits: std::mem::take(&mut batch),
                })
                .is_err()
        {
            return;
        }
    }

    if !batch.is_empty() {
        let _ = tx.send(SearchMsg::Batch {
            generation,
            hits: batch,
        });
    }
    let _ = tx.send(SearchMsg::Done { generation });
    debug!(total, "walker finished");
}

fn hit_to_row(h: SearchHit) -> FileRowData {
    let (size_lo, size_hi) = pack_u64(h.size);
    FileRowData {
        display_name: h.display_name.into(),
        icon_name: h.icon_name.into(),
        is_dir: h.is_dir,
        is_symlink: h.is_symlink,
        is_hidden: h.is_hidden,
        is_cut: false,
        selected: false,
        has_thumbnail: false,
        thumbnail: slint::Image::default(),
        size_text: if h.is_dir {
            "—".into()
        } else {
            human_size(h.size).into()
        },
        modified_text: human_mtime(h.mtime).into(),
        kind_text: h.kind.into(),
        size_lo,
        size_hi,
    }
}

fn mime_for(path: &std::path::Path, ft: FileType) -> Option<String> {
    if ft == FileType::Directory {
        return Some("inode/directory".to_string());
    }
    mime_guess::from_path(path).first().map(|m| m.essence_str().to_string())
}

fn pack_u64(v: u64) -> (i32, i32) {
    (v as i32, (v >> 32) as i32)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn tmp() -> std::path::PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("fm-test-search-{n:x}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn walker_finds_substring_matches_recursively() {
        let dir = tmp();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("report.txt"), b"").unwrap();
        fs::write(dir.join("other.md"), b"").unwrap();
        fs::write(dir.join("sub/report-2.log"), b"").unwrap();

        let (tx, rx) = channel::<SearchMsg>();
        let cancel = CancelFlag::new();
        walker(&dir, "report", &cancel, 1, &tx);

        let mut hits: Vec<String> = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let SearchMsg::Batch { hits: batch, .. } = msg {
                for h in batch {
                    hits.push(h.display_name);
                }
            }
        }
        hits.sort();
        assert_eq!(hits, vec!["report-2.log", "report.txt"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn walker_is_case_insensitive() {
        let dir = tmp();
        fs::write(dir.join("README.md"), b"").unwrap();
        fs::write(dir.join("notes.txt"), b"").unwrap();

        let (tx, rx) = channel::<SearchMsg>();
        let cancel = CancelFlag::new();
        walker(&dir, "readme", &cancel, 1, &tx);

        let mut hits = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let SearchMsg::Batch { hits: b, .. } = msg {
                for h in b {
                    hits.push(h.display_name);
                }
            }
        }
        assert_eq!(hits, vec!["README.md"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn walker_respects_cancel() {
        let dir = tmp();
        for i in 0..200 {
            fs::write(dir.join(format!("f{i}.txt")), b"").unwrap();
        }
        let cancel = CancelFlag::new();
        cancel.cancel(); // pre-cancelled

        let (tx, rx) = channel::<SearchMsg>();
        walker(&dir, "f", &cancel, 1, &tx);

        let mut total = 0usize;
        while let Ok(msg) = rx.try_recv() {
            if let SearchMsg::Batch { hits, .. } = msg {
                total += hits.len();
            }
        }
        assert_eq!(total, 0, "cancelled walker should produce no hits");
        let _ = fs::remove_dir_all(&dir);
    }
}
