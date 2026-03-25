// ── Network ───────────────────────────────────────────────────────────
pub const DEFAULT_SAT_IP:      &str = "127.0.0.1";
pub const DEFAULT_GCS_IP:      &str = "127.0.0.1";
pub const SIM_TCP_PORT:        u16  = 8888;  // Unified bi-directional link
pub const UDP_MAX_PAYLOAD:     usize = 512;   

// ── Sensor Periods (milliseconds) ────────────────────────────────────
pub const THERMAL_PERIOD_MS:   u64 = 100;
pub const POWER_PERIOD_MS:     u64 = 200;
pub const IMU_PERIOD_MS:       u64 = 500;

// ── Jitter Limits ─────────────────────────────────────────────────────
/// Max allowed |actual_period − nominal_period| per sensor (µs), host simulation.
/// Flight specs are often stricter; Tokio wakeups on Windows/desktop add several ms
/// of spacing variance versus the nominal period.
pub const THERMAL_JITTER_LIMIT_US: u64 = 900;
pub const POWER_JITTER_LIMIT_US: u64 = 15_000;
pub const IMU_JITTER_LIMIT_US: u64 = 25_000;

// ── Buffer ────────────────────────────────────────────────────────────
pub const SENSOR_BUFFER_CAPACITY:  usize = 64;
pub const BUFFER_DEGRADED_PCT:     f64   = 0.80; // 80% → degraded mode

// ── Downlink Timing ───────────────────────────────────────────────────
pub const DOWNLINK_WINDOW_MS:      u64 = 30;  // spec: prepare data within 30ms visibility window
pub const DOWNLINK_INIT_TIMEOUT_MS: u64 = 5; // spec: pipeline ready within 5ms or missed communication window

// ── Safety Alerts ─────────────────────────────────────────────────────
pub const THERMAL_MISS_ALERT:      u32 = 3;  // spec: >3 consecutive misses → safety alert
pub const GCS_PACKET_LOSS_ALERT:   u32 = 10;  
pub const TELEMETRY_DECODE_MS:     u64 = 3;
pub const CMD_DISPATCH_MS:         u64 = 2;   // urgent deadline <= 2ms

// ── Fault Injection ───────────────────────────────────────────────────
/// Seconds after startup before each fault injection. Runs shorter than this never inject.
pub const FAULT_INJECT_INTERVAL_S: u64 = 60;
pub const FAULT_RECOVERY_LIMIT_MS: u64 = 200; // recovery must complete < 200ms
pub const GCS_INTERLOCK_LIMIT_MS:  u64 = 100; // interlock must apply < 100ms

// ── Watchdog ──────────────────────────────────────────────────────────
pub const WATCHDOG_CHECK_INTERVAL_S: u64 = 1;
pub const WATCHDOG_TIMEOUT_S:        u64 = 10;

// ── Scheduler (RMS) ───────────────────────────────────────────────────
pub const THERMAL_CTRL_PERIOD_MS:    u64 = 100;
pub const DATA_COMPRESS_PERIOD_MS:   u64 = 500;
pub const HEALTH_MONITOR_PERIOD_MS:  u64 = 1_000;

// ── Simulation ────────────────────────────────────────────────────────
pub const SIM_DURATION_S: u64 = 300; // 5-minute simulation run
