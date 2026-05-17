use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use mykrut_core::{FileEntry, Location};
use slint::VecModel;

use crate::FileRowData;

/// One tab's persistent content (the non-active tabs of the active pane sit here).
#[derive(Default)]
pub struct TabSnapshot {
    pub rows_model: Option<Rc<VecModel<FileRowData>>>,
    pub back_stack: Vec<Location>,
    pub forward_stack: Vec<Location>,
    pub current: Option<Location>,
    pub entries: Vec<FileEntry>,
    /// Full unfiltered listing. `entries` is the filtered+sorted view derived
    /// from this; kept so toggling "show hidden" can re-include dropped items.
    pub all_entries: Vec<FileEntry>,
    pub selected: HashSet<usize>,
    pub anchor: Option<usize>,
    pub title: String,
}

/// Full state of one pane. When this pane is *active*, these fields live
/// flattened up to `AppState` so callbacks don't care about panes. When the
/// pane is *inactive* (the non-focused side of a split view) the struct holds
/// the snapshot in `inactive_pane`.
pub struct PaneState {
    pub rows_model: Rc<VecModel<FileRowData>>,
    pub back_stack: Vec<Location>,
    pub forward_stack: Vec<Location>,
    pub current: Option<Location>,
    pub entries: Vec<FileEntry>,
    pub all_entries: Vec<FileEntry>,
    pub selected: HashSet<usize>,
    pub anchor: Option<usize>,
    pub tabs: Vec<TabSnapshot>,
    pub active_tab_idx: usize,
}

#[derive(Default)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// A reversible filesystem action recorded for undo/redo.
///
/// Only operations with a non-destructive inverse are recorded, so an undo can
/// never silently destroy data:
/// * `Rename` ↔ rename back to the previous name.
/// * `Create` ↔ the inverse is "remove what we just created" (sent to trash, so
///   it stays recoverable); redo re-creates the empty file/folder.
/// * `Trash` ↔ restore from trash; redo re-trashes.
/// * `Copy` ↔ trash the created copies; redo restores them from trash.
/// * `Move` ↔ move each item back to its original location; redo re-moves.
#[derive(Clone, Debug)]
pub enum UndoOp {
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    Create {
        path: PathBuf,
        is_dir: bool,
    },
    /// Files moved to trash; undo restores them, redo re-trashes.
    Trash {
        originals: Vec<PathBuf>,
    },
    /// Files copied in. `dests` are the paths actually created (post conflict
    /// resolution). Undo trashes them (recoverable); redo restores from trash.
    Copy {
        dests: Vec<PathBuf>,
    },
    /// Files moved. Each pair is (original source, final destination). Undo
    /// moves dest → src; redo moves src → dest.
    Move {
        moves: Vec<(PathBuf, PathBuf)>,
    },
}

/// Two-stack undo/redo history. A new recorded action clears the redo stack
/// (standard linear-history semantics).
#[derive(Default)]
pub struct UndoStack {
    pub undo: Vec<UndoOp>,
    pub redo: Vec<UndoOp>,
}

pub struct AppState {
    // ── Active pane × active tab — flattened ──
    pub rows_model: Rc<VecModel<FileRowData>>,
    pub back_stack: Vec<Location>,
    pub forward_stack: Vec<Location>,
    pub current: Option<Location>,
    pub entries: Vec<FileEntry>,
    pub all_entries: Vec<FileEntry>,
    pub selected: HashSet<usize>,
    pub anchor: Option<usize>,

    // ── Active pane's tabs ──
    pub tabs: Vec<TabSnapshot>,
    pub active_tab_idx: usize,

    // ── Shared across panes ──
    pub clipboard: Clipboard,
    /// Undo/redo history of reversible file operations (rename, create, trash).
    pub undo: UndoStack,
    /// Absolute paths currently being dragged (internal drag-and-drop). Captured
    /// at drag start; consumed on drop.
    pub drag_paths: Vec<PathBuf>,
    /// Selection snapshot taken when a rubber-band drag begins. With Ctrl held,
    /// the band toggles intersected items against this base (Nemo-style XOR).
    pub rubberband_base: HashSet<usize>,
    /// Path to select + scroll into view once the next listing finishes. Set by
    /// "open containing folder" (and any reveal-an-item flow); consumed by the
    /// navigation success path. Cleared whether or not the path was found.
    pub pending_select: Option<PathBuf>,

