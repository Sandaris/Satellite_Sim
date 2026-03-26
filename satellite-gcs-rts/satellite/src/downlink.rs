use std::collections::VecDeque;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use shared::packets::{FaultPacket, PacketType, TelemetryPacket};
use crate::buffer::{SensorBuffer, SensorReading};
use crate::state::SystemState;
use crate::telemetry_cache::TelemetryCache;

use tokio_util::codec::{FramedWrite, LengthDelimitedCodec};
use futures::SinkExt;
use tokio::net::tcp::OwnedWriteHalf;
use bytes::Bytes;
use std::time::{SystemTime, UNIX_EPOCH};

fn wall_clock_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn exercise_gcs_edges() -> bool {
    std::env::var("SAT_SIM_EXERCISE_GCS_EDGE_CASES")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub async fn run_downlink_tx(
    buffer:          Arc<Mutex<SensorBuffer>>,
    telemetry_cache: Arc<Mutex<TelemetryCache>>,
    retransmit_q:    Arc<Mutex<VecDeque<TelemetryPacket>>>,
    writer:          OwnedWriteHalf,
    sim_start:       Arc<Instant>,
    _state:          Arc<Mutex<SystemState>>,
    mut cancel:      tokio::sync::watch::Receiver<bool>,
    heartbeat:       Arc<AtomicU64>,
    mut fault_rx:    tokio::sync::mpsc::Receiver<FaultPacket>,
    ui_metrics:      Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    // Spec: downlink must be ready to send within 5ms of TCP connection (task start here).
    let init_start = Instant::now();

    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(1024);
    let mut framed_writer = FramedWrite::new(writer, codec.new_codec());

    let init_limit = Duration::from_millis(shared::config::DOWNLINK_INIT_TIMEOUT_MS);
    let init_elapsed = init_start.elapsed();
    let init_elapsed_ms = init_elapsed.as_millis() as u64;
    if init_elapsed > init_limit {
        tracing::warn!(
            init_elapsed_ms,
            init_elapsed_us = init_elapsed.as_micros() as u64,
            limit_ms = shared::config::DOWNLINK_INIT_TIMEOUT_MS,
            "DOWNLINK INIT TIMEOUT: pipeline not ready within 5ms — missed communication window"
        );
        crate::ui::push_log(
            &ui_metrics,
            1,
            format!(
                "DOWNLINK INIT TIMEOUT: {:.3}ms (limit {}ms) — missed communication window",
                init_elapsed.as_secs_f64() * 1000.0,
                shared::config::DOWNLINK_INIT_TIMEOUT_MS
            ),
            &sim_start,
        );
        if let Ok(mut m) = ui_metrics.try_lock() {
            m.downlink_window_violations += 1;
        }
    } else {
        tracing::info!(
            init_elapsed_ms,
            init_elapsed_us = init_elapsed.as_micros() as u64,
            "downlink_tx: pipeline initialized within 5ms window"
        );
    }

    let mut interval = tokio::time::interval(Duration::from_millis(shared::config::DOWNLINK_WINDOW_MS));
    let mut last_abort_warn = Instant::now() - Duration::from_secs(60);
    let mut tx_log_seq: u32 = 0;
    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();
    let exercise_mode = exercise_gcs_edges();
    let mut blackout_done = false;

    loop {
        tokio::select! {
            _ = cancel.changed() => { break; }
            _ = interval.tick() => {}
        }

        let elapsed_us = sim_start.elapsed().as_micros() as u64;
        let window_start = Instant::now();

        if exercise_mode && !blackout_done && sim_start.elapsed().as_secs() >= 90 {
            blackout_done = true;
            tracing::warn!(
                elapsed_us,
                blackout_s=12,
                "EXERCISE: telemetry blackout start to trigger GCS loss of contact"
            );
            crate::ui::push_log(
                &ui_metrics,
                1,
                "EXERCISE: telemetry blackout start".to_string(),
                &sim_start,
            );
            tokio::time::sleep(Duration::from_secs(12)).await;
            tracing::warn!(
                elapsed_us=sim_start.elapsed().as_micros() as u64,
                "EXERCISE: telemetry blackout end"
            );
            crate::ui::push_log(
                &ui_metrics,
                1,
                "EXERCISE: telemetry blackout end".to_string(),
                &sim_start,
            );
        }

        // Forward fault packets preferentially (Always allowed)
        while let Ok(fault) = fault_rx.try_recv() {
            if let Ok(bytes) = bincode::serialize(&fault) {
                let mut prefixed = Vec::with_capacity(bytes.len() + 1);
                prefixed.push(PacketType::FaultNotify as u8);
                prefixed.extend_from_slice(&bytes);
                let _ = framed_writer.send(Bytes::from(prefixed)).await;
            }
        }

        let (degraded, abort) = {
            let s = _state.lock().await;
            let d = buffer.lock().await.is_degraded();
            (d, *s == SystemState::MissionAbort)
        };

        if abort {
            if last_abort_warn.elapsed() > Duration::from_secs(5) {
                tracing::warn!("Satellite in MISSION ABORT - halting non-fault telemetry");
                crate::ui::push_log(&ui_metrics, 1, "MISSION ABORT - telemetry halted".to_string(), &sim_start);
                last_abort_warn = Instant::now();
            }
        } else {
            if degraded {
                let mut s = _state.lock().await;
                if *s == SystemState::Nominal { *s = SystemState::Degraded; }
            } else {
                let mut s = _state.lock().await;
                if *s == SystemState::Degraded { *s = SystemState::Nominal; }
            }
        }
        
        for _ in 0..10 {
            if abort { break; } // Respect MissionAbort

            let (reading, from_retransmit) = {
                let mut rq = retransmit_q.lock().await;
                if let Some(mut pkt) = rq.pop_front() {
                    pkt.timestamp_us = wall_clock_us();
                    (
                        SensorReading {
                            packet: pkt,
                            buffer_insert_us: sim_start.elapsed().as_micros() as u64,
                        },
                        true,
                    )
                } else {
                    match buffer.lock().await.pop() {
                        Some(r) => (r, false),
                        None => break,
                    }
                }
            };

            if degraded && reading.packet.priority > 1 { continue; }

            let mut pkt = reading.packet;
            pkt.timestamp_us = wall_clock_us();

            let bytes = match bincode::serialize(&pkt) {
                Ok(b)  => b,
                Err(e) => { tracing::error!("Serialize failed: {}", e); continue; }
            };
            
            let mut prefixed = Vec::with_capacity(bytes.len() + 1);
            prefixed.push(PacketType::SensorData as u8);
            prefixed.extend_from_slice(&bytes);

            let send_result = tokio::time::timeout(
                Duration::from_millis(shared::config::DOWNLINK_WINDOW_MS),
                framed_writer.send(Bytes::from(prefixed))
            ).await;

            let queue_latency_us = sim_start.elapsed().as_micros() as u64 - reading.buffer_insert_us;
            hist.record(queue_latency_us).ok();

            match send_result {
                Ok(Ok(_)) => {
                    telemetry_cache.lock().await.record(pkt);
                    tracing::info!(
                        tx_log_seq,
                        sensor=?pkt.sensor_id,
                        sensor_seq=pkt.seq_no,
                        queue_latency_us,
                        elapsed_us,
                        retransmit=from_retransmit,
                        "downlink_tx: sent"
                    );
                }
                _ => { 
                    tracing::warn!(elapsed_us, "downlink_tx: send timeout/error"); 
                }
            }
            tx_log_seq += 1;
        }

        let elapsed_ms = window_start.elapsed().as_millis();
        if elapsed_ms > shared::config::DOWNLINK_WINDOW_MS as u128 {
            if let Ok(mut m) = ui_metrics.try_lock() {
                m.downlink_window_violations += 1;
            }
        }
        
        if let Ok(mut m) = ui_metrics.try_lock() {
            if m.downlink_queue_latency_sparkline.len() >= 60 { m.downlink_queue_latency_sparkline.pop_front(); }
            m.downlink_queue_latency_sparkline.push_back(hist.value_at_percentile(50.0) as u64);
            m.downlink_queue_p50_us = hist.value_at_percentile(50.0);
            m.downlink_queue_p99_us = hist.value_at_percentile(99.0);
            m.downlink_queue_max_us = hist.max();
            m.downlink_total_sent = tx_log_seq as u64;
        }
        
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
    
    tracing::info!(p50=hist.value_at_percentile(50.0), p99=hist.value_at_percentile(99.0),
                   "downlink_tx final queue latency stats");
    crate::ui::push_log(&ui_metrics, 0, "downlink_tx finished".to_string(), &sim_start);
}
