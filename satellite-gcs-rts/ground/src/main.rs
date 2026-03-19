use tracing_subscriber::EnvFilter;
use clap::Parser;
use std::sync::{Arc, atomic::AtomicU64};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use std::collections::BinaryHeap;

mod state;
mod telemetry_rx;
mod uplink_tx;
mod fault_mgr;
mod perf_monitor;
mod watchdog;
mod ui;

use state::GcsSystemState;
use uplink_tx::PrioritizedCommand;

use tracing_subscriber::prelude::*;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    sat_ip: String,

    #[arg(long, default_value_t = shared::config::SIM_TCP_PORT)]
    port: u16,

    #[arg(long, default_value_t = shared::config::SIM_DURATION_S)]
    duration_s: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let sim_start = Arc::new(Instant::now());
    let metrics_snapshot = Arc::new(Mutex::new(ui::GcsMetricsSnapshot::default()));

    // Setup logging: TUI layer + File layer (no stderr to avoid jumping)
    let tui_layer = ui::TuiLogger::new(metrics_snapshot.clone(), sim_start.clone());
    
    let file_appender = tracing_appender::rolling::never(".", "ground.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env()
            .add_directive("ground=info".parse().unwrap()))
        .with(tui_layer)
        .with(file_layer)
        .init();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let state = Arc::new(Mutex::new(GcsSystemState::Nominal));
    let cmd_queue = Arc::new(Mutex::new(BinaryHeap::<PrioritizedCommand>::new()));

    // Watchdog metrics
    let hb_telemetry = Arc::new(AtomicU64::new(0));
    let hb_uplink = Arc::new(AtomicU64::new(0));
    let hb_fault_mgr = Arc::new(AtomicU64::new(0));

    let heartbeats = vec![
        ("telemetry_rx", hb_telemetry.clone()),
        ("uplink_tx", hb_uplink.clone()),
        ("fault_mgr", hb_fault_mgr.clone()),
    ];

    tracing::info!("Starting Satellite Ground Control Simulation (TCP MODE)");

    // TCP Server Setup
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.port)).await?;
    tracing::info!("GCS listening for satellite connection on port {}...", args.port);

    // Global tasks (stay alive across reconnections)
    let mut global_handles = vec![];
    global_handles.push(tokio::spawn(perf_monitor::run_perf_monitor(
        sim_start.clone(), cancel_rx.clone()
    )));

    global_handles.push(tokio::spawn(watchdog::run_watchdog(
        heartbeats, sim_start.clone(), cancel_rx.clone()
    )));

    global_handles.push(tokio::spawn(ui::run_ui(
        Arc::clone(&metrics_snapshot),
        Arc::clone(&sim_start),
        cancel_tx.clone(),
    )));

    let mut session_cancel_rx = cancel_rx.clone();
    
    // Connection handling loop
    let session_metrics = metrics_snapshot.clone();
    let session_start = sim_start.clone();
    let session_state = state.clone();
    let session_cmd_queue = cmd_queue.clone();

    let session_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = session_cancel_rx.changed() => break,
                acc = listener.accept() => {
                    let (stream, addr) = match acc {
                        Ok(res) => res,
                        Err(e) => {
                            tracing::error!("TCP Accept error: {}", e);
                            continue;
                        }
                    };
                    tracing::info!("Satellite connected from: {}", addr);
                    let (reader, writer) = stream.into_split();

                    if let Ok(mut m) = session_metrics.try_lock() {
                        m.contact_status = "ESTABLISHED".to_string();
                        crate::ui::push_log(&session_metrics, 0, format!("Satellite connected from {}", addr), &session_start);
                    }

                    // Per-session cancellation token
                    let session_token = tokio_util::sync::CancellationToken::new();
                    let mut session_handles = vec![];
                    let (s_fault_tx, s_fault_rx) = tokio::sync::mpsc::channel(100);

                    // Telemetry RX (triggers session cancellation on disconnect)
                    let token_tel = session_token.clone();
                    let metrics_tel = session_metrics.clone();
                    let start_tel = session_start.clone();
                    let state_tel = session_state.clone();
                    let cmd_tel = session_cmd_queue.clone();
                    let hb_tel_metric = hb_telemetry.clone();
                    let cancel_tel = session_cancel_rx.clone();

                    session_handles.push(tokio::spawn(async move {
                        telemetry_rx::run_telemetry_rx(
                            reader, state_tel, s_fault_tx, cmd_tel, start_tel.clone(), cancel_tel, hb_tel_metric, metrics_tel.clone()
                        ).await;
                        tracing::warn!("telemetry_rx stopped, signaling session end.");
                        token_tel.cancel();
                        if let Ok(mut m) = metrics_tel.try_lock() {
                            m.contact_status = "LOST".to_string();
                            crate::ui::push_log(&metrics_tel, 1, "Satellite connection lost".to_string(), &start_tel);
                        }
                    }));

                    // Uplink TX (wrapped with session cancellation)
                    let token_up = session_token.clone();
                    let state_up = session_state.clone();
                    let cmd_up = session_cmd_queue.clone();
                    let start_up = session_start.clone();
                    let hb_up_metric = hb_uplink.clone();
                    let metrics_up = session_metrics.clone();
                    let cancel_up = session_cancel_rx.clone();
                    session_handles.push(tokio::spawn(async move {
                        tokio::select! {
                            _ = token_up.cancelled() => {}
                            _ = uplink_tx::run_uplink_tx(
                                writer, cmd_up, state_up, start_up, cancel_up, hb_up_metric, metrics_up
                            ) => {}
                        }
                    }));

                    // Fault Mgr (wrapped with session cancellation)
                    let token_fm = session_token.clone();
                    let state_fm = session_state.clone();
                    let cmd_fm = session_cmd_queue.clone();
                    let start_fm = session_start.clone();
                    let metrics_fm = session_metrics.clone();
                    let cancel_fm = session_cancel_rx.clone();
                    let hb_fm_metric = hb_fault_mgr.clone();
                    session_handles.push(tokio::spawn(async move {
                        tokio::select! {
                            _ = token_fm.cancelled() => {}
                            _ = fault_mgr::run_fault_mgr(
                                s_fault_rx, state_fm, cmd_fm, start_fm, cancel_fm, hb_fm_metric, metrics_fm
                            ) => {}
                        }
                    }));

                    // Heartbeat Task (per session, wrapped with session cancellation)
                    let token_hb = session_token.clone();
                    let h_state = session_state.clone();
                    let h_start = session_start.clone();
                    let h_cmd = session_cmd_queue.clone();
                    let mut h_cancel_global = session_cancel_rx.clone();
                    session_handles.push(tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(5));
                        loop {
                            tokio::select! {
                                _ = h_cancel_global.changed() => break,
                                _ = token_hb.cancelled() => break,
                                _ = interval.tick() => {
                                    let s = h_state.lock().await.clone();
                                    if s == GcsSystemState::LossOfContact { continue; }
                                    let ts = h_start.elapsed().as_micros() as u64;
                                    let pkt = shared::packets::CommandPacket {
                                        seq_no: 0, 
                                        timestamp_us: ts,
                                        cmd_type: shared::packets::CommandType::Heartbeat,
                                        priority: 3,
                                        payload: [0u8; 32],
                                    };
                                    let cmd = PrioritizedCommand { packet: pkt, enqueue_us: ts };
                                    h_cmd.lock().await.push(cmd);
                                }
                            }
                        }
                    }));

                    // Wait for the session to be canceled (e.g. by telemetry_rx)
                    session_token.cancelled().await;
                    tracing::warn!("Satellite session signaling end. Cleaning up handles...");
                    
                    // Small delay to let other tasks see the cancellation
                    tokio::time::sleep(Duration::from_millis(50)).await;

                    // Clean out command queue for next session so old heartbeats don't persist
                    session_cmd_queue.lock().await.clear();

                    // Wait for all session tasks to complete
                    for h in session_handles {
                        let _ = h.await;
                    }
                    tracing::warn!("Satellite session tasks cleaned up. Ready for new connection.");
                }
            }
        }
    });
    global_handles.push(session_task);


    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(args.duration_s)) => {
            tracing::info!("Simulation timer expired. Shutting down.");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl-C received. Shutting down.");
        }
    }

    let _ = cancel_tx.send(true);

    for h in global_handles {
        let _ = h.await;
    }

    // Print final summary log
    tracing::info!(
        total_pkts_received = 0,
        total_pkts_lost = 0,
        pkt_loss_rate_pct = 0.0,
        avg_one_way_latency_us = 0,
        p99_latency_us = 0,
        total_cmds_sent = 0,
        cmds_dispatch_missed = 0,
        total_faults_received = 0,
        avg_interlock_latency_us = 0,
        loss_of_contact_events = 0,
        "=== GCS SIMULATION COMPLETE ==="
    );

    Ok(())
}
