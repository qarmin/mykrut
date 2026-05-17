use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use mykrut_core::{Location, disk_space};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Runtime;
use tracing::{info, warn};

use crate::bookmarks::{self, BookmarkStore};
use crate::format_util::human_size;
use crate::glue::mtp::MtpController;
use crate::glue::udisks::UDisksController;
use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{AppState as SlintAppState, Callabler, MainWindow, PlaceItemData, Translations};

thread_local! {
    /// Snapshot of which paths are already reachable from the sidebar, refreshed
    /// at the end of every `PlacesController::rebuild`. Read on the UI thread by
    /// the selection + navigation glue to drive the context-menu
    /// "Add ↔ Remove bookmark" toggle. Single-threaded (Slint event loop) so a
    /// thread-local is enough — no locking needed.
    static PLACES_INDEX: std::cell::RefCell<PlacesIndex> = std::cell::RefCell::new(PlacesIndex::default());
}

#[derive(Default)]
struct PlacesIndex {
    /// Paths currently in the bookmark store (display strings).
    bookmarks: std::collections::HashSet<String>,
    /// Fixed Computer/Places/Devices entries — already reachable without a
    /// bookmark, so bookmarking them is pointless and disallowed.
    reserved: std::collections::HashSet<String>,
}

/// Recompute the file context-menu bookmark flags from the single selected
/// directory (if any) against the live places index.
pub fn refresh_selection_bookmark_flags(app: &MainWindow, state: &AppStateRc) {
    let path = {
        let s = state.borrow();
        if s.selected.len() == 1 {
            s.selected
                .iter()
                .next()
                .and_then(|&i| s.entries.get(i))
                .filter(|e| e.is_dir())
                .map(|e| e.path.display().to_string())
        } else {
            None
        }
    };
    let (is_bm, can_bm) = bookmark_state_of(path.as_deref());
    let st = app.global::<SlintAppState>();
    st.set_selection_is_bookmarked(is_bm);
    st.set_selection_can_bookmark(can_bm);
}

/// Recompute the empty-area folder menu bookmark flags from the current folder.
pub fn refresh_current_bookmark_flags(app: &MainWindow, state: &AppStateRc) {
    let path = match state.borrow().current.clone() {
        Some(Location::Local(p)) => Some(p.display().to_string()),
        _ => None,
    };
    let (is_bm, can_bm) = bookmark_state_of(path.as_deref());
    let st = app.global::<SlintAppState>();
    st.set_current_folder_is_bookmarked(is_bm);
    st.set_current_folder_can_bookmark(can_bm);
}

/// `(is_bookmarked, can_bookmark)` for `path` against the live index. A `None`
/// path (no/multi selection, non-local folder) is neither.
fn bookmark_state_of(path: Option<&str>) -> (bool, bool) {
    match path {
        Some(p) => PLACES_INDEX.with(|idx| {
            let idx = idx.borrow();
            (idx.bookmarks.contains(p), !idx.reserved.contains(p))
        }),
        None => (false, false),
    }
}

/// Owns the rebuildable places model and the on-disk bookmark store.
pub struct PlacesController {
    pub model: Rc<VecModel<PlaceItemData>>,
    pub bookmarks: BookmarkStore,
    pub udisks: Option<Arc<UDisksController>>,
    pub mtp: Option<Arc<MtpController>>,
    /// Free/total bytes per mount point, refreshed off the UI thread. `rebuild`
    /// reads from here instead of calling the blocking `statvfs` directly, so a
    /// slow/network mount can't freeze the sidebar.
    disk_usage: std::collections::HashMap<PathBuf, disk_space::DiskSpace>,
}

