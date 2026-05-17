//! "Open in terminal" + "Open as root" wiring.
//!
//! Terminal lookup walks a small ordered list (`$TERMINAL` first, then
//! `x-terminal-emulator`, then common emulators). We let the child inherit
//! our environment + set `current_dir(target)` so we don't have to guess
//! each emulator's "open in this folder" flag (which varies wildly).
//!
//! Root re-launch uses `pkexec` — the standard polkit GUI prompt. pkexec
//! sanitises the environment by default so we explicitly pass through
//! DISPLAY / WAYLAND_DISPLAY / XAUTHORITY (otherwise the new process can't
//! talk to the display server). The new process is given the current
//! folder as its first positional argument, matching how `fm <path>` works
//! from a shell.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use mykrut_core::Location;
use slint::ComponentHandle;
use tokio::runtime::Runtime;
use tracing::{error, info, info_span, warn};

use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{Callabler, MainWindow, SearchState};

/// Terminal candidates tried in order. First one found on $PATH wins.
const TERMINAL_CANDIDATES: &[&str] = &[
    "x-terminal-emulator", // Debian alternative
    "gnome-terminal",
    "konsole",
    "alacritty",
    "kitty",
    "wezterm",
    "tilix",
    "xfce4-terminal",
    "lxterminal",
    "terminator",
    "foot",
    "xterm",
];

pub fn wire(app: &MainWindow, _rt: &Arc<Runtime>, state: AppStateRc, _watcher: WatcherHandle) {
    wire_open_in_terminal(app, state.clone());
    wire_open_as_root(app, state);
}

fn wire_open_in_terminal(app: &MainWindow, state: AppStateRc) {
    let weak = app.as_weak();
    app.global::<Callabler>().on_open_in_terminal(move |arg| {
        let app = weak.upgrade().expect("MainWindow alive in open-in-terminal");
        let target = resolve_target(&app, &state, arg.as_str());
        let Some(target) = target else {
            warn!("open-in-terminal: no target path");
            return;
        };
        spawn_terminal_in(&target);
    });
}

fn wire_open_as_root(app: &MainWindow, state: AppStateRc) {
    let weak = app.as_weak();
    app.global::<Callabler>().on_open_as_root(move |arg| {
        let app = weak.upgrade().expect("MainWindow alive in open-as-root");
        let target = resolve_target(&app, &state, arg.as_str());
        let Some(target) = target else {
            warn!("open-as-root: no target path");
            return;
        };
        spawn_self_as_root(&target);
    });
}

/// Empty arg → use the current folder; otherwise use the explicit path. If
/// a single folder is selected (and search isn't masking the listing), the
/// callers prefer to pass that explicitly; the menu currently leaves the
/// arg empty in both cases and we resolve here for simplicity.
fn resolve_target(app: &MainWindow, state: &AppStateRc, arg: &str) -> Option<PathBuf> {
    if !arg.is_empty() {
        return Some(PathBuf::from(arg));
    }
    // Search mode: if exactly one folder is highlighted, prefer it.
    if app.global::<SearchState>().get_active() {
        // selection lives on the search-rows model; we don't bother wiring
        // it through here because the empty-space menu doesn't fire during
        // search, and the item-row menu uses the empty-arg fallback too.
    }
    // Single selected folder → use it.
    let single_dir = {
        let s = state.borrow();
        if s.selected.len() == 1 {
            s.selected
                .iter()
                .next()
                .and_then(|&i| s.entries.get(i))
                .filter(|e| e.is_dir())
                .map(|e| e.path.clone())
        } else {
            None
        }
    };
    if let Some(p) = single_dir {
        return Some(p);
    }
    // Fall back to the current directory.
    match state.borrow().current.clone()? {
        Location::Local(p) => Some(p),
        Location::Trash => None,
    }
}

fn spawn_terminal_in(dir: &Path) {
    let _span = info_span!("open_in_terminal", dir = %dir.display()).entered();
    if let Ok(custom) = std::env::var("TERMINAL")
        && !custom.trim().is_empty()
        && try_spawn(&custom, dir)
    {
        return;
    }
    for term in TERMINAL_CANDIDATES {
        if try_spawn(term, dir) {
            return;
        }
    }
    error!("no terminal emulator found on $PATH — install one of: gnome-terminal, konsole, xterm, …");
}

