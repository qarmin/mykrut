//! "Extract here" support for archives.
//!
//! Backend: we shell out to `7z` (p7zip-full) for every supported format —
//! that's the one tool that covers zip / 7z / rar / tar.* / standalone
//! .gz/.bz2/.xz/.zst *and* handles password-protected archives uniformly.
//! Trade-off: it's a runtime dependency. If `7z` isn't on PATH we log a
//! clean error rather than attempting per-format fallbacks (that would
//! triple the code size for marginal benefit; the password vault story in
//! TODO.md also assumes 7z's command-line interface).
//!
//! Queueing model: the user can select multiple archives. We process them
//! one at a time on the tokio runtime, so the UI stays responsive. If an
//! archive is encrypted we open the password dialog, pause the queue, and
//! resume after the user submits / cancels. On a successful password we
//! cache it in-memory and try it as the first guess for subsequent
//! archives in the same batch — common case: a folder of archives sharing
//! one password.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use slint::{ComponentHandle, SharedString};
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

use crate::format_util::human_size;
use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{Callabler, DialogState, MainWindow, ProgressData, Translations};

/// Lowercase extensions we treat as extractable. Single-file compressors
/// (.gz/.bz2/.xz/.zst) are listed because 7z handles them too; the result
/// is the uncompressed payload sitting next to the source.
const ARCHIVE_EXT_DOUBLE: &[&str] = &[".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"];
const ARCHIVE_EXT_SINGLE: &[&str] = &[
    ".zip", ".7z", ".rar", ".tar", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".gz", ".bz2", ".xz", ".zst",
];

pub fn is_archive_path(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase());
    let Some(name) = name else { return false };
    ARCHIVE_EXT_DOUBLE.iter().any(|e| name.ends_with(e)) || ARCHIVE_EXT_SINGLE.iter().any(|e| name.ends_with(e))
}

/// True for archive MIME types. Lets content-sniffed (extensionless) files such
/// as a zip simply named "C" still offer Extract, since 7z extracts by content.
pub fn is_archive_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/zip"
            | "application/x-7z-compressed"
            | "application/x-rar-compressed"
            | "application/vnd.rar"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-bzip2"
            | "application/x-xz"
            | "application/zstd"
            | "application/x-zstd"
            | "application/x-tar"
            | "application/x-compressed-tar"
    )
}

/// An entry is extractable if its name has an archive extension OR its MIME
/// (possibly content-sniffed) is an archive type.
pub fn is_archive_entry(e: &mykrut_core::FileEntry) -> bool {
    !e.is_dir() && (is_archive_path(&e.path) || e.mime.as_deref().is_some_and(is_archive_mime))
}

thread_local! {
    static EXTRACT_STATE: RefCell<ExtractState> = const { RefCell::new(ExtractState::new()) };
    /// (parent dir, selected paths) captured when the Compress dialog opens.
    static COMPRESS_STATE: RefCell<(PathBuf, Vec<PathBuf>)> = const { RefCell::new((PathBuf::new(), Vec::new())) };
}

struct ExtractState {
    /// Archives left to process (after `current`).
    queue: VecDeque<PathBuf>,
    /// Archive being worked on; `Some` while a 7z spawn is in flight or
    /// while the password dialog is waiting for input.
    current: Option<PathBuf>,
    /// Last password that worked in this batch — tried first on the next
    /// archive before falling back to "no password".
    cached_password: Option<String>,
}

