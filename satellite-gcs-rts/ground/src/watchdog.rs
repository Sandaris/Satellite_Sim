use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::time::{Duration, Instant};
use shared::config::{WATCHDOG_CHECK_INTERVAL_S, WATCHDOG_TIMEOUT_S};

pub async fn run_watchdog(
    heartbeats: Vec<(&'static str, Arc<AtomicU64>)>,
    sim_start:  Arc<Instant>,
    mut cancel:     tokio::sync::watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(WATCHDOG_CHECK_INTERVAL_S));
    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = interval.tick() => {
                let now_s = sim_start.elapsed().as_secs();
                for (name, hb) in &heartbeats {
                    let last = hb.load(Ordering::Relaxed);
                    // Filter starting zero condition
                    if last == 0 && now_s < WATCHDOG_TIMEOUT_S {
                        continue;
                    }
                    if now_s.saturating_sub(last) > WATCHDOG_TIMEOUT_S {
                        tracing::error!(task=name, last_seen_s=last, now_s,
                                       "WATCHDOG: task is STALE — would restart");
                    }
                }
            }
        }
    }
}
