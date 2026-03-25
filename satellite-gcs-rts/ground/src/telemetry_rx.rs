use std::collections::{HashMap, BinaryHeap};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::Instant;
use shared::packets::{TelemetryPacket, FaultPacket, SensorId, CommandPacket, CommandType, PacketType};
use shared::config::{GCS_PACKET_LOSS_ALERT, THERMAL_PERIOD_MS, POWER_PERIOD_MS, IMU_PERIOD_MS};
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

    loop {
        let now_us = sim_start.elapsed().as_micros() as u64;
        let loss_of_contact = now_us.saturating_sub(last_any_recv_us) > (GCS_PACKET_LOSS_ALERT as u64 * 1_000_000);

        let frame = tokio::select! {
            _ = cancel.changed() => break,
            f = framed_reader.next() => f,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                 // Check loss of contact every second even if no packets arrive
                 if sim_start.elapsed().as_micros() as u64 - last_any_recv_us > (GCS_PACKET_LOSS_ALERT as u64 * 1_000_000) {
                      if let Ok(mut m) = ui_metrics.try_lock() { m.contact_status = "LOST".to_string(); }
                      if let Ok(mut s) = state.try_lock() { *s = GcsSystemState::LossOfContact; }
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
        if decode_duration_us > 3000 {
            tracing::error!(sensor=?packet.sensor_id, decode_duration_us, "DECODE DEADLINE MISSED (>3ms)");
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
                } else {
                    consecutive_gap = 0;
                }
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
                }
            }
            if m.total_pkts_received + m.total_pkts_lost > 0 {
                m.reception_rate_pct = m.total_pkts_received as f64 / (m.total_pkts_received + m.total_pkts_lost) as f64 * 100.0;
            }
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
    let pkt = CommandPacket {
        seq_no: missing_from,           // encodes which seq we want re-sent
        timestamp_us: ts_us,
        cmd_type: CommandType::RequestTelemetry,
        priority: 2,                    // URGENT — above routine heartbeats
        payload: [0u8; 32],
    };
    let cmd = PrioritizedCommand { packet: pkt, enqueue_us: enqueue_ts };
    cmd_queue.lock().await.push(cmd);
    tracing::info!(
        sensor=?sensor, missing_from, gap,
        enqueue_us=enqueue_ts, elapsed_us=enqueue_ts,
        "telemetry_rx: RequestTelemetry enqueued"
    );
}