impl ExtractState {
    const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            current: None,
            cached_password: None,
        }
    }
}

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    let _g = rt.enter();

    // ── Compress (create archive) ────────────────────────────────────────
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_request_compress(move || {
            let app = weak.upgrade().expect("MainWindow alive in request-compress");
            let paths = collect_selected_all(&app, &state);
            if paths.is_empty() {
                return;
            }
            let parent = paths[0].parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            // Default name: the single item's stem, or "archive" for several.
            let default = if paths.len() == 1 {
                paths[0]
                    .file_stem()
                    .map_or_else(|| "archive".to_string(), |s| s.to_string_lossy().into_owned())
            } else {
                "archive".to_string()
            };
            COMPRESS_STATE.with(|c| *c.borrow_mut() = (parent, paths));
            let ds = app.global::<DialogState>();
            ds.set_compress_default_name(default.into());
            ds.set_compress_open(true);
        });
    }
    {
        let weak = app.as_weak();
        let rt_c = rt.clone();
        let state_c = state.clone();
        let watcher_c = watcher.clone();
        app.global::<Callabler>()
            .on_compress_confirmed(move |name, format, password| {
                let app = weak.upgrade().expect("MainWindow alive in compress-confirmed");
                start_compress(
                    &app,
                    &rt_c,
                    &state_c,
                    &watcher_c,
                    name.to_string(),
                    format,
                    password.to_string(),
                );
            });
    }

    let weak = app.as_weak();
    let rt_for_start = rt.clone();
    let state_for_start = state.clone();
    let watcher_for_start = watcher.clone();
    app.global::<Callabler>().on_extract_selected(move || {
        let app = weak.upgrade().expect("MainWindow alive in extract-selected");
        let archives = collect_selected_archives(&app, &state_for_start);
        if archives.is_empty() {
            info!("extract-selected: nothing extractable in selection");
            return;
        }
        EXTRACT_STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.queue = archives.into();
            s.current = None;
        });
        process_next(
            app.as_weak(),
            rt_for_start.clone(),
            state_for_start.clone(),
            watcher_for_start.clone(),
            None,
        );
    });

    let weak = app.as_weak();
    let rt_for_pw = rt.clone();
    let state_for_pw = state;
    let watcher_for_pw = watcher.clone();
    app.global::<Callabler>().on_extract_password_confirmed(move |pw| {
        let app = weak.upgrade().expect("MainWindow alive in extract-pw");
        let pw = pw.to_string();
        // Retry the current archive with the new password.
        run_current(
            app.as_weak(),
            rt_for_pw.clone(),
            state_for_pw.clone(),
            watcher_for_pw.clone(),
            Some(pw),
        );
    });

    let weak = app.as_weak();
    let _watcher_for_cancel = watcher;
    app.global::<Callabler>().on_extract_cancelled(move || {
        let _app = weak.upgrade().expect("MainWindow alive in extract-cancel");
        info!("extract: cancelled by user — clearing queue");
        EXTRACT_STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.queue.clear();
            s.current = None;
            s.cached_password = None;
        });
    });
}

/// All currently-selected paths (search-aware): in search mode they come from
/// the search-rows model, otherwise from `state.entries`.
fn collect_selected_all(app: &MainWindow, state: &AppStateRc) -> Vec<PathBuf> {
    if crate::glue::search_focused(app, state) {
        use slint::Model;
        let model = app.get_search_rows();
        let n = model.row_count();
        let s = state.borrow();
        (0..n)
            .filter_map(|i| {
                let row = model.row_data(i)?;
                if !row.selected {
                    return None;
                }
                s.search_hit_paths.get(i).cloned()
            })
            .collect()
    } else {
        let s = state.borrow();
        let mut idxs: Vec<usize> = s.selected.iter().copied().collect();
        idxs.sort_unstable();
        idxs.into_iter()
            .filter_map(|i| s.entries.get(i).map(|e| e.path.clone()))
            .collect()
    }
}

fn collect_selected_archives(app: &MainWindow, state: &AppStateRc) -> Vec<PathBuf> {
    // Search results don't carry MIME, so fall back to extension there.
    if crate::glue::search_focused(app, state) {
        return collect_selected_all(app, state)
            .into_iter()
            .filter(|p| is_archive_path(p))
            .collect();
    }
    let s = state.borrow();
    let mut idxs: Vec<usize> = s.selected.iter().copied().collect();
    idxs.sort_unstable();
    idxs.into_iter()
        .filter_map(|i| s.entries.get(i))
        .filter(|e| is_archive_entry(e))
        .map(|e| e.path.clone())
        .collect()
}

/// Pop the next archive from the queue and try to extract it (with the
/// cached batch password as the first guess if any). Refreshes the view
/// when the queue empties.
fn process_next(
    weak: slint::Weak<MainWindow>,
    rt: Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    explicit_pw: Option<String>,
) {
    let next = EXTRACT_STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.current = s.queue.pop_front();
        s.current.clone()
    });
    let Some(archive) = next else {
        // Queue drained — refresh the current folder so the new dirs show.
        if let Some(app) = weak.upgrade() {
            refresh_current(&app, &rt, &state, &watcher);
        }
        return;
    };
    info!(archive = %archive.display(), "extract: starting");
    let pw = explicit_pw.or_else(|| EXTRACT_STATE.with(|s| s.borrow().cached_password.clone()));
    run_current(weak, rt, state, watcher, pw);
}

