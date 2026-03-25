use std::collections::VecDeque;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::Instant;
use shared::packets::{CommandPacket, CommandType, SensorId, TelemetryPacket};
use crate::state::SystemState;
use crate::telemetry_cache::TelemetryCache;

use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use futures::StreamExt;
use tokio::net::tcp::OwnedReadHalf;

const MAX_RETRANSMIT_QUEUE: usize = 64;

pub async fn run_uplink_rx(
    reader:         OwnedReadHalf,
    state:          Arc<Mutex<SystemState>>,
    sim_start:      Arc<Instant>,
    mut cancel:     tokio::sync::watch::Receiver<bool>,
    heartbeat:      Arc<AtomicU64>,
    telemetry_cache: Arc<Mutex<TelemetryCache>>,
    retransmit_q:   Arc<Mutex<VecDeque<TelemetryPacket>>>,
) {
    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(1024);
    let mut framed_reader = FramedRead::new(reader, codec.new_codec());

    loop {
        let frame = tokio::select! {
            _ = cancel.changed() => break,
            f = framed_reader.next() => f,
        };

        let bytes = match frame {
            Some(Ok(b)) => b,
            Some(Err(e)) => {
                tracing::error!("TCP Uplink Read Error: {}", e);
                break;
            }
            None => {
                tracing::warn!("GCS uplink connection closed.");
                break;
            }
        };

        let recv_us = sim_start.elapsed().as_micros() as u64;
        let cmd: CommandPacket = match bincode::deserialize(&bytes) {
            Ok(c)  => c,
            Err(e) => { tracing::warn!("uplink_rx: deser error: {}", e); continue; }
        };

        let dispatch_latency = recv_us.saturating_sub(cmd.timestamp_us);
        tracing::info!(cmd=?cmd.cmd_type, seq=cmd.seq_no,
                      dispatch_latency_us=dispatch_latency, elapsed_us=recv_us, "uplink_rx: received");
        
        handle_command(
            cmd,
            &state,
            &telemetry_cache,
            &retransmit_q,
        ).await;
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}

fn sensor_from_payload(b: u8) -> Option<SensorId> {
    match b {
        0 => Some(SensorId::Thermal),
        1 => Some(SensorId::Power),
        2 => Some(SensorId::Imu),
        _ => None,
    }
}

async fn handle_command(
    cmd: CommandPacket,
    state: &Arc<Mutex<SystemState>>,
    cache: &Arc<Mutex<TelemetryCache>>,
    retransmit_q: &Arc<Mutex<VecDeque<TelemetryPacket>>>,
) {
    if cmd.cmd_type == CommandType::RequestTelemetry {
        let sensor = match sensor_from_payload(cmd.payload[0]) {
            Some(s) => s,
            None => {
                tracing::warn!(byte=cmd.payload[0], "RE-REQUEST: invalid sensor id in payload[0]");
                return;
            }
        };
        let req_seq = cmd.seq_no;
        let pkt_opt = { cache.lock().await.get(sensor, req_seq) };
        match pkt_opt {
            Some(pkt) => {
                let mut q = retransmit_q.lock().await;
                while q.len() >= MAX_RETRANSMIT_QUEUE {
                    q.pop_front();
                }
                q.push_back(pkt);
                tracing::info!(
                    sensor=?sensor,
                    req_seq,
                    "RE-REQUEST: queued cached telemetry for retransmit"
                );
            }
            None => {
                tracing::warn!(
                    sensor=?sensor,
                    req_seq,
                    "RE-REQUEST: packet not in cache (too old or never sent)"
                );
            }
        }
        tracing::info!(cmd=?cmd.cmd_type, "command executed");
        return;
    }

    let mut s = state.lock().await;
    match (cmd.cmd_type, s.clone()) {
        (CommandType::EmergencyStop, _) => {
            *s = SystemState::MissionAbort;
        }
        (CommandType::SafeMode, _) => {
            *s = SystemState::Fault;
        }
        (CommandType::ResetSensor, SystemState::Fault) => {
            *s = SystemState::Nominal;
        }
        (_, SystemState::Fault) | (_, SystemState::MissionAbort) => {
            tracing::warn!(cmd=?cmd.cmd_type, reason="interlock_active", 
                           "INTERLOCK: command blocked — system in Fault state");
            return;
        }
        _ => {}
    }
    tracing::info!(cmd=?cmd.cmd_type, "command executed");
}
