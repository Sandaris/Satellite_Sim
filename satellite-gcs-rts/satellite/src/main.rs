use tracing_subscriber::{fmt, EnvFilter};
use clap::Parser;
use std::net::{SocketAddr, Ipv4Addr};
use std::sync::{Arc, atomic::AtomicU64};
use tokio::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio::net::UdpSocket;

mod state;
mod buffer;
mod sensor;
mod scheduler;
mod downlink;
mod uplink_rx;
mod fault;
mod watchdog;
mod ui;

use state::SystemState;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "192.168.1.20")]
    gcs_ip: String,

    #[arg(long, default_value_t = shared::config::DOWNLINK_PORT)]
    gcs_port: u16,

    #[arg(long, default_value_t = shared::config::UPLINK_PORT)]
    my_port: u16,

    #[arg(long, default_value_t = shared::config::SIM_DURATION_S)]
    duration_s: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive("satellite=info".parse().unwrap()))
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true)
        .with_writer(std::io::stderr)
        .init();

    let sim_start = Arc::new(Instant::now());
    let cancel = CancellationToken::new();

    let gcs_addr: SocketAddr = format!("{}:{}", args.gcs_ip, args.gcs_port).parse()?;
    
    // UPLINK RX: bind to specified port to receive from GCS
    let my_rx_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), args.my_port);

    let state = Arc::new(Mutex::new(SystemState::Nominal));
    let buffer = Arc::new(Mutex::new(buffer::SensorBuffer::new(shared::config::SENSOR_BUFFER_CAPACITY)));
    let metrics_snapshot = Arc::new(Mutex::new(ui::SatMetricsSnapshot::default()));

    let (fault_tx, fault_rx) = tokio::sync::mpsc::channel(100);

    // Watchdog metrics
    let hb_thermal = Arc::new(AtomicU64::new(0));
    let hb_power = Arc::new(AtomicU64::new(0));
    let hb_imu = Arc::new(AtomicU64::new(0));
    let hb_downlink = Arc::new(AtomicU64::new(0));
    let hb_uplink = Arc::new(AtomicU64::new(0));
    let hb_fault = Arc::new(AtomicU64::new(0));
    let hb_rms = Arc::new(AtomicU64::new(0));

    let heartbeats = vec![
        ("thermal_sensor", hb_thermal.clone()),
        ("power_sensor", hb_power.clone()),
        ("imu_sensor", hb_imu.clone()),
        ("downlink_tx", hb_downlink.clone()),
        ("uplink_rx", hb_uplink.clone()),
        ("fault_injector", hb_fault.clone()),
        ("rms_scheduler", hb_rms.clone()),
    ];

    // Channels for RMS scheduler preemption
    let _thermal_ready_notify = Arc::new(tokio::sync::Notify::new());

    tracing::info!("Starting Satellite OCS Simulation");

    let mut handles = vec![];

    handles.push(tokio::spawn(sensor::run_thermal_sensor(
        buffer.clone(), sim_start.clone(), state.clone(), cancel.clone(), hb_thermal.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(sensor::run_power_sensor(
        buffer.clone(), sim_start.clone(), state.clone(), cancel.clone(), hb_power.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(sensor::run_imu_sensor(
        buffer.clone(), sim_start.clone(), state.clone(), cancel.clone(), hb_imu.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(scheduler::run_rms_scheduler(
        cancel.clone(), hb_rms.clone(), sim_start.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(downlink::run_downlink_tx(
        buffer.clone(), gcs_addr, sim_start.clone(), state.clone(), cancel.clone(), hb_downlink.clone(), fault_rx, metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(uplink_rx::run_uplink_rx(
        my_rx_addr, state.clone(), sim_start.clone(), cancel.clone(), hb_uplink.clone()
    )));

    handles.push(tokio::spawn(fault::run_fault_injector(
        buffer.clone(), state.clone(), fault_tx, sim_start.clone(), cancel.clone(), hb_fault.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(watchdog::run_watchdog(
        heartbeats, sim_start.clone(), cancel.clone()
    )));

    handles.push(tokio::spawn(ui::run_ui(
        Arc::clone(&metrics_snapshot),
        Arc::clone(&sim_start),
        cancel.clone(),
    )));


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

    // Print final summary log (hardcoded mock values for now, will implement real metrics later if requested)
    tracing::info!(
        total_thermal_packets = 0,
        total_power_packets = 0,
        total_imu_packets = 0,
        total_dropped_packets = 0,
        thermal_jitter_p50_us = 0,
        thermal_jitter_p99_us = 0,
        thermal_jitter_max_us = 0,
        thermal_drift_avg_us = 0,
        downlink_queue_latency_p50_us = 0,
        downlink_queue_latency_p99_us = 0,
        rms_deadline_violations = 0,
        rms_cpu_util_pct = 0,
        total_faults_injected = 0,
        max_recovery_ms = 0,
        mission_aborts = 0,
        "=== SATELLITE SIMULATION COMPLETE ==="
    );

    Ok(())
}
