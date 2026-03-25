use std::collections::{HashMap, BinaryHeap};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::Instant;
use shared::packets::{TelemetryPacket, FaultPacket, SensorId, CommandPacket, CommandType, PacketType};
use shared::config::{GCS_PACKET_LOSS_ALERT, THERMAL_PERIOD_MS, POWER_PERIOD_MS, IMU_PERIOD_MS, TELEMETRY_DECODE_MS};
use hdrhistogram::Histogram;
use crate::state::GcsSystemState;
use crate::uplink_tx::PrioritizedCommand;

use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use futures::StreamExt;
use tokio::net::tcp::OwnedReadHalf;

pub async fn run_telemetry_rx(
    reader:       OwnedReadHalf,
    state:        Arc<Mutex<GcsSystemState>>,
    fault_tx:     tokio::sync::mpsc::Sender<FaultPacket>,
    cmd_queue:    Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
    sim_start:    Arc<Instant>,
    mut cancel:       tokio::sync::watch::Receiver<bool>,
    heartbeat:    Arc<AtomicU64>,
    ui_metrics:   Arc<Mutex<crate::ui::GcsMetricsSnapshot>>,
) {
    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(1024);
    let mut framed_reader = FramedRead::new(reader, codec.new_codec());

    let mut last_seq: HashMap<SensorId, u32> = HashMap::new();
    
    let mut lat_hist: HashMap<SensorId, Histogram<u64>> = HashMap::new();
    for id in [SensorId::Thermal, SensorId::Power, SensorId::Imu] {
        lat_hist.insert(id, Histogram::<u64>::new(3).unwrap());
    }

    let mut last_recv_us: HashMap<SensorId, u64> = HashMap::new();
    let mut last_any_recv_us = sim_start.elapsed().as_micros() as u64;
    let mut consecutive_gap = 0u32;  // tracks consecutive sensors with missing packets
    let mut task_next_us = sim_start.elapsed().as_micros() as u64 + 200_000;
    let mut delayed_fail_streak = 0u32;
    let mut pending_rerequests: HashMap<(SensorId, u32), u64> = HashMap::new();
    let mut last_delayed_seq_requested: HashMap<SensorId, u32> = HashMap::new();

    loop {
        let now_us = sim_start.elapsed().as_micros() as u64;
        let task_drift_us = now_us as i64 - task_next_us as i64;
        task_next_us = now_us + 200_000;
        if let Ok(mut m) = ui_metrics.try_lock() {
            m.task_drift_telemetry_last_us = task_drift_us;
        }

        let frame = tokio::select! {
            _ = cancel.changed() => break,
            f = framed_reader.next() => f,
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                // Periodic loss-of-contact and delayed-packet handling.
                let now_check_us = sim_start.elapsed().as_micros() as u64;
                if now_check_us.saturating_sub(last_any_recv_us) > (GCS_PACKET_LOSS_ALERT as u64 * 1_000_000) {
                    if let Ok(mut m) = ui_metrics.try_lock() { m.contact_status = "LOST".to_string(); }
                    if let Ok(mut s) = state.try_lock() { *s = GcsSystemState::LossOfContact; }
                }

                // Trigger re-request when packet is delayed beyond 1.5x expected period.
                for sensor in [SensorId::Thermal, SensorId::Power, SensorId::Imu] {
                    let expected_us = match sensor {
                        SensorId::Thermal => THERMAL_PERIOD_MS * 1000,
                        SensorId::Power => POWER_PERIOD_MS * 1000,
                        SensorId::Imu => IMU_PERIOD_MS * 1000,
                    } as u64;
                    let last = *last_recv_us.get(&sensor).unwrap_or(&last_any_recv_us);
                    if now_check_us.saturating_sub(last) > (expected_us * 3 / 2) {
                        let expected_seq = last_seq.get(&sensor).copied().unwrap_or(0).saturating_add(1);
                        let already_requested = last_delayed_seq_requested.get(&sensor).copied().unwrap_or(u32::MAX) == expected_seq;
                        if !already_requested {
                            delayed_fail_streak += 1;
                            if let Ok(mut m) = ui_metrics.try_lock() {
                                m.delayed_packet_events += 1;
                                m.re_request_count += 1;
                            }
                            tracing::warn!(
                                sensor=?sensor,
                                expected_seq,
                                expected_period_us=expected_us,
                                since_last_us=now_check_us.saturating_sub(last),
                                "DELAYED PACKET DETECTED — enqueuing RequestTelemetry"
                            );
                            enqueue_re_request(&cmd_queue, now_check_us, &sim_start, sensor, expected_seq, 1).await;
                            pending_rerequests.insert((sensor, expected_seq), now_check_us);
                            last_delayed_seq_requested.insert(sensor, expected_seq);
                        }
                    }
                }

                if delayed_fail_streak >= 3 {
                    if let Ok(mut m) = ui_metrics.try_lock() {
                        m.contact_status = "LOST".to_string();
                        m.consecutive_gaps = delayed_fail_streak;
                    }
                    if let Ok(mut s) = state.try_lock() {
                        *s = GcsSystemState::LossOfContact;
                    }
                    tracing::error!(fails=delayed_fail_streak, "SATELLITE LOSS OF CONTACT: 3+ delayed packets in sequence");
                }

                if let Ok(mut m) = ui_metrics.try_lock() {
                    m.telemetry_backlog_current = pending_rerequests.len() as u64;
                    m.telemetry_backlog_max = m.telemetry_backlog_max.max(m.telemetry_backlog_current);
                }
                continue;
            }
        };

        let bytes = match frame {
            Some(Ok(b)) => b,
            Some(Err(e)) => {
                tracing::error!("TCP Telemetry Read Error: {}", e);
                if let Ok(mut m) = ui_metrics.try_lock() { m.contact_status = "LOST".to_string(); }
                if let Ok(mut s) = state.try_lock() { *s = GcsSystemState::LossOfContact; }
                break;
            }
            None => {
                tracing::warn!("Satellite telemetry connection closed.");
                if let Ok(mut m) = ui_metrics.try_lock() { m.contact_status = "LOST".to_string(); }
                if let Ok(mut s) = state.try_lock() { *s = GcsSystemState::LossOfContact; }
                break;
            }
        };

        let recv_us = sim_start.elapsed().as_micros() as u64; 

        if bytes.is_empty() { continue; }
        let packet_type = bytes[0];
        let payload = &bytes[1..];

        // AUTHORITATIVE ROUTING: The first byte of the raw frame is the byte-prefix 
        // that determines how to route the remaining bytes. We must not rely on 
        // internal fields of the deserialized packet for routing decisions to 
        // avoid silent drops if those fields are misconfigured.
        if packet_type == PacketType::FaultNotify as u8 {
            if let Ok(fault) = bincode::deserialize::<FaultPacket>(payload) {
                let _ = fault_tx.send(fault).await;
            }
            last_any_recv_us = recv_us;
            continue;
        }

        if packet_type != PacketType::SensorData as u8 {
            tracing::warn!("Received unknown packet type: {}", packet_type);
            continue;
        }

        let decode_start = Instant::now();
        let packet: TelemetryPacket = match bincode::deserialize(payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to deserialize TelemetryPacket: {}", e);
                continue;
            }
        };
        let decode_duration_us = decode_start.elapsed().as_micros() as u64;
        if decode_duration_us > TELEMETRY_DECODE_MS * 1000 {
            tracing::error!(sensor=?packet.sensor_id, decode_duration_us, "DECODE DEADLINE MISSED (>3ms)");
            if let Ok(mut m) = ui_metrics.try_lock() {
                m.decode_deadline_misses += 1;
            }
        }


        if packet.is_corrupted {
            tracing::warn!(sensor=?packet.sensor_id, seq=packet.seq_no,
                           "CORRUPTED PACKET received — discarding");
            crate::ui::push_log(&ui_metrics, 1,
                                format!("CORRUPTED PACKET {:?} seq={}", packet.sensor_id, packet.seq_no),
                                &sim_start);
            if let Ok(mut m) = ui_metrics.try_lock() { m.total_pkts_lost += 1; }
            continue;
        }

        let latency_us = recv_us.saturating_sub(packet.timestamp_us);
        if let Some(hist) = lat_hist.get_mut(&packet.sensor_id) {
            hist.record(latency_us).ok();
        }

        let mut drift_us = 0i64;
        if let Some(last_us) = last_recv_us.get(&packet.sensor_id) {
            let expected_period_us = match packet.sensor_id {
                SensorId::Thermal => THERMAL_PERIOD_MS * 1000,
                SensorId::Power   => POWER_PERIOD_MS   * 1000,
                SensorId::Imu     => IMU_PERIOD_MS     * 1000,
            } as u64;
            let actual_interval = recv_us.saturating_sub(*last_us);
            drift_us = actual_interval as i64 - expected_period_us as i64;
        }
        
        tracing::info!(
            sensor=?packet.sensor_id, latency_us, drift_us, decode_us=decode_duration_us,
            seq=packet.seq_no, value=packet.value, elapsed_us=recv_us, "telemetry_rx"
        );
        crate::ui::push_log(&ui_metrics, 0, format!("Telemetry {:?} seq={} lat={}us", packet.sensor_id, packet.seq_no, latency_us), &sim_start);
        
        last_recv_us.insert(packet.sensor_id, recv_us);

        if let Ok(mut m) = ui_metrics.try_lock() {
            m.decode_latency_last_us = latency_us;
            m.pipeline_packet_to_uplink_last_us = latency_us;
            m.total_pkts_received += 1;
            match packet.sensor_id {
                SensorId::Thermal => { m.thermal_recv_count += 1; m.thermal_drift_last_us = drift_us; m.thermal_last_recv_elapsed_ms = recv_us / 1000; }
                SensorId::Power   => { m.power_recv_count   += 1; m.power_drift_last_us   = drift_us; m.power_last_recv_elapsed_ms   = recv_us / 1000; }
                SensorId::Imu     => { m.imu_recv_count     += 1; m.imu_drift_last_us     = drift_us; m.imu_last_recv_elapsed_ms     = recv_us / 1000; }
            }
            if let Some(&prev_seq) = last_seq.get(&packet.sensor_id) {
                let gap = packet.seq_no.saturating_sub(prev_seq + 1);
                if gap > 0 {
                    m.total_pkts_lost += gap as u64;
                    match packet.sensor_id {
                        SensorId::Thermal => m.thermal_lost_count += gap as u64,
                        SensorId::Power   => m.power_lost_count   += gap as u64,
                        SensorId::Imu     => m.imu_lost_count     += gap as u64,
                    }
                    consecutive_gap += 1;
                    m.re_request_count += 1;
                    tracing::warn!(
                        sensor=?packet.sensor_id, expected=prev_seq+1,
                        got=packet.seq_no, gap, elapsed_us=recv_us,
                        "PACKET LOSS DETECTED — enqueuing RequestTelemetry"
                    );
                    // Enqueue a re-request uplink command to the satellite
                    enqueue_re_request(&cmd_queue, recv_us, &sim_start,
                                       packet.sensor_id, prev_seq + 1, gap).await;
                    pending_rerequests.insert((packet.sensor_id, prev_seq + 1), recv_us);
                } else {
                    consecutive_gap = 0;
                }
            }

            if pending_rerequests.remove(&(packet.sensor_id, packet.seq_no)).is_some() {
                let now = sim_start.elapsed().as_micros() as u64;
                m.pipeline_command_to_response_last_us = now.saturating_sub(packet.timestamp_us);
            }
            if let Some(h) = lat_hist.get(&packet.sensor_id) {
                m.latency_p50_us = h.value_at_percentile(50.0);
                m.latency_p99_us = h.value_at_percentile(99.0);
                m.latency_max_us = h.max();
                m.latency_avg_us = if h.len() > 0 { h.mean() as u64 } else { 0 };

                // Update buckets for visual histogram
                let lat_ms = latency_us / 1000;
                let bucket_idx = match lat_ms {
                    0 => 0,           // <1ms
                    1 => 1,           // 1-2ms
                    2..=4 => 2,       // 2-5ms
                    5..=9 => 3,       // 5-10ms
                    10..=19 => 4,     // 10-20ms
                    20..=49 => 5,     // 20-50ms
                    50..=99 => 6,     // 50-100ms
                    _ => 7,           // >100ms
                };
                m.latency_buckets[bucket_idx] += 1;
            }
            
            m.consecutive_gaps = consecutive_gap;
            if let Ok(mut s) = state.try_lock() {
                if *s == GcsSystemState::LossOfContact {
                    *s = GcsSystemState::Nominal;
                }
                m.gcs_state = format!("{:?}", *s).to_uppercase();
                if consecutive_gap >= 3 {
                    m.contact_status = "LOST".to_string();
                    *s = GcsSystemState::LossOfContact;
                    tracing::error!(gaps=consecutive_gap, "SATELLITE LOSS OF CONTACT: 3+ gaps detected");
                } else if consecutive_gap > 0 {
                    m.contact_status = "DEGRADED".to_string();
                } else {
                    m.contact_status = "ESTABLISHED".to_string();
                    delayed_fail_streak = 0;
                }
            }
            if m.total_pkts_received + m.total_pkts_lost > 0 {
                m.reception_rate_pct = m.total_pkts_received as f64 / (m.total_pkts_received + m.total_pkts_lost) as f64 * 100.0;
            }
            m.telemetry_backlog_current = pending_rerequests.len() as u64;
            m.telemetry_backlog_max = m.telemetry_backlog_max.max(m.telemetry_backlog_current);
        }
        last_any_recv_us = recv_us;
        last_seq.insert(packet.sensor_id, packet.seq_no);
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}
async fn enqueue_re_request(
    cmd_queue: &Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
    ts_us: u64,
    sim_start: &Arc<Instant>,
    sensor: SensorId,
    missing_from: u32,
    gap: u32,
) {
    let enqueue_ts = sim_start.elapsed().as_micros() as u64;
    let mut payload = [0u8; 32];
    payload[0] = sensor as u8;
    let pkt = CommandPacket {
        seq_no: missing_from,           // encodes which seq we want re-sent
        timestamp_us: ts_us,
        cmd_type: CommandType::RequestTelemetry,
        priority: 2,                    // URGENT — above routine heartbeats
        payload,
    };
    let cmd = PrioritizedCommand { packet: pkt, enqueue_us: enqueue_ts };
    cmd_queue.lock().await.push(cmd);
    tracing::info!(
        sensor=?sensor, missing_from, gap,
        enqueue_us=enqueue_ts, elapsed_us=enqueue_ts,
        "telemetry_rx: RequestTelemetry enqueued"
    );
}
