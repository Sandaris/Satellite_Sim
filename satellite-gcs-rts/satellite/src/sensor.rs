use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use shared::packets::{TelemetryPacket, SensorId};
use shared::config::{
    THERMAL_PERIOD_MS, POWER_PERIOD_MS, IMU_PERIOD_MS, THERMAL_JITTER_LIMIT_US, POWER_JITTER_LIMIT_US,
    IMU_JITTER_LIMIT_US, THERMAL_MISS_ALERT,
};
use crate::buffer::{SensorBuffer, SensorReading};
use crate::state::SystemState;
use rand::Rng;

#[cfg(windows)]
fn elevate_timing_thread_priority() {
    unsafe {
        let thread = windows_sys::Win32::System::Threading::GetCurrentThread();
        let _ = windows_sys::Win32::System::Threading::SetThreadPriority(
            thread,
            windows_sys::Win32::System::Threading::THREAD_PRIORITY_TIME_CRITICAL,
        );
    }
}

#[cfg(not(windows))]
fn elevate_timing_thread_priority() {}

async fn push_with_buffer_logging(
    buffer: &Arc<Mutex<SensorBuffer>>,
    metrics: &Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
    sim_start: &Arc<Instant>,
    reading: SensorReading,
) {
    let insert_us = reading.buffer_insert_us;
    let (dropped_packet, fill_pct, degraded) = {
        let mut buf = buffer.lock().await;
        let fill_pct = buf.fill_pct();
        let dropped_packet = buf.push(reading, sim_start);
        let degraded = buf.is_degraded();
        (dropped_packet, fill_pct, degraded)
    };

    if let Some(dropped) = dropped_packet {
        tracing::warn!(
            dropped_sensor=?dropped.packet.sensor_id,
            dropped_seq=dropped.packet.seq_no,
            buffer_fill_pct=fill_pct,
            elapsed_us=insert_us,
            "Buffer full: dropped packet"
        );
        crate::ui::push_log(
            metrics,
            1,
            format!("Buffer full: dropped {:?} seq={}", dropped.packet.sensor_id, dropped.packet.seq_no),
            sim_start,
        );
    }

    if degraded {
        tracing::warn!(
            buffer_fill_pct=fill_pct,
            elapsed_us=insert_us,
            "degraded mode enter/active"
        );
        crate::ui::push_log(metrics, 1, "Buffer degraded mode active".to_string(), sim_start);
    }

    if let Ok(mut m) = metrics.try_lock() {
        if let Ok(b) = buffer.try_lock() {
            m.buffer_len = b.len();
            m.buffer_fill_pct = b.fill_pct() * 100.0;
            m.buffer_total_dropped = b.stats.total_dropped;
            m.buffer_degraded = b.is_degraded();
        }
    }
}

