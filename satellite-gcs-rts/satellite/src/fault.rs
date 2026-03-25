use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use shared::packets::{FaultPacket, FaultType, SensorId};
use shared::config::{FAULT_INJECT_INTERVAL_S, FAULT_RECOVERY_LIMIT_MS};
use crate::buffer::SensorBuffer;
use crate::state::SystemState;

pub enum CircuitState { Closed, Open(Instant), #[allow(dead_code)] HalfOpen }

pub struct FaultEngine {
    pub circuit: CircuitState,
    pub consecutive_faults: u32,
    pub last_fault_at: Option<Instant>,
    pub total_faults: u64,
    pub total_recoveries: u64,
    pub max_recovery_ms: u64,
}

pub async fn run_fault_injector(
    _buffer:    Arc<Mutex<SensorBuffer>>,
    state:     Arc<Mutex<SystemState>>,
    fault_tx:  tokio::sync::mpsc::Sender<FaultPacket>,
    sim_start: Arc<Instant>,
    mut cancel:    tokio::sync::watch::Receiver<bool>,
    heartbeat: Arc<AtomicU64>,
    ui_metrics: Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
    corrupt_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(FAULT_INJECT_INTERVAL_S));
    interval.tick().await; // skip first 
    let mut engine = FaultEngine { circuit: CircuitState::Closed, consecutive_faults: 0, last_fault_at: None, total_faults: 0, total_recoveries: 0, max_recovery_ms: 0 };
    let mut toggle = false;
    let mut seconds_since_last_inject = 0u64;

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
        seconds_since_last_inject += 1;

        if let CircuitState::Open(_) = engine.circuit {
            tracing::info!("Circuit is OPEN (MissionAbort) - stopping fault injector");
            break;
        }

        if seconds_since_last_inject < FAULT_INJECT_INTERVAL_S {
            continue;
        }
        seconds_since_last_inject = 0;

        let fault_start = Instant::now();
        engine.last_fault_at = Some(fault_start);
        let fault_type = if toggle { FaultType::CorruptedData } else { FaultType::DelayedSensor };
        toggle = !toggle;

        let elapsed_us = sim_start.elapsed().as_micros() as u64;
        tracing::warn!(fault=?fault_type, severity=2, elapsed_us, "FAULT INJECTED");
        crate::ui::push_log(&ui_metrics, 1, format!("FAULT INJECTED: {:?}", fault_type), &sim_start);

        // STEP 1: Set satellite to Fault state NOW so wait_for_nominal
        // measures the REAL round-trip (Satellite→Fault → GCS detects → GCS sends
        // ResetSensor → Satellite→Nominal). Without this the state was never
        // changed and recovery completed in 0ms (a lie).
        {
            let mut s = state.lock().await;
            if matches!(
                *s,
                SystemState::Nominal | SystemState::Degraded | SystemState::SafeMode
            ) {
                *s = SystemState::Fault;
                tracing::info!(fault=?fault_type, elapsed_us, "satellite state → Fault (fault injection)");
            }
        }

        // STEP 2: Simulate the fault condition itself
        match fault_type {
            FaultType::DelayedSensor => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            FaultType::CorruptedData => {
                corrupt_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(150)).await;
                corrupt_flag.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }

        // STEP 3: Notify GCS. Recovery timer starts NOW — measuring the
        // true GCS round-trip latency (detect → interlock → ResetSensor → Nominal).
        let recovery_start = Instant::now();
        let fault_pkt = FaultPacket {
            seq_no: engine.total_faults as u32,
            timestamp_us: sim_start.elapsed().as_micros() as u64,
            fault_type, affected_sensor: SensorId::Thermal,
            severity: 2, payload: [0u8; 32]
        };
        let _ = fault_tx.send(fault_pkt).await;
        tracing::info!(elapsed_us=sim_start.elapsed().as_micros() as u64,
                       "fault_pkt sent to GCS — awaiting ResetSensor command");

        // STEP 4: Wait for satellite state to return to Nominal.
        // This only resolves when uplink_rx processes the GCS ResetSensor command.
        let recovery_result = tokio::time::timeout(
            Duration::from_millis(FAULT_RECOVERY_LIMIT_MS),
            wait_for_nominal(&state, &mut cancel)
        ).await;

        let recovery_ms = recovery_start.elapsed().as_millis() as u64;
        engine.max_recovery_ms = engine.max_recovery_ms.max(recovery_ms);

        match recovery_result {
            Ok(_) => {
                tracing::info!(recovery_ms, elapsed_us=sim_start.elapsed().as_micros() as u64,
                               "FAULT RECOVERED — satellite state → Nominal");
                crate::ui::push_log(&ui_metrics, 0, format!("FAULT RECOVERED in {}ms", recovery_ms), &sim_start);
                engine.circuit = CircuitState::Closed;
                engine.total_recoveries += 1;
                engine.consecutive_faults = 0;
            }
            Err(_) => {
                tracing::error!(recovery_ms, elapsed_us=sim_start.elapsed().as_micros() as u64,
                                limit_ms=FAULT_RECOVERY_LIMIT_MS,
                                "RECOVERY EXCEEDED 200ms — MISSION ABORT");
                crate::ui::push_log(&ui_metrics, 2, "RECOVERY EXCEEDED 200ms — MISSION ABORT".to_string(), &sim_start);
                let mut s = state.lock().await;
                if *s != SystemState::MissionAbort { *s = SystemState::MissionAbort; }
                engine.circuit = CircuitState::Open(Instant::now());
                engine.consecutive_faults += 1;
            }
        }
        
        engine.total_faults += 1;

        if let Ok(mut m) = ui_metrics.try_lock() {
            m.fault_total_injected = engine.total_faults as u64;
            m.fault_total_recovered = engine.total_recoveries;
            m.fault_next_in_s = FAULT_INJECT_INTERVAL_S;
            m.fault_next_fire_at_s = sim_start.elapsed().as_secs() + FAULT_INJECT_INTERVAL_S;
            m.fault_last_type = format!("{:?}", fault_type);
            m.fault_last_recovery_ms = recovery_ms;
            m.fault_max_recovery_ms = engine.max_recovery_ms;
            m.fault_circuit_state = match engine.circuit {
                CircuitState::Closed => "CLOSED".to_string(),
                CircuitState::HalfOpen => "HALF-OPEN".to_string(),
                CircuitState::Open(t) => {
                    let open_duration = t.elapsed().as_secs();
                    format!("OPEN ({}s)", open_duration)
                }
            };
            if let CircuitState::Open(_) = engine.circuit { m.mission_aborts += 1; }
        }
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
    
    tracing::info!(total_faults=engine.total_faults, total_recoveries=engine.total_recoveries,
                   max_recovery_ms=engine.max_recovery_ms, "fault_injector final stats");
}

async fn wait_for_nominal(state: &Arc<Mutex<SystemState>>, cancel: &mut tokio::sync::watch::Receiver<bool>) {
    let mut check_interval = tokio::time::interval(Duration::from_millis(10));
    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            _ = check_interval.tick() => {
                if *state.lock().await == SystemState::Nominal {
                    break;
                }
            }
        }
    }
}
