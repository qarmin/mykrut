//! "Open with…" picker: list installed apps for the selected file's MIME type,
//! launch the chosen one, and optionally make it the default.
//!
//! The heavy lifting (enumeration, mimeapps.list parsing/writing, launching)
//! lives in `default_app`; this module just bridges it to the dialog. The
//! pending paths/apps are held in a thread-local because everything here runs
//! on the UI thread between the dialog opening and the user choosing.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, VecModel};
use tracing::{error, info, warn};

use crate::glue::default_app::{self, DesktopApp};
use crate::state::AppStateRc;
use crate::{Callabler, DialogState, MainWindow, OpenWithApp};

thread_local! {
    static PENDING: RefCell<Pending> = RefCell::new(Pending::default());
}

#[derive(Default)]
struct Pending {
    paths: Vec<PathBuf>,
    mime: Option<String>,
    apps: Vec<DesktopApp>,
}

pub fn wire(app: &MainWindow, state: AppStateRc) {
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.global::<Callabler>().on_request_open_with(move || {
            let app = weak.upgrade().expect("MainWindow alive in request-open-with");
            request(&app, &state);
        });
    }
    {
        let weak = app.as_weak();
        // Consume the owned `state` here (the other closures clone it) so it's
        // not needlessly cloned.
        app.global::<Callabler>().on_open_with_chosen(move |id, set_default| {
            let app = weak.upgrade().expect("MainWindow alive in open-with-chosen");
            chosen(&app, &state, id.as_str(), set_default);
        });
    }
    {
        let weak = app.as_weak();
        app.global::<Callabler>().on_open_with_command(move |cmd, remember| {
            let _app = weak.upgrade().expect("MainWindow alive in open-with-command");
            run_command(cmd.as_str(), remember);
        });
    }
}

/// A user-defined "open with" entry typed into the dialog and remembered.
#[derive(Serialize, Deserialize, Clone)]
struct CustomApp {
    name: String,
    exec: String,
}

#[derive(Serialize, Deserialize, Default)]
struct CustomStore {
    apps: Vec<CustomApp>,
}

fn custom_store_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("mykrut").join("data").join("custom_apps.toml"))
}

fn load_custom() -> Vec<CustomApp> {
    let Some(path) = custom_store_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    toml::from_str::<CustomStore>(&text).map(|s| s.apps).unwrap_or_default()
}

fn add_custom(new: CustomApp) {
    let Some(path) = custom_store_path() else { return };
    let mut apps = load_custom();
    // De-dup by exec so re-running the same command doesn't pile up.
    if apps.iter().any(|a| a.exec == new.exec) {
        return;
    }
    apps.push(new);
    let store = CustomStore { apps };
    let Ok(text) = toml::to_string_pretty(&store) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(&path, text) {
        warn!(?err, "could not save custom apps");
    }
}

/// Turn a typed command into a launchable Exec line: if it has no field code,
/// append `%U` so the selected paths are passed as arguments.
fn normalize_exec(cmd: &str) -> String {
    let cmd = cmd.trim();
    if cmd.contains('%') {
        cmd.to_string()
    } else {
        format!("{cmd} %U")
    }
}

/// Run a typed command on the pending paths, optionally remembering it.
fn run_command(cmd: &str, remember: bool) {
    let cmd = cmd.trim();
    let paths = PENDING.with(|p| p.borrow().paths.clone());
    if cmd.is_empty() || paths.is_empty() {
        return;
    }
    let exec = normalize_exec(cmd);
    info!(cmd, remember, "open-with custom command");
    if let Err(err) = default_app::launch(&exec, &paths) {
        error!(?err, cmd, "custom command launch failed");
        return;
    }
    if remember {
        add_custom(CustomApp {
            name: cmd.to_string(),
            exec,
        });
    }
}

/// Build the app list for the selected file(s) and open the dialog. Folders are
/// skipped; the MIME type comes from the first selected file.
fn request(app: &MainWindow, state: &AppStateRc) {
    let paths: Vec<PathBuf> = {
        let s = state.borrow();
        let mut idxs: Vec<usize> = s.selected.iter().copied().collect();
        idxs.sort_unstable();
        idxs.into_iter()
            .filter_map(|i| s.entries.get(i))
            .filter(|e| !e.is_dir())
            .map(|e| e.path.clone())
            .collect()
    };
    if paths.is_empty() {
        return;
    }

    let mime = default_app::mime_for(&paths[0]);
    let mut apps = default_app::apps_for_mime(mime.as_deref());
    // Surface remembered custom commands at the top (recommended), keyed by a
    // synthetic id so `chosen` can launch their stored Exec line.
    for c in load_custom() {
        apps.insert(
            0,
            DesktopApp {
                id: format!("custom:{}", c.exec),
                name: c.name,
                exec: c.exec,
                recommended: true,
            },
        );
    }
    let default_id = mime.as_deref().and_then(default_app::default_desktop_id);

    let rows: Vec<OpenWithApp> = apps
        .iter()
        .map(|a| OpenWithApp {
            name: a.name.clone().into(),
            id: a.id.clone().into(),
            recommended: a.recommended,
            is_default: default_id.as_deref() == Some(a.id.as_str()),
        })
        .collect();

    app.set_open_with_apps(ModelRc::from(Rc::new(VecModel::from(rows))));
    let ds = app.global::<DialogState>();
    ds.set_open_with_subtitle(mime.clone().unwrap_or_default().into());
    ds.set_open_with_open(true);

    PENDING.with(|p| *p.borrow_mut() = Pending { paths, mime, apps });
}

/// Launch the chosen app on the pending paths; optionally persist it as the
/// default for the file's MIME type.
fn chosen(app: &MainWindow, state: &AppStateRc, id: &str, set_default: bool) {
    let (paths, mime, exec) = PENDING.with(|p| {
        let pend = p.borrow();
        let exec = pend.apps.iter().find(|a| a.id == id).map(|a| a.exec.clone());
        (pend.paths.clone(), pend.mime.clone(), exec)
    });
    let Some(exec) = exec else {
        return;
    };

    info!(id, set_default, count = paths.len(), "open with");
    if let Err(err) = default_app::launch(&exec, &paths) {
        error!(?err, id, "open-with launch failed");
    }

    // Custom commands aren't real .desktop entries, so they can't be a MIME
    // default; only persist a default for installed apps.
    if set_default
        && !id.starts_with("custom:")
        && let Some(mime) = mime.as_deref()
    {
        match default_app::set_default(mime, id) {
            Ok(()) => {
                default_app::invalidate_cache();
                // Refresh the context-menu "Open" label for the new default.
                default_app::refresh(app, state);
            }
            Err(err) => error!(?err, mime, id, "set default app failed"),
        }
    }

    PENDING.with(|p| *p.borrow_mut() = Pending::default());
}
