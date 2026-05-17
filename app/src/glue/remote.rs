//! Remote browsing (ssh/sftp/smb/ftp/dav) via GVFS.
//!
//! We don't implement SFTP/SMB ourselves. Instead we mount the URI with the
//! `gio` CLI (shipped with glib2/gvfs on every GTK-based desktop) and then
//! browse the share through its FUSE mountpoint under
//! `/run/user/<uid>/gvfs/...`. That mountpoint is a normal local directory, so
//! every existing listing / sorting / file-operation path works unchanged — we
//! just translate the `ssh://`/`smb://` URI into the local FUSE path and hand
//! it to the regular `Location::Local` navigation.
//!
//! Auth: if the share isn't already mounted we first try an **anonymous** mount
//! (`gio mount -a`), which is what guest SMB / public shares want — no password
//! prompt at all. Only if the server rejects anonymous access do we pop the
//! password dialog and feed the password to `gio mount` over stdin. Key-only
//! SSH auth can leave the field blank. Interactive host-key confirmation on a
//! brand-new SSH host is not handled here — connect once via your desktop/
//! terminal so the key is trusted, then it works from here.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use mykrut_core::Location;
use slint::ComponentHandle;
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

use crate::glue::watcher::WatcherHandle;
use crate::state::AppStateRc;
use crate::{Callabler, DialogState, MainWindow};

/// URI schemes we route through GVFS rather than treating as a local path.
const REMOTE_SCHEMES: &[&str] = &[
    "ssh://", "sftp://", "smb://", "ftp://", "ftps://", "dav://", "davs://", "nfs://",
];

pub fn is_remote_uri(s: &str) -> bool {
    let s = s.trim();
    REMOTE_SCHEMES.iter().any(|p| s.starts_with(p))
}

thread_local! {
    /// URI awaiting a password (between opening the dialog and the user
    /// confirming). Single in-flight mount at a time is plenty for a file
    /// manager's address bar.
    static PENDING_URI: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Entry point from the address bar for a remote URI.
pub fn open(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle, uri: &str) {
    let uri = uri.trim().to_string();
    // Already mounted (by us earlier, or by the desktop) → jump straight in.
    if let Some(local) = resolve_mounted(&uri) {
        info!(%uri, local = %local.display(), "remote already mounted");
        crate::glue::navigation::navigate_to(app, rt, state, watcher, Location::Local(local));
        return;
    }
    // Not mounted yet: try anonymously first. Guest/public SMB shares mount
    // with no credentials; we only prompt if the server rejects anonymous.
    PENDING_URI.with(|p| *p.borrow_mut() = Some(uri.clone()));
    info!(%uri, "remote: trying anonymous mount");
    try_mount(app, rt, state, watcher, uri, None);
}

fn open_password_dialog(app: &MainWindow, uri: &str, retry: bool) {
    let ds = app.global::<DialogState>();
    ds.set_remote_host(display_authority(uri).into());
    ds.set_remote_error(retry);
    ds.set_remote_mount_open(true);
}

pub fn wire(app: &MainWindow, rt: &Arc<Runtime>, state: AppStateRc, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let rt = rt.clone();
        let state = state;
        let watcher = watcher;
        app.global::<Callabler>().on_remote_mount_confirmed(move |pw| {
            let app = weak.upgrade().expect("MainWindow alive in remote-mount-confirmed");
            let Some(uri) = PENDING_URI.with(|p| p.borrow().clone()) else {
                return;
            };
            try_mount(&app, &rt, state.clone(), watcher.clone(), uri, Some(pw.to_string()));
        });
    }
    {
        app.global::<Callabler>().on_remote_mount_cancelled(move || {
            PENDING_URI.with(|p| *p.borrow_mut() = None);
            info!("remote mount cancelled");
        });
    }
}

/// Mount `uri` with `gio mount`, then navigate into the resulting FUSE
/// mountpoint. `password = None` → anonymous (`gio mount -a`); `Some(pw)` →
/// feed the password over stdin. On an anonymous failure we open the password
/// dialog (no error shown); on a credentialed failure we re-open it with the
/// error so the user can retry.
fn try_mount(
    app: &MainWindow,
    rt: &Arc<Runtime>,
    state: AppStateRc,
    watcher: WatcherHandle,
    uri: String,
    password: Option<String>,
) {
    let was_anonymous = password.is_none();
    let weak = app.as_weak();
    let rt_outer = rt.clone();
    let uri_for_task = uri.clone();
    let _g = rt.enter();
    let _ = slint::spawn_local(async move {
        let res = rt_outer
            .spawn(async move { gio_mount(&uri_for_task, password).await })
            .await;
        let Some(app) = weak.upgrade() else { return };
        match res {
            Ok(Ok(())) => match resolve_mounted(&uri) {
                Some(local) => {
                    PENDING_URI.with(|p| *p.borrow_mut() = None);
                    info!(%uri, local = %local.display(), "remote mounted");
                    crate::glue::navigation::navigate_to(&app, &rt_outer, state, watcher, Location::Local(local));
                }
                None => {
                    warn!(%uri, "mounted but FUSE mountpoint not found");
                    open_password_dialog(&app, &uri, false);
                }
            },
            Ok(Err(msg)) => {
                if was_anonymous {
                    // Anonymous rejected → ask for credentials (first prompt,
                    // no error banner).
                    info!(%uri, %msg, "anonymous mount rejected — prompting for password");
                    open_password_dialog(&app, &uri, false);
                } else {
                    warn!(%uri, %msg, "gio mount failed");
                    open_password_dialog(&app, &uri, true);
                }
            }
            Err(join_err) => {
                error!(?join_err, "remote mount task panicked");
                PENDING_URI.with(|p| *p.borrow_mut() = None);
            }
        }
    });
}