impl PlacesController {
    pub fn install(app: &MainWindow) -> Rc<std::cell::RefCell<Self>> {
        let bookmarks = bookmarks::load();
        let model = Rc::new(VecModel::<PlaceItemData>::default());
        app.set_places(ModelRc::from(model.clone()));

        let ctrl = Rc::new(std::cell::RefCell::new(Self {
            model,
            bookmarks,
            udisks: None,
            mtp: None,
            disk_usage: std::collections::HashMap::new(),
        }));
        ctrl.borrow().rebuild(app);
        ctrl
    }

    pub fn set_udisks(&mut self, udisks: Arc<UDisksController>) {
        self.udisks = Some(udisks);
    }

    pub fn set_mtp(&mut self, mtp: Arc<MtpController>) {
        self.mtp = Some(mtp);
    }

    pub fn rebuild(&self, app: &MainWindow) {
        let tr = app.global::<Translations>();
        let mut items: Vec<PlaceItemData> = Vec::with_capacity(20);

        // ── Computer section ──────────────────────────────────────────
        items.push(header(tr.get_places_section_computer().to_string()));
        // Locally-mounted "real" filesystems (/, /home if separate, /mnt/X,
        // …). UDisks-tracked removable media still live in the Devices
        // section below; we deliberately don't double-list them here.
        let removable_mounts: std::collections::HashSet<PathBuf> = self
            .udisks
            .as_ref()
            .map(|u| {
                u.devices
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|d| d.mount_point.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Usage comes from the async-refreshed cache; a candidate not yet in
        // the cache (or below the tiny-mount threshold) is skipped so we never
        // call statvfs on the UI thread here.
        let mut disks: Vec<ComputerDisk> = computer_mount_candidates()
            .into_iter()
            .filter(|(_, path)| !removable_mounts.contains(path))
            .filter_map(|(label, path)| {
                let usage = self.disk_usage.get(&path)?;
                if usage.total < 256 * 1024 * 1024 {
                    return None; // EFI/boot partitions and the like
                }
                Some(ComputerDisk {
                    label,
                    path,
                    free: usage.free,
                    total: usage.total,
                })
            })
            .collect();
        let root = Path::new("/");
        // Root first, then alphabetical by mount path.
        disks.sort_by(|a, b| match (a.path == root, b.path == root) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.path.cmp(&b.path),
        });
        // The user's home (e.g. /home/rafal) belongs with the Places bookmarks
        // below, not here. Only suppress it there if the home dir is *itself* a
        // mounted filesystem already listed in Computer (then `disk_item`
        // represents it). The parent `/home` mount is a different path and must
        // NOT hide the user's home shortcut.
        let home = dirs::home_dir();
        let home_is_own_mount = home.as_ref().is_some_and(|h| disks.iter().any(|d| &d.path == h));
        for mount in &disks {
            items.push(disk_item(mount));
        }

        // ── Places (XDG user dirs) ────────────────────────────────────
        items.push(header(tr.get_places_section_places().to_string()));
        // Home first, then the XDG user dirs (Desktop, Documents, …).
        if !home_is_own_mount && let Some(h) = &home {
            items.push(item("Home", "folder-home", h, false));
        }
        for (label_fallback, icon, path) in xdg_dirs() {
            items.push(item(&label_fallback, icon, &path, false));
        }
        // Trash entry (path is the actual on-disk trash location for now).
        if let Some(p) = trash_files_dir() {
            let mut trash_item = item(&tr.get_places_trash(), "trash", &p, false);
            trash_item.is_trash = true;
            items.push(trash_item);
        }

        // ── Devices (UDisks2 + MTP) ───────────────────────────────────
        let mtp_devs: Vec<crate::glue::mtp::MtpDeviceEntry> = self
            .mtp
            .as_ref()
            .map(|m| m.devices.lock().unwrap().clone())
            .unwrap_or_default();

        let has_any_device = self
            .udisks
            .as_ref()
            .is_some_and(|u| !u.devices.lock().unwrap().is_empty())
            || !mtp_devs.is_empty();

        if has_any_device {
            items.push(header(tr.get_places_section_devices().to_string()));
        }

        for dev in &mtp_devs {
            // mtp:loc<id> prefix routes click → "phone browsing not yet wired" toast.
            items.push(PlaceItemData {
                label: dev.label.clone().into(),
                icon_name: "disk-removable".into(),
                path: format!("mtp:loc{}", dev.location_id).into(),
                is_section_header: false,
                is_bookmark: false,
                is_trash: false,
                accepts_drop: false,
                has_usage: false,
                usage_fraction: 0.0,
                usage_text: "".into(),
            });
        }

        if let Some(udisks) = &self.udisks {
            let devices = udisks.devices.lock().unwrap().clone();
            if !devices.is_empty() {
                let not_mounted = tr.get_places_device_not_mounted().to_string();
                for dev in &devices {
                    // Path stored on the place item is the mount-point when mounted,
                    // or the UDisks object path with a `udisks:` prefix when not.
                    // navigate_to handler tells the two apart.
                    let path_for_click = match &dev.mount_point {
                        Some(mp) => mp.display().to_string(),
                        None => format!("udisks:{}", dev.object_path),
                    };
                    let label = match &dev.mount_point {
                        Some(_) => dev.label.clone(),
                        None => format!("{} {}", dev.label, not_mounted),
                    };
                    let (has_usage, usage_fraction, usage_text) = match &dev.mount_point {
                        Some(mp) => match self.disk_usage.get(mp).copied() {
                            Some(s) if s.total > 0 => {
                                let frac = (s.total - s.free) as f32 / s.total as f32;
                                let txt = format!("{} free of {}", human_size(s.free), human_size(s.total));
                                (true, frac, slint::SharedString::from(txt))
                            }
                            _ => (false, 0.0, "".into()),
                        },
                        None => (false, 0.0, "".into()),
                    };
                    items.push(PlaceItemData {
                        label: label.into(),
                        icon_name: "disk-removable".into(),
                        path: path_for_click.into(),
                        is_section_header: false,
                        is_bookmark: false,
                        is_trash: false,
                        // Only a mounted device has a real local path to drop into.
                        accepts_drop: dev.mount_point.is_some(),
                        has_usage,
                        usage_fraction,
                        usage_text,
                    });
                }
            }
        }

        // ── Bookmarks ─────────────────────────────────────────────────
        if !self.bookmarks.entries.is_empty() {
            items.push(header(tr.get_places_section_bookmarks().to_string()));
            for b in &self.bookmarks.entries {
                items.push(item(&b.name, "folder", &PathBuf::from(&b.path), true));
            }
        }

        // Refresh the shared index before handing `items` to the model: every
        // non-header, non-bookmark entry is a "reserved" place that shouldn't be
        // bookmarkable again.
        let reserved: std::collections::HashSet<String> = items
            .iter()
            .filter(|it| !it.is_section_header && !it.is_bookmark && !it.path.is_empty())
            .map(|it| it.path.to_string())
            .collect();
        let bookmarks: std::collections::HashSet<String> =
            self.bookmarks.entries.iter().map(|b| b.path.clone()).collect();
        PLACES_INDEX.with(|idx| {
            let mut idx = idx.borrow_mut();
            idx.reserved = reserved;
            idx.bookmarks = bookmarks;
        });

        self.model.set_vec(items);
    }

    /// All mount points whose free/total we want measured: computer mounts
    /// plus any mounted UDisks device. Used to refresh `disk_usage` off-thread.
    fn disk_usage_candidates(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = computer_mount_candidates().into_iter().map(|(_, p)| p).collect();
        if let Some(u) = &self.udisks {
            for d in u.devices.lock().unwrap().iter() {
                if let Some(mp) = &d.mount_point {
                    v.push(mp.clone());
                }
            }
        }
        v
    }
}

/// Measure free/total for every candidate mount on a blocking thread, then
/// store the results and rebuild the sidebar on the UI thread. Keeps the
/// potentially-slow `statvfs` syscalls off the event loop.
fn refresh_disk_usage(app: &MainWindow, rt: &Arc<Runtime>, ctrl: Rc<std::cell::RefCell<PlacesController>>) {
    let candidates = ctrl.borrow().disk_usage_candidates();
    let weak = app.as_weak();
    let rt2 = rt.clone();
    let _g = rt.enter();
    let _ = slint::spawn_local(async move {
        let map = rt2
            .spawn_blocking(move || {
                let mut m: std::collections::HashMap<PathBuf, disk_space::DiskSpace> =
                    std::collections::HashMap::with_capacity(candidates.len());
                for p in candidates {
                    if let Some(d) = disk_space::query(&p) {
                        m.insert(p, d);
                    }
                }
                m
            })
            .await
            .unwrap_or_default();
        let Some(app) = weak.upgrade() else { return };
        ctrl.borrow_mut().disk_usage = map;
        ctrl.borrow().rebuild(&app);
    });
}

fn header(label: String) -> PlaceItemData {
    PlaceItemData {
        label: label.into(),
        icon_name: "".into(),
        path: "".into(),
        is_section_header: true,
        is_bookmark: false,
        is_trash: false,
        accepts_drop: false,
        has_usage: false,
        usage_fraction: 0.0,
        usage_text: "".into(),
    }
}

fn item(label: &str, icon: &str, path: &Path, is_bookmark: bool) -> PlaceItemData {
    PlaceItemData {
        label: label.to_string().into(),
        icon_name: icon.to_string().into(),
        path: path.display().to_string().into(),
        is_section_header: false,
        is_bookmark,
        is_trash: false,
        accepts_drop: true,
        has_usage: false,
        usage_fraction: 0.0,
        usage_text: "".into(),
    }
}

struct ComputerDisk {
    label: String,
    path: PathBuf,
    free: u64,
    total: u64,
}

fn disk_item(m: &ComputerDisk) -> PlaceItemData {
    let fraction = if m.total > 0 {
        (m.total - m.free) as f32 / m.total as f32
    } else {
        0.0
    };
    let usage_text = format!("{} free of {}", human_size(m.free), human_size(m.total));
    PlaceItemData {
        label: m.label.clone().into(),
        icon_name: "disk-removable".into(),
        path: m.path.display().to_string().into(),
        is_section_header: false,
        is_bookmark: false,
        is_trash: false,
        accepts_drop: true,
        has_usage: true,
        usage_fraction: fraction,
        usage_text: usage_text.into(),
    }
}

/// Enumerate candidate "Computer" mount points as (label, path) pairs.
/// Reads `/proc/mounts` directly (fast — it's a pseudo file) and filters out
/// pseudo/virtual filesystems and noise like /snap, /boot, container
/// bind-mounts. Deliberately does NOT call `statvfs` — disk usage is filled
/// asynchronously into `PlacesController::disk_usage` and read by `rebuild`.
fn computer_mount_candidates() -> Vec<(String, PathBuf)> {
    let raw = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut out: Vec<(String, PathBuf)> = Vec::new();

    for line in raw.lines() {
        // /proc/mounts format: device mountpoint fstype options dump pass
        let mut parts = line.split_whitespace();
        let _device = parts.next();
        let Some(mp_raw) = parts.next() else { continue };
        let Some(fstype) = parts.next() else { continue };

        if !is_user_relevant_fs(fstype) {
            continue;
        }
        // Mount points in /proc/mounts encode spaces as \040 etc. We don't
        // realistically hit that on /, /home, /mnt/X — keep this simple.
        let mp = PathBuf::from(mp_raw);
        if !is_user_relevant_path(&mp) {
            continue;
        }
        if !seen.insert(mp.clone()) {
            continue; // duplicate from bind-mount, etc.
        }
        out.push((pretty_mount_label(&mp), mp));
    }
    out
}

fn is_user_relevant_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "ext2"
            | "ext3"
            | "ext4"
            | "btrfs"
            | "xfs"
            | "zfs"
            | "f2fs"
            | "jfs"
            | "reiserfs"
            | "vfat"
            | "exfat"
            | "ntfs"
            | "ntfs3"
            | "fuseblk"
            | "hfsplus"
            | "apfs"
    )
}