/// Try to spawn `term` with cwd=`dir`. Returns true if spawn succeeded.
/// We deliberately don't pass any emulator-specific "open in this folder"
/// flag — every emulator handles cwd inheritance, the flags are not
/// uniform.
fn try_spawn(term: &str, dir: &Path) -> bool {
    match Command::new(term)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_child) => {
            info!(term, "terminal spawned");
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            warn!(?err, term, "terminal spawn failed");
            false
        }
    }
}

fn spawn_self_as_root(dir: &Path) {
    let _span = info_span!("open_as_root", dir = %dir.display()).entered();
    let Ok(exe) = std::env::current_exe() else {
        error!("can't resolve current_exe — can't relaunch as root");
        return;
    };
    // When the binary on disk was replaced while running (every `cargo run`
    // rebuild), the kernel reports the exe path with a literal " (deleted)"
    // suffix — `current_exe()` returns it verbatim, so pkexec would try to
    // exec a path that doesn't exist and silently fail. Strip it.
    let exe = sanitize_exe(exe);
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let xauth = std::env::var("XAUTHORITY").unwrap_or_default();
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();

    // pkexec strips most env vars by default; pass the bits the GUI
    // toolkit needs through `env`. The resulting argv is essentially:
    //   pkexec env DISPLAY=… WAYLAND_DISPLAY=… XAUTHORITY=… XDG_RUNTIME_DIR=… /path/to/fm /target
    let result = Command::new("pkexec")
        .arg("env")
        .arg(format!("DISPLAY={display}"))
        .arg(format!("WAYLAND_DISPLAY={wayland}"))
        .arg(format!("XAUTHORITY={xauth}"))
        .arg(format!("XDG_RUNTIME_DIR={xdg_runtime}"))
        .arg(&exe)
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Inherit stderr so pkexec/polkit auth failures and the child's startup
        // errors are visible in the launching terminal instead of vanishing.
        .stderr(Stdio::inherit())
        .spawn();
    match result {
        Ok(_) => info!(exe = %exe.display(), "spawned root instance via pkexec"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            error!("pkexec not found — install polkit to use \"Open as root\"");
        }
        Err(err) => error!(?err, "pkexec spawn failed"),
    }
}

/// Open a file the way a double-click (or "Open") should: if it's something the
/// user can run - an ELF binary or a shebang script carrying the executable bit
/// - launch it detached with its own folder as the working directory; otherwise
/// hand it to the desktop's default handler via `opener` (xdg-open et al.).
///
/// This matches Nemo/Files: a marked-executable program runs on activation
/// rather than being opened in a text editor or silently doing nothing.
pub fn open_file(path: &Path) {
    let _span = info_span!("open_file", path = %path.display()).entered();
    if is_runnable_executable(path) {
        let cwd = path.parent().unwrap_or_else(|| Path::new("/"));
        match Command::new(path)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {
                info!("launched executable");
                return;
            }
            // Couldn't exec (e.g. missing interpreter) — fall through to the
            // default handler rather than leaving the click dead.
            Err(err) => warn!(?err, "exec spawn failed; falling back to opener"),
        }
    }
    match opener::open(path) {
        Ok(()) => info!("opened"),
        Err(err) => error!(?err, "open failed"),
    }
}

/// True only for files we should actually *run* on activation: a regular file
/// with the executable bit that begins with the ELF magic or a `#!` shebang.
/// The magic check guards against data files that merely carry the exec bit
/// (common on FAT/NTFS mounts, where everything is +x).
#[cfg(unix)]
fn is_runnable_executable(path: &Path) -> bool {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    let n = f.read(&mut buf).unwrap_or(0);
    let head = &buf[..n];
    head.starts_with(b"\x7fELF") || head.starts_with(b"#!")
}

#[cfg(not(unix))]
fn is_runnable_executable(_path: &Path) -> bool {
    false
}

/// Strip a trailing " (deleted)" that Linux appends to `/proc/self/exe` (and
/// thus `current_exe()`) once the running binary's file has been replaced.
fn sanitize_exe(exe: PathBuf) -> PathBuf {
    let s = exe.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(real) => PathBuf::from(real),
        None => exe,
    }
}