async fn gio_mount(uri: &str, password: Option<String>) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut cmd = Command::new("gio");
    cmd.arg("mount");
    if password.is_none() {
        // Anonymous user — no prompts, so don't feed stdin.
        cmd.arg("-a").stdin(Stdio::null());
    } else {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .arg(uri)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "`gio` not found — install glib2/gvfs to browse network shares".to_string()
            } else {
                format!("spawn gio: {e}")
            }
        })?;

    // `gio mount` prompts for each required field on stdin in order. We put the
    // user in the URI (user@host) so only the password is typically asked; send
    // the password (plus a trailing newline to satisfy any final prompt).
    if let Some(pw) = password
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(format!("{pw}\n\n").as_bytes()).await;
        // Drop closes stdin → EOF, so gio stops waiting for more input.
    }

    let out = child.wait_with_output().await.map_err(|e| format!("gio mount: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(stderr.trim().to_string())
    }
}

/// Local GVFS FUSE root for this user, if present.
fn gvfs_root() -> Option<PathBuf> {
    // SAFETY: `libc::getuid()` is always safe — it takes no arguments, reads
    // the calling process's real UID, and cannot fail.
    let uid = unsafe { libc::getuid() };
    let p = PathBuf::from(format!("/run/user/{uid}/gvfs"));
    p.is_dir().then_some(p)
}

/// If `uri` is already mounted, return the local FUSE path to its target
/// (including any sub-path component of the URI).
fn resolve_mounted(uri: &str) -> Option<PathBuf> {
    let parsed = parse_uri(uri)?;
    let root = gvfs_root()?;

    // gvfs mount directory names look like:
    //   sftp:host=192.168.0.1,user=rafal
    //   smb-share:server=nas,share=public
    //   dav:host=...   ftp:host=...
    let host_key = match parsed.scheme.as_str() {
        "smb" => format!("server={}", parsed.host.to_lowercase()),
        _ => format!("host={}", parsed.host.to_lowercase()),
    };

    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if !name.contains(&host_key) {
            continue;
        }
        if let Some(share) = &parsed.share
            && !name.contains(&format!("share={}", share.to_lowercase()))
        {
            continue;
        }
        let mut path = entry.path();
        if let Some(sub) = &parsed.subpath
            && !sub.is_empty()
        {
            path = path.join(sub);
        }
        return Some(path);
    }
    None
}

struct ParsedUri {
    scheme: String,
    host: String,
    /// SMB share name (the first path segment); `None` for sftp/ftp/dav.
    share: Option<String>,
    /// Path beneath the mount root, if any (no leading slash).
    subpath: Option<String>,
}

fn parse_uri(uri: &str) -> Option<ParsedUri> {
    let (scheme, rest) = uri.trim().split_once("://")?;
    let scheme = scheme.to_lowercase();
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    // Strip any user[:pass]@ prefix; host is after the last '@'.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port).to_string();
    if host.is_empty() {
        return None;
    }

    let (share, subpath) = if scheme == "smb" {
        let mut segs = path.splitn(2, '/');
        let share = segs.next().filter(|s| !s.is_empty()).map(str::to_string);
        let sub = segs.next().filter(|s| !s.is_empty()).map(str::to_string);
        (share, sub)
    } else {
        let sub = (!path.is_empty()).then(|| path.to_string());
        (None, sub)
    };

    Some(ParsedUri {
        scheme,
        host,
        share,
        subpath,
    })
}

/// Human-friendly "host" (or "share on host") for the password dialog.
fn display_authority(uri: &str) -> String {
    match parse_uri(uri) {
        Some(p) => match &p.share {
            Some(s) => format!("{} on {}", s, p.host),
            None => p.host,
        },
        None => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_remote_schemes() {
        assert!(is_remote_uri("ssh://192.168.0.1"));
        assert!(is_remote_uri("smb://nas/share"));
        assert!(is_remote_uri("sftp://user@host/dir"));
        assert!(!is_remote_uri("/home/user"));
        assert!(!is_remote_uri("trash:///"));
    }

    #[test]
    fn parses_sftp() {
        let p = parse_uri("ssh://rafal@192.168.0.5/home/rafal/docs").unwrap();
        assert_eq!(p.scheme, "ssh");
        assert_eq!(p.host, "192.168.0.5");
        assert!(p.share.is_none());
        assert_eq!(p.subpath.as_deref(), Some("home/rafal/docs"));
    }

    #[test]
    fn parses_smb_with_share() {
        let p = parse_uri("smb://nas/public/movies").unwrap();
        assert_eq!(p.scheme, "smb");
        assert_eq!(p.host, "nas");
        assert_eq!(p.share.as_deref(), Some("public"));
        assert_eq!(p.subpath.as_deref(), Some("movies"));
    }

    #[test]
    fn parses_host_only() {
        let p = parse_uri("smb://server").unwrap();
        assert_eq!(p.host, "server");
        assert!(p.share.is_none());
        assert!(p.subpath.is_none());
    }
}
