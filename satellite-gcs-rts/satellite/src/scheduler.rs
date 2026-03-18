use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
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
}

pub async fn run_rms_scheduler(
    cancel: CancellationToken,
    heartbeat: Arc<AtomicU64>,
    sim_start: Arc<Instant>,
    ui_metrics: Arc<tokio::sync::Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let mut tasks = vec![
        ScheduledTask {
            name: "ThermalControl",
            priority: 1,
            period_ms: THERMAL_CTRL_PERIOD_MS,
            wcet_ms: 5,
            deadline_ms: THERMAL_CTRL_PERIOD_MS,
            next_release: *sim_start + Duration::from_millis(THERMAL_CTRL_PERIOD_MS),
            exec_count: 0, miss_count: 0,
        },
        ScheduledTask {
            name: "DataCompress",
            priority: 2,
            period_ms: DATA_COMPRESS_PERIOD_MS,
            wcet_ms: 20,
            deadline_ms: DATA_COMPRESS_PERIOD_MS,
            next_release: *sim_start + Duration::from_millis(DATA_COMPRESS_PERIOD_MS),
            exec_count: 0, miss_count: 0,
        },
        ScheduledTask {
            name: "HealthMonitor",
            priority: 3,
            period_ms: HEALTH_MONITOR_PERIOD_MS,
            wcet_ms: 50,
            deadline_ms: HEALTH_MONITOR_PERIOD_MS,
            next_release: *sim_start + Duration::from_millis(HEALTH_MONITOR_PERIOD_MS),
            exec_count: 0, miss_count: 0,
        },
    ];

    let mut tick = tokio::time::interval(Duration::from_millis(10));
    let mut active_ticks = 0u64;
    let mut total_ticks = 0u64;

    let mut next_report = *sim_start + Duration::from_secs(10);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
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

        let mut ready_idx = None;
        let mut highest_pri = u8::MAX;

        for (i, t) in tasks.iter().enumerate() {
            if now >= t.next_release {
                if t.priority < highest_pri {
                    highest_pri = t.priority;
                    ready_idx = Some(i);
                }
            }
        }

        if let Some(idx) = ready_idx {
            active_ticks += 1;
            let expected_start_us = tasks[idx].next_release.duration_since(*sim_start).as_micros() as u64;
            let actual_start_us = now.duration_since(*sim_start).as_micros() as u64;

            let wcet = Duration::from_millis(tasks[idx].wcet_ms);
            
            // Preemption check for DataCompress and HealthMonitor if ThermalCtrl becomes ready
            let thermal_release = tasks[0].next_release;
            
            if idx > 0 && now + wcet > thermal_release {
                let time_until_thermal = thermal_release.saturating_duration_since(now);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(time_until_thermal) => {
                        // Preempted by ThermalControl
                    }
                }
                continue; // Skip advancing next_release for preempted task
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(wcet) => {}
                }
            }

            let actual_finish = Instant::now();
            let execution_time_us = actual_finish.duration_since(now).as_micros() as u64;

            let metrics = TaskMetrics {
                task_name: tasks[idx].name.to_string(),
                expected_start_us,
                actual_start_us,
                execution_time_us,
                deadline_us: expected_start_us + (tasks[idx].deadline_ms * 1000),
                deadline_missed: actual_finish > tasks[idx].next_release + Duration::from_millis(tasks[idx].deadline_ms),
            };

            let drift_us = metrics.scheduling_drift_us();
            tracing::info!(task=tasks[idx].name, drift_us, elapsed_us=actual_finish.duration_since(*sim_start).as_micros() as u64, "Task scheduled");
            crate::ui::push_log(&ui_metrics, 0, format!("Task scheduled {} drift={}us", tasks[idx].name, drift_us), &sim_start);

            let violation_us = metrics.deadline_violation_us();
            if let Some(v) = violation_us {
                tasks[idx].miss_count += 1;
                tracing::warn!(task=tasks[idx].name, violation_us=v, elapsed_us=actual_start_us, "DEADLINE VIOLATION");
                crate::ui::push_log(&ui_metrics, 1, format!("DEADLINE VIOLATION {} ({}us)", tasks[idx].name, v), &sim_start);
            }

            if let Ok(mut m) = ui_metrics.try_lock() {
                match tasks[idx].name {
                    "ThermalControl" => {
                        m.thermal_ctrl_drift_us = drift_us as i64;
                        m.thermal_ctrl_violations = tasks[idx].miss_count;
                    }
                    "DataCompress" => {
                        m.data_compress_drift_us = drift_us as i64;
                        m.data_compress_violations = tasks[idx].miss_count;
                    }
                    "HealthMonitor" => {
                        m.health_monitor_drift_us = drift_us as i64;
                        m.health_monitor_violations = tasks[idx].miss_count;
                    }
                    _ => {}
                }
            }

            tasks[idx].exec_count += 1;
            let period_ms = tasks[idx].period_ms;
            tasks[idx].next_release += Duration::from_millis(period_ms);
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}
