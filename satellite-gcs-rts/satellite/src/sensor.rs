use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use shared::packets::{TelemetryPacket, SensorId};
use shared::config::{THERMAL_PERIOD_MS, POWER_PERIOD_MS, IMU_PERIOD_MS, THERMAL_JITTER_LIMIT_US, THERMAL_MISS_ALERT};
use crate::buffer::{SensorBuffer, SensorReading};
use crate::state::SystemState;
use rand::Rng;

pub async fn run_thermal_sensor(
    buffer:     Arc<Mutex<SensorBuffer>>,
    sim_start:  Arc<Instant>,
    state:      Arc<Mutex<SystemState>>,
    cancel:     CancellationToken,
    heartbeat:  Arc<AtomicU64>,
    metrics:    Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let period = Duration::from_millis(THERMAL_PERIOD_MS);
    let mut next_deadline = *sim_start + period;
    let mut seq: u32 = 0;
    let mut consecutive_miss: u32 = 0;
    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { tracing::info!("thermal_sensor: cancelled"); break; }
            _ = tokio::time::sleep_until(next_deadline) => {}
        }

        let actual_start_us   = sim_start.elapsed().as_micros() as u64;
        let expected_start_us = seq as u64 * THERMAL_PERIOD_MS * 1000;
        let drift_us          = (actual_start_us as i64 - expected_start_us as i64).abs() as u64;
        let jitter_us         = drift_us;
        
        hist.record(jitter_us).ok();

        let value: f64 = rand::thread_rng().gen_range(20.0..80.0);

        if jitter_us > THERMAL_JITTER_LIMIT_US {
            tracing::warn!(sensor="thermal", jitter_us, limit_us=THERMAL_JITTER_LIMIT_US, elapsed_us=actual_start_us, "JITTER EXCEEDED 1ms limit");
            crate::ui::push_log(&metrics, 1, format!("JITTER EXCEEDED 1ms limit: {}us", jitter_us), &sim_start);
            if jitter_us > THERMAL_JITTER_LIMIT_US * 5 {
                consecutive_miss += 1;
            } else {
                consecutive_miss = 0;
            }
        } else {
            consecutive_miss = 0;
        }

        if consecutive_miss >= THERMAL_MISS_ALERT {
            tracing::error!(sensor="thermal", consecutive_miss, elapsed_us=actual_start_us, "SAFETY ALERT: 3 consecutive misses");
            crate::ui::push_log(&metrics, 2, format!("SAFETY ALERT: {} consecutive misses", consecutive_miss), &sim_start);
            let mut s = state.lock().await;
            if *s != SystemState::MissionAbort {
                *s = SystemState::Fault;
            }
        }

        let ts_us = sim_start.elapsed().as_micros() as u64;
        let packet = TelemetryPacket::new(seq, ts_us, SensorId::Thermal, value);

        let insert_us = sim_start.elapsed().as_micros() as u64;
        let reading = SensorReading { packet, buffer_insert_us: insert_us };
        let latency_us = insert_us - ts_us;

        {
            let mut buf = buffer.lock().await;
            let fill_pct = buf.fill_pct();
            if let Some(dropped) = buf.push(reading, &sim_start) {
                tracing::warn!(dropped_sensor=?dropped.packet.sensor_id,
                               dropped_seq=dropped.packet.seq_no, buffer_fill_pct=fill_pct, elapsed_us=insert_us,
                               "Buffer full: dropped packet");
                crate::ui::push_log(&metrics, 1, format!("Buffer full: dropped pc sq={}", dropped.packet.seq_no), &sim_start);
            }
            if buf.is_degraded() {
                tracing::warn!(buffer_fill_pct=fill_pct, elapsed_us=insert_us, "degraded mode enter/active");
                crate::ui::push_log(&metrics, 1, "Buffer degraded mode active".to_string(), &sim_start);
            }
        }

        tracing::info!(
            sensor="thermal", seq, value, drift_us, jitter_us, latency_us, elapsed_us=actual_start_us,
            "sensor_read"
        );
        crate::ui::push_log(&metrics, 0, format!("sensor_read seq={} val={:.2}", seq, value), &sim_start);

        if let Ok(mut m) = metrics.try_lock() {
            m.thermal_jitter_last_us = jitter_us;
            m.thermal_jitter_p50_us = hist.value_at_percentile(50.0);
            m.thermal_jitter_p99_us = hist.value_at_percentile(99.0);
            m.thermal_jitter_max_us = hist.max();
            m.thermal_consecutive_miss = consecutive_miss;
            
            if let Ok(s) = state.try_lock() { m.system_state = format!("{:?}", *s).to_uppercase(); }
            if let Ok(b) = buffer.try_lock() {
                m.buffer_len = b.len();
                m.buffer_fill_pct = b.fill_pct() * 100.0;
                m.buffer_total_dropped = b.stats.total_dropped;
                m.buffer_degraded = b.is_degraded();
            }
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
        seq += 1;
        next_deadline += period;
    }

    tracing::info!(sensor="thermal", p50=hist.value_at_percentile(50.0),
                   p99=hist.value_at_percentile(99.0), max=hist.max(),
                   "thermal_sensor final jitter stats");
    crate::ui::push_log(&metrics, 0, "thermal_sensor finished".to_string(), &sim_start);
}

