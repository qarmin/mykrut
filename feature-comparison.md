# cosmic-files vs fm — Feature Comparison

Comparison snapshot (refreshed 2026-06-12): `other/` is the upstream **cosmic-files** (System76, Iced + libcosmic, ~26k LOC of Rust + 70+ language packs). `app/` + `crates/core/` is our **fm** (Slint UI + own glue, ~11k LOC + tests).

Method: walked every major file in `other/src/` (menu.rs, key_bind.rs, app.rs, tab.rs, dialog.rs, operation/, mounter/, archive.rs, clipboard.rs, trash.rs, mime_app.rs, config.rs, context_action.rs, thumbnailer.rs, thumbnail_cacher.rs, large_image.rs); cross-checked against `app/src/glue/*.rs`, `app/ui/**.slint`, and `crates/core/src/*.rs`. Counts below are item counts in this document — see end of file.

---

## 1. Features present in cosmic-files

### 1.1 Application mode / launch surfaces
- **Three Modes**: `Mode::App` (window manager), `Mode::Desktop` (renders the desktop), `Mode::Dialog(DialogKind)` (file-picker for other apps). Each mode rewires menus, keybinds, and locations on init. (`other/src/app.rs:114`, `other/src/dialog.rs:55`, `other/src/key_bind.rs`).
- **File-picker dialog crate**: exported `Dialog<M>` API for embedding in other apps (Open File / Folder / Save / Multi-Open). Used via `examples/dialog.rs`. Supports filters (`DialogFilter` with glob + MIME), choices (`DialogChoice` checkbox/combo), per-launch `path_opt` and `app_id`. (`other/src/dialog.rs:55-200`).
- **CLI args**: opens supplied paths as tabs/locations on startup; URIs handled separately and trigger network-drive mount-then-open. (`other/src/app.rs:2498-2522`).

### 1.2 Tabs & windows
- **Multi-tab** with reorderable, draggable tab bar (Tab DnD threshold 25 px), per-tab `Tab` struct holding location, history, view config, gallery state, scroll offset. (`other/src/app.rs:6465-6485`, `other/src/tab.rs`).
- **Ctrl+T** new tab, **Ctrl+W** close tab, **Ctrl+Tab / Ctrl+Shift+Tab** cycle, **Ctrl+N** new window. (`other/src/key_bind.rs:59-64`).
- **Open in new tab / new window** (Ctrl+Enter / Shift+Enter) from selection, context menu, nav bar, breadcrumbs context menu. (`other/src/app.rs:165-167`, `other/src/menu.rs:782-806`).
- **Tab middle-click on nav bar** opens entry in new tab. (`other/src/app.rs:2553-2555`).
- **Scroll-tab gesture**: horizontal wheel on the tab bar scrolls between tabs. (`Message::ScrollTab` `other/src/app.rs:426`).
- **Per-tab back/forward/up history**: Alt+Left/Right (or Backspace), Alt+Up. (`Message::GoNext/GoPrevious/LocationUp`, `other/src/key_bind.rs:82-85`).

### 1.3 Locations / navigation
- Locations include: **Local Path, Desktop(path, display, DesktopConfig), Network(uri, name, mounted-path), Recents, Trash, Search(SearchLocation, term, show_hidden, instant)**. (`other/src/tab.rs:1438-1446`).
- **Breadcrumb path bar** with per-segment context menu (Open in tab / window / Show details / Add to sidebar). (`other/src/menu.rs:781-826`).
- **Edit-location mode**: Ctrl+L switches breadcrumbs to a text field with **tab completion** (`Message::TabComplete`, `EditLocationTab`, completion list maintained via `EditLocation.completions`). (`other/src/tab.rs:1709-1713`).
- **Tilde expansion** in entered paths (`~/Foo` → `$HOME/Foo`). (`other/src/tab.rs:1610-1622`).
- **Network drive UI**: dedicated `ContextPage::NetworkDrive` with scheme table (afp/ftp/ftps/nfs/smb/sftp/ssh/dav/davs), URI input + Connect, anonymous connect, username/domain/password, Remember password. Auth-required dialog triggered on demand. (`other/src/app.rs:1938-1967`, i18n keys 175-200).
- **Path normalization**: canonicalize where possible, trailing slash for dirs, URI trailing slash for network. (`other/src/tab.rs:1466-1488`).

