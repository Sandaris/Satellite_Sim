use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::time::{Duration, Instant};
use shared::config::{THERMAL_CTRL_PERIOD_MS, DATA_COMPRESS_PERIOD_MS, HEALTH_MONITOR_PERIOD_MS};

#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub name:        &'static str,
    pub period_ms:   u64,
    pub wcet_ms:     u64,
    pub deadline_ms: u64,
    pub next_release: Instant,
    pub exec_count:  u64,
    pub miss_count:  u64,
}

pub async fn run_rms_scheduler(
    mut cancel: tokio::sync::watch::Receiver<bool>,
    heartbeat: Arc<AtomicU64>,
    sim_start: Arc<Instant>,
    ui_metrics: Arc<tokio::sync::Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let task_start = Instant::now();
    
    // In Rate Monotonic Scheduling (RMS), highest frequency has highest priority.
    // 1. ThermalControl (fastest)
    // 2. DataCompress 
    // 3. HealthMonitor (slowest)
    // We execute them sequentially to simulate real CPU sharing.
    let mut tasks = vec![
        ScheduledTask {
            name: "ThermalControl",
            period_ms: THERMAL_CTRL_PERIOD_MS,
            wcet_ms: 5,
            deadline_ms: THERMAL_CTRL_PERIOD_MS,
            next_release: task_start + Duration::from_millis(THERMAL_CTRL_PERIOD_MS),
            exec_count: 0, miss_count: 0,
        },
        ScheduledTask {
            name: "DataCompress",
            period_ms: DATA_COMPRESS_PERIOD_MS,
            wcet_ms: 20,
            deadline_ms: DATA_COMPRESS_PERIOD_MS,
            next_release: task_start + Duration::from_millis(DATA_COMPRESS_PERIOD_MS),
            exec_count: 0, miss_count: 0,
        },
        ScheduledTask {
            name: "HealthMonitor",
            period_ms: HEALTH_MONITOR_PERIOD_MS,
            wcet_ms: 50,
            deadline_ms: HEALTH_MONITOR_PERIOD_MS,
            next_release: task_start + Duration::from_millis(HEALTH_MONITOR_PERIOD_MS),
            exec_count: 0, miss_count: 0,
        },
    ];

    let mut tick = tokio::time::interval(Duration::from_millis(5)); // smaller tick for better resolution
    let mut total_active_us = 0u64;
    let mut interval_start = Instant::now();

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = tick.tick() => {}
        }

        let mut now = Instant::now();

        // 10-second utilization reporting
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

        // Find the highest priority task (lowest index) that is ready to run
        let mut task_to_run = None;
        for i in 0..tasks.len() {
            if now >= tasks[i].next_release {
                task_to_run = Some(i);
                break; // Highest priority task found
            }
        }

        if let Some(i) = task_to_run {
            let task_name = tasks[i].name;
            let wcet = Duration::from_millis(tasks[i].wcet_ms);
            let release_time = tasks[i].next_release;
            let period = Duration::from_millis(tasks[i].period_ms);
            let deadline = release_time + Duration::from_millis(tasks[i].deadline_ms);

            let expected_start_us = release_time.duration_since(*sim_start).as_micros() as u64;
            let actual_start_us = now.duration_since(*sim_start).as_micros() as u64;
            let drift_us = actual_start_us.saturating_sub(expected_start_us);

            tracing::info!(task=task_name, drift_us, "Task dispatched");
            crate::ui::push_log(&ui_metrics, 0, format!("Task dispatched {} drift={}us", task_name, drift_us), &sim_start);

            // Execute the task sequentially (Simulated CPU execution blocking)
            tokio::time::sleep(wcet).await;
            
            now = Instant::now(); // Update now after execution
            total_active_us += wcet.as_micros() as u64;
            
            // Advance release time
            tasks[i].next_release += period;
            tasks[i].exec_count += 1;

            let is_miss = now > deadline;
            if is_miss {
                tasks[i].miss_count += 1;
                let violation_us = now.duration_since(deadline).as_micros() as u64;
                tracing::warn!(task=task_name, violation_us, "DEADLINE VIOLATION");
                crate::ui::push_log(&ui_metrics, 1, format!("DEADLINE VIOLATION {} ({}us)", task_name, violation_us), &sim_start);
            }

            if let Ok(mut m) = ui_metrics.try_lock() {
                match task_name {
                    "ThermalControl" => {
                        m.thermal_ctrl_drift_us = drift_us as i64;
                        m.thermal_ctrl_violations = tasks[i].miss_count;
                    }
                    "DataCompress" => {
                        m.data_compress_drift_us = drift_us as i64;
                        m.data_compress_violations = tasks[i].miss_count;
                    }
                    "HealthMonitor" => {
                        m.health_monitor_drift_us = drift_us as i64;
                        m.health_monitor_violations = tasks[i].miss_count;
                    }
                    _ => {}
                }
            }
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}
