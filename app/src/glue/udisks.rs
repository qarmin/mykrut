//! UDisks2 integration — DBus-driven device enumeration + mount/unmount.
//!
//! Uses zbus dynamic Proxies (no codegen): one for `ObjectManager` to enumerate
//! and listen for add/remove, one per-device for `Filesystem.Mount/Unmount`.
//!
//! Devices are pushed to the UI through a channel; the Slint `Timer` pump rebuilds
//! the places sidebar whenever the list changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use futures_util::StreamExt;
use slint::{ComponentHandle, Timer, TimerMode};
use tokio::runtime::Runtime;
use tracing::{Instrument, debug, error, info, info_span, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

use crate::state::AppStateRc;
use crate::{Callabler, MainWindow};

const SERVICE: &str = "org.freedesktop.UDisks2";
const ROOT_PATH: &str = "/org/freedesktop/UDisks2";
const IFACE_OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const IFACE_BLOCK: &str = "org.freedesktop.UDisks2.Block";
const IFACE_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";

/// What the UI needs to display + act on a single device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub object_path: String,
    pub label: String,
    pub device_path: String,
    pub mount_point: Option<PathBuf>,
    pub size_bytes: u64,
}

pub struct UDisksController {
    /// Latest device list pushed by the worker.
    pub devices: std::sync::Mutex<Vec<DeviceInfo>>,
    /// Used by glue to request a mount/unmount.
    cmd_tx: tokio::sync::mpsc::UnboundedSender<UDisksCmd>,
}

enum UDisksCmd {
    Mount(String), // object path
    Unmount(String),
}

enum UiMsg {
    DevicesUpdated(Vec<DeviceInfo>),
}

pub fn install(app: &MainWindow, rt: &Arc<Runtime>) -> Arc<UDisksController> {
    let (ui_tx, ui_rx) = channel::<UiMsg>();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<UDisksCmd>();

    let ctrl = Arc::new(UDisksController {
        devices: std::sync::Mutex::new(Vec::new()),
        cmd_tx,
    });

    install_pump(app, ctrl.clone(), ui_rx);

    let rt_clone = rt.clone();
    rt.spawn(async move {
        if let Err(err) = run_udisks_loop(ui_tx, cmd_rx, rt_clone).await {
            warn!(?err, "udisks loop ended");
        }
    });

    ctrl
}

fn install_pump(app: &MainWindow, ctrl: Arc<UDisksController>, rx: Receiver<UiMsg>) {
    let weak = app.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(app) = weak.upgrade() else { return };
        let mut changed = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UiMsg::DevicesUpdated(list) => {
                    let mut guard = ctrl.devices.lock().unwrap();
                    if *guard != list {
                        *guard = list;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            // PlacesController will pick up the new list when it rebuilds.
            app.global::<Callabler>().invoke_devices_changed();
        }
    });
    Box::leak(Box::new(timer));
}

pub fn request_mount(ctrl: &UDisksController, object_path: &str) {
    let _ = ctrl.cmd_tx.send(UDisksCmd::Mount(object_path.to_string()));
}

pub fn request_unmount(ctrl: &UDisksController, object_path: &str) {
    let _ = ctrl.cmd_tx.send(UDisksCmd::Unmount(object_path.to_string()));
}

async fn run_udisks_loop(
    ui_tx: Sender<UiMsg>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<UDisksCmd>,
    _rt: Arc<Runtime>,
) -> zbus::Result<()> {
    let span = info_span!("udisks");
    let _g = span.enter();

    let conn = match Connection::system().await {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "system bus unavailable; mount/unmount disabled");
            return Err(err);
        }
    };
    info!("connected to system bus");

    let obj_mgr = Proxy::new(&conn, SERVICE, ROOT_PATH, IFACE_OBJECT_MANAGER).await?;

    // Initial enumeration.
    push_devices(&conn, &obj_mgr, &ui_tx).await;

    // Subscribe to InterfacesAdded/InterfacesRemoved/PropertiesChanged.
    let mut added = obj_mgr.receive_signal("InterfacesAdded").await?;
    let mut removed = obj_mgr.receive_signal("InterfacesRemoved").await?;

    loop {
        tokio::select! {
            Some(_) = added.next() => {
                debug!("InterfacesAdded → re-enumerate");
                push_devices(&conn, &obj_mgr, &ui_tx).await;
            }
            Some(_) = removed.next() => {
                debug!("InterfacesRemoved → re-enumerate");
                push_devices(&conn, &obj_mgr, &ui_tx).await;
            }
            Some(cmd) = cmd_rx.recv() => {
                if let Err(err) = run_command(&conn, cmd).await {
                    warn!(?err, "mount/unmount failed");
                }
                push_devices(&conn, &obj_mgr, &ui_tx).await;
            }
        }
    }
}

