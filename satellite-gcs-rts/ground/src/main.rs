use tracing_subscriber::{fmt, EnvFilter};
use clap::Parser;
use std::net::{SocketAddr, Ipv4Addr};
use std::sync::{Arc, atomic::AtomicU64};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tokio::net::UdpSocket;
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

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "192.168.1.10")]
    sat_ip: String,

    #[arg(long, default_value_t = shared::config::UPLINK_PORT)]
    sat_port: u16,

    #[arg(long, default_value_t = shared::config::DOWNLINK_PORT)]
    my_port: u16,

    #[arg(long, default_value_t = shared::config::SIM_DURATION_S)]
    duration_s: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive("ground=info".parse().unwrap()))
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true)
        .with_writer(std::io::stderr)
        .init();

    let sim_start = Arc::new(Instant::now());
    let cancel = CancellationToken::new();

    let sat_addr: SocketAddr = format!("{}:{}", args.sat_ip, args.sat_port).parse()?;
    
    // DOWNLINK RX: bind to specified port to receive from Satellite
    let my_rx_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), args.my_port);

    let state = Arc::new(Mutex::new(GcsSystemState::Nominal));
    let cmd_queue = Arc::new(Mutex::new(BinaryHeap::<PrioritizedCommand>::new()));
    let metrics_snapshot = Arc::new(Mutex::new(ui::GcsMetricsSnapshot::default()));

    let (fault_tx, fault_rx) = tokio::sync::mpsc::channel(100);

    // Watchdog metrics
    let hb_telemetry = Arc::new(AtomicU64::new(0));
    let hb_uplink = Arc::new(AtomicU64::new(0));
    let hb_fault_mgr = Arc::new(AtomicU64::new(0));

    let heartbeats = vec![
        ("telemetry_rx", hb_telemetry.clone()),
        ("uplink_tx", hb_uplink.clone()),
        ("fault_mgr", hb_fault_mgr.clone()),
    ];

    tracing::info!("Starting Ground Control Station Simulation");

    let mut handles = vec![];

    handles.push(tokio::spawn(telemetry_rx::run_telemetry_rx(
        my_rx_addr, state.clone(), fault_tx, cmd_queue.clone(), sim_start.clone(), cancel.clone(), hb_telemetry.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(uplink_tx::run_uplink_tx(
        sat_addr, cmd_queue.clone(), state.clone(), sim_start.clone(), cancel.clone(), hb_uplink.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(fault_mgr::run_fault_mgr(
        fault_rx, state.clone(), cmd_queue.clone(), sim_start.clone(), cancel.clone(), hb_fault_mgr.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(perf_monitor::run_perf_monitor(
        sim_start.clone(), cancel.clone()
    )));

    handles.push(tokio::spawn(watchdog::run_watchdog(
        heartbeats, sim_start.clone(), cancel.clone()
    )));

    handles.push(tokio::spawn(ui::run_ui(
        Arc::clone(&metrics_snapshot),
        Arc::clone(&sim_start),
        cancel.clone(),
    )));

    // Heartbeat Task - sends CommandType::Heartbeat every 5s
    let state_clone = state.clone();
    let sim_start_clone = sim_start.clone();
    let cmd_queue_clone = cmd_queue.clone();
    let cancel_clone = cancel.clone();
    handles.push(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => break,
                _ = interval.tick() => {
                    let s = state_clone.lock().await.clone();
                    if s == GcsSystemState::LossOfContact {
                        continue;
                    }

                    let ts = sim_start_clone.elapsed().as_micros() as u64;
                    let pkt = shared::packets::CommandPacket {
                        seq_no: 0, 
                        timestamp_us: ts,
                        cmd_type: shared::packets::CommandType::Heartbeat,
                        priority: 3,
                        payload: [0u8; 32],
                    };

                    let cmd = PrioritizedCommand { packet: pkt, enqueue_us: ts };
                    cmd_queue_clone.lock().await.push(cmd);
                }
            }
        }
    }));


    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(args.duration_s)) => {
            tracing::info!("Simulation timer expired. Shutting down.");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl-C received. Shutting down.");
        }
    }

    cancel.cancel();

    for h in handles {
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
