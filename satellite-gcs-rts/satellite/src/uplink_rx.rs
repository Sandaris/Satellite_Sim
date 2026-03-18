use std::net::SocketAddr;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use shared::packets::{CommandPacket, CommandType};
use shared::config::UDP_MAX_PAYLOAD;
use crate::state::SystemState;

pub async fn run_uplink_rx(
    my_addr:    SocketAddr,
    state:      Arc<Mutex<SystemState>>,
    sim_start:  Arc<Instant>,
    cancel:     CancellationToken,
    heartbeat:  Arc<AtomicU64>,
) {
    let socket = UdpSocket::bind(my_addr).await.expect("Uplink RX bind failed");
    let mut buf = [0u8; UDP_MAX_PAYLOAD];

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                let recv_us = sim_start.elapsed().as_micros() as u64;
                match result {
                    Err(e) => { tracing::error!("uplink_rx: recv error: {}", e); continue; }
                    Ok((len, _src)) => {
                        let cmd: CommandPacket = match bincode::deserialize(&buf[..len]) {
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
            }
        }
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
        (CommandType::ResetSensor, SystemState::Fault) => {
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