/// Run 7z against `state.current` with the supplied password (or none).
/// Branches on outcome: success → next queue entry; wrong-password →
/// re-open dialog; other failure → log + skip.
fn run_current(
    weak: slint::Weak<MainWindow>,
    rt: Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    pw: Option<String>,
) {
    let archive = EXTRACT_STATE.with(|s| s.borrow().current.clone());
    let Some(archive) = archive else {
        return;
    };
    let dest = destination_for(&archive);
    let rt_for_task = rt.clone();
    let pw_for_task = pw.clone();
    let archive_for_task = archive.clone();
    let dest_for_task = dest.clone();

    let _g = rt.enter();
    let _ = slint::spawn_local(async move {
        let res = rt_for_task
            .spawn(async move { extract_via_7z(&archive_for_task, &dest_for_task, pw_for_task).await })
            .await
            .unwrap_or_else(|err| {
                error!(?err, "extract worker panicked");
                Err(ExtractError::Other("extract worker panicked".into()))
            });

        let Some(app) = weak.upgrade() else { return };
        match res {
            Ok(()) => {
                info!(archive = %archive.display(), dest = %dest.display(), "extract ok");
                // Remember the password that just worked for the next archive.
                if pw.is_some() {
                    EXTRACT_STATE.with(|s| s.borrow_mut().cached_password = pw.clone());
                }
                process_next(app.as_weak(), rt.clone(), state.clone(), watcher.clone(), None);
            }
            Err(ExtractError::WrongPassword) => {
                warn!(archive = %archive.display(), "extract: wrong/missing password");
                open_password_dialog(&app, &archive, pw.is_some());
            }
            Err(ExtractError::ToolMissing) => {
                error!("extract: 7z not on $PATH — install p7zip-full");
                EXTRACT_STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    s.queue.clear();
                    s.current = None;
                });
            }
            Err(ExtractError::Other(msg)) => {
                error!(archive = %archive.display(), %msg, "extract failed");
                // Skip this one, try the rest.
                process_next(app.as_weak(), rt.clone(), state.clone(), watcher.clone(), None);
            }
        }
    });
}

fn open_password_dialog(app: &MainWindow, archive: &Path, retry: bool) {
    let ds = app.global::<DialogState>();
    let name = archive
        .file_name()
        .map_or_else(|| archive.display().to_string(), |s| s.to_string_lossy().into_owned());
    ds.set_extract_archive_name(name.into());
    ds.set_extract_error(if retry { "retry".into() } else { "".into() });
    ds.set_extract_password_open(true);
}

#[derive(Debug)]
enum ExtractError {
    WrongPassword,
    ToolMissing,
    Other(String),
}

async fn extract_via_7z(archive: &Path, dest: &Path, password: Option<String>) -> Result<(), ExtractError> {
    use tokio::process::Command;

    info!(archive = %archive.display(), dest = %dest.display(), "7z extract");

    // Make sure the destination exists. 7z would create it, but we want
    // unique-suffix semantics handled here (the parent dir definitely
    // exists; only the leaf may collide and we picked a unique name in
    // destination_for already).
    if let Err(err) = tokio::fs::create_dir_all(dest).await {
        return Err(ExtractError::Other(format!("create dest: {err}")));
    }

    // Always supply -p (empty if no password) so 7z never prompts on
    // stdin. -y answers any remaining "yes/no" prompts.
    let pw_arg = match &password {
        Some(p) => format!("-p{p}"),
        None => "-p".to_string(),
    };

    let output = Command::new("7z")
        .arg("x")
        .arg(archive)
        .arg(format!("-o{}", dest.display()))
        .arg(pw_arg)
        .arg("-y")
        .output()
        .await
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                ExtractError::ToolMissing
            } else {
                ExtractError::Other(format!("spawn 7z: {err}"))
            }
        })?;

    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = format!("{stdout}{stderr}").to_lowercase();
    // 7z prints variants of "Wrong password?" / "Data Error in encrypted
    // file. Wrong password?" / "Can not open encrypted archive" when the
    // password is missing or wrong.
    if lower.contains("wrong password")
        || lower.contains("data error in encrypted file")
        || lower.contains("cannot open encrypted")
        || lower.contains("can not open encrypted")
        || lower.contains("encrypted archive")
        || lower.contains("enter password")
    {
        return Err(ExtractError::WrongPassword);
    }
    Err(ExtractError::Other(stderr.into_owned()))
}

