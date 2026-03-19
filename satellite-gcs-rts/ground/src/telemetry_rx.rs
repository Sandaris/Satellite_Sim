use std::collections::{HashMap, BinaryHeap};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::Instant;
use shared::packets::{TelemetryPacket, FaultPacket, SensorId, CommandPacket, CommandType};
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
    _cmd_queue:   Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
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
    let consecutive_gap = 0u32;

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

        let packet: TelemetryPacket = match bincode::deserialize(&bytes) {
            Ok(p) => p,
            Err(_) => {
                // If it failed as TelemetryPacket, try as FaultPacket
                if let Ok(fault) = bincode::deserialize::<FaultPacket>(&bytes) {
                    let _ = fault_tx.send(fault).await;
                }
                continue;
            }
        };

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
            sensor=?packet.sensor_id, latency_us, drift_us,
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
                }
            }
            if let Some(h) = lat_hist.get(&packet.sensor_id) {
                m.latency_p50_us = h.value_at_percentile(50.0);
                m.latency_p99_us = h.value_at_percentile(99.0);
                m.latency_max_us = h.max();
                m.latency_avg_us = if h.len() > 0 { h.mean() as u64 } else { 0 };
            }
            
            m.consecutive_gaps = consecutive_gap;
            if let Ok(mut s) = state.try_lock() {
                if *s == GcsSystemState::LossOfContact {
                    *s = GcsSystemState::Nominal;
                }
                m.gcs_state = format!("{:?}", *s).to_uppercase();
                if *s == GcsSystemState::LossOfContact || loss_of_contact {
                    m.contact_status = "LOST".to_string();
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
async fn enqueue_re_request(cmd_queue: &Arc<Mutex<BinaryHeap<PrioritizedCommand>>>, ts_us: u64, sim_start: &Arc<Instant>) {
    let _enqueue_ts = sim_start.elapsed().as_micros() as u64;
    let pkt = CommandPacket {
        seq_no: 0,
        timestamp_us: ts_us,
        cmd_type: CommandType::RequestTelemetry,
        priority: 3,
        payload: [0u8; 32],
    };
    let cmd = PrioritizedCommand { packet: pkt, enqueue_us: ts_us };
    cmd_queue.lock().await.push(cmd);
}