pub async fn run_power_sensor(
    buffer:     Arc<Mutex<SensorBuffer>>,
    sim_start:  Arc<Instant>,
    _state:      Arc<Mutex<SystemState>>,
    cancel:     CancellationToken,
    heartbeat:  Arc<AtomicU64>,
    metrics:    Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let period = Duration::from_millis(POWER_PERIOD_MS);
    let mut next_deadline = *sim_start + period;
    let mut seq: u32 = 0;
    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { break; }
            _ = tokio::time::sleep_until(next_deadline) => {}
        }

        let actual_start_us   = sim_start.elapsed().as_micros() as u64;
        let expected_start_us = seq as u64 * POWER_PERIOD_MS * 1000;
        let drift_us          = (actual_start_us as i64 - expected_start_us as i64).abs() as u64;
        let jitter_us         = drift_us;
        hist.record(jitter_us).ok();

        let value: f64 = rand::thread_rng().gen_range(0.5..5.0);
        let ts_us = sim_start.elapsed().as_micros() as u64;
        let packet = TelemetryPacket::new(seq, ts_us, SensorId::Power, value);

        let insert_us = sim_start.elapsed().as_micros() as u64;
        let reading = SensorReading { packet, buffer_insert_us: insert_us };
        let latency_us = insert_us - ts_us;

        {
            let mut buf = buffer.lock().await;
            buf.push(reading, &sim_start);
        }

        tracing::info!(
            sensor="power", seq, value, drift_us, jitter_us, latency_us, elapsed_us=actual_start_us,
            "sensor_read"
        );
        // Omitted push_log for power read to prevent log spam, but wait, prompt said "every task that currently calls tracing::info!". 
        crate::ui::push_log(&metrics, 0, format!("Power read seq={}", seq), &sim_start);

        if let Ok(mut m) = metrics.try_lock() {
            m.power_jitter_last_us = jitter_us;
            m.power_jitter_p50_us = hist.value_at_percentile(50.0);
            m.power_jitter_p99_us = hist.value_at_percentile(99.0);
            m.power_jitter_max_us = hist.max();
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
        seq += 1;
        next_deadline += period;
    }
}

pub async fn run_imu_sensor(
    buffer:     Arc<Mutex<SensorBuffer>>,
    sim_start:  Arc<Instant>,
    _state:      Arc<Mutex<SystemState>>,
    cancel:     CancellationToken,
    heartbeat:  Arc<AtomicU64>,
    metrics:    Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let period = Duration::from_millis(IMU_PERIOD_MS);
    let mut next_deadline = *sim_start + period;
    let mut seq: u32 = 0;
    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { break; }
            _ = tokio::time::sleep_until(next_deadline) => {}
        }

        let actual_start_us   = sim_start.elapsed().as_micros() as u64;
        let expected_start_us = seq as u64 * IMU_PERIOD_MS * 1000;
        let drift_us          = (actual_start_us as i64 - expected_start_us as i64).abs() as u64;
        let jitter_us         = drift_us;
        hist.record(jitter_us).ok();

        let value: f64 = rand::thread_rng().gen_range(-0.1..0.1);
        let ts_us = sim_start.elapsed().as_micros() as u64;
        let packet = TelemetryPacket::new(seq, ts_us, SensorId::Imu, value);

        let insert_us = sim_start.elapsed().as_micros() as u64;
        let reading = SensorReading { packet, buffer_insert_us: insert_us };
        let latency_us = insert_us - ts_us;

        {
            let mut buf = buffer.lock().await;
            buf.push(reading, &sim_start);
        }

        tracing::info!(
            sensor="imu", seq, value, drift_us, jitter_us, latency_us, elapsed_us=actual_start_us,
            "sensor_read"
        );
        crate::ui::push_log(&metrics, 0, format!("IMU read seq={}", seq), &sim_start);

        if let Ok(mut m) = metrics.try_lock() {
            m.imu_jitter_last_us = jitter_us;
            m.imu_jitter_p50_us = hist.value_at_percentile(50.0);
            m.imu_jitter_p99_us = hist.value_at_percentile(99.0);
            m.imu_jitter_max_us = hist.max();
        }

        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
        seq += 1;
        next_deadline += period;
    }
}
