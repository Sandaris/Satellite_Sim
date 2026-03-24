use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::time::{Duration, Instant};
use shared::config::{THERMAL_CTRL_PERIOD_MS, DATA_COMPRESS_PERIOD_MS, HEALTH_MONITOR_PERIOD_MS};
use shared::metrics::TaskMetrics;

#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub name:        &'static str,
    pub priority:    u8,
    pub period_ms:   u64,
    pub wcet_ms:     u64,
    pub deadline_ms: u64,
    pub next_release: Instant,
    pub exec_count:  u64,
    pub miss_count:  u64,
    pub preemption_count: u64,
}

pub async fn run_rms_scheduler(
    mut cancel: tokio::sync::watch::Receiver<bool>,
    heartbeat: Arc<AtomicU64>,
    sim_start: Arc<Instant>,
    ui_metrics: Arc<tokio::sync::Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let task_start = Instant::now();
    let tasks = Arc::new(tokio::sync::Mutex::new(vec![
        ScheduledTask {
            name: "ThermalControl",
            priority: 1,
            period_ms: THERMAL_CTRL_PERIOD_MS,
            wcet_ms: 5,
            deadline_ms: THERMAL_CTRL_PERIOD_MS,
            next_release: task_start + Duration::from_millis(THERMAL_CTRL_PERIOD_MS),
            exec_count: 0, miss_count: 0, preemption_count: 0,
        },
        ScheduledTask {
            name: "DataCompress",
            priority: 2,
            period_ms: DATA_COMPRESS_PERIOD_MS,
            wcet_ms: 20,
            deadline_ms: DATA_COMPRESS_PERIOD_MS,
            next_release: task_start + Duration::from_millis(DATA_COMPRESS_PERIOD_MS),
            exec_count: 0, miss_count: 0, preemption_count: 0,
        },
        ScheduledTask {
            name: "HealthMonitor",
            priority: 3,
            period_ms: HEALTH_MONITOR_PERIOD_MS,
            wcet_ms: 50,
            deadline_ms: HEALTH_MONITOR_PERIOD_MS,
            next_release: task_start + Duration::from_millis(HEALTH_MONITOR_PERIOD_MS),
            exec_count: 0, miss_count: 0, preemption_count: 0,
        },
    ]));

    let mut tick = tokio::time::interval(Duration::from_millis(10));
    let mut total_ticks = 0u64;
    let mut active_ticks = 0u64;
    let mut next_report = *sim_start + Duration::from_secs(10);

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = tick.tick() => {}
        }

        let now = Instant::now();
        total_ticks += 1;

        if now >= next_report {
            let cpu_util_pct = (active_ticks as f64 / total_ticks as f64) * 100.0;
            tracing::info!(cpu_util_pct, "rms_scheduler CPU usage");
            crate::ui::push_log(&ui_metrics, 0, format!("rms_scheduler CPU usage {:.1}%", cpu_util_pct), &sim_start);
            if let Ok(mut m) = ui_metrics.try_lock() {
                m.cpu_util_pct = cpu_util_pct;
            }
            active_ticks = 0;
            total_ticks = 0;
            next_report = now + Duration::from_secs(10);
        }

        let mut tasks_guard = tasks.lock().await;
        // Priority sorted dispatch: Thermal (Prio 1) is always at tasks[0]
        for i in 0..tasks_guard.len() {
            if now >= tasks_guard[i].next_release {
                active_ticks += 1;
                
                let task_name = tasks_guard[i].name;
                let wcet = Duration::from_millis(tasks_guard[i].wcet_ms);
                let deadline = Duration::from_millis(tasks_guard[i].deadline_ms);
                let release_time = tasks_guard[i].next_release;
                let period = Duration::from_millis(tasks_guard[i].period_ms);

                // Advance release time immediately
                tasks_guard[i].next_release += period;
                tasks_guard[i].exec_count += 1;

                // Spawn task execution
                let ui_metrics_clone = ui_metrics.clone();
                let sim_start_clone = sim_start.clone();
                let tasks_shared = tasks.clone();
                let task_idx = i;

                tokio::spawn(async move {
                    let start_exec = Instant::now();
                    let expected_start_us = release_time.duration_since(*sim_start_clone).as_micros() as u64;
                    let actual_start_us = start_exec.duration_since(*sim_start_clone).as_micros() as u64;
                    let drift_us = actual_start_us.saturating_sub(expected_start_us);

                    tracing::info!(task=task_name, drift_us, "Task dispatched");
                    crate::ui::push_log(&ui_metrics_clone, 0, format!("Task dispatched {} drift={}us", task_name, drift_us), &sim_start_clone);

                    // Simulate workload
                    tokio::time::sleep(wcet).await;

                    let actual_finish = Instant::now();
                    let execution_time_us = actual_finish.duration_since(start_exec).as_micros() as u64;
                    let is_miss = actual_finish > (release_time + deadline);

                    let mut final_tasks = tasks_shared.lock().await;
                    if is_miss {
                        final_tasks[task_idx].miss_count += 1;
                        let violation_us = actual_finish.duration_since(release_time + deadline).as_micros() as u64;
                        tracing::warn!(task=task_name, violation_us, "DEADLINE VIOLATION");
                        crate::ui::push_log(&ui_metrics_clone, 1, format!("DEADLINE VIOLATION {} ({}us)", task_name, violation_us), &sim_start_clone);
                    }

                    if let Ok(mut m) = ui_metrics_clone.try_lock() {
                        match task_name {
                            "ThermalControl" => {
                                m.thermal_ctrl_drift_us = drift_us as i64;
                                m.thermal_ctrl_violations = final_tasks[task_idx].miss_count;
                            }
                            "DataCompress" => {
                                m.data_compress_drift_us = drift_us as i64;
                                m.data_compress_violations = final_tasks[task_idx].miss_count;
                            }
                            "HealthMonitor" => {
                                m.health_monitor_drift_us = drift_us as i64;
                                m.health_monitor_violations = final_tasks[task_idx].miss_count;
                            }
                            _ => {}
                        }
                    }
                });
            }
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}