    // ── Search-mode anchor (for shift-click range select on search results).
    // Search results live in a separate model owned by SearchController, but
    // selection visuals are still toggled here so shift-click has somewhere
    // to anchor.
    pub search_anchor: Option<usize>,
    /// Parallel to the search-rows model: hit_paths[i] is the absolute path of
    /// the i-th search result. Lets row-activated / open-default-app / clipboard
    /// ops resolve row index → path correctly while search is active (the
    /// regular `entries` array still holds the underlying folder).
    pub search_hit_paths: Vec<PathBuf>,
    /// Which physical side (left=false / right=true) hosts the search overlay.
    /// Set when search opens (= the then-active side) and deliberately NOT
    /// changed by `swap_active`, so the search results stay pinned to the pane
    /// they were started in even after the user clicks into the other pane.
    pub search_on_right: bool,
    /// Folder the active search is rooted at, captured when search opens. Used
    /// instead of `current` so re-running a query keeps targeting the right
    /// folder even when the search pane is no longer the active one.
    pub search_root: Option<Location>,

    // ── Split view ──
    /// When `Some`, a second pane is shown. Whether it's physically left or
    /// right is governed by `active_is_right`.
    pub inactive_pane: Option<PaneState>,
    pub active_is_right: bool,
}

pub type AppStateRcInner = Rc<RefCell<AppState>>;

#[derive(Clone)]
pub struct AppStateRc(pub AppStateRcInner);

impl AppStateRc {
    pub fn new(initial_rows_model: Rc<VecModel<FileRowData>>) -> Self {
        let tabs = vec![TabSnapshot {
            rows_model: Some(initial_rows_model.clone()),
            ..Default::default()
        }];
        Self(Rc::new(RefCell::new(AppState {
            rows_model: initial_rows_model,
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            current: None,
            entries: Vec::new(),
            all_entries: Vec::new(),
            selected: HashSet::new(),
            anchor: None,
            tabs,
            active_tab_idx: 0,
            clipboard: Clipboard::default(),
            undo: UndoStack::default(),
            drag_paths: Vec::new(),
            rubberband_base: HashSet::new(),
            pending_select: None,
            search_anchor: None,
            search_hit_paths: Vec::new(),
            search_on_right: false,
            search_root: None,
            inactive_pane: None,
            active_is_right: false,
        })))
    }

    pub fn borrow(&self) -> std::cell::Ref<'_, AppState> {
        self.0.borrow()
    }

    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, AppState> {
        self.0.borrow_mut()
    }
}

impl AppState {
    // ── Tabs within the active pane ──────────────────────────────────────

    fn snapshot_active_tab(&mut self) {
        let snap = TabSnapshot {
            rows_model: Some(self.rows_model.clone()),
            back_stack: std::mem::take(&mut self.back_stack),
            forward_stack: std::mem::take(&mut self.forward_stack),
            current: self.current.clone(),
            entries: std::mem::take(&mut self.entries),
            all_entries: std::mem::take(&mut self.all_entries),
            selected: std::mem::take(&mut self.selected),
            anchor: self.anchor,
            title: tab_title(self.current.as_ref()),
        };
        if let Some(slot) = self.tabs.get_mut(self.active_tab_idx) {
            *slot = snap;
        }
    }

    fn restore_active_tab(&mut self, idx: usize) -> Option<Rc<VecModel<FileRowData>>> {
        let snap = self.tabs.get_mut(idx)?;
        let model = snap.rows_model.clone().unwrap_or_else(|| {
            let m = Rc::new(VecModel::default());
            snap.rows_model = Some(m.clone());
            m
        });
        self.rows_model = model.clone();
        self.back_stack = std::mem::take(&mut snap.back_stack);
        self.forward_stack = std::mem::take(&mut snap.forward_stack);
        self.current = snap.current.clone();
        self.entries = std::mem::take(&mut snap.entries);
        self.all_entries = std::mem::take(&mut snap.all_entries);
        self.selected = std::mem::take(&mut snap.selected);
        self.anchor = snap.anchor;
        self.active_tab_idx = idx;
        Some(model)
    }