pub async fn run_thermal_sensor(
    buffer:     Arc<Mutex<SensorBuffer>>,
    sim_start:  Arc<Instant>,
    state:      Arc<Mutex<SystemState>>,
    mut cancel:      tokio::sync::watch::Receiver<bool>,
    heartbeat:  Arc<AtomicU64>,
    metrics:    Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
    corrupt_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    struct ThermalSample {
        actual_start_us: u64,
        drift_us: u64,
        jitter_us: u64,
        packet: TelemetryPacket,
    }

    let period = Duration::from_millis(THERMAL_PERIOD_MS);
    let period_us = THERMAL_PERIOD_MS * 1000;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ThermalSample>(256);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let sim_start_thread = sim_start.clone();
    let heartbeat_thread = heartbeat.clone();
    let corrupt_flag_thread = corrupt_flag.clone();

    tokio::task::spawn_blocking(move || {
        elevate_timing_thread_priority();
        let spin_guard = Duration::from_millis(5);
        let task_start = Instant::now();
        let startup_offset_us = task_start.duration_since(*sim_start_thread).as_micros() as u64;
        let mut next_deadline = task_start + period;
        let mut seq: u32 = 0;
        let mut prev_actual_start_us: Option<u64> = None;

        while !stop_thread.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now + spin_guard < next_deadline {
                std::thread::sleep(next_deadline - now - spin_guard);
            }
            while Instant::now() < next_deadline {
                std::hint::spin_loop();
            }
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }

            let actual_start_us = sim_start_thread.elapsed().as_micros() as u64;
            let expected_start_us = startup_offset_us + ((seq + 1) as u64 * period_us);
            let drift_us = (actual_start_us as i64 - expected_start_us as i64).unsigned_abs();
            let actual_interval = match prev_actual_start_us {
                Some(prev) => actual_start_us.saturating_sub(prev),
                None => period_us,
            };
            let jitter_us = actual_interval.abs_diff(period_us);

            let value: f64 = rand::thread_rng().gen_range(20.0..80.0);
            let ts_us = sim_start_thread.elapsed().as_micros() as u64;
            let mut packet = TelemetryPacket::new(seq, ts_us, SensorId::Thermal, value);
            if corrupt_flag_thread.load(Ordering::Relaxed) {
                packet.is_corrupted = true;
                packet.value = f64::NAN;
                packet.payload[0] = 0xFF;
            }

            if tx.blocking_send(ThermalSample { actual_start_us, drift_us, jitter_us, packet }).is_err() {
                break;
            }
            heartbeat_thread.store(sim_start_thread.elapsed().as_secs(), Ordering::Relaxed);
            prev_actual_start_us = Some(actual_start_us);
            seq = seq.wrapping_add(1);
            next_deadline += period;
        }
    });

    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();
    let mut consecutive_miss: u32 = 0;
    loop {
        tokio::select! {
            _ = cancel.changed() => {
                stop.store(true, Ordering::Relaxed);
                tracing::info!("thermal_sensor: cancelled");
                break;
            }
            maybe_sample = rx.recv() => {
                let sample = match maybe_sample { Some(s) => s, None => break };
                hist.record(sample.jitter_us).ok();

                if sample.jitter_us > THERMAL_JITTER_LIMIT_US {
                    consecutive_miss += 1;
                    tracing::warn!(sensor="thermal", jitter_us=sample.jitter_us, consecutive_miss,
                                   limit_us=THERMAL_JITTER_LIMIT_US, elapsed_us=sample.actual_start_us,
                                   "JITTER EXCEEDED thermal limit");
                    crate::ui::push_log(&metrics, 1,
                        format!("THERMAL JITTER EXCEEDED: {}us > {}us (miss #{})", sample.jitter_us, THERMAL_JITTER_LIMIT_US, consecutive_miss),
                        &sim_start);
                } else {
                    consecutive_miss = 0;
                }

                if consecutive_miss >= THERMAL_MISS_ALERT {
                    tracing::error!(sensor="thermal", consecutive_miss, elapsed_us=sample.actual_start_us, "SAFETY ALERT: TIMING FAILURE");
                    let mut s = state.lock().await;
                    if *s != SystemState::MissionAbort {
                        *s = SystemState::Fault;
                    }
                }

                let insert_us = sim_start.elapsed().as_micros() as u64;
                let reading = SensorReading { packet: sample.packet, buffer_insert_us: insert_us };
                let latency_us = insert_us.saturating_sub(reading.packet.sample_timestamp_us);
                push_with_buffer_logging(&buffer, &metrics, &sim_start, reading.clone()).await;

                tracing::info!(
                    sensor="thermal", seq=reading.packet.seq_no, value=reading.packet.value,
                    drift_us=sample.drift_us, jitter_us=sample.jitter_us, latency_us, elapsed_us=sample.actual_start_us,
                    "sensor_read"
                );
                crate::ui::push_log(&metrics, 0, format!("sensor_read seq={} val={:.2}", reading.packet.seq_no, reading.packet.value), &sim_start);

                if let Ok(mut m) = metrics.try_lock() {
                    m.thermal_jitter_last_us = sample.jitter_us;
                    m.thermal_jitter_p50_us = hist.value_at_percentile(50.0);
                    m.thermal_jitter_p99_us = hist.value_at_percentile(99.0);
                    m.thermal_jitter_max_us = hist.max();
                    m.thermal_consecutive_miss = consecutive_miss;
                    if let Ok(s) = state.try_lock() { m.system_state = format!("{:?}", *s).to_uppercase(); }
                }
            }
        }
    }

    tracing::info!(sensor="thermal", p50=hist.value_at_percentile(50.0),
                   p99=hist.value_at_percentile(99.0), max=hist.max(),
                   "thermal_sensor final jitter stats");
    crate::ui::push_log(&metrics, 0, "thermal_sensor finished".to_string(), &sim_start);
}

