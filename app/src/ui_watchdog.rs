//! Detects main-thread (event-loop) stalls and logs them.
//!
//! A Slint timer pings a heartbeat from the UI thread; an independent watcher
//! thread samples it and logs a warning whenever the UI thread goes
//! unresponsive (a heavy render/layout pass, a blocking call on the event
//! loop, ...), bracketing the freeze with its peak duration. Because the
//! watcher is its own thread it keeps sampling even while the UI thread is
//! frozen, so the warning lands in real time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slint::{Timer, TimerMode};
use tracing::warn;

/// How often the UI thread refreshes the heartbeat (and the watcher samples it).
const HEARTBEAT_MS: u64 = 100;
/// A gap longer than this between heartbeats counts as a user-visible stall.
const STALL_THRESHOLD_MS: u64 = 400;

/// Install the heartbeat timer (must be called on the Slint/UI thread) and spawn
/// the watcher thread. The timer is leaked so it lives for the whole session.
pub fn install() {
    let start = Instant::now();
    let beat = Arc::new(AtomicU64::new(0));

    let timer = Timer::default();
    {
        let beat = beat.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(HEARTBEAT_MS), move || {
            beat.store(start.elapsed().as_millis() as u64, Ordering::Release);
        });
    }
    Box::leak(Box::new(timer));

    std::thread::Builder::new()
        .name("fm-ui-watchdog".into())
        .spawn(move || {
            let mut in_stall = false;
            let mut peak = 0u64;
            loop {
                std::thread::sleep(Duration::from_millis(HEARTBEAT_MS));
                let now = start.elapsed().as_millis() as u64;
                let since = now.saturating_sub(beat.load(Ordering::Acquire));
                if since >= STALL_THRESHOLD_MS {
                    if !in_stall {
                        in_stall = true;
                        warn!(stalled_ms = since, "UI thread unresponsive (event loop blocked)");
                    }
                    peak = peak.max(since);
                } else if in_stall {
                    warn!(peak_ms = peak, "UI thread recovered after stall");
                    in_stall = false;
                    peak = 0;
                }
            }
        })
        .expect("spawn ui watchdog thread");
}
