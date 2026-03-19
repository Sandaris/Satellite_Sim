use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use std::collections::BinaryHeap;
use shared::packets::{CommandPacket, CommandType, FaultPacket};
use shared::config::GCS_INTERLOCK_LIMIT_MS;
use crate::state::GcsSystemState;
use crate::uplink_tx::PrioritizedCommand;

pub async fn run_fault_mgr(
    mut fault_rx:  tokio::sync::mpsc::Receiver<FaultPacket>,
    state:     Arc<Mutex<GcsSystemState>>,
    cmd_queue: Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
    sim_start: Arc<Instant>,
    mut cancel:    tokio::sync::watch::Receiver<bool>,
    heartbeat: Arc<AtomicU64>,
    ui_metrics: Arc<Mutex<crate::ui::GcsMetricsSnapshot>>,
) {
    let mut total_faults = 0u64;
    let mut interlock_latencies: Vec<u64> = Vec::new();

    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = interval.tick() => {
                heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
            }
            fault = fault_rx.recv() => {
                let fault = match fault { Some(f) => f, None => break };
                let detect_us = sim_start.elapsed().as_micros() as u64;

                tracing::warn!(fault=?fault.fault_type, severity=fault.severity, elapsed_us=detect_us,
                               "fault_mgr: fault received");
                crate::ui::push_log(&ui_metrics, 1, format!("fault received: {:?}", fault.fault_type), &sim_start);

                {
                    let mut s = state.lock().await;
                    *s = GcsSystemState::InterlockActive;
                }

                let interlock_us = sim_start.elapsed().as_micros() as u64;
                let interlock_latency = interlock_us.saturating_sub(detect_us);
                interlock_latencies.push(interlock_latency);

                tracing::info!(interlock_latency_us=interlock_latency, elapsed_us=interlock_us,
                               "fault_mgr: interlock APPLIED");
                crate::ui::push_log(&ui_metrics, 0, format!("interlock APPLIED in {}us", interlock_latency), &sim_start);

                if interlock_latency > GCS_INTERLOCK_LIMIT_MS * 1000 {
                    tracing::error!(interlock_latency_us=interlock_latency, limit_us=GCS_INTERLOCK_LIMIT_MS*1000, elapsed_us=interlock_us,
                                   "CRITICAL GROUND ALERT: interlock exceeded 100ms");
                    crate::ui::push_log(&ui_metrics, 2, "CRITICAL: interlock exceeded 100ms".to_string(), &sim_start);
                    let mut s = state.lock().await;
                    *s = GcsSystemState::CriticalAlert;
                    if let Ok(mut m) = ui_metrics.try_lock() { m.critical_alerts += 1; }
                }

                let enqueue_ts = sim_start.elapsed().as_micros() as u64;
                let safe_cmd = PrioritizedCommand {
                    packet: CommandPacket { seq_no: 0, timestamp_us: enqueue_ts,
                                            cmd_type: CommandType::SafeMode,
                                            priority: 1, payload: [0u8; 32] },
                    enqueue_us: enqueue_ts,
                };
                cmd_queue.lock().await.push(safe_cmd);

                // Auto-resolve after 50ms (real-time recovery window < satellite's 200ms limit)
                let state_clone = state.clone();
                let ui_metrics_clone = ui_metrics.clone();
                let sim_start_clone = sim_start.clone();
                let cmd_queue_clone = cmd_queue.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    { 
                        let mut s = state_clone.lock().await;
                        if *s == GcsSystemState::InterlockActive || *s == GcsSystemState::CriticalAlert {
                            *s = GcsSystemState::Nominal; 
                        }
                    }

                    // Send ResetSensor command to satellite to clear its fault/abort state
                    let enqueue_ts = sim_start_clone.elapsed().as_micros() as u64;
                    let reset_cmd = PrioritizedCommand {
                        packet: CommandPacket { seq_no: 0, timestamp_us: enqueue_ts,
                                                cmd_type: CommandType::ResetSensor,
                                                priority: 1, payload: [0u8; 32] }, // High priority to clear fault
                        enqueue_us: enqueue_ts,
                    };
                    cmd_queue_clone.lock().await.push(reset_cmd);

                    tracing::info!("fault_mgr: interlock CLEARED, reset command sent");
                    crate::ui::push_log(&ui_metrics_clone, 0, "interlock CLEARED, reset sent".to_string(), &sim_start_clone);
                    if let Ok(mut m) = ui_metrics_clone.try_lock() { m.interlock_active = false; }
                });

                total_faults += 1;
                if let Ok(mut m) = ui_metrics.try_lock() {
                    m.fault_received_count = total_faults;
                    m.fault_last_type = format!("{:?}", fault.fault_type);
                    m.fault_last_time_s = detect_us / 1_000_000;
                    m.interlock_last_us = interlock_latency;
                    m.interlock_max_us = *interlock_latencies.iter().max().unwrap_or(&0);
                    if let Ok(s) = state.try_lock() { m.interlock_active = *s == GcsSystemState::InterlockActive; }
                }
            }
        }
    }
    let avg_interlock = if interlock_latencies.is_empty() { 0 }
        else { interlock_latencies.iter().sum::<u64>() / interlock_latencies.len() as u64 };
    tracing::info!(total_faults, avg_interlock_us=avg_interlock,
                   "fault_mgr final stats");
}