/// Pick a destination directory next to the archive named after the
/// archive's stem, falling back to a unique suffix if it already exists.
fn destination_for(archive: &Path) -> PathBuf {
    let parent = archive.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let name = archive
        .file_name()
        .map_or_else(|| "extracted".to_string(), |s| s.to_string_lossy().into_owned());
    let stem = strip_archive_extensions(&name);
    mykrut_core::unique_destination(&parent, &stem)
}

#[expect(
    clippy::string_slice,
    reason = "ARCHIVE_EXT_* are all ASCII, so when `lower` ends with `ext` the trailing \
              `ext.len()` bytes of `name` are ASCII too, making `name.len() - ext.len()` a char boundary"
)]
fn strip_archive_extensions(name: &str) -> String {
    let lower = name.to_lowercase();
    for ext in ARCHIVE_EXT_DOUBLE {
        if lower.ends_with(ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    for ext in ARCHIVE_EXT_SINGLE {
        if lower.ends_with(ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name.to_string()
}

fn refresh_current(app: &MainWindow, rt: &Arc<Runtime>, state: &AppStateRc, watcher: &WatcherHandle) {
    let Some(cur) = state.borrow().current.clone() else {
        return;
    };
    crate::glue::navigation::navigate_internal_refresh(app, rt, state.clone(), watcher.clone(), cur);
}

/// Kick off archive creation from the captured selection with a live progress
/// dialog (collecting → compressing N/total + bytes), then refresh the folder.
/// Errors surface in the shared message dialog.
fn start_compress(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: &AppStateRc,
    watcher: &WatcherHandle,
    name: String,
    format: i32,
    password: String,
) {
    let (parent, paths) = COMPRESS_STATE.with(|c| c.borrow().clone());
    if paths.is_empty() || name.trim().is_empty() {
        return;
    }
    let names: Vec<std::ffi::OsString> = paths
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_os_string()))
        .collect();
    let pw = if password.is_empty() { None } else { Some(password) };

    let collecting = app.global::<Translations>().get_progress_op_collecting();
    let compressing = app.global::<Translations>().get_progress_op_compress();

    // Instant feedback: "Collecting files…" while we pre-scan.
    app.set_progress(blank_progress(collecting));

    // Fresh cancel state for this run; the progress dialog's Cancel trips it.
    let cancel = crate::glue::transfer_cancel();
    cancel.reset();

    let rt_spawn = rt.clone();
    let rt_refresh = rt.clone();
    let weak = app.as_weak();
    let state_c = state.clone();
    let watcher_c = watcher.clone();

    let _g = rt.enter();
    let _ = slint::spawn_local(async move {
        let weak_cb = weak.clone();
        let res = rt_spawn
            .spawn(async move {
                // Pre-scan ourselves for exact file count + total bytes.
                let srcs: Vec<PathBuf> = names.iter().map(|n| parent.join(n)).collect();
                let (files_total, bytes_total) = mykrut_core::prescan(&srcs, &cancel);
                push_progress(
                    &weak_cb,
                    compressing.clone(),
                    SharedString::new(),
                    0,
                    files_total as i32,
                    0,
                    bytes_total,
                    0.0,
                );
                create_archive(
                    &parent,
                    &name,
                    &names,
                    format,
                    pw,
                    files_total,
                    bytes_total,
                    &weak_cb,
                    compressing,
                    &cancel,
                )
                .await
            })
            .await
            .unwrap_or_else(|err| Err(format!("compress worker panicked: {err}")));

        let Some(app) = weak.upgrade() else { return };
        let mut pd = app.get_progress();
        pd.visible = false;
        app.set_progress(pd);
        match res {
            Ok(Some(out)) => {
                info!(out = %out.display(), "archive created");
                refresh_current(&app, &rt_refresh, &state_c, &watcher_c);
            }
            Ok(None) => {
                info!("compress cancelled");
                // Source folder unchanged; refresh anyway in case a partial
                // archive was created and then removed.
                refresh_current(&app, &rt_refresh, &state_c, &watcher_c);
            }
            Err(msg) => {
                error!(%msg, "compress failed");
                let ds = app.global::<DialogState>();
                ds.set_nav_error_title("Could not create archive".into());
                ds.set_nav_error_message(msg.into());
                ds.set_nav_error_open(true);
            }
        }
    });
}

fn pack_u64(v: u64) -> (i32, i32) {
    (v as i32, (v >> 32) as i32)
}

fn blank_progress(op: SharedString) -> ProgressData {
    ProgressData {
        visible: true,
        op,
        current: SharedString::new(),
        files_done: 0,
        files_total: 0,
        percent: 0.0,
        bytes_done_lo: 0,
        bytes_done_hi: 0,
        bytes_total_lo: 0,
        bytes_total_hi: 0,
        bytes_text: SharedString::new(),
        paused: false,
    }
}

/// Push a progress update to the UI thread. `bytes_text` ("X of Y") is built
/// here (on the UI thread) so we can read the localized "of".
#[expect(
    clippy::too_many_arguments,
    reason = "flat progress fields; a struct would not read clearer"
)]
fn push_progress(
    weak: &slint::Weak<MainWindow>,
    op: SharedString,
    current: SharedString,
    files_done: i32,
    files_total: i32,
    bytes_done: u64,
    bytes_total: u64,
    fraction: f32,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(app) = weak.upgrade() else { return };
        let bytes_text = if bytes_total > 0 {
            let of = app.global::<Translations>().get_progress_files_of();
            format!("{} {} {}", human_size(bytes_done), of, human_size(bytes_total)).into()
        } else {
            SharedString::new()
        };
        let (bd_lo, bd_hi) = pack_u64(bytes_done);
        let (bt_lo, bt_hi) = pack_u64(bytes_total);
        app.set_progress(ProgressData {
            visible: true,
            op,
            current,
            files_done,
            files_total,
            percent: fraction,
            bytes_done_lo: bd_lo,
            bytes_done_hi: bd_hi,
            bytes_total_lo: bt_lo,
            bytes_total_hi: bt_hi,
            bytes_text,
            paused: false,
        });
    });
}