    pub fn new_tab(&mut self) -> usize {
        let rows_model = Rc::new(VecModel::<FileRowData>::default());
        self.tabs.push(TabSnapshot {
            rows_model: Some(rows_model),
            ..Default::default()
        });
        self.tabs.len() - 1
    }

    pub fn close_tab(&mut self, idx: usize) -> Option<usize> {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return None;
        }
        let need_switch = idx == self.active_tab_idx;
        self.tabs.remove(idx);
        if need_switch {
            let new_idx = if idx == 0 { 0 } else { idx - 1 };
            self.active_tab_idx = new_idx;
        } else if self.active_tab_idx > idx {
            self.active_tab_idx -= 1;
        }
        Some(idx)
    }

    pub fn switch_tab(&mut self, idx: usize) -> Option<Rc<VecModel<FileRowData>>> {
        if idx >= self.tabs.len() || idx == self.active_tab_idx {
            return None;
        }
        self.snapshot_active_tab();
        self.restore_active_tab(idx)
    }

    pub fn restore_after_close(&mut self, idx: usize) -> Option<Rc<VecModel<FileRowData>>> {
        if idx >= self.tabs.len() {
            return None;
        }
        self.restore_active_tab(idx)
    }

    // ── Split view ───────────────────────────────────────────────────────

    /// Snapshot current active-pane fields into a fresh `PaneState` (consumes
    /// the active fields).
    fn take_active_into_pane(&mut self) -> PaneState {
        // First make sure the active tab's snapshot is current.
        self.snapshot_active_tab();
        // Move out everything; leave defaults behind so the active fields can
        // be repopulated by the caller.
        let rows_model = std::mem::replace(&mut self.rows_model, Rc::new(VecModel::<FileRowData>::default()));
        PaneState {
            rows_model,
            back_stack: std::mem::take(&mut self.back_stack),
            forward_stack: std::mem::take(&mut self.forward_stack),
            current: self.current.take(),
            entries: std::mem::take(&mut self.entries),
            all_entries: std::mem::take(&mut self.all_entries),
            selected: std::mem::take(&mut self.selected),
            anchor: self.anchor.take(),
            tabs: std::mem::take(&mut self.tabs),
            active_tab_idx: self.active_tab_idx,
        }
    }

    /// Replace the (now-empty) active fields with the contents of `p`.
    fn restore_active_from_pane(&mut self, p: PaneState) {
        self.rows_model = p.rows_model;
        self.back_stack = p.back_stack;
        self.forward_stack = p.forward_stack;
        self.current = p.current;
        self.entries = p.entries;
        self.all_entries = p.all_entries;
        self.selected = p.selected;
        self.anchor = p.anchor;
        self.tabs = p.tabs;
        self.active_tab_idx = p.active_tab_idx;
    }

    pub fn split_active(&self) -> bool {
        self.inactive_pane.is_some()
    }

    /// F3 — toggle split.
    ///   off → on : create a fresh pane (single tab pointing at `seed_dir`),
    ///              store it as inactive. Active focus stays where it was.
    ///   on  → off: drop the inactive (non-focused) pane.
    pub fn toggle_split(&mut self, seed_dir: Option<Location>) {
        if self.inactive_pane.is_some() {
            self.inactive_pane = None;
            self.active_is_right = false;
            return;
        }

        // Spawn the new pane with its own rows_model + single fresh tab.
        let rows_model = Rc::new(VecModel::<FileRowData>::default());
        let title = tab_title(seed_dir.as_ref());
        let new_pane = PaneState {
            rows_model: rows_model.clone(),
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
            current: seed_dir,
            entries: Vec::new(),
            all_entries: Vec::new(),
            selected: HashSet::new(),
            anchor: None,
            tabs: vec![TabSnapshot {
                rows_model: Some(rows_model),
                title,
                ..Default::default()
            }],
            active_tab_idx: 0,
        };
        self.inactive_pane = Some(new_pane);
        // Focus stays on whichever side it was. Right pane is new and inactive.
        // active_is_right defaults to false → new pane shows on the right.
    }

    /// User clicked in the inactive pane; make it active. Returns the new
    /// active rows model so the caller can rebind `MainWindow.rows`.
    pub fn swap_active(&mut self) -> Option<Rc<VecModel<FileRowData>>> {
        let other = self.inactive_pane.take()?;
        let now_inactive = self.take_active_into_pane();
        self.restore_active_from_pane(other);
        self.inactive_pane = Some(now_inactive);
        self.active_is_right = !self.active_is_right;
        Some(self.rows_model.clone())
    }
}