fn is_user_relevant_path(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    // Skip system / container / package-manager mounts. /boot* often holds
    // the EFI partition which the user almost never wants to browse;
    // /snap, /var/snap, /var/lib/docker, etc. are similarly noise.
    let blocked_prefixes = [
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/snap",
        "/var/snap",
        "/var/lib/docker",
        "/var/lib/containers",
        "/var/lib/lxc",
        "/var/lib/lxd",
        "/var/lib/flatpak/exports",
        "/boot",
    ];
    !blocked_prefixes.iter().any(|prefix| s.starts_with(prefix))
}

fn pretty_mount_label(mp: &std::path::Path) -> String {
    if mp == std::path::Path::new("/") {
        return "Filesystem".to_string();
    }
    mp.file_name().map_or_else(
        || mp.display().to_string(),
        |n| {
            let s = n.to_string_lossy();
            // Capitalise first letter so /home → "Home", /data → "Data".
            let mut chars = s.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => s.into_owned(),
            }
        },
    )
}

fn xdg_dirs() -> Vec<(String, &'static str, PathBuf)> {
    let mut out: Vec<(String, &'static str, PathBuf)> = Vec::new();
    let push =
        |out: &mut Vec<(String, &'static str, PathBuf)>, opt: Option<PathBuf>, label: &str, icon: &'static str| {
            if let Some(p) = opt {
                // Skip dirs that don't actually exist on disk (e.g. user removed Public).
                if p.is_dir() {
                    out.push((label.to_string(), icon, p));
                }
            }
        };
    push(&mut out, dirs::desktop_dir(), "Desktop", "folder-desktop");
    push(&mut out, dirs::document_dir(), "Documents", "folder-documents");
    push(&mut out, dirs::download_dir(), "Downloads", "folder-downloads");
    push(&mut out, dirs::picture_dir(), "Pictures", "folder-pictures");
    push(&mut out, dirs::audio_dir(), "Music", "folder-music");
    push(&mut out, dirs::video_dir(), "Videos", "folder-videos");
    push(&mut out, dirs::template_dir(), "Templates", "folder-templates");
    push(&mut out, dirs::public_dir(), "Public", "folder-public");
    out
}

fn trash_files_dir() -> Option<PathBuf> {
    let data = dirs::data_local_dir()?;
    let p = data.join("Trash").join("files");
    if p.is_dir() { Some(p) } else { None }
}

pub fn wire(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    ctrl: Rc<std::cell::RefCell<PlacesController>>,
) {
    // Measure disk usage for the initial sidebar off the UI thread.
    refresh_disk_usage(app, rt, ctrl.clone());

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state.clone();
        let watcher = watcher;
        let ctrl_for_click = ctrl.clone();
        app.global::<Callabler>().on_place_clicked(move |path| {
            let app = weak.upgrade().expect("MainWindow alive in place-clicked");
            let raw = path.to_string();
            if let Some(obj_path) = raw.strip_prefix("udisks:") {
                if let Some(udisks) = &ctrl_for_click.borrow().udisks {
                    info!(object_path = %obj_path, "mount requested");
                    crate::glue::udisks::request_mount(udisks, obj_path);
                }
                return;
            }
            if let Some(loc) = raw.strip_prefix("mtp:") {
                // Phase 7b will implement actual browsing; for now log + skip.
                tracing::warn!(loc = %loc, "MTP browsing not yet wired (Phase 7b)");
                return;
            }
            let p = PathBuf::from(raw);
            info!(path = %p.display(), "place clicked");
            crate::glue::navigation::navigate_to(&app, &rt, state.clone(), watcher.clone(), Location::Local(p));
        });
    }

    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let ctrl = ctrl.clone();
        app.global::<Callabler>().on_devices_changed(move || {
            let app = weak.upgrade().expect("MainWindow alive in devices-changed");
            // Rebuild immediately for the device list, then re-measure usage
            // (a newly mounted device's free/total) off-thread and rebuild again.
            ctrl.borrow().rebuild(&app);
            refresh_disk_usage(&app, &rt, ctrl.clone());
        });
    }

    {
        let weak = app.as_weak();
        let state = state.clone();
        let ctrl = ctrl.clone();
        app.global::<Callabler>().on_toggle_bookmark_of_selected(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-bookmark-selected");
            let target = {
                let s = state.borrow();
                if s.selected.len() == 1 {
                    s.selected
                        .iter()
                        .next()
                        .and_then(|&i| s.entries.get(i).cloned())
                        .filter(|e| e.is_dir())
                } else {
                    None
                }
            };
            let Some(entry) = target else {
                warn!("toggle-bookmark: no single directory selected");
                return;
            };
            toggle_bookmark(&app, &ctrl, &state, &entry.path, &entry.display_name);
        });
    }

    {
        let weak = app.as_weak();
        let state = state.clone();
        let ctrl = ctrl.clone();
        app.global::<Callabler>().on_toggle_bookmark_of_current(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-bookmark-current");
            let Some(Location::Local(path)) = state.borrow().current.clone() else {
                warn!("toggle-bookmark-current: current folder is not a local dir");
                return;
            };
            let name = path
                .file_name()
                .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            toggle_bookmark(&app, &ctrl, &state, &path, &name);
        });
    }

    {
        let weak = app.as_weak();
        let state = state;
        let ctrl_remove = ctrl;
        app.global::<Callabler>().on_remove_bookmark(move |path| {
            let app = weak.upgrade().expect("MainWindow alive in remove-bookmark");
            let p = path.to_string();
            {
                let mut c = ctrl_remove.borrow_mut();
                c.bookmarks.remove_by_path(&p);
                if let Err(e) = bookmarks::save(&c.bookmarks) {
                    warn!(?e, "bookmark save failed");
                }
                c.rebuild(&app);
            }
            // The removed bookmark may be the current/selected folder — refresh
            // the menu toggles so they don't keep offering "Remove bookmark".
            refresh_selection_bookmark_flags(&app, &state);
            refresh_current_bookmark_flags(&app, &state);
            info!(path = %p, "bookmark removed");
        });
    }
}

