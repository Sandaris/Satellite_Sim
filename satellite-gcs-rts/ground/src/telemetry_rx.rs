use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::net::UdpSocket;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use shared::packets::{TelemetryPacket, FaultPacket, PacketType, SensorId, CommandPacket, CommandType};
use shared::config::{UDP_MAX_PAYLOAD, TELEMETRY_DECODE_MS, GCS_PACKET_LOSS_ALERT, THERMAL_PERIOD_MS, POWER_PERIOD_MS, IMU_PERIOD_MS};
use hdrhistogram::Histogram;
use crate::state::GcsSystemState;
use crate::uplink_tx::PrioritizedCommand;
use std::collections::BinaryHeap;

pub async fn run_telemetry_rx(
    bind_addr:    SocketAddr,
    state:        Arc<Mutex<GcsSystemState>>,
    fault_tx:     tokio::sync::mpsc::Sender<FaultPacket>,
    cmd_queue:    Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
    sim_start:    Arc<Instant>,
    cancel:       CancellationToken,
    heartbeat:    Arc<AtomicU64>,
    ui_metrics:   Arc<Mutex<crate::ui::GcsMetricsSnapshot>>,
) {
    let socket = UdpSocket::bind(bind_addr).await.expect("GCS RX bind failed");
    let mut buf = [0u8; UDP_MAX_PAYLOAD];

    let mut last_seq: HashMap<SensorId, u32> = HashMap::new();
    let mut consecutive_gap: u32 = 0;
    let mut loss_of_contact = false;

    let mut lat_hist: HashMap<SensorId, Histogram<u64>> = HashMap::new();
    for id in [SensorId::Thermal, SensorId::Power, SensorId::Imu] {
        lat_hist.insert(id, Histogram::<u64>::new(3).unwrap());
    }

    let mut last_recv_us: HashMap<SensorId, u64> = HashMap::new();

    loop {
        let recv_result = tokio::select! {
            _ = cancel.cancelled() => break,
            r = tokio::time::timeout(
                Duration::from_millis(200),
                socket.recv_from(&mut buf)
            ) => r
        };

        let recv_us = sim_start.elapsed().as_micros() as u64; 

        match recv_result {
            Err(_timeout) => {
                consecutive_gap += 1;
                tracing::warn!(consecutive_gap, elapsed_us=recv_us, "DMS: no packet for 200ms");
                crate::ui::push_log(&ui_metrics, 1, format!("DMS: no packet for 200ms (gap: {})", consecutive_gap), &sim_start);
                
                if consecutive_gap >= GCS_PACKET_LOSS_ALERT && !loss_of_contact {
                    loss_of_contact = true;
                    tracing::error!(consecutive_gap, elapsed_us=recv_us, "LOSS OF CONTACT — >= 3 consecutive failures");
                    crate::ui::push_log(&ui_metrics, 2, "LOSS OF CONTACT — >= 3 consecutive failures".to_string(), &sim_start);
                    enqueue_re_request(&cmd_queue, recv_us, &sim_start).await;
                    let mut s = state.lock().await;
                    if *s != GcsSystemState::CriticalAlert {
                        *s = GcsSystemState::LossOfContact;
                    }
                }
                continue;
            }
            Ok(Err(e)) => { tracing::error!("recv_from error: {}", e); continue; }
            Ok(Ok((len, _src))) => {
                consecutive_gap = 0;
                
                if loss_of_contact {
                    loss_of_contact = false;
                    let mut s = state.lock().await;
                    if *s == GcsSystemState::LossOfContact {
                        *s = GcsSystemState::Nominal;
                    }
                }

                if len > 0 && buf[0] == PacketType::FaultNotify as u8 {
                    if let Ok(fault_pkt) = bincode::deserialize::<FaultPacket>(&buf[1..len]) {
                        let _ = fault_tx.send(fault_pkt).await;
                    }
                    continue;
                }

                let decode_result = tokio::time::timeout(
                    Duration::from_millis(TELEMETRY_DECODE_MS),
                    async { bincode::deserialize::<TelemetryPacket>(&buf[..len]) }
                ).await;

                let packet = match decode_result {
                    Ok(Ok(p))  => p,
                    Ok(Err(e)) => { 
                        tracing::warn!("decode error: {}", e); 
                        crate::ui::push_log(&ui_metrics, 1, format!("decode error: {}", e), &sim_start);
                        continue; 
                    }
                    Err(_)     => { 
                        tracing::warn!(elapsed_us=recv_us, limit_ms=TELEMETRY_DECODE_MS, "DECODE EXCEEDED 3ms deadline"); 
                        crate::ui::push_log(&ui_metrics, 1, "DECODE EXCEEDED 3ms deadline".to_string(), &sim_start);
                        if let Ok(mut m) = ui_metrics.try_lock() { m.decode_deadline_misses += 1; }
                        continue; 
                    }
                };

                if packet.packet_type == PacketType::FaultNotify {
                    continue;
                }
                
                if packet.packet_type == PacketType::Heartbeat {
                    tracing::debug!("satellite heartbeat received");
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
                    sensor=?packet.sensor_id, latency_us, drift_us,
                    seq=packet.seq_no, value=packet.value, elapsed_us=recv_us, "telemetry_rx"
                );
                crate::ui::push_log(&ui_metrics, 0, format!("Telemetry {:?} seq={} lat={}us", packet.sensor_id, packet.seq_no, latency_us), &sim_start);
                
                last_recv_us.insert(packet.sensor_id, recv_us);

                if let Some(&prev_seq) = last_seq.get(&packet.sensor_id) {
                    let gap = packet.seq_no.saturating_sub(prev_seq + 1);
                    if gap > 0 {
                        tracing::warn!(sensor=?packet.sensor_id, expected=prev_seq+1,
                                      got=packet.seq_no, gap, elapsed_us=recv_us, "PACKET LOSS DETECTED");
                        crate::ui::push_log(&ui_metrics, 1, format!("PACKET LOSS: {:?} gap={}", packet.sensor_id, gap), &sim_start);
                    }
                }
                
                if let Ok(mut m) = ui_metrics.try_lock() {
                    m.decode_latency_last_us = recv_us.saturating_sub(packet.timestamp_us);
                    m.total_pkts_received += 1;
                    match packet.sensor_id {
                        SensorId::Thermal => { m.thermal_recv_count += 1; m.thermal_drift_last_us = drift_us; }
                        SensorId::Power => { m.power_recv_count += 1; m.power_drift_last_us = drift_us; }
                        SensorId::Imu => { m.imu_recv_count += 1; m.imu_drift_last_us = drift_us; }
                    }
                    if let Some(&prev_seq) = last_seq.get(&packet.sensor_id) {
                        let gap = packet.seq_no.saturating_sub(prev_seq + 1);
                        if gap > 0 {
                            m.total_pkts_lost += gap as u64;
                            match packet.sensor_id {
                                SensorId::Thermal => m.thermal_lost_count += gap as u64,
                                SensorId::Power => m.power_lost_count += gap as u64,
                                SensorId::Imu => m.imu_lost_count += gap as u64,
                            }
                        }
                    }
                    let lat_ms = latency_us as f64 / 1000.0;
                    let bucket = if lat_ms < 1.0 { 0 } else if lat_ms < 2.0 { 1 } else if lat_ms < 5.0 { 2 } else if lat_ms < 10.0 { 3 } else if lat_ms < 20.0 { 4 } else if lat_ms < 50.0 { 5 } else if lat_ms < 100.0 { 6 } else { 7 };
                    m.latency_buckets[bucket] += 1;
                    
                    if let Some(h) = lat_hist.get(&packet.sensor_id) {
                        m.latency_p50_us = h.value_at_percentile(50.0);
                        m.latency_p99_us = h.value_at_percentile(99.0);
                        m.latency_max_us = h.max();
                        m.latency_avg_us = if h.len() > 0 { h.mean() as u64 } else { 0 };
                    }
                    m.consecutive_gaps = consecutive_gap;
                    if let Ok(s) = state.try_lock() {
                        m.gcs_state = format!("{:?}", *s).to_uppercase();
                        if loss_of_contact {
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
                last_seq.insert(packet.sensor_id, packet.seq_no);

                heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
            }
        }
    }
    
    // Print final histogram summary per sensor
    for (id, hist) in lat_hist.iter() {
        if hist.len() > 0 {
            tracing::info!(sensor=?id, p50=hist.value_at_percentile(50.0), p99=hist.value_at_percentile(99.0),
                           "telemetry_rx final latency stats");
        }
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
