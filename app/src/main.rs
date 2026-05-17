#![allow(clippy::unwrap_used)]
#![allow(clippy::indexing_slicing)]
#![allow(clippy::todo)]

mod bookmarks;
mod format_util;
mod fs_atomic;
mod glue;
mod logging;
mod settings;
mod state;
mod ui_watchdog;

use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context, Result};
use slint::ModelRc;
use tracing::{error, info};

slint::include_modules!();

fn main() -> Result<()> {
    logging::init();
    let _panic_guard = logging::install_panic_hook();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "fm starting"
    );

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(num_cpus_or(4))
            .thread_name("fm-tokio")
            .enable_all()
            .build()
            .context("create tokio runtime")?,
    );

    // Enter the Tokio runtime on the main thread so Slint's internal zbus usage
    // (xdg_desktop_settings dark-mode watcher) can call spawn_blocking.
    let _rt_guard = rt.handle().enter();

    let app = MainWindow::new().context("create MainWindow")?;

    // Flag root mode up front so the warning banner + title prefix render
    // on first paint. SAFETY: geteuid is reentrant + always-safe.
    let is_root = unsafe { libc::geteuid() } == 0;
    if is_root {
        info!("running as root — banner + title suffix active");
    }
    {
        use slint::ComponentHandle;
        app.global::<AppState>().set_is_root(is_root);
    }

    // Restore persisted settings (theme, view mode, etc.) before wiring callbacks.
    let persisted = settings::load();
    apply_persisted(&app, &persisted);

    let rows_model: Rc<slint::VecModel<FileRowData>> = Rc::new(slint::VecModel::default());
    app.set_rows(ModelRc::from(rows_model.clone()));

    let app_state = state::AppStateRc::new(rows_model);
    let watcher = glue::wire_all(&app, &rt, app_state.clone());
    install_view_mode_cycle(&app);
    install_settings_autosave(&app);

    // Optional positional argument: starting directory.
    let initial = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
        // Resolve relative args like "." into an absolute path so the breadcrumb isn't blank/"/".
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")));
    info!(start_dir = %initial.display(), "initial navigation");
    glue::navigation::navigate_to(&app, &rt, app_state, watcher, mykrut_core::Location::Local(initial));

    // Logs any event-loop stall (e.g. a heavy gallery render) with its duration.
    ui_watchdog::install();

    match app.run() {
        Ok(()) => {
            info!("event loop exited cleanly");
            Ok(())
        }
        Err(err) => {
            error!(?err, "event loop returned error");
            Err(err).context("Slint event loop")
        }
    }
}

fn num_cpus_or(fallback: usize) -> usize {
    std::thread::available_parallelism().map_or(fallback, |n| n.get())
}

/// Ctrl+wheel and the 7-step status-bar widget cycle the item-size in 1..=7.
/// View mode (list vs gallery) is unaffected — both modes honour item-size.
fn install_view_mode_cycle(app: &MainWindow) {
    use slint::ComponentHandle;
    let weak = app.as_weak();
    app.global::<Callabler>().on_cycle_item_size(move |delta| {
        let app = weak.upgrade().expect("MainWindow alive in cycle-item-size");
        let cur = app.global::<Settings>().get_item_size();
        let new_v = (cur + delta).clamp(1, 7);
        info!(delta, cur, new_v, "cycle-item-size invoked");
        if new_v != cur {
            app.global::<Settings>().set_item_size(new_v);
        }
    });

    // Debug taps so the user can diagnose intermittent Ctrl+wheel. The wheel
    // tap logs each scroll-event with the live `ctrl-held` flag; the ctrl-change
    // tap logs every press/release that updates the flag.
    app.global::<Callabler>()
        .on_debug_wheel(|location, delta_y, ctrl_held| {
            info!(
                location = %location,
                delta_y,
                ctrl_held,
                wheel_only = !ctrl_held && delta_y != 0.0,
                ctrl_only = ctrl_held && delta_y == 0.0,
                both = ctrl_held && delta_y != 0.0,
                "wheel"
            );
        });
    app.global::<Callabler>().on_debug_ctrl_change(|ctrl_held, phase| {
        info!(ctrl_held, phase = %phase, "ctrl-flag updated");
    });
}

fn apply_persisted(app: &MainWindow, p: &settings::PersistedSettings) {
    use slint::ComponentHandle;
    let s = app.global::<Settings>();
    let (mode, size) = migrate_view_mode(p.view_mode, p.item_size);
    s.set_dark_theme(p.dark_theme);
    s.set_view_mode(mode);
    s.set_item_size(size);
    s.set_show_hidden(p.show_hidden);
    s.set_sort_key(sort_key_from_u8(p.sort_key));
    s.set_sort_order(sort_order_from_u8(p.sort_order));
    s.set_auto_refresh(p.auto_refresh);
    s.set_sidebar_width(p.sidebar_width as f32);
    s.set_thumb_size(p.thumb_size.min(3) as i32);
    s.set_icon_style(icon_style_from_u8(p.icon_style));
    s.set_col_size_visible(p.col_size_visible);
    s.set_col_modified_visible(p.col_modified_visible);
    s.set_col_kind_visible(p.col_kind_visible);
    s.set_thumb_mem_budget_mb(p.thumb_mem_budget_mb as i32);
}

