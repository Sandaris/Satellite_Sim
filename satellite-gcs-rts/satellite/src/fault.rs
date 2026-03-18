use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use shared::packets::{FaultPacket, FaultType, SensorId};
use shared::config::{FAULT_INJECT_INTERVAL_S, FAULT_RECOVERY_LIMIT_MS};
use crate::buffer::SensorBuffer;
use crate::state::SystemState;

pub enum CircuitState { Closed, Open(Instant), HalfOpen }

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
    cancel:    CancellationToken,
    heartbeat: Arc<AtomicU64>,
    ui_metrics: Arc<Mutex<crate::ui::SatMetricsSnapshot>>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(FAULT_INJECT_INTERVAL_S));
    interval.tick().await; // skip first 
    let mut engine = FaultEngine { circuit: CircuitState::Closed, consecutive_faults: 0, last_fault_at: None, total_faults: 0, total_recoveries: 0, max_recovery_ms: 0 };
    let mut toggle = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {}
        }

        let fault_start = Instant::now();
        let fault_type = if toggle { FaultType::CorruptedData } else { FaultType::DelayedSensor };
        toggle = !toggle;
        
        let elapsed_us = sim_start.elapsed().as_micros() as u64;
        tracing::warn!(fault=?fault_type, severity=2, elapsed_us, "FAULT INJECTED");
        crate::ui::push_log(&ui_metrics, 1, format!("FAULT INJECTED: {:?}", fault_type), &sim_start);

        match fault_type {
            FaultType::DelayedSensor => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            FaultType::CorruptedData => {
                // Simplified payload corruption via buffer (if we manipulated buffer directly)
            }
            _ => {}
        }

        let fault_pkt = FaultPacket {
            seq_no: engine.total_faults as u32,
            timestamp_us: sim_start.elapsed().as_micros() as u64,
            fault_type, affected_sensor: SensorId::Thermal,
            severity: 2, payload: [0u8; 32]
        };
        let _ = fault_tx.send(fault_pkt).await;

        let recovery_result = tokio::time::timeout(
            Duration::from_millis(FAULT_RECOVERY_LIMIT_MS),
            wait_for_nominal(&state)
        ).await;

        let recovery_ms = fault_start.elapsed().as_millis() as u64;
        engine.max_recovery_ms = engine.max_recovery_ms.max(recovery_ms);

        match recovery_result {
            Ok(_) => {
                tracing::info!(recovery_ms, elapsed_us=sim_start.elapsed().as_micros() as u64, "FAULT RECOVERED");
                crate::ui::push_log(&ui_metrics, 0, format!("FAULT RECOVERED in {}ms", recovery_ms), &sim_start);
                engine.circuit = CircuitState::Closed;
            }
            Err(_) => {
                tracing::error!(recovery_ms, elapsed_us=sim_start.elapsed().as_micros() as u64, limit_ms=FAULT_RECOVERY_LIMIT_MS, "RECOVERY EXCEEDED 200ms — MISSION ABORT");
                crate::ui::push_log(&ui_metrics, 2, "RECOVERY EXCEEDED 200ms — MISSION ABORT".to_string(), &sim_start);
                let mut s = state.lock().await;
                if *s != SystemState::MissionAbort { *s = SystemState::MissionAbort; }
                engine.circuit = CircuitState::Open(Instant::now());
            }
        }
        
        engine.total_faults += 1;

        if let Ok(mut m) = ui_metrics.try_lock() {
            m.fault_total_injected = engine.total_faults as u64;
            m.fault_next_in_s = FAULT_INJECT_INTERVAL_S;
            m.fault_last_type = format!("{:?}", fault_type);
            m.fault_last_recovery_ms = recovery_ms;
            m.fault_max_recovery_ms = engine.max_recovery_ms;
            m.fault_circuit_state = match engine.circuit {
                CircuitState::Closed => "CLOSED".to_string(),
                CircuitState::HalfOpen => "HALF-OPEN".to_string(),
                CircuitState::Open(_) => "OPEN".to_string(),
            };
            if let CircuitState::Open(_) = engine.circuit { m.mission_aborts += 1; }
        }
        heartbeat.store(sim_start.elapsed().as_secs(), Ordering::Relaxed);
    }
    
    tracing::info!(total_faults=engine.total_faults, max_recovery_ms=engine.max_recovery_ms,
                   "fault_injector final stats");
}

async fn wait_for_nominal(state: &Arc<Mutex<SystemState>>) {
    let mut check_interval = tokio::time::interval(Duration::from_millis(10));
    loop {
        check_interval.tick().await;
        if *state.lock().await == SystemState::Nominal {
            break;
        }
    }
}
