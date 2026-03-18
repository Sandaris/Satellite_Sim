use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::sync::{Arc, atomic::{AtomicU64, Ordering as AtomicOrdering}};
use tokio::sync::Mutex;
use tokio::net::UdpSocket;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use shared::packets::CommandPacket;
use shared::config::CMD_DISPATCH_MS;
use crate::state::GcsSystemState;

#[derive(Debug, Clone)]
pub struct PrioritizedCommand {
    pub packet:    CommandPacket,
    pub enqueue_us: u64,
}

impl Ord for PrioritizedCommand {
    fn cmp(&self, other: &Self) -> Ordering {
        other.packet.priority.cmp(&self.packet.priority)
            .then_with(|| other.packet.timestamp_us.cmp(&self.packet.timestamp_us))
    }
}

impl PartialOrd for PrioritizedCommand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PrioritizedCommand {
    fn eq(&self, other: &Self) -> bool {
        self.packet.priority == other.packet.priority &&
        self.packet.timestamp_us == other.packet.timestamp_us
    }
}

impl Eq for PrioritizedCommand {}

pub async fn run_uplink_tx(
    sat_addr:  SocketAddr,
    cmd_queue: Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
    state:     Arc<Mutex<GcsSystemState>>,
    sim_start: Arc<Instant>,
    cancel:    CancellationToken,
    heartbeat: Arc<AtomicU64>,
    ui_metrics: Arc<Mutex<crate::ui::GcsMetricsSnapshot>>,
) {
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => { tracing::error!("uplink_tx bind failed: {}", e); return; }
    };

    let mut interval = tokio::time::interval(Duration::from_millis(5));
    let mut tx_seq: u32 = 0;
    let mut deadline_misses: u64 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {}
        }

        let cmd = { cmd_queue.lock().await.pop() };
        let cmd = match cmd { Some(c) => c, None => continue };

        let gcs_state = { state.lock().await.clone() };
        if matches!(gcs_state, GcsSystemState::InterlockActive | GcsSystemState::LossOfContact) {
            if cmd.packet.priority > 1 {
                tracing::warn!(cmd=?cmd.packet.cmd_type, reason="interlock_active", elapsed_us=sim_start.elapsed().as_micros() as u64,
                               "COMMAND REJECTED");
                crate::ui::push_log(&ui_metrics, 1, format!("COMMAND REJECTED {:?} (interlock)", cmd.packet.cmd_type), &sim_start);
                if let Ok(mut m) = ui_metrics.try_lock() { m.cmd_rejected_count += 1; }
                continue;
            }
        }

        let dispatch_start = Instant::now();
        let deadline_ms = CMD_DISPATCH_MS; 

        let mut pkt = cmd.packet;
        pkt.seq_no = tx_seq;
        pkt.timestamp_us = sim_start.elapsed().as_micros() as u64;

        let bytes = bincode::serialize(&pkt).unwrap();

        let send_result = tokio::time::timeout(
            Duration::from_millis(deadline_ms),
            socket.send_to(&bytes, sat_addr)
        ).await;

        let dispatch_us = dispatch_start.elapsed().as_micros() as u64;
        let queue_latency_us = pkt.timestamp_us.saturating_sub(cmd.enqueue_us);
        let elapsed_us = sim_start.elapsed().as_micros() as u64;

        match send_result {
            Ok(Ok(_)) => {
                tracing::info!(cmd=?pkt.cmd_type, dispatch_us, queue_latency_us,
                               seq=tx_seq, elapsed_us, "uplink_tx: sent");
                crate::ui::push_log(&ui_metrics, 0, format!("uplink_tx: sent {:?} seq={}", pkt.cmd_type, tx_seq), &sim_start);
            }
            _ => {
                deadline_misses += 1;
                tracing::warn!(cmd=?pkt.cmd_type, dispatch_us, elapsed_us, limit_ms=deadline_ms,
                               "uplink_tx: DISPATCH DEADLINE MISSED");
                crate::ui::push_log(&ui_metrics, 1, format!("DISPATCH DEADLINE MISSED: {:?}", pkt.cmd_type), &sim_start);
                if let Ok(mut m) = ui_metrics.try_lock() { m.cmd_deadline_misses += 1; }
            }
        }
        if let Ok(mut m) = ui_metrics.try_lock() {
            if m.recent_commands.len() >= 5 { m.recent_commands.pop_front(); }
            let res_str = if send_result.is_ok() { "SENT" } else { "TIMEOUT" };
            let elapsed = sim_start.elapsed();
            m.recent_commands.push_back((
                format!("{:02}:{:02}:{:02}.{:03}", elapsed.as_secs() / 3600, (elapsed.as_secs() % 3600) / 60, elapsed.as_secs() % 60, elapsed.subsec_millis()),
                format!("{:?}", pkt.cmd_type), dispatch_us, res_str.to_string()
            ));
            m.cmd_total_sent += 1;
            if let Ok(cq) = cmd_queue.try_lock() {
                m.cmd_queue_depth = cq.len();
                m.cmd_emergency_count = cq.iter().filter(|c| c.packet.priority == 1).count();
                m.cmd_urgent_count = cq.iter().filter(|c| c.packet.priority == 2).count();
                m.cmd_routine_count = cq.iter().filter(|c| c.packet.priority == 3).count();
            }
        }
        tx_seq += 1;
        heartbeat.store(sim_start.elapsed().as_secs(), AtomicOrdering::Relaxed);
    }
    tracing::info!(deadline_misses, "uplink_tx final stats");
}
