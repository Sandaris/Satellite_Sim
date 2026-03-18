use std::net::SocketAddr;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::net::UdpSocket;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use shared::config::{DOWNLINK_WINDOW_MS, DOWNLINK_INIT_TIMEOUT_MS};
use shared::packets::FaultPacket;
use crate::buffer::SensorBuffer;
use crate::state::SystemState;

pub async fn run_downlink_tx(
    buffer:     Arc<Mutex<SensorBuffer>>,
    gcs_addr:   SocketAddr,
    sim_start:  Arc<Instant>,
    _state:      Arc<Mutex<SystemState>>,
    cancel:     CancellationToken,
    heartbeat:  Arc<AtomicU64>,
    mut fault_rx: tokio::sync::mpsc::Receiver<FaultPacket>,
    ui_metrics: Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let socket = tokio::time::timeout(
        Duration::from_millis(DOWNLINK_INIT_TIMEOUT_MS),
        UdpSocket::bind("0.0.0.0:0")
    ).await;
    
    let socket = match socket {
        Ok(Ok(s))  => s,
        Ok(Err(e)) => { 
            tracing::error!("Downlink socket bind failed: {}", e); 
            crate::ui::push_log(&ui_metrics, 2, format!("Downlink bind failed: {}", e), &sim_start);
            return; 
        }
        Err(_)     => { 
            tracing::error!("MISSED COMM: downlink socket init exceeded 5ms"); 
            crate::ui::push_log(&ui_metrics, 2, "MISSED COMM: downlink init timeout".to_string(), &sim_start);
            return; 
        }
    };

    let mut interval = tokio::time::interval(Duration::from_millis(DOWNLINK_WINDOW_MS));
    let mut tx_seq: u32 = 0;
    let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { break; }
            _ = interval.tick() => {}
        }

        let elapsed_us = sim_start.elapsed().as_micros() as u64;
        let window_start = Instant::now();

        // Forward fault packets preferentially
        while let Ok(fault) = fault_rx.try_recv() {
            if let Ok(bytes) = bincode::serialize(&fault) {
                // Prepend type header to let GCS identify it as Fault
                let mut payload = vec![shared::packets::PacketType::FaultNotify as u8];
                payload.extend_from_slice(&bytes);
                let _ = socket.send_to(&payload, gcs_addr).await;
            }
        }

        let degraded = { buffer.lock().await.is_degraded() };
        
        for _ in 0..10 {
            let reading = { buffer.lock().await.pop() };
            let reading = match reading { Some(r) => r, None => break };

            if degraded && reading.packet.priority > 1 { continue; }

            let mut pkt = reading.packet;
            pkt.seq_no = tx_seq; // Let downstream assign true sequence for transmission order tracking

            let bytes = match bincode::serialize(&pkt) {
                Ok(b)  => b,
                Err(e) => { tracing::error!("Serialize failed: {}", e); continue; }
            };
            
            // Note: Prefix with PacketType::SensorData manually for routing purposes, or rely on bincode layout matching
            let send_result = tokio::time::timeout(
                Duration::from_millis(DOWNLINK_WINDOW_MS),
                socket.send_to(&bytes, gcs_addr)
            ).await;

            let queue_latency_us = sim_start.elapsed().as_micros() as u64 - reading.buffer_insert_us;
            hist.record(queue_latency_us).ok();

            match send_result {
                Ok(Ok(_)) => {
                    tracing::info!(tx_seq, sensor=?pkt.sensor_id, queue_latency_us, elapsed_us, "downlink_tx: sent");
                    crate::ui::push_log(&ui_metrics, 0, format!("downlink_tx: sent seq={}", tx_seq), &sim_start);
                }
                _ => { 
                    tracing::warn!(elapsed_us, "downlink_tx: send timeout/error"); 
                    crate::ui::push_log(&ui_metrics, 1, "downlink_tx: send timeout/error".to_string(), &sim_start);
                }
            }
            tx_seq += 1;
        }

        let elapsed_ms = window_start.elapsed().as_millis();
        if elapsed_ms > DOWNLINK_WINDOW_MS as u128 {
            tracing::warn!(elapsed_ms, limit_ms=DOWNLINK_WINDOW_MS, elapsed_us, "downlink_tx: exceeded 30ms window");
            crate::ui::push_log(&ui_metrics, 1, format!("downlink_tx: window exceeded ({}ms)", elapsed_ms), &sim_start);
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
            m.downlink_total_sent = tx_seq as u64;
        }
        
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
    
    tracing::info!(p50=hist.value_at_percentile(50.0), p99=hist.value_at_percentile(99.0),
                   "downlink_tx final queue latency stats");
    crate::ui::push_log(&ui_metrics, 0, "downlink_tx finished".to_string(), &sim_start);
}
