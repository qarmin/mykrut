//! MTP device detection — Phase 7a (sidebar entry only).
//!
//! Full browsing (Location::Mtp + MtpFs adapter) lives in Phase 7b. For now we
//! just poll the USB bus every few seconds with `mtp_rs::MtpDevice::list_devices`
//! (sync, fast — no session opened) and surface what we find as sidebar items.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use slint::{ComponentHandle, Timer, TimerMode};
use tokio::runtime::Runtime;
use tracing::{debug, info};

use crate::{Callabler, MainWindow};

const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtpDeviceEntry {
    pub serial: String,
    pub label: String,
    pub vendor: String,
    pub model: String,
    pub location_id: u64,
}

pub struct MtpController {
    pub devices: std::sync::Mutex<Vec<MtpDeviceEntry>>,
}

enum UiMsg {
    Updated(Vec<MtpDeviceEntry>),
}

pub fn install(app: &MainWindow, rt: &Arc<Runtime>) -> Arc<MtpController> {
    let (tx, rx) = channel::<UiMsg>();
    let ctrl = Arc::new(MtpController {
        devices: std::sync::Mutex::new(Vec::new()),
    });

    install_pump(app, ctrl.clone(), rx);

    rt.spawn(async move {
        run_poll_loop(tx).await;
    });

    ctrl
}

fn install_pump(app: &MainWindow, ctrl: Arc<MtpController>, rx: Receiver<UiMsg>) {
    let weak = app.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(app) = weak.upgrade() else { return };
        let mut changed = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UiMsg::Updated(list) => {
                    let mut guard = ctrl.devices.lock().unwrap();
                    if *guard != list {
                        *guard = list;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            // PlacesController re-renders the sidebar via the same callback used
            // for UDisks2 device changes.
            app.global::<Callabler>().invoke_devices_changed();
        }
    });
    Box::leak(Box::new(timer));
}

async fn run_poll_loop(tx: Sender<UiMsg>) {
    let mut last: Vec<MtpDeviceEntry> = Vec::new();
    loop {
        let devices = match scan_devices() {
            Ok(d) => d,
            Err(err) => {
                debug!(?err, "mtp scan failed (likely no permission or no devices)");
                Vec::new()
            }
        };

        if devices != last {
            info!(count = devices.len(), "mtp device set changed");
            last = devices.clone();
            if tx.send(UiMsg::Updated(devices)).is_err() {
                return;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn scan_devices() -> Result<Vec<MtpDeviceEntry>, mtp_rs::Error> {
    // list_devices is sync but quick — just a USB bus enumeration, no session.
    let raw = mtp_rs::mtp::MtpDevice::list_devices()?;
    let mut out = Vec::with_capacity(raw.len());
    for info in raw {
        let label = info.display();
        out.push(MtpDeviceEntry {
            serial: format!("loc{}", info.location_id),
            label,
            vendor: format!("0x{:04x}", info.vendor_id),
            model: format!("0x{:04x}", info.product_id),
            location_id: info.location_id,
        });
    }
    Ok(out)
}
