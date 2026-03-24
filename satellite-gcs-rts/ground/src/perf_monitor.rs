use std::sync::Arc;
use tokio::time::{Duration, Instant};

pub async fn run_perf_monitor(
    ui_metrics: Arc<tokio::sync::Mutex<crate::ui::GcsMetricsSnapshot>>,
    sim_start: Arc<Instant>,
    mut cancel:    tokio::sync::watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.tick().await; // skip first
    let mut report_num = 1u32;

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
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
                    decode_deadline_misses=m.cmd_deadline_misses, // or similar
                    fault_received_count=m.fault_received_count,
                    cmd_deadline_misses=m.cmd_deadline_misses,
                    "=== PERFORMANCE REPORT ==="
                );
                report_num += 1;
            }
        }
    }
}