pub async fn run_power_sensor(
    buffer:     Arc<Mutex<SensorBuffer>>,
    sim_start:  Arc<Instant>,
    state:      Arc<Mutex<SystemState>>,
    mut cancel:      tokio::sync::watch::Receiver<bool>,
    heartbeat:  Arc<AtomicU64>,
    metrics:    Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
    corrupt_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    struct PowerSample {
        actual_start_us: u64,
        drift_us: u64,
        jitter_us: u64,
        packet: TelemetryPacket,
    }

    let period = Duration::from_millis(POWER_PERIOD_MS);
    let period_us = POWER_PERIOD_MS * 1000;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PowerSample>(256);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let sim_start_thread = sim_start.clone();
    let heartbeat_thread = heartbeat.clone();
    let corrupt_flag_thread = corrupt_flag.clone();

    tokio::task::spawn_blocking(move || {
        elevate_timing_thread_priority();
        let spin_guard = Duration::from_millis(2);
        let task_start = Instant::now();
        let startup_offset_us = task_start.duration_since(*sim_start_thread).as_micros() as u64;
        let mut next_deadline = task_start + period;
        let mut seq: u32 = 0;
        let mut prev_actual_start_us: Option<u64> = None;

        while !stop_thread.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now + spin_guard < next_deadline {
                std::thread::sleep(next_deadline - now - spin_guard);
            }
            while Instant::now() < next_deadline {
                std::hint::spin_loop();
            }
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }

            let actual_start_us = sim_start_thread.elapsed().as_micros() as u64;
            let expected_start_us = startup_offset_us + ((seq + 1) as u64 * period_us);
            let drift_us = (actual_start_us as i64 - expected_start_us as i64).unsigned_abs();
            let actual_interval = match prev_actual_start_us {
                Some(prev) => actual_start_us.saturating_sub(prev),
                None => period_us,
            };
            let jitter_us = actual_interval.abs_diff(period_us);

            let value: f64 = rand::thread_rng().gen_range(0.5..5.0);
            let ts_us = sim_start_thread.elapsed().as_micros() as u64;
            let mut packet = TelemetryPacket::new(seq, ts_us, SensorId::Power, value);
            if corrupt_flag_thread.load(Ordering::Relaxed) {
                packet.is_corrupted = true;
            }

            if tx.blocking_send(PowerSample { actual_start_us, drift_us, jitter_us, packet }).is_err() {
                break;
            }
            heartbeat_thread.store(sim_start_thread.elapsed().as_secs(), Ordering::Relaxed);
            prev_actual_start_us = Some(actual_start_us);
            seq = seq.wrapping_add(1);
            next_deadline += period;
        }
    });

    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();
    loop {
        tokio::select! {
            _ = cancel.changed() => {
                stop.store(true, Ordering::Relaxed);
                break;
            }
            maybe_sample = rx.recv() => {
                let sample = match maybe_sample { Some(s) => s, None => break };
                hist.record(sample.jitter_us).ok();

                let insert_us = sim_start.elapsed().as_micros() as u64;
                let latency_us = insert_us.saturating_sub(sample.packet.sample_timestamp_us);
                let reading = SensorReading { packet: sample.packet, buffer_insert_us: insert_us };
                let seq = reading.packet.seq_no;
                let value = reading.packet.value;

                push_with_buffer_logging(&buffer, &metrics, &sim_start, reading).await;

                if sample.jitter_us > POWER_JITTER_LIMIT_US {
                    tracing::error!(sensor="power", jitter_us=sample.jitter_us, limit_us=POWER_JITTER_LIMIT_US, "POWER JITTER LIMIT EXCEEDED");
                    crate::ui::push_log(&metrics, 2, format!("POWER JITTER: {}us > {}us", sample.jitter_us, POWER_JITTER_LIMIT_US), &sim_start);
                }

                tracing::info!(
                    sensor="power", seq, value, drift_us=sample.drift_us, jitter_us=sample.jitter_us, latency_us, elapsed_us=sample.actual_start_us,
                    "sensor_read"
                );
                crate::ui::push_log(&metrics, 0, format!("Power read seq={}", seq), &sim_start);

                if let Ok(mut m) = metrics.try_lock() {
                    m.power_jitter_last_us = sample.jitter_us;
                    m.power_jitter_p50_us = hist.value_at_percentile(50.0);
                    m.power_jitter_p99_us = hist.value_at_percentile(99.0);
                    m.power_jitter_max_us = hist.max();
                    if let Ok(s) = state.try_lock() { m.system_state = format!("{:?}", *s).to_uppercase(); }
                }
            }
        }
    }
}