### 1.4 View modes
- **Two view modes**: Grid and List. Ctrl+1 / Ctrl+2. (`other/src/tab.rs:2575`).
- **Per-tab view override**: each tab can have its own view (`tab.config.view`); global default lives in `TabConfig`. (`other/src/config.rs:319`).
- **Show details** sidebar (toggle Preview pane right-hand context drawer): file metadata, thumbnail, "Open with" picker, mode/perm shifting controls, accessed/created/modified timestamps. (`Message::Preview`, `other/src/app.rs:2133-2203`, i18n keys 296-305).
- **Gallery view** (image immersive mode): Space to toggle, arrow keys to step, ESC to leave. (`Message::Gallery/GalleryNext/GalleryPrevious/GalleryToggle`, `other/src/tab.rs:1719-1722`).
- **Large image support**: dedicated worker pool (`LargeImageManager`, `decode_large_image`, tiling, memory limit) so huge images don't OOM the UI. (`other/src/large_image.rs`).
- **Zoom controls**: Ctrl+= / Ctrl+- / Ctrl+0, stored per-tab via `IconSizes` (list% and grid% scaled against base 32/48/64 px, capped at 5× via `ICON_SCALE_MAX`). Also wired through View menu and "..." menu in dialog mode. (`other/src/config.rs:21-32, 351-379`, `other/src/zoom.rs`).
- **Column resize / column sort** in list view (Name / Modified / Size / Trashed). Header click toggles direction. (`HeadingOptions` `other/src/tab.rs:2580-2596`).
- **Folders-first toggle** persisted per dialog and per tab. (`Config.tab.folders_first`).
- **Show hidden** toggle (Ctrl+H, also menu). Persisted per dialog and per tab. (`Config.tab.show_hidden`).
- **24h/12h time**: pulled live from `cosmic-applet-time` config; not persisted locally. (`other/src/config.rs:381-387`).

### 1.5 Selection & keyboard navigation
- **Type-ahead**: 3 modes ("recursive search", "enter path", "select by prefix"), user-selectable in Settings. `TYPE_SELECT_TIMEOUT` resets the prefix after 1 s of idle. (`other/src/config.rs:106-110`, `other/src/tab.rs:72`).
- **Arrow keys** including Shift-modified (range extension), Home / End / Shift+Home / Shift+End. (`other/src/key_bind.rs:26-38`).
- Ctrl+A select all, Ctrl+Click toggle, Shift+Click range, Shift+arrow extend.
- **Auto-scroll** when dragging near the edge (`AutoScroll(Option<f32>)` message — speed-based ramping). (`other/src/tab.rs:1697`).

### 1.6 Drag & drop
- **DnD source**: items can be dragged out (uses `ClipboardKind::Cut { is_dnd: true }` semantics). (`other/src/clipboard.rs:12-15`).
- **DnD destinations**: tabs, nav-bar (sidebar), folder items, the active listing. Hover-timeout to auto-switch tabs / open folders. (`Message::DndHoverTabTimeout/DndHoverLocTimeout`, `HOVER_DURATION=1600ms`, `other/src/tab.rs:71`).
- **DndAction** support (Copy / Move / Link from the source app).
- **Tab reorder** via drag (cosmic tab bar). (`enable_tab_drag` `other/src/app.rs:6471-6483`).

### 1.7 Clipboard
- **Cut / Copy / Paste**: Ctrl+X / Ctrl+C / Ctrl+V. Multi-MIME clipboard: `text/plain`, `text/plain;charset=utf-8`, `UTF8_STRING`, `text/uri-list`, `x-special/gnome-copied-files` (with leading `copy`/`cut` line). Inter-app interop with Nautilus / Nemo. (`other/src/clipboard.rs:25-105`).
- **Cut visual**: items marked as cut appear dimmed until pasted.
- **Paste image / text / video** from clipboard: clipboard-cache polling (`CheckClipboard`, `CheckClipboardImage`, `CheckClipboardVideo`, `CheckClipboardText`) auto-detects pasteable non-file content and pastes into the current dir as `Pasted Image`, `Pasted Text`, `Pasted Video`. (`other/src/app.rs:395-407`, i18n `pasted-image/text/video`).
- **Copy path** (Ctrl+Shift+C) — puts path text in clipboard, not the file. (`other/src/key_bind.rs:70`).

### 1.8 File operations (full list)
Centralised in `other/src/operation/mod.rs:350` as the `Operation` enum and executed by `recursive.rs` with cancellation, pause/resume, and progress streaming:
- **Copy** (paths → to). Cross-device move falls back to copy+remove.
- **Move** with `cross_device_copy` flag.
- **Delete** (move to trash).
- **DeleteTrash** (purge specific trash items).
- **EmptyTrash**.
- **PermanentlyDelete** (skip trash, Shift+Delete).
- **Rename** (single-file).
- **NewFile / NewFolder** with name validation, hidden warning, slash forbidden, name-invalid handling.
- **Restore** from trash.
- **RemoveFromRecents** (with `recently-used-xbel` integration).
- **Compress**: tar.gz or zip, optional AES-256 password for zip. (`other/src/archive.rs`).
- **Extract**: any archive — `extract-here` or `extract-to` (destination picker). Password-protected zips trigger an `ExtractPassword` dialog. (`Operation::Extract`, `other/src/archive.rs:43`).
- **SetExecutableAndLaunch**: chmod +x then run, with confirmation dialog. (`Operation::SetExecutableAndLaunch`, i18n `set-executable-and-launch*`).
- **SetPermissions**: change file mode; multi-selection variant uses `Command::SetMultiplePermissions`. UI uses 0-7 mode labels (None / Execute-only / Write-only / ... / Read-write-execute) split owner/group/other. (`other/src/tab.rs:99-118`).
- **Operations are async via compio runtime** on a dedicated thread (io_uring on Linux). (`other/src/app.rs:2407-2421`).
- **Pause/resume per-operation** via `Controller`, plus global pause/resume. (`Message::PendingPause/PendingPauseAll`).
- **Replace dialog** on conflicts: Replace, Replace + apply to all, Skip, Skip + apply to all, Keep both, Cancel. Shows from-vs-to preview, multiple-conflict counter. (`DialogPage::Replace`, `operation/mod.rs:29-69`).
- **Operations queue + retry of failed**: `failed_operations` map allows requeue.
- **Undo toast**: trash op pushes a toast with "Undo" that restores the items. (`Message::UndoTrash/UndoTrashStart`, `other/src/app.rs:1336-1356`).
- **Persistent edit history** (`ContextPage::EditHistory`) listing pending / failed / completed ops with progress bars and dismiss controls. (`other/src/app.rs:2046-2131`).
- **Footer progress bar** with title, percent, pause-all/resume-all/cancel-all, click-through to history. (`other/src/app.rs:6282-6394`).
- **Bytes-precise progress** with file pre-scan, ~30Hz throttling. (`other/src/operation/mod.rs`, similar pattern to our `copy_progress.rs`).
- **Desktop notifications** via `notify-rust` while ops run (feature-gated). (`other/src/app.rs:1858-1877`).

