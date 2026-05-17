use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::event::EventKind;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use slint::{ComponentHandle, Weak};
use tokio::sync::Mutex;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::state::AppStateRc;
use crate::{Callabler, MainWindow, Settings};

const DEBOUNCE: Duration = Duration::from_millis(300);

/// Manages a single non-recursive notify::Watcher tied to the current directory.
///
/// Watcher is OFF by default (see `Settings.auto-refresh`). When enabled, only
/// the current directory is watched — not its subtree — so attaching to a
/// folder with millions of files is O(1), not O(N).
///
/// All events are coalesced through a debounce window before triggering one
/// refresh. Rapid bursts (e.g. compiler producing many .o files) become a
/// single redraw.
#[derive(Clone)]
pub struct WatcherHandle {
    inner: Arc<Mutex<WatcherInner>>,
    /// Captured so notify-thread callbacks (which fire outside a tokio TLS
    /// context) can still schedule the debounce sleep via `rt.spawn`.
    rt: Arc<tokio::runtime::Runtime>,
}

struct WatcherInner {
    watcher: Option<RecommendedWatcher>,
    watched_path: Option<PathBuf>,
    enabled: bool,
    dirty_tick: u64,
}

impl WatcherHandle {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WatcherInner {
                watcher: None,
                watched_path: None,
                enabled: false,
                dirty_tick: 0,
            })),
            rt,
        }
    }

    /// Switch watched directory. No-op if the watcher is disabled.
    pub async fn set_path(&self, weak: Weak<MainWindow>, path: PathBuf) {
        let span = info_span!("watcher.set_path", path = %path.display());
        async move {
            let mut inner = self.inner.lock().await;
            if !inner.enabled {
                debug!("disabled; skipping watch attachment");
                inner.watched_path = Some(path);
                return;
            }

            attach_watcher(&mut inner, self.inner.clone(), self.rt.clone(), weak, &path);
        }
        .instrument(span)
        .await;
    }

    /// Toggle the watcher on/off. When turning on, attach to the last known path.
    pub async fn set_enabled(&self, enabled: bool, weak: Weak<MainWindow>) {
        let mut inner = self.inner.lock().await;
        if inner.enabled == enabled {
            return;
        }
        inner.enabled = enabled;
        if enabled {
            info!("watcher enabled");
            if let Some(p) = inner.watched_path.clone() {
                attach_watcher(&mut inner, self.inner.clone(), self.rt.clone(), weak, &p);
            }
        } else {
            info!("watcher disabled");
            inner.watcher = None;
        }
    }
}

fn attach_watcher(
    inner: &mut WatcherInner,
    arc: Arc<Mutex<WatcherInner>>,
    rt: Arc<tokio::runtime::Runtime>,
    weak: Weak<MainWindow>,
    path: &Path,
) {
    inner.watcher = None;
    inner.watched_path = Some(path.to_path_buf());

    let config = Config::default().with_poll_interval(Duration::from_secs(5));
    let weak_clone = weak;
    let arc_for_cb = arc;
    let rt_for_cb = rt;

    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| match res {
            Ok(event) => handle_event(&event, &arc_for_cb, &rt_for_cb, &weak_clone),
            Err(err) => warn!(?err, "watcher error event"),
        },
        config,
    ) {
        Ok(w) => w,
        Err(err) => {
            error!(?err, "notify::Watcher::new failed");
            return;
        }
    };

    if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
        error!(?err, path = %path.display(), "watcher.watch failed");
        return;
    }

    inner.watcher = Some(watcher);
    info!(path = %path.display(), "watching");
}

fn handle_event(
    event: &notify::Event,
    arc: &Arc<Mutex<WatcherInner>>,
    rt: &Arc<tokio::runtime::Runtime>,
    weak: &Weak<MainWindow>,
) {
    let interesting = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) | EventKind::Any
    );
    if !interesting {
        return;
    }

    let arc = arc.clone();
    let weak = weak.clone();

    // `rt.spawn` avoids needing a tokio TLS context on the notify thread.
    rt.spawn(async move {
        let my_tick = {
            let mut inner = arc.lock().await;
            inner.dirty_tick = inner.dirty_tick.wrapping_add(1);
            inner.dirty_tick
        };
        tokio::time::sleep(DEBOUNCE).await;
        let current_tick = arc.lock().await.dirty_tick;
        if current_tick != my_tick {
            // Newer event arrived during the sleep — that newer scheduler will fire.
            return;
        }
        debug!("debounced refresh fire");
        let _ = weak.upgrade_in_event_loop(|app| {
            app.global::<Callabler>().invoke_refresh();
        });
    });
}

pub fn wire(app: &MainWindow, rt: &Arc<tokio::runtime::Runtime>, watcher: WatcherHandle) {
    {
        let weak = app.as_weak();
        let w = watcher.clone();
        let rt = rt.clone();
        app.global::<Callabler>().on_toggle_auto_refresh(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-auto-refresh");
            let cur = app.global::<Settings>().get_auto_refresh();
            let new_val = !cur;
            app.global::<Settings>().set_auto_refresh(new_val);
            info!(auto_refresh = new_val, "toggled");

            let w = w.clone();
            let weak2 = app.as_weak();
            // `rt.spawn` works without an active tokio TLS context because the
            // handle carries its own reference to the runtime — unlike the
            // global `tokio::task::spawn` which requires `runtime.enter()`.
            rt.spawn(async move {
                w.set_enabled(new_val, weak2).await;
            });
        });
    }

    // Settings popup flips Settings.auto-refresh directly; we react here to
    // attach/detach the watcher accordingly.
    {
        let weak = app.as_weak();
        let w = watcher;
        let rt = rt.clone();
        app.global::<Callabler>().on_toggle_watcher_noop(move || {
            let app = weak.upgrade().expect("MainWindow alive in toggle-watcher-noop");
            let new_val = app.global::<Settings>().get_auto_refresh();
            info!(auto_refresh = new_val, "settings-popup toggled");
            let w = w.clone();
            let weak2 = app.as_weak();
            rt.spawn(async move {
                w.set_enabled(new_val, weak2).await;
            });
        });
    }
}

/// Called from navigation after a successful directory load.
pub fn on_navigated(
    rt: &tokio::runtime::Runtime,
    watcher: &WatcherHandle,
    weak: Weak<MainWindow>,
    target: &mykrut_core::Location,
) {
    let path = match target {
        mykrut_core::Location::Local(p) => p.clone(),
        mykrut_core::Location::Trash => return,
    };
    let w = watcher.clone();
    rt.spawn(async move {
        w.set_path(weak, path).await;
    });
}

