//! Rate-monotonic scheduler with **simulated single-CPU preemption**.
//!
//! Only one RMS job consumes a 1 ms quantum per tick. The highest-priority runnable
//! job (lowest period / index 0 = thermal) always runs. If it becomes ready while a
//! lower-priority job still has remaining WCET, the lower job is preempted and its
//! preemption counter is incremented.

use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::time::{Duration, Instant};
use shared::config::{THERMAL_CTRL_PERIOD_MS, DATA_COMPRESS_PERIOD_MS, HEALTH_MONITOR_PERIOD_MS};

const QUANTUM_MS: u64 = 1;

#[derive(Debug, Clone)]
struct RmsTask {
    name:             &'static str,
    period_ms:        u64,
    wcet_ms:          u64,
    deadline_ms:      u64,
    next_release:     Instant,
    /// Remaining simulated execution time (WCET not yet consumed) for the current job.
    pending_work_ms:  u64,
    /// Release instant of the job currently in progress (for drift / deadline checks).
    current_release:  Option<Instant>,
    exec_count:       u64,
    miss_count:       u64,
}

pub async fn run_rms_scheduler(
    mut cancel: tokio::sync::watch::Receiver<bool>,
    heartbeat: Arc<AtomicU64>,
    sim_start: Arc<Instant>,
    ui_metrics: Arc<tokio::sync::Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let task_start = Instant::now();

    let mut tasks = vec![
        RmsTask {
            name: "ThermalControl",
            period_ms: THERMAL_CTRL_PERIOD_MS,
            wcet_ms: 5,
            deadline_ms: THERMAL_CTRL_PERIOD_MS,
            next_release: task_start + Duration::from_millis(THERMAL_CTRL_PERIOD_MS),
            pending_work_ms: 0,
            current_release: None,
            exec_count: 0,
            miss_count: 0,
        },
        RmsTask {
            name: "DataCompress",
            period_ms: DATA_COMPRESS_PERIOD_MS,
            wcet_ms: 20,
            deadline_ms: DATA_COMPRESS_PERIOD_MS,
            next_release: task_start + Duration::from_millis(DATA_COMPRESS_PERIOD_MS),
            pending_work_ms: 0,
            current_release: None,
            exec_count: 0,
            miss_count: 0,
        },
        RmsTask {
            name: "HealthMonitor",
            period_ms: HEALTH_MONITOR_PERIOD_MS,
            wcet_ms: 50,
            deadline_ms: HEALTH_MONITOR_PERIOD_MS,
            next_release: task_start + Duration::from_millis(HEALTH_MONITOR_PERIOD_MS),
            pending_work_ms: 0,
            current_release: None,
            exec_count: 0,
            miss_count: 0,
        },
    ];

    let mut tick = tokio::time::interval(Duration::from_millis(QUANTUM_MS));
    let mut last_running: Option<usize> = None;
    let mut total_active_us = 0u64;
    let mut interval_start = Instant::now();

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = tick.tick() => {}
        }

        let now = Instant::now();

        if now.duration_since(interval_start) >= Duration::from_secs(10) {
            let elapsed_us = now.duration_since(interval_start).as_micros() as u64;
            let cpu_util_pct = (total_active_us as f64 / elapsed_us as f64) * 100.0;
            tracing::info!(cpu_util_pct, "rms_scheduler CPU usage");
            crate::ui::push_log(&ui_metrics, 0, format!("rms_scheduler CPU usage {:.1}%", cpu_util_pct), &sim_start);
            if let Ok(mut m) = ui_metrics.try_lock() {
                m.cpu_util_pct = cpu_util_pct;
            }
            total_active_us = 0;
            interval_start = now;
        }

        // New job releases (at most one pending instance per task — backlog collapses to one job).
        for t in tasks.iter_mut() {
            if t.pending_work_ms == 0 && now >= t.next_release {
                t.pending_work_ms = t.wcet_ms;
                t.current_release = Some(t.next_release);
            }
        }

        let highest = (0..tasks.len()).find(|&i| tasks[i].pending_work_ms > 0);

        if let Some(h) = highest {
            if let Some(prev) = last_running {
                if prev != h && h < prev {
                    let preempted = tasks[prev].name;
                    let by = tasks[h].name;
                    tracing::info!(preempted, by, "RMS preemption: higher-priority task runs");
                    crate::ui::push_log(
                        &ui_metrics,
                        0,
                        format!("RMS preemption: {} preempted by {}", preempted, by),
                        &sim_start,
                    );
                    if let Ok(mut m) = ui_metrics.try_lock() {
                        match preempted {
                            "ThermalControl" => m.thermal_ctrl_preemptions += 1,
                            "DataCompress" => m.data_compress_preemptions += 1,
                            "HealthMonitor" => m.health_monitor_preemptions += 1,
                            _ => {}
                        }
                    }
                }
            }
            last_running = Some(h);

            if tasks[h].pending_work_ms == tasks[h].wcet_ms {
                if let Some(rel) = tasks[h].current_release {
                    let expected_start_us = rel.duration_since(*sim_start).as_micros() as u64;
                    let actual_start_us = now.duration_since(*sim_start).as_micros() as u64;
                    let drift_us = actual_start_us as i64 - expected_start_us as i64;
                    tracing::info!(task = tasks[h].name, drift_us, "Task dispatched");
                    crate::ui::push_log(
                        &ui_metrics,
                        0,
                        format!("Task dispatched {} drift={}us", tasks[h].name, drift_us),
                        &sim_start,
                    );
                    if let Ok(mut m) = ui_metrics.try_lock() {
                        match tasks[h].name {
                            "ThermalControl" => m.thermal_ctrl_drift_us = drift_us,
                            "DataCompress" => m.data_compress_drift_us = drift_us,
                            "HealthMonitor" => m.health_monitor_drift_us = drift_us,
                            _ => {}
                        }
                    }
                }
            }

            tasks[h].pending_work_ms = tasks[h].pending_work_ms.saturating_sub(QUANTUM_MS);
            total_active_us += QUANTUM_MS * 1000;

            if tasks[h].pending_work_ms == 0 {
                let task_name = tasks[h].name;
                let period_ms = tasks[h].period_ms;
                let deadline_ms = tasks[h].deadline_ms;
                let rel = tasks[h].current_release;
                let deadline = rel.map(|r| r + Duration::from_millis(deadline_ms));
                let is_miss = deadline.map(|d| now > d).unwrap_or(false);
                if is_miss {
                    tasks[h].miss_count += 1;
                    let violation_us = deadline
                        .map(|d| now.duration_since(d).as_micros() as u64)
                        .unwrap_or(0);
                    tracing::warn!(task=task_name, violation_us, "DEADLINE VIOLATION");
                    crate::ui::push_log(
                        &ui_metrics,
                        1,
                        format!("DEADLINE VIOLATION {} ({}us)", task_name, violation_us),
                        &sim_start,
                    );
                }

                let miss = tasks[h].miss_count;
                tasks[h].next_release += Duration::from_millis(period_ms);
                tasks[h].exec_count += 1;
                tasks[h].current_release = None;
                last_running = None;

                if let Ok(mut m) = ui_metrics.try_lock() {
                    match task_name {
                        "ThermalControl" => m.thermal_ctrl_violations = miss,
                        "DataCompress" => m.data_compress_violations = miss,
                        "HealthMonitor" => m.health_monitor_violations = miss,
                        _ => {}
                    }
                }
            }
        } else {
            last_running = None;
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}