/// Add the bookmark if `path` isn't bookmarked yet, otherwise remove it. Saves,
/// rebuilds the sidebar, and refreshes the context-menu toggle flags. Built-in
/// Computer/Places entries are refused (the UI also disables them).
fn toggle_bookmark(
    app: &MainWindow,
    ctrl: &Rc<std::cell::RefCell<PlacesController>>,
    state: &AppStateRc,
    path: &Path,
    name: &str,
) {
    let key = path.display().to_string();
    {
        let mut c = ctrl.borrow_mut();
        let already = c.bookmarks.entries.iter().any(|e| e.path == key);
        if already {
            c.bookmarks.remove_by_path(&key);
            info!(path = %key, "bookmark removed (toggle)");
        } else {
            if PLACES_INDEX.with(|idx| idx.borrow().reserved.contains(&key)) {
                warn!(path = %key, "refusing to bookmark a built-in Computer/Places entry");
                return;
            }
            c.bookmarks.upsert(name, path);
            info!(path = %key, "bookmark added (toggle)");
        }
        if let Err(e) = bookmarks::save(&c.bookmarks) {
            warn!(?e, "bookmark save failed");
        }
        c.rebuild(app);
    }
    // Borrow released — recompute both menus' flags against the new index.
    refresh_selection_bookmark_flags(app, state);
    refresh_current_bookmark_flags(app, state);
}