/// Honour both the new 2-value view_mode + item_size schema and old configs
/// that packed size into view_mode (1..4 = grid sizes).
fn migrate_view_mode(persisted_mode: u8, persisted_item_size: u8) -> (AppViewMode, i32) {
    match persisted_mode {
        0 => (AppViewMode::List, persisted_item_size.clamp(1, 7) as i32),
        // Old 1=grid-small … 4=grid-xlarge → gallery, with size derived from old
        // step so the user roughly keeps their previous tile dimensions.
        1 => (AppViewMode::Gallery, 2),
        2 => (AppViewMode::Gallery, 3),
        3 => (AppViewMode::Gallery, 5),
        4 => (AppViewMode::Gallery, 7),
        _ => (AppViewMode::Gallery, persisted_item_size.clamp(1, 7) as i32),
    }
}

fn install_settings_autosave(app: &MainWindow) {
    use std::time::Duration;

    use slint::ComponentHandle;
    // Snapshot + write on a 500 ms debounce — avoids hammering disk while user
    // clicks through view modes.
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    let last = std::rc::Rc::new(std::cell::RefCell::new(settings::PersistedSettings {
        dark_theme: app.global::<Settings>().get_dark_theme(),
        view_mode: view_mode_to_u8(app.global::<Settings>().get_view_mode()),
        item_size: app.global::<Settings>().get_item_size().clamp(1, 7) as u8,
        show_hidden: app.global::<Settings>().get_show_hidden(),
        sort_key: sort_key_to_u8(app.global::<Settings>().get_sort_key()),
        sort_order: sort_order_to_u8(app.global::<Settings>().get_sort_order()),
        auto_refresh: app.global::<Settings>().get_auto_refresh(),
        window_size: (1100, 700),
        sidebar_width: app.global::<Settings>().get_sidebar_width() as u32,
        thumb_size: app.global::<Settings>().get_thumb_size().clamp(0, 3) as u8,
        icon_style: icon_style_to_u8(app.global::<Settings>().get_icon_style()),
        col_size_visible: app.global::<Settings>().get_col_size_visible(),
        col_modified_visible: app.global::<Settings>().get_col_modified_visible(),
        col_kind_visible: app.global::<Settings>().get_col_kind_visible(),
        thumb_mem_budget_mb: app.global::<Settings>().get_thumb_mem_budget_mb().max(0) as u32,
    }));
    let last_for_timer = last;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(app) = weak.upgrade() else { return };
        let s = app.global::<Settings>();
        let current = settings::PersistedSettings {
            dark_theme: s.get_dark_theme(),
            view_mode: view_mode_to_u8(s.get_view_mode()),
            item_size: s.get_item_size().clamp(1, 7) as u8,
            show_hidden: s.get_show_hidden(),
            sort_key: sort_key_to_u8(s.get_sort_key()),
            sort_order: sort_order_to_u8(s.get_sort_order()),
            auto_refresh: s.get_auto_refresh(),
            window_size: last_for_timer.borrow().window_size,
            sidebar_width: s.get_sidebar_width() as u32,
            thumb_size: s.get_thumb_size().clamp(0, 3) as u8,
            icon_style: icon_style_to_u8(s.get_icon_style()),
            col_size_visible: s.get_col_size_visible(),
            col_modified_visible: s.get_col_modified_visible(),
            col_kind_visible: s.get_col_kind_visible(),
            thumb_mem_budget_mb: s.get_thumb_mem_budget_mb().max(0) as u32,
        };
        let need_save = !same(&last_for_timer.borrow(), &current);
        if need_save {
            tracing::debug!(
                view_mode = current.view_mode,
                dark = current.dark_theme,
                "settings changed → save"
            );
            *last_for_timer.borrow_mut() = current.clone();
            if let Err(err) = settings::save(&current) {
                tracing::warn!(?err, "settings save failed");
            }
        }
    });
    Box::leak(Box::new(timer));
}

fn same(a: &settings::PersistedSettings, b: &settings::PersistedSettings) -> bool {
    a.dark_theme == b.dark_theme
        && a.view_mode == b.view_mode
        && a.item_size == b.item_size
        && a.show_hidden == b.show_hidden
        && a.sort_key == b.sort_key
        && a.sort_order == b.sort_order
        && a.auto_refresh == b.auto_refresh
        && a.sidebar_width == b.sidebar_width
        && a.thumb_size == b.thumb_size
        && a.icon_style == b.icon_style
        && a.col_size_visible == b.col_size_visible
        && a.col_modified_visible == b.col_modified_visible
        && a.col_kind_visible == b.col_kind_visible
        && a.thumb_mem_budget_mb == b.thumb_mem_budget_mb
}

fn view_mode_to_u8(v: AppViewMode) -> u8 {
    match v {
        AppViewMode::List => 0,
        AppViewMode::Gallery => 1,
    }
}

fn sort_key_from_u8(v: u8) -> SortKey {
    match v {
        1 => SortKey::Size,
        2 => SortKey::Mtime,
        3 => SortKey::Kind,
        _ => SortKey::Name,
    }
}
fn sort_key_to_u8(v: SortKey) -> u8 {
    match v {
        SortKey::Name => 0,
        SortKey::Size => 1,
        SortKey::Mtime => 2,
        SortKey::Kind => 3,
    }
}

fn icon_style_from_u8(v: u8) -> IconStyle {
    match v {
        1 => IconStyle::Color,
        _ => IconStyle::Mono,
    }
}
fn icon_style_to_u8(v: IconStyle) -> u8 {
    match v {
        IconStyle::Mono => 0,
        IconStyle::Color => 1,
    }
}

fn sort_order_from_u8(v: u8) -> SortOrder {
    match v {
        1 => SortOrder::Descending,
        _ => SortOrder::Ascending,
    }
}
fn sort_order_to_u8(v: SortOrder) -> u8 {
    match v {
        SortOrder::Ascending => 0,
        SortOrder::Descending => 1,
    }
}