### 1.9 Open With / MIME dispatch
- **Full MIME-app handling**: scans .desktop entries, sorts by exact-match / parent-MIME / other-apps. (`other/src/mime_app.rs`).
- **Open With dialog** (custom DialogPage::OpenWith) listing matching apps + "Browse store" button per app store. (`DialogPage::OpenWith` `other/src/app.rs:557-562`, i18n `open-with-title`, `browse-store`).
- **Set default app** for a MIME type (`Message::TabMessage(.., SetOpenWith(mime, id))`). Writes to `mimeapps.list`. (`other/src/mime_app.rs:388-395`).
- **Desktop entry actions** (Firefox's "Open Private Window" etc.): right-click on a .desktop file lists its `Actions=` entries inline. (`Action::ExecEntryAction(usize)`).
- **Open in terminal**: spawns the user's preferred terminal in the current/selected dir (queried via `mime_app::get_default_terminal`, with fallback chain). (`other/src/menu.rs:225`, `mime_app.rs:347-385`).
- **Run context actions** (user-defined post-actions on selection): the `ContextActionPreset` config lets users define named, multi-step shell pipelines that appear in the context menu, filtered by "any / files / folders" matchers and optionally with a confirm dialog. (`other/src/context_action.rs`).
- **Bulk-open guard**: opening many items with one Enter triggers a confirmation prompt above a threshold.
- **`%f` / `%F` / `%u` / `%U` Exec field-code handling** in desktop launchers (full edge-case set, well-tested). (`other/src/mime_app.rs:17-100`).

### 1.10 Search
- **Ctrl+F** activates a header-end search field. (`other/src/app.rs:6407-6440`).
- **Three search-location kinds**: `SearchLocation::Path(p)` (subtree of current), `::Recents`, `::Trash`. The Recents / Trash searches reuse the same UI. (`other/src/tab.rs:1406-1410`).
- **Live streaming results** with `MAX_SEARCH_LATENCY=20ms` time-slice, `MAX_SEARCH_RESULTS=200`. (`other/src/tab.rs:73-75`).
- **Regex search** (input compiled into a `Regex`, see `Trash::scan_search` and `tab::scan_search`).
- **"Open item location"** action moves you from a search hit / Recents row to its parent folder and selects the item. (`Action::OpenItemLocation`, `Message::OpenItemLocation`).
- **Type-ahead = Recursive search** is a Settings option that makes typing in the listing open the search box pre-filled. (`TypeToSearch::Recursive`).

### 1.11 Thumbnails / previews
- **Asynchronous thumbnailer** with disk cache under XDG thumbnail spec, normal + large sizes via `freedesktop_icons` integration. (`other/src/thumbnailer.rs`, `other/src/thumbnail_cacher.rs`).
- **Configurable** in `ThumbCfg`: parallel jobs, max memory MB, max source file size MB. (`other/src/config.rs:296-312`).
- **Image, video, PDF, audio art** all rendered. Massive image tiling via `large_image.rs`.
- **Inline preview pane**: the "Show details" right drawer renders a full preview view including text-file preview (capped at 256 KiB shaping, 8 MiB max file). (`other/src/tab.rs:78-82`).
- **Gallery view** (Space) renders the focused image full-screen with prev/next arrows.

### 1.12 Sidebar / places (nav bar)
- **Sections**: Home, Recents (toggleable in settings), favorites (Documents/Downloads/Music/Pictures/Videos by default; user can add arbitrary `Favorite::Path`), Network favorites (`Favorite::Network { uri, name, path }`), trash, mounter (mounted drives / network mounts via gvfs).
- **Drag-and-drop reorder** within the nav bar (segmented-button DragId).
- **Context menu per sidebar item**: Open, Open with, Open in new tab, Open in new window, Show details, Add/Remove from sidebar, Run context-action, Clear Recents, Empty trash, plus an Eject icon on mounted entries.
- **Add to sidebar** (Ctrl+D) for any selected folder or network item. (`Action::AddToSidebar`).
- **Favorite-path-error dialog**: if a saved favorite no longer exists/works, dialog offers Remove or Keep. (`DialogPage::FavoritePathError`).

### 1.13 Mounts (gvfs)
- **`mounter/gvfs.rs`** (756 LOC): full gvfs integration via gio for network drives, USB/UDisks mounts, eject. (`other/src/mounter/`).
- **Mount auth dialog**: username, domain, password, remember-password toggle, anonymous mode. (`MounterAuth` struct, `DialogPage::NetworkAuth`).
- **Mount error dialog** with diagnostics. (`DialogPage::MountError`, i18n `mount-error`).
- **Eject** action on mounted volume.
- **Network scan**: lists items inside a network URI (`mounter.network_scan(uri, sizes)`).
- **Network share addressable as a tab location** (`Location::Network(uri, name, Option<path>)`).
- **gvfs-backed remote item metadata** (`ItemMetadata::GvfsPath { mtime, size, children }`) so listings work even when paths aren't local. (`other/src/tab.rs:1806-1812`).

### 1.14 Desktop mode features
- **Desktop renders the user's Desktop folder as the wallpaper-area**: `Location::Desktop(path, display, DesktopConfig)`. (`other/src/tab.rs:1440`).
- **Per-display config** (multi-monitor): each display can override.
- **"Show on Desktop" panel** (`Message::DesktopViewOptions`): toggles for Desktop folder content / Mounted drives / Trash folder icon, icon size and grid spacing sliders (50–500%, step 25). (`other/src/app.rs:1969-2044`).
- **Wayland layer-surface** rendering for desktop (`OutputEvent`, `OverlapNotifyEvent`, `Focused`, `WlOutput` hooks, feature-gated). (`other/src/app.rs:356-394`).
- **Wallpaper / Display / Desktop-appearance shortcuts**: right-click on the desktop offers menu entries that launch `cosmic-settings` with the right page. (`Action::CosmicSettings*`).

### 1.15 Recents
- Implements XDG `recently-used-xbel` (`recently_used_xbel` crate).
- **Auto-population**: every `Open` writes the path to Recents. (`other/src/app.rs:787-883`).
- **Remove from recents** (per-item) and **Clear Recents history** (top-level) actions. (`Action::RemoveFromRecents`, `Command::ClearRecents`, `NavMenuAction::ClearRecents`).
- **Toggle "Recents folder in the sidebar"** in Settings. (`Config.show_recents`).

### 1.16 Trash
- **`trash` crate, XDG spec**: per-volume trash dirs, `.trashinfo` metadata, restore + restore-to-original-location.
- **Trash freshness watcher**: `RescanTrash` message + watcher on the trash folders; sidebar count updates automatically.
- **Sort by deletion-time** (`HeadingOptions::TrashedOn`).
- **Search in trash** as a first-class location.

### 1.17 Dialogs (catalogue)
All in `DialogPage` `other/src/app.rs:517-590`:
- **Compress**: name + archive-type dropdown (zip/tgz) + optional zip password.
- **EmptyTrash**: are-you-sure.
- **FailedOperation / FailedOperations**: shows operation error, can retry.
- **ExtractPassword**: per-extract password prompt.
- **MountError, NetworkAuth, NetworkError**: see Mounts.
- **NewItem**: new folder or new file with hidden-warning and reserved-name validation.
- **RunContextAction**: confirms running a configured shell pipeline on N items.
- **OpenWith**: app picker (see 1.9).
- **PermanentlyDelete, DeleteTrash**: separate confirmations.
- **RenameItem**: with conflict detection. Reserves Ctrl+Enter-vs-Enter so apply-to-all interaction works.
- **Replace**: full conflict resolution dialog (see Operations).
- **SetExecutableAndLaunch**: confirm + run.
- **FavoritePathError**: see Sidebar.
- **DialogPages queue** (`other/src/app.rs:592-654`): a `VecDeque` so multiple dialogs can stack; pushes/pops emit `DesktopDialogs(true/false)` so Wayland desktop mode shows a top-level layer.

### 1.18 Notifications, toasts, footer
- **Toast system** (`widget::toaster::Toasts`) with action buttons ("Undo") and timeout. (`other/src/app.rs:480, 755`).
- **`notify-rust` system notifications** while ops are pending; auto-close when ops finish. (`other/src/app.rs:1858-1877`).
- **Footer progress bar** with global pause/cancel/dismiss. (1.8).

### 1.19 Settings panel (in-app)
- Appearance: theme dropdown (Match desktop / Dark / Light).
- **Type-to-search** mode (3 radios).
- **Single click to open** (per-tab `TabConfig.single_click`).
- **Show Recents in sidebar** toggle.
- All settings persisted through `cosmic-config` (versioned, atomic). (`other/src/config.rs`).

### 1.20 Persisted configuration (everything in `Config`)
`other/src/config.rs:163-216` — every persisted key:
- `app_theme`, `dialog` (DialogConfig: folders_first, icon_sizes, show_details, show_hidden, view).
- `desktop` (DesktopConfig: grid_spacing, icon_size, show_content, show_mounted_drives, show_trash).
- `context_actions: Vec<ContextActionPreset>` — user shell actions.
- `thumb_cfg` (jobs, max_mem_mb, max_size_mb).
- `favorites: Vec<Favorite>` (Home / Documents / ... / Path / Network).
- `show_details`, `show_recents`.
- `tab` (TabConfig: folders_first, icon_sizes, military_time, show_hidden, single_click, view).
- `type_to_search`.
- **Per-folder sort preference**: `State.sort_names: FxOrderMap<path_string, (HeadingOptions, ascending)>` — each visited folder remembers its sort. (`other/src/config.rs:114-129`).

### 1.21 Window / system integration
- **System tray icon / quit** wiring (`Message::WindowClose`, `Message::MaybeExit`).
- **Window maximise / drag handle** custom callbacks (`WindowDrag`, `WindowToggleMaximize`).
- **Multiple top-level windows**: per-window context menus, preview pane in detached window, file-dialog window. (`WindowKind` `other/src/app.rs:667-674`).
- **Wayland-specific**: `OutputEvent`, `OverlapNotifyEvent`, layer-surface, applet hooks (feature-gated). (`other/src/app.rs:356-394, 6608-7060`).
- **Cosmic theme integration**: dark/light/system follows desktop preference, live-updates on system theme change.

### 1.22 i18n & accessibility
- **70+ language packs** in `other/i18n/` via `i18n_embed` + Fluent (`.ftl`).
- All visible strings in `cosmic_files.ftl`; mode names (file permissions) translated too.
- **Underline-marked mnemonics in dialog labels** (`_Save` → Alt+S key bind auto-generated from `_X` markup). (`other/src/dialog.rs:139-189`).

### 1.23 Subtle / "hidden" features worth flagging
- **Sort persistence** is per-folder (folder string is the key), not global.
- **Tab `gallery` state** is per-tab (you can have one tab in gallery mode while others stay in list).
- **`Item::is_mount_point` flag** changes the right-click menu (Eject only, no Cut/Rename/MoveToTrash).
- **`location.supports_paste()`** disables Paste in Trash / network views without local mount.
- **Mode `Mode::App | Mode::Desktop | Mode::Dialog(_)` filters** sit on every key bind so dialog mode doesn't accept `Ctrl+W` etc. (`other/src/key_bind.rs:55-87`).
- **Tab DnD hover-to-switch** (1.6 s) lets you drop into a tab by hovering it.
- **Auto-scroll while drag-selecting** (`AutoScroll(f32)` ramp).
- **Replace dialog has an `apply_to_all` checkbox** so 500-conflict copies don't 500-prompt.
- **`item_from_search_item`** branches between Path and Trash to keep one Item type. (`other/src/tab.rs:737`).
- **`folder_name()` returns `(name, is_home)`** so home dir gets a different icon in breadcrumbs.
- **SPECIAL_DIRS map** maps XDG dirs to themed folder icons (folder-documents, folder-download, etc.). (`other/src/tab.rs:120-150`).
- **Dialog filename "double-click on dot" select-stem behaviour** (`.double_click_select_delimiter('.')`). (`other/src/dialog.rs:586`).
- **Search-input box swaps to icon button** when window is condensed. (`other/src/app.rs:6411-6437`).
- **Cross-device move** detection — falls back to copy+delete with progress.
- **Trash auto-empty-detection** drives the sidebar icon (`Trash::is_empty()` checks via `trash::os_limited::is_empty`).
- **State `must_save_sort_names` flag** debounces sort-state writes. (`other/src/app.rs:734`).
- **`Item::location_opt`** lets the same Item live in Trash, Path, or Network and behave correctly in every context.
- **`tab.rs` has a `THUMB_SEMAPHORE`** (`num_cpus.min(4)`) gating thumbnail workers globally. (`other/src/tab.rs:86-87`).

---

## 2. Features in fm but NOT in cosmic-files

### 2.1 UI / layout
- **Two-pane split view (F3)** with movable divider, swap-active-pane on click. Each pane has its own tabs, history, listing, watcher path; only the active pane reacts to keyboard. Ratio persisted in `Settings.split-ratio`. (`app/src/state.rs:199-285`, `app/src/glue/split.rs`).
- **7-step element-size scrubber in the status bar** (bottom right), separate from view mode — you can crank list-view rows huge or shrink gallery tiles. Ctrl+wheel hits the same callback. List and Gallery both honour it. (`app/ui/components/status_bar.slint`, `app/src/main.rs:87-118`).
- **View-mode is decoupled from icon size**. cosmic-files implicitly grows tiles with zoom; fm has independent dimensions.
- **Three-section places sidebar** with explicit "Computer / Places / Devices / Bookmarks" headers (cosmic uses favorites + mounts implicitly). (`app/src/glue/places.rs:49-134`).
- **Status bar shows live free / total disk** for current folder, refreshed on navigation via `disk_space::query`. (`app/src/glue/navigation.rs:601-614`, `crates/core/src/disk_space.rs`).
- **Resizable sidebar with drag handle**, width persisted. (`app/ui/components/places_sidebar.slint:72-92`).
- **Resizable list columns** (Name / Size / Modified / Kind) with drag grippers on each header edge, plus a **per-column visibility toggle** (right-click header → check Size / Modified / Kind), persisted in `Settings`. (`app/ui/components/file_list_view.slint`).
- **Colour-coded disk-usage bar per device** in the sidebar: green < 75% used, amber 75-90%, red > 90%. (`app/ui/components/places_sidebar.slint`).
- **Monochrome / Colored icon-set switch** in settings (the colour set is kept flat - no gradients/shadows). (`app/ui/components/settings_popup.slint`).
- **Path bar click-anywhere-to-edit** zone: clicking the empty trailing space of the breadcrumbs switches to free LineEdit. (`app/ui/components/toolbar.slint:127-139`).
- **Custom "place clicked" routing** that recognises `udisks:<path>` and `mtp:<id>` URIs to dispatch to the right backend instead of opening as a path. (`app/src/glue/places.rs:200-225`).

### 2.2 Bulk rename
- **Dedicated bulk-rename dialog** triggered when ≥ 2 items are selected and the user hits F2. (`app/src/glue/file_ops.rs:519-558`).
- **Template pattern** with placeholders: `{idx}`, `{N:idx}` (zero-padded), `{name}`, `{ext}`, `{ext.}`, escapes `{{` `}}`. Unknown placeholders pass through verbatim. (`crates/core/src/bulk_rename.rs`).
- **Live preview model** with conflict detection: empty/invalid name, duplicate-within-batch, hits-existing-file-not-in-batch. Counter pushed to the dialog. (`app/src/glue/file_ops.rs:114-177`).

### 2.3 Devices / MTP
- **Direct UDisks2 integration via zbus** (no gvfs intermediate). Live add/remove via `InterfacesAdded` / `InterfacesRemoved` signals. (`app/src/glue/udisks.rs`).
- **System partition filtering**: skips devices where `HintSystem=true` unless mounted under /run/media, /media, /mnt.
- **Mount on click** of an unmounted device entry (calls `org.freedesktop.UDisks2.Filesystem.Mount`).
- **MTP device detection** via `mtp_rs` (`app/src/glue/mtp.rs`). Polls every 5 s, surfaces label + vendor/product as a sidebar row. Browsing is stubbed for now.

### 2.4 Folder watcher (live refresh)
- **Per-current-folder `notify` watcher**, **opt-in** (`Settings.auto-refresh`, default off). Non-recursive so attaching to a giant tree is O(1). Debounced 300 ms — a compiler producing 100 .o files results in one refresh, not 100. (`app/src/glue/watcher.rs`).
- Cosmic-files reloads on `RescanRecents/RescanTrash` and via a debounce-watcher for current tab, but ours is **user-toggleable** with explicit UI affordance.

### 2.5 Search behaviour deltas
- **Search uses a separate Slint model** (`search-rows`) so the underlying folder listing is preserved underneath; closing search returns you to the prior selection. (`app/src/glue/search.rs:64-78`).
- **Search hit-paths array** maintained parallel to the model so row clicks / open / clipboard ops can resolve to absolute paths (since the search results don't share `entries`' index space). (`app/src/state.rs:71-72`).

### 2.6 Bulk-open confirmation
- Threshold of **5 items**: opening 5+ files with Enter / row-activated triggers a **BulkOpenConfirmDialog** so an accidental Ctrl+A + Enter doesn't fire 30 PDFs at once. (`app/src/glue/file_ops.rs:373-403`).

### 2.7 Navigation error dialog
- **Pre-flight validation of local paths** before navigation (`preflight_local_path`): handles NotFound, PermissionDenied, broken symlink, "this is a file not a folder", "symlink target is a file" each with a user-readable message in a dedicated **NavErrorDialog** instead of silently rendering an empty listing. (`app/src/glue/navigation.rs:561-597`).

### 2.8 Bookmarks store
- **Independent on-disk bookmarks** (`$XDG_CONFIG_HOME/fm/data/bookmarks.toml`) separate from XDG favorites. Add via right-click → Add to bookmarks. Removable. Schema-resilient (malformed file → empty store, never panic). (`app/src/bookmarks.rs`).
- Cosmic-files has a single `favorites` list (XDG + arbitrary paths + network) in the same config.

### 2.9 Folder Properties (empty-area)
- Right-click on empty area → **"Folder properties"** runs the properties dialog on the *current directory*. cosmic-files only offers item-level "Show details". (`app/src/glue/properties.rs:55-81`).

### 2.10 Multi-selection Properties
- Properties on N items: walks every entry, deep-counts directories, sums sizes in the background with live progress. Owner/group/mode displayed as "Mixed" placeholder. (`app/src/glue/properties.rs:162-240`).
- cosmic-files' Show-details pane has a `multi_preview_view` but doesn't deep-count.

### 2.11 Deep-count infrastructure
- **`fm_core::deep_count`** uses jwalk (rayon-backed parallel walk), respects cancellation, emits ~every 120 ms. (`crates/core/src/file_ops.rs:126-170`).
- cosmic-files has `DirSize::Calculating(Controller)` but uses single-threaded walking.

### 2.12 Rubber-band selection (grid)
- **Drag in empty area of grid view** sends `(x1, y1, x2, y2, tile_w, tile_h, cell_w, cols, gutter)` to Rust which computes intersect → selection set. (`app/src/glue/selection.rs:94-136`).
- cosmic-files has tab-bar selection and item drag but no rubber-band marquee.

### 2.13 Ctrl+wheel diagnostics
- The "debug-wheel" + "debug-ctrl-change" callbacks log every wheel event with the live Ctrl flag (Slint 1.16 winit workaround). Practical for users diagnosing missed-event reports. (`app/src/main.rs:104-118`, `app/ui/globals.slint:245-254`).

### 2.14 Internal architecture niceties (not features per se but visible)
- **Bytes packed as (lo, hi) i32 pairs** into Slint structs so we can pass `u64` through Slint property types. (`app/src/glue/clipboard.rs:299-301`).
- **Tracing spans across every async op** (paste/rename/trash/restore/search/properties/navigate). User-observable as more useful debug logs.
- **All settings auto-save on 500 ms debounce** (`install_settings_autosave` `app/src/main.rs:150-201`). cosmic-files writes through `cosmic-config` immediately.
- **Validate-then-navigate flow** also blocks history-poisoning: a bad typed path never gets onto the back stack.

---

## 3. Notable differences in shared features

- **Operations engine**: cosmic-files runs ops through a compio (io_uring) runtime with per-op pause/resume/cancel **and** a queue, a Replace conflict-resolution dialog, retry of failed ops, a footer progress bar, system notifications, undo toasts, and an "Edit history" pane. Ours is tokio-based: copy/move have progress + cancel, but no pause/resume, no Replace dialog (collisions go through `unique_destination` → " (1)" suffix), no failed-op retry, no system notifications. Single progress dialog at a time. **fm now has keyboard undo/redo** (Ctrl+Z / Ctrl+Y / Ctrl+Shift+Z) but only for the reversible non-destructive ops - rename, new-file/folder (undo trashes the created item), and move-to-trash (undo restores). Copy/move/delete are intentionally not undoable. No undo toast - it's keyboard-driven and session-only (not persisted). (`app/src/glue/undo.rs`).
- **Search**: cosmic-files uses regex with three search-location kinds (Path/Recents/Trash) and supports "Open item location"; ours is case-insensitive substring on file names only, no regex, no trash/recents search, ~5000-result cap.
- **Sort**: both sort by Name / Size / Modified / Kind via clickable list-view column headers (click toggles direction, dirs always first). cosmic-files persists sort **per folder** (`State.sort_names` map); fm uses a single global sort key/order in `Settings` (`app/src/glue/sort_select.rs`, `app/src/settings.rs`). fm's name comparison is case-insensitive lexicographic, not yet natural-order (so `file10` sorts before `file2`).
- **Thumbnails**: both have async workers with generation counters. cosmic-files generates per XDG spec at multiple sizes + has tiling for big images (multi-GB images don't OOM). Ours uses XLarge (512 px) only, no tiling fallback, no per-size cache reuse.
- **Tabs**: cosmic-files has reorderable tabs with DnD, hover-to-switch, scroll-to-cycle. Ours is a click-only tab strip with a "+" button (no DnD, no scroll, no reorder, no hover-to-switch).
- **Clipboard**: cosmic-files writes 5 MIME types (interoperates with Nautilus/Nemo cut/copy) and reads images/text/video too. Ours stores file paths in-process only — copy/paste of files between apps won't work, and pasting an image from the OS clipboard isn't supported. fm does write to the OS clipboard for one thing: Copy Path (Ctrl+Shift+C) puts the path text on the system clipboard via `arboard`. (`app/src/glue/clipboard.rs`).
- **DnD**: cosmic-files has full DnD source + destination on tabs, nav-bar, listing, hover timers, plus Wayland integration. Ours has none.
- **Trash**: cosmic-files supports search-in-trash, sort-by-deletion-time, per-item restore-to-original-location, and Empty-Trash from anywhere. Ours: trash listing only via XDG dir browsing; restore picks original location through `trash` crate; no sort-by-deletion-time, no in-trash search.
- **Open With**: both now have an Open With app-picker dialog. cosmic-files lists MIME-related apps, has a browse-store button, and writes the default to `mimeapps.list`. fm now has its own picker (`app/src/glue/open_with.rs`): scans `.desktop` entries via `default_app::apps_for_mime`, ranks recommended-first, lets you run a custom shell command (de-duplicated and remembered in `~/.config/fm/data/custom_apps.toml`), and **sets the default** by writing `[Default Applications]` in `~/.config/mimeapps.list` (`default_app::set_default`). Differences vs cosmic: fm operates on one MIME at a time (MIME of the first selected file), has no browse-store, can't set a *custom command* as the persistent default, and has no UI to clear a default once set. (`app/src/glue/open_with.rs`, `app/src/glue/default_app.rs`).
- **Archives**: both compress and extract. cosmic-files uses its own `archive.rs` (tar.gz or zip, optional AES-256 zip password) and an Extract op with extract-here / extract-to and an ExtractPassword dialog. fm shells out to `7z` (p7zip) for everything (`app/src/glue/archive.rs`): Extract-here covers zip/7z/rar/tar.*/standalone .gz/.bz2/.xz/.zst and password-protected archives, with a multi-archive queue that caches a working password across the batch; Compress opens a dialog (`CompressDialog`). Trade-off: fm needs `7z` on PATH; it logs a clean error if absent. fm extracts content-sniffed archives even with no extension; cosmic offers a wider compress-format toggle in-dialog.
- **New file/folder**: cosmic-files validates names against "no slashes", "not '.'/'..'", warns about hidden-prefix, blocks reserved Windows names in dialogs. Ours validates "no '/', not empty, not '.'/'..'" (no hidden-prefix warning).
- **Permissions**: both can change file mode (chmod). cosmic-files uses a 0-7 mode picker in the preview pane with a `SetPermissions` op. fm now has writable permissions too - the Properties dialog shows 9 owner/group/other r/w/x checkboxes plus an octal readout and applies via `fm_core::set_permissions` → `PermissionsExt::set_mode` (`app/src/glue/properties.rs`, `crates/core/src/file_ops.rs`). fm's is single-file only (disabled for multi-selection); no multi-selection `SetMultiplePermissions` equivalent.
- **Network / mounts**: both reach network drives. cosmic-files has gvfs (SMB/SFTP/FTP/WebDAV/NFS/AFP) baked into the app with its own auth/eject/network-scan UI and a dedicated NetworkDrive context page. fm now also speaks gvfs but via the `gio` CLI (`app/src/glue/remote.rs`): typing `ssh://`/`sftp://`/`smb://`/`ftp://`/`ftps://`/`dav://`/`davs://`/`nfs://` into the address bar mounts the share (anonymous first, password dialog on rejection) and browses it through its FUSE mountpoint as a normal `Location::Local`. So all listing/sort/file-ops work unchanged on remote shares. fm lacks AFP, an in-app eject for network mounts, remember-password, and a network-scan/browse UI; new-host SSH key trust must be established outside the app first. For local drives fm uses direct UDisks2 over zbus + MTP detection (browsing stubbed), where cosmic uses gvfs for those too.
- **Settings UI**: cosmic-files has theme dropdown + type-to-search radios + single-click + show-recents toggles. Ours has a dark-theme toggle, show-hidden, auto-refresh, a thumbnail-size dropdown (Normal 128 / Large 256 / X-Large 512 / XX-Large 1024), a thumbnail memory-budget dropdown (512 MB - 8 GB / Unlimited), and an icon-style dropdown (Monochrome / Colored). No type-to-search mode, no single-click-to-open. (`app/ui/components/settings_popup.slint`).
- **Localization**: cosmic-files: 70+ Fluent locales. Ours: English only, all strings hard-coded in `globals.slint` `Translations`.
- **Sidebar favorites**: cosmic-files Add-to-sidebar accepts both folders and network items; supports `Favorite::Network { uri, name, path }` directly. Ours: bookmarks only, paths only, must be a directory.
- **Desktop mode**: cosmic-files runs as the desktop with wallpaper integration, mounted-drive icons, trash icon, per-display config. Ours has no desktop mode.
- **File-picker API**: cosmic-files exposes `Dialog<M>` for embedding in other Iced apps. Ours has none — fm is a self-contained binary.
- **Keyboard surface**: cosmic-files has ~35 shortcuts including Ctrl+L (edit location), Ctrl+D (add to sidebar), Ctrl+Shift+C (copy path), Space (gallery), Ctrl+Space (preview), Shift+arrows (range), F5 (reload). Ours now covers ~28 (`app/ui/main_window.slint`): select-all, Ctrl+F search, F5 refresh, F3 split, arrows / PgUp / PgDn / Home / End nav, Ctrl+H hidden, Backspace up, **Ctrl+Z undo / Ctrl+Y / Ctrl+Shift+Z redo**, Ctrl+X/C/V, **Ctrl+Shift+C copy-path**, Delete / Shift+Delete, F2 rename, Alt+Return properties, Return activate, Ctrl+N new folder, Ctrl+T new tab, Ctrl+W close tab, Ctrl+1 list / Ctrl+2 gallery, Ctrl+= / Ctrl+- size. Still missing vs cosmic: Ctrl+L (edit location - fm uses click-on-breadcrumb instead), Ctrl+D add-to-sidebar, Space gallery, Ctrl+Space preview. Alt+L is mapped to "toggle auto-refresh" instead of any cosmic-files action.

---

## Counts

- cosmic-files distinct features catalogued in §1: **~145 bullet items** spread across 23 sections.
- fm features not in cosmic-files (§2): **~25 distinct items**.
- Shared-but-different deltas (§3): **17 items**.

Since the previous revision fm has **closed several of the big gaps**: it now has an Open-With picker with set-default-app (`mimeapps.list`), keyboard undo/redo for reversible ops, writable chmod permissions, gvfs network drives via the `gio` CLI, and compress alongside extract. The gaps that remain the other way: **inter-app clipboard / DnD**, **regex search & search-in-trash/recents**, **operations queue + pause/resume + replace dialog + undo toast (and undo of copy/move/delete)**, **desktop mode**, **i18n**, **per-folder sort persistence**, and a polished in-app network UI (eject / remember-password / network-scan). The dominant strengths of fm: **split view**, **bulk rename with template engine**, **direct UDisks/MTP**, **deep-count properties with multi-selection**, **disk-space indicator**, **bulk-open confirmation**, **nav-error dialog**, **rubber-band marquee**, and **opt-in folder watcher**.