pub fn tab_title(loc: Option<&Location>) -> String {
    match loc {
        Some(Location::Local(p)) => p
            .file_name()
            .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()),
        Some(Location::Trash) => "Trash".to_string(),
        None => "New tab".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AppStateRc {
        AppStateRc::new(Rc::new(VecModel::default()))
    }

    #[test]
    fn starts_with_one_tab_no_split() {
        let s = make_state();
        let st = s.borrow();
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.active_tab_idx, 0);
        assert!(st.inactive_pane.is_none());
        assert!(!st.active_is_right);
    }

    #[test]
    fn tab_ops_still_work() {
        let s = make_state();
        s.borrow_mut().current = Some(Location::Local("/a".into()));
        s.borrow_mut().new_tab();
        let m = s.borrow_mut().switch_tab(1);
        assert!(m.is_some());
        assert_eq!(s.borrow().active_tab_idx, 1);
    }

    #[test]
    fn toggle_split_creates_inactive_pane() {
        let s = make_state();
        s.borrow_mut().current = Some(Location::Local("/home".into()));
        let seed = s.borrow().current.clone();
        s.borrow_mut().toggle_split(seed);
        let st = s.borrow();
        assert!(st.inactive_pane.is_some());
        let inactive = st.inactive_pane.as_ref().unwrap();
        assert_eq!(inactive.tabs.len(), 1, "new pane has exactly one tab");
        assert!(matches!(
            inactive.current,
            Some(Location::Local(ref p)) if p.to_string_lossy() == "/home"
        ));
    }

    #[test]
    fn toggle_split_off_drops_inactive_pane() {
        let s = make_state();
        s.borrow_mut().toggle_split(Some(Location::Local("/a".into())));
        s.borrow_mut().toggle_split(None);
        assert!(s.borrow().inactive_pane.is_none());
        assert!(!s.borrow().active_is_right);
    }

    #[test]
    fn swap_makes_other_pane_active() {
        let s = make_state();
        s.borrow_mut().current = Some(Location::Local("/left".into()));
        s.borrow_mut().toggle_split(Some(Location::Local("/right".into())));
        // Initially: active = left
        assert_eq!(
            s.borrow().current.as_ref().and_then(|l| l.as_path()).unwrap(),
            std::path::Path::new("/left")
        );

        s.borrow_mut().swap_active();
        assert!(s.borrow().active_is_right);
        assert_eq!(
            s.borrow().current.as_ref().and_then(|l| l.as_path()).unwrap(),
            std::path::Path::new("/right")
        );

        // Swap back.
        s.borrow_mut().swap_active();
        assert!(!s.borrow().active_is_right);
        assert_eq!(
            s.borrow().current.as_ref().and_then(|l| l.as_path()).unwrap(),
            std::path::Path::new("/left")
        );
    }

    #[test]
    fn each_pane_keeps_its_own_tabs() {
        let s = make_state();
        s.borrow_mut().current = Some(Location::Local("/left".into()));
        s.borrow_mut().new_tab();
        s.borrow_mut().switch_tab(1);
        s.borrow_mut().current = Some(Location::Local("/left2".into()));
        // Left has 2 tabs now.
        assert_eq!(s.borrow().tabs.len(), 2);

        s.borrow_mut().toggle_split(Some(Location::Local("/right".into())));
        // Right pane has only 1 tab and is on the right (inactive).
        assert_eq!(s.borrow().inactive_pane.as_ref().unwrap().tabs.len(), 1);

        s.borrow_mut().swap_active();
        // Now right is active — should see 1 tab only.
        assert_eq!(s.borrow().tabs.len(), 1);
        // Left (now inactive) keeps its 2 tabs.
        assert_eq!(s.borrow().inactive_pane.as_ref().unwrap().tabs.len(), 2);
    }
}