async fn run_command(conn: &Connection, cmd: UDisksCmd) -> zbus::Result<()> {
    let (op_path, method) = match &cmd {
        UDisksCmd::Mount(p) => (p.clone(), "Mount"),
        UDisksCmd::Unmount(p) => (p.clone(), "Unmount"),
    };
    let span = info_span!("udisks_call", method = method, path = %op_path);
    async move {
        info!("invoke");
        let proxy = Proxy::new(conn, SERVICE, op_path.as_str(), IFACE_FILESYSTEM).await?;
        let opts: HashMap<&str, Value<'_>> = HashMap::new();
        match method {
            "Mount" => {
                let mp: String = proxy.call("Mount", &(opts,)).await?;
                info!(mount_point = %mp, "mounted");
            }
            "Unmount" => {
                proxy.call::<_, _, ()>("Unmount", &(opts,)).await?;
                info!("unmounted");
            }
            #[expect(
                clippy::unreachable,
                reason = "`method` is set to \"Mount\" or \"Unmount\" from the UDisksCmd match above; no other value can reach here"
            )]
            _ => unreachable!(),
        }
        Ok::<_, zbus::Error>(())
    }
    .instrument(span)
    .await
}

async fn push_devices(conn: &Connection, obj_mgr: &Proxy<'_>, ui_tx: &Sender<UiMsg>) {
    match enumerate_devices(conn, obj_mgr).await {
        Ok(devices) => {
            debug!(count = devices.len(), "enumerated devices");
            let _ = ui_tx.send(UiMsg::DevicesUpdated(devices));
        }
        Err(err) => {
            error!(?err, "enumeration failed");
        }
    }
}

type ManagedMap = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

async fn enumerate_devices(conn: &Connection, obj_mgr: &Proxy<'_>) -> zbus::Result<Vec<DeviceInfo>> {
    let managed: ManagedMap = obj_mgr.call("GetManagedObjects", &()).await?;

    let mut out: Vec<DeviceInfo> = Vec::new();

    for (path, ifaces) in &managed {
        let Some(fs_iface) = ifaces.get(IFACE_FILESYSTEM) else {
            continue;
        };
        let Some(block_iface) = ifaces.get(IFACE_BLOCK) else {
            continue;
        };

        let device_path = bytes_prop(block_iface, "Device").unwrap_or_default();
        let id_label = string_prop(block_iface, "IdLabel").unwrap_or_default();
        let id_uuid = string_prop(block_iface, "IdUuid").unwrap_or_default();
        let size = u64_prop(block_iface, "Size").unwrap_or(0);
        let hint_system = bool_prop(block_iface, "HintSystem").unwrap_or(false);

        let mount_point = first_mount_point(fs_iface);

        // Skip system mounts (root, /boot, /home if those happen to live there).
        if hint_system {
            // System partitions: skip ones not under /run/media, /media, /mnt.
            if let Some(mp) = &mount_point {
                let s = mp.to_string_lossy();
                if !s.starts_with("/run/media") && !s.starts_with("/media") && !s.starts_with("/mnt") {
                    continue;
                }
            } else {
                // Unmounted system partition — also skip.
                continue;
            }
        }

        let label = if !id_label.is_empty() {
            id_label
        } else if !id_uuid.is_empty() {
            id_uuid
        } else {
            device_path.rsplit('/').next().unwrap_or("unknown").to_string()
        };

        out.push(DeviceInfo {
            object_path: path.to_string(),
            label,
            device_path,
            mount_point,
            size_bytes: size,
        });
    }

    // Stable ordering so the sidebar doesn't shuffle on every refresh.
    out.sort_by(|a, b| a.device_path.cmp(&b.device_path));
    let _ = conn; // silence unused
    Ok(out)
}

fn first_mount_point(fs_iface: &HashMap<String, OwnedValue>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let v = fs_iface.get("MountPoints")?;
    let arr: Vec<Vec<u8>> = v.try_clone().ok()?.try_into().ok()?;
    arr.into_iter().next().map(|bytes| {
        // UDisks returns nul-terminated byte strings.
        let trimmed = bytes.split(|b| *b == 0).next().unwrap_or(&[]);
        PathBuf::from(std::ffi::OsString::from_vec(trimmed.to_vec()))
    })
}

fn string_prop(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let v = map.get(key)?;
    let s: String = v.try_clone().ok()?.try_into().ok()?;
    Some(s)
}

fn bytes_prop(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let v = map.get(key)?;
    let bytes: Vec<u8> = v.try_clone().ok()?.try_into().ok()?;
    let trimmed = bytes.split(|b| *b == 0).next().unwrap_or(&[]);
    String::from_utf8(trimmed.to_vec()).ok()
}

fn u64_prop(map: &HashMap<String, OwnedValue>, key: &str) -> Option<u64> {
    let v = map.get(key)?;
    v.try_clone().ok()?.try_into().ok()
}

fn bool_prop(map: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    let v = map.get(key)?;
    v.try_clone().ok()?.try_into().ok()
}