/// Pull a `NN%` value (0..100) out of a 7z progress line, if present.
fn parse_percent(line: &str) -> Option<f32> {
    let bytes = line.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'%' {
            let mut j = i;
            while j > 0 && bytes[j - 1].is_ascii_digit() {
                j -= 1;
            }
            if j < i {
                return line.get(j..i).and_then(|s| s.parse::<f32>().ok());
            }
        }
    }
    None
}

/// Detect a 7z "file added" log line (with `-bb1`) and return the file name.
/// Covers both modern 7-Zip ("+ name") and older p7zip ("Compressing  name").
fn detect_7z_file(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("+ ") {
        return Some(rest.to_string());
    }
    line.strip_prefix("Compressing")
        .map(|rest| rest.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Build an archive named `base.<ext>` in `parent` from `names` (relative to
/// `parent`), streaming progress to the UI. format: 0=zip, 1=7z, 2=tar.gz.
/// zip/7z honour `password`; tar.gz can't be encrypted. Uses `7z` for zip/7z
/// (progress on stdout) and `tar` for tar.gz (file list on stderr).
#[expect(clippy::too_many_arguments, reason = "progress totals threaded through for the UI")]
async fn create_archive(
    parent: &Path,
    base: &str,
    names: &[std::ffi::OsString],
    format: i32,
    password: Option<String>,
    files_total: u64,
    bytes_total: u64,
    weak: &slint::Weak<MainWindow>,
    op: SharedString,
    cancel: &mykrut_core::CancelFlag,
) -> Result<Option<PathBuf>, String> {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    let ext = match format {
        1 => "7z",
        2 => "tar.gz",
        _ => "zip",
    };
    let out = mykrut_core::unique_destination(parent, &format!("{}.{}", base.trim(), ext));

    let mut cmd = if format == 2 {
        let mut c = Command::new("tar");
        // -v lists each file (to stderr) so we can count progress.
        c.current_dir(parent).arg("-czvf").arg(&out);
        for n in names {
            c.arg(n);
        }
        c
    } else {
        let mut c = Command::new("7z");
        c.current_dir(parent).arg("a");
        c.arg(if format == 1 { "-t7z" } else { "-tzip" });
        if let Some(pw) = &password {
            c.arg(format!("-p{pw}"));
            // 7z: encrypt headers too; zip: use AES instead of weak ZipCrypto.
            c.arg(if format == 1 { "-mhe=on" } else { "-mem=AES256" });
        }
        // -bsp1: progress % to stdout; -bb1: log each processed file name.
        c.arg("-y").arg("-bsp1").arg("-bb1").arg(&out);
        for n in names {
            c.arg(n);
        }
        c
    };

    let tool = if format == 2 { "tar" } else { "7z" };
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                format!("`{tool}` is not installed")
            } else {
                format!("spawn {tool}: {err}")
            }
        })?;

    // tar writes its file list to stderr; 7z writes progress to stdout.
    let mut progress_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if format == 2 {
        Box::new(child.stderr.take().expect("stderr piped"))
    } else {
        Box::new(child.stdout.take().expect("stdout piped"))
    };

    // Helper: kill the child, drop the partial archive, and report cancellation.
    async fn cancelled(mut child: tokio::process::Child, out: &Path) -> Result<Option<PathBuf>, String> {
        let _ = child.kill().await;
        let _ = tokio::fs::remove_file(out).await;
        Ok(None)
    }

    let mut acc = String::new();
    let mut buf = [0u8; 4096];
    let mut files_done: u64 = 0;
    let mut fraction: f32 = 0.0;
    loop {
        if cancel.is_cancelled() {
            return cancelled(child, &out).await;
        }
        // Time-box the read so we still poll `cancel` even when the child is
        // busy compressing a large file and emitting no output (~5×/sec).
        let n = match tokio::time::timeout(Duration::from_millis(200), progress_stream.read(&mut buf)).await {
            Err(_elapsed) => continue, // timed out → loop back, re-check cancel
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(n)) => n,
        };
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
        // 7z emits progress with carriage returns, file lists with newlines.
        while let Some(pos) = acc.find(['\r', '\n']) {
            let token: String = acc.drain(..=pos).collect();
            let t = token.trim();
            if t.is_empty() {
                continue;
            }
            let mut current: Option<String> = None;
            let mut advanced = false;
            match format {
                2 => {
                    files_done += 1;
                    current = Some(t.to_string());
                    advanced = true;
                }
                _ => {
                    if let Some(name) = detect_7z_file(t) {
                        files_done += 1;
                        current = Some(name);
                        advanced = true;
                    }
                    if let Some(p) = parse_percent(t) {
                        fraction = (p / 100.0).clamp(0.0, 1.0);
                        advanced = true;
                    }
                }
            }
            if !advanced {
                continue;
            }
            // For tar (no %) derive the fraction from the file count.
            if format == 2 && files_total > 0 {
                fraction = (files_done as f32 / files_total as f32).min(1.0);
            }
            let bytes_done = (f64::from(fraction) * bytes_total as f64) as u64;
            let fd = if files_done > 0 {
                files_done.min(files_total) as i32
            } else {
                (fraction * files_total as f32) as i32
            };
            push_progress(
                weak,
                op.clone(),
                current.map(SharedString::from).unwrap_or_default(),
                fd,
                files_total as i32,
                bytes_done,
                bytes_total,
                fraction,
            );
        }
    }

    // Stream ended (EOF). If the user cancelled meanwhile, stop here.
    if cancel.is_cancelled() {
        return cancelled(child, &out).await;
    }
    let status = child.wait().await.map_err(|e| format!("wait {tool}: {e}"))?;
    if status.success() {
        return Ok(Some(out));
    }
    if cancel.is_cancelled() {
        let _ = tokio::fs::remove_file(&out).await;
        return Ok(None);
    }
    // On failure, read whatever's left on the OTHER stream for the message.
    let mut errbuf = String::new();
    if format == 2 {
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_string(&mut errbuf).await;
        }
    } else if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut errbuf).await;
    }
    Err(if errbuf.trim().is_empty() {
        format!("{tool} exited with {status}")
    } else {
        errbuf
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn detects_common_archives() {
        for n in [
            "foo.zip",
            "foo.7z",
            "FOO.7Z",
            "x.tar.gz",
            "x.TAR.GZ",
            "x.tgz",
            "deep/path/x.tar.bz2",
            "x.zst",
        ] {
            assert!(is_archive_path(&PathBuf::from(n)), "should be archive: {n}");
        }
        for n in ["foo.txt", "x.png", "noext", "x.tar.png"] {
            assert!(!is_archive_path(&PathBuf::from(n)), "should not: {n}");
        }
    }

    #[test]
    fn strips_double_then_single_extensions() {
        assert_eq!(strip_archive_extensions("foo.tar.gz"), "foo");
        assert_eq!(strip_archive_extensions("Foo.TAR.GZ"), "Foo");
        assert_eq!(strip_archive_extensions("foo.tgz"), "foo");
        assert_eq!(strip_archive_extensions("foo.7z"), "foo");
        assert_eq!(strip_archive_extensions("nothing"), "nothing");
    }
}
