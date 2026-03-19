use tracing_subscriber::EnvFilter;
use clap::Parser;
use std::sync::{Arc, atomic::AtomicU64};
use tokio::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

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

use tracing_subscriber::prelude::*;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    gcs_ip: String,

    #[arg(long, default_value_t = shared::config::SIM_TCP_PORT)]
    port: u16,

    #[arg(long, default_value_t = shared::config::SIM_DURATION_S)]
    duration_s: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    unsafe { windows_sys::Win32::Media::timeBeginPeriod(1); }

    let args = Args::parse();

    let sim_start = Arc::new(Instant::now());
    let metrics_snapshot = Arc::new(Mutex::new(ui::SatMetricsSnapshot::default()));

    // Setup logging: TUI layer + File layer (no stderr to avoid jumping)
    let tui_layer = ui::TuiLogger::new(metrics_snapshot.clone(), sim_start.clone());
    
    let file_appender = tracing_appender::rolling::never(".", "satellite.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env()
            .add_directive("satellite=info".parse().unwrap()))
        .with(tui_layer)
        .with(file_layer)
        .init();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let state = Arc::new(Mutex::new(SystemState::Nominal));
    let buffer = Arc::new(Mutex::new(buffer::SensorBuffer::new(shared::config::SENSOR_BUFFER_CAPACITY)));

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

    tracing::info!("Starting Satellite OCS Simulation (TCP MODE)");

    // TCP Connection with Retry
    let gcs_addr = format!("{}:{}", args.gcs_ip, args.port);
    tracing::info!("Connecting to Ground Control at {}...", gcs_addr);

    let stream = loop {
        match tokio::net::TcpStream::connect(&gcs_addr).await {
            Ok(s) => break s,
            Err(e) => {
                tracing::warn!("Failed to connect to GCS: {}. Retrying in 1s...", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    tracing::info!("Connected to Ground Control Station.");
    let (reader, writer) = stream.into_split();

    let mut handles = vec![];

    // Background Tasks
    handles.push(tokio::spawn(sensor::run_thermal_sensor(
        buffer.clone(), sim_start.clone(), state.clone(), cancel_rx.clone(), hb_thermal.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(sensor::run_power_sensor(
        buffer.clone(), sim_start.clone(), state.clone(), cancel_rx.clone(), hb_power.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(sensor::run_imu_sensor(
        buffer.clone(), sim_start.clone(), state.clone(), cancel_rx.clone(), hb_imu.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(scheduler::run_rms_scheduler(
        cancel_rx.clone(), hb_rms.clone(), sim_start.clone(), metrics_snapshot.clone()
    )));

    // Communication Tasks (Split TCP)
    handles.push(tokio::spawn(downlink::run_downlink_tx(
        buffer.clone(), writer, sim_start.clone(), state.clone(), cancel_rx.clone(), hb_downlink.clone(), fault_rx, metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(uplink_rx::run_uplink_rx(
        reader, state.clone(), sim_start.clone(), cancel_rx.clone(), hb_uplink.clone()
    )));

    handles.push(tokio::spawn(fault::run_fault_injector(
        buffer.clone(), state.clone(), fault_tx, sim_start.clone(), cancel_rx.clone(), hb_fault.clone(), metrics_snapshot.clone()
    )));

    handles.push(tokio::spawn(watchdog::run_watchdog(
        heartbeats, sim_start.clone(), cancel_rx.clone()
    )));

    handles.push(tokio::spawn(ui::run_ui(
        Arc::clone(&metrics_snapshot),
        Arc::clone(&sim_start),
        cancel_tx.clone(), // Pass the sender so 'q' works
    )));



    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(args.duration_s)) => {
            tracing::info!("Simulation timer expired. Shutting down.");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl-C received. Shutting down.");
        }
    }

    let _ = cancel_tx.send(true);

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
