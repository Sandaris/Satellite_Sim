use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{Duration, Instant};

pub async fn run_perf_monitor(
    ui_metrics: Arc<tokio::sync::Mutex<crate::ui::GcsMetricsSnapshot>>,
    sim_start: Arc<Instant>,
    mut cancel:    tokio::sync::watch::Receiver<bool>,
    gcs_busy_telemetry_us: Arc<AtomicU64>,
    gcs_busy_uplink_us: Arc<AtomicU64>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.tick().await; // skip first
    let mut load_interval = tokio::time::interval(Duration::from_secs(1));
    load_interval.tick().await; // skip first immediate tick
    let mut last_load_wall = Instant::now();
    let mut report_num = 1u32;

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = load_interval.tick() => {
                let now = Instant::now();
                let window_us = now.duration_since(last_load_wall).as_micros() as u64;
                last_load_wall = now;
                if window_us > 0 {
                    let tel = gcs_busy_telemetry_us.swap(0, Ordering::Relaxed);
                    let upl = gcs_busy_uplink_us.swap(0, Ordering::Relaxed);
                    let total = tel.saturating_add(upl);
                    let pct = ((total as f64) / (window_us as f64)) * 100.0;
                    if let Ok(mut m) = ui_metrics.try_lock() {
                        m.system_load_pct = pct.min(100.0);
                    }
                }
            }
            _ = interval.tick() => {
                let elapsed_s = sim_start.elapsed().as_secs();
                let elapsed_us = sim_start.elapsed().as_micros() as u64;
                
                let m = ui_metrics.lock().await.clone();
                tracing::info!(
                    report=report_num,
                    elapsed_s,
                    elapsed_us,
                    pkts_received=m.total_pkts_received,
                    pkts_lost=m.total_pkts_lost,
                    latency_p50_us=m.latency_p50_us,
                    latency_p99_us=m.latency_p99_us,
                    decode_deadline_misses=m.decode_deadline_misses,
                    fault_received_count=m.fault_received_count,
                    cmd_deadline_misses=m.cmd_deadline_misses,
                    uplink_jitter_p99_us=m.uplink_jitter_p99_us,
                    telemetry_backlog_max=m.telemetry_backlog_max,
                    task_drift_uplink_last_us=m.task_drift_uplink_last_us,
                    task_drift_telemetry_last_us=m.task_drift_telemetry_last_us,
                    system_load_pct=m.system_load_pct,
                    "=== PERFORMANCE REPORT ==="
                );
                report_num += 1;
            }
        }
    }
}
