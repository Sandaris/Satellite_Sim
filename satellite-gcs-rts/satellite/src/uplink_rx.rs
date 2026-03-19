use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::Instant;
use shared::packets::{CommandPacket, CommandType};
use crate::state::SystemState;

use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use futures::StreamExt;
use tokio::net::tcp::OwnedReadHalf;

pub async fn run_uplink_rx(
    reader:     OwnedReadHalf,
    state:      Arc<Mutex<SystemState>>,
    sim_start:  Arc<Instant>,
    mut cancel:      tokio::sync::watch::Receiver<bool>,
    heartbeat:  Arc<AtomicU64>,
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
        
        handle_command(cmd, &state).await;
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
}

async fn handle_command(cmd: CommandPacket, state: &Arc<Mutex<SystemState>>) {
    let current = { state.lock().await.clone() };
    match (cmd.cmd_type, current) {
        (CommandType::EmergencyStop, _) => {
            *state.lock().await = SystemState::MissionAbort;
        }
        (CommandType::SafeMode, _) => {
            *state.lock().await = SystemState::Fault;
        }
        (CommandType::ResetSensor, SystemState::Fault) | (CommandType::ResetSensor, SystemState::MissionAbort) => {
            *state.lock().await = SystemState::Nominal;
        }
        (_, SystemState::Fault) | (_, SystemState::MissionAbort) => {
            tracing::warn!(cmd=?cmd.cmd_type, reason="interlock_active", "INTERLOCK: command blocked — system in Fault state");
            return;
        }
        _ => {}
    }
    tracing::info!(cmd=?cmd.cmd_type, "command executed");
}
