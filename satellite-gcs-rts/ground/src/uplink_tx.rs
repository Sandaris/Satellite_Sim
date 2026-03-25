use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, atomic::{AtomicU64, Ordering as AtomicOrdering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use shared::packets::{CommandPacket, CommandType};
use shared::config::CMD_DISPATCH_MS;
use crate::state::GcsSystemState;
use hdrhistogram::Histogram;

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

use tokio_util::codec::{FramedWrite, LengthDelimitedCodec};
use futures::SinkExt;
use tokio::net::tcp::OwnedWriteHalf;
use bytes::Bytes;

pub async fn run_uplink_tx(
    writer:    OwnedWriteHalf,
    cmd_queue: Arc<Mutex<BinaryHeap<PrioritizedCommand>>>,
    state:     Arc<Mutex<GcsSystemState>>,
    sim_start: Arc<Instant>,
    mut cancel:    tokio::sync::watch::Receiver<bool>,
    heartbeat: Arc<AtomicU64>,
    ui_metrics: Arc<Mutex<crate::ui::GcsMetricsSnapshot>>,
    gcs_busy_uplink_us: Arc<AtomicU64>,
) {
    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(1024);
    let mut framed_writer = FramedWrite::new(writer, codec.new_codec());

    let mut interval = tokio::time::interval(Duration::from_millis(5));
    let mut tx_seq: u32 = 0;
    let mut deadline_misses: u64 = 0;
    let mut next_tick_us = sim_start.elapsed().as_micros() as u64 + 5_000;
    let mut last_dispatch_end_us: Option<u64> = None;
    let mut jitter_hist = Histogram::<u64>::new(3).unwrap();

    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = interval.tick() => {}
        }
        let tick_now_us = sim_start.elapsed().as_micros() as u64;
        let drift = tick_now_us as i64 - next_tick_us as i64;
        next_tick_us = tick_now_us + 5_000;

        let cmd = { cmd_queue.lock().await.pop() };
        heartbeat.store(sim_start.elapsed().as_secs(), AtomicOrdering::Relaxed);
        let cmd = match cmd { Some(c) => c, None => continue };

        let gcs_state = { state.lock().await.clone() };
        if matches!(gcs_state, GcsSystemState::InterlockActive | GcsSystemState::LossOfContact) {
            if cmd.packet.priority > 1 {
                let reason = match gcs_state {
                    GcsSystemState::LossOfContact => "loss_of_contact_non_emergency_blocked",
                    GcsSystemState::InterlockActive => "interlock_active_non_emergency_blocked",
                    _ => "safety_interlock",
                };
                tracing::warn!(cmd=?cmd.packet.cmd_type, reason, elapsed_us=sim_start.elapsed().as_micros() as u64,
                               "COMMAND REJECTED");
                crate::ui::push_log(
                    &ui_metrics,
                    1,
                    format!("COMMAND REJECTED {:?} ({})", cmd.packet.cmd_type, reason),
                    &sim_start,
                );
                if let Ok(mut m) = ui_metrics.try_lock() {
                    m.cmd_rejected_count += 1;
                    m.cmd_rejection_last_reason = reason.to_string();
                }
                continue;
            }
        }

        let enqueue_ts = cmd.enqueue_us;
        let dispatch_start = Instant::now();
        let deadline_ms = if cmd.packet.priority <= 2 { 2 } else { CMD_DISPATCH_MS };

        let mut pkt = cmd.packet;
        // RequestTelemetry carries the missing per-sensor seq in seq_no; do not overwrite.
        if pkt.cmd_type != CommandType::RequestTelemetry {
            pkt.seq_no = tx_seq;
        }
        let send_time_us = sim_start.elapsed().as_micros() as u64;
        pkt.timestamp_us = send_time_us;

        let bytes = bincode::serialize(&pkt).unwrap();

        let send_result = tokio::time::timeout(
            Duration::from_millis(deadline_ms),
            framed_writer.send(Bytes::from(bytes))
        ).await;

        let dispatch_us = dispatch_start.elapsed().as_micros() as u64;
        let queue_latency_us = send_time_us.saturating_sub(enqueue_ts);
        let elapsed_us = sim_start.elapsed().as_micros() as u64;
        gcs_busy_uplink_us.fetch_add(dispatch_us, AtomicOrdering::Relaxed);

        match send_result {
            Ok(Ok(_)) => {
                tracing::info!(cmd=?pkt.cmd_type, dispatch_us, queue_latency_us,
                               seq=pkt.seq_no, elapsed_us, "uplink_tx: sent");
                crate::ui::push_log(&ui_metrics, 0, format!("uplink_tx: sent {:?} seq={}", pkt.cmd_type, pkt.seq_no), &sim_start);
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
                format!("{:?}", pkt.cmd_type), pkt.priority, dispatch_us, res_str.to_string()
            ));
            m.cmd_total_sent += 1;
            m.task_drift_uplink_last_us = drift;
            if let Some(last_end_us) = last_dispatch_end_us {
                let interval_us = elapsed_us.saturating_sub(last_end_us);
                let expected_gap_us = 5_000u64;
                let jitter_us = interval_us.abs_diff(expected_gap_us);
                let _ = jitter_hist.record(jitter_us.max(1));
                m.uplink_jitter_last_us = jitter_us;
                m.uplink_jitter_p50_us = jitter_hist.value_at_percentile(50.0);
                m.uplink_jitter_p99_us = jitter_hist.value_at_percentile(99.0);
                m.uplink_jitter_max_us = jitter_hist.max();
            }
            m.uplink_dispatch_drift_last_us = drift;
            if let Ok(cq) = cmd_queue.try_lock() {
                m.cmd_queue_depth = cq.len();
                m.cmd_emergency_count = cq.iter().filter(|c| c.packet.priority == 1).count();
                m.cmd_urgent_count = cq.iter().filter(|c| c.packet.priority == 2).count();
                m.cmd_routine_count = cq.iter().filter(|c| c.packet.priority == 3).count();
            }
        }
        last_dispatch_end_us = Some(elapsed_us);
        tx_seq += 1;
    }
    tracing::info!(deadline_misses, "uplink_tx final stats");
}