pub async fn run_imu_sensor(
    buffer:     Arc<Mutex<SensorBuffer>>,
    sim_start:  Arc<Instant>,
    state:      Arc<Mutex<SystemState>>,
    mut cancel:      tokio::sync::watch::Receiver<bool>,
    heartbeat:  Arc<AtomicU64>,
    metrics:    Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
    corrupt_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    struct ImuSample {
        actual_start_us: u64,
        drift_us: u64,
        jitter_us: u64,
        packet: TelemetryPacket,
    }

    let period = Duration::from_millis(IMU_PERIOD_MS);
    let period_us = IMU_PERIOD_MS * 1000;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ImuSample>(256);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let sim_start_thread = sim_start.clone();
    let heartbeat_thread = heartbeat.clone();
    let corrupt_flag_thread = corrupt_flag.clone();

    tokio::task::spawn_blocking(move || {
        elevate_timing_thread_priority();
        let spin_guard = Duration::from_millis(2);
        let task_start = Instant::now();
        let startup_offset_us = task_start.duration_since(*sim_start_thread).as_micros() as u64;
        let mut next_deadline = task_start + period;
        let mut seq: u32 = 0;
        let mut prev_actual_start_us: Option<u64> = None;

        while !stop_thread.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now + spin_guard < next_deadline {
                std::thread::sleep(next_deadline - now - spin_guard);
            }
            while Instant::now() < next_deadline {
                std::hint::spin_loop();
            }
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }

            let actual_start_us = sim_start_thread.elapsed().as_micros() as u64;
            let expected_start_us = startup_offset_us + ((seq + 1) as u64 * period_us);
            let drift_us = (actual_start_us as i64 - expected_start_us as i64).unsigned_abs();
            let actual_interval = match prev_actual_start_us {
                Some(prev) => actual_start_us.saturating_sub(prev),
                None => period_us,
            };
            let jitter_us = actual_interval.abs_diff(period_us);

            let value: f64 = rand::thread_rng().gen_range(-0.1..0.1);
            let ts_us = sim_start_thread.elapsed().as_micros() as u64;
            let mut packet = TelemetryPacket::new(seq, ts_us, SensorId::Imu, value);
            if corrupt_flag_thread.load(Ordering::Relaxed) {
                packet.is_corrupted = true;
            }

            if tx.blocking_send(ImuSample { actual_start_us, drift_us, jitter_us, packet }).is_err() {
                break;
            }
            heartbeat_thread.store(sim_start_thread.elapsed().as_secs(), Ordering::Relaxed);
            prev_actual_start_us = Some(actual_start_us);
            seq = seq.wrapping_add(1);
            next_deadline += period;
        }
    });

    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();
    loop {
        tokio::select! {
            _ = cancel.changed() => {
                stop.store(true, Ordering::Relaxed);
                break;
            }
            maybe_sample = rx.recv() => {
                let sample = match maybe_sample { Some(s) => s, None => break };
                hist.record(sample.jitter_us).ok();

                let insert_us = sim_start.elapsed().as_micros() as u64;
                let latency_us = insert_us.saturating_sub(sample.packet.sample_timestamp_us);
                let reading = SensorReading { packet: sample.packet, buffer_insert_us: insert_us };
                let seq = reading.packet.seq_no;
                let value = reading.packet.value;

                push_with_buffer_logging(&buffer, &metrics, &sim_start, reading).await;

                if sample.jitter_us > IMU_JITTER_LIMIT_US {
                    tracing::error!(sensor="imu", jitter_us=sample.jitter_us, limit_us=IMU_JITTER_LIMIT_US, "IMU JITTER LIMIT EXCEEDED");
                    crate::ui::push_log(&metrics, 2, format!("IMU JITTER: {}us > {}us", sample.jitter_us, IMU_JITTER_LIMIT_US), &sim_start);
                }

                tracing::info!(
                    sensor="imu", seq, value, drift_us=sample.drift_us, jitter_us=sample.jitter_us, latency_us, elapsed_us=sample.actual_start_us,
                    "sensor_read"
                );
                crate::ui::push_log(&metrics, 0, format!("IMU read seq={}", seq), &sim_start);

                if let Ok(mut m) = metrics.try_lock() {
                    m.imu_jitter_last_us = sample.jitter_us;
                    m.imu_jitter_p50_us = hist.value_at_percentile(50.0);
                    m.imu_jitter_p99_us = hist.value_at_percentile(99.0);
                    m.imu_jitter_max_us = hist.max();
                    if let Ok(s) = state.try_lock() { m.system_state = format!("{:?}", *s).to_uppercase(); }
                }
            }
        }
    }
}
