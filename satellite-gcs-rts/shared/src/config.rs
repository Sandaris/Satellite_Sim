// ── Network ───────────────────────────────────────────────────────────
pub const DEFAULT_SAT_IP:      &str = "192.168.1.10";
pub const DEFAULT_GCS_IP:      &str = "192.168.1.20";
pub const DOWNLINK_PORT:       u16  = 8080;  // OCS sends → GCS listens
pub const UPLINK_PORT:         u16  = 9090;  // GCS sends → OCS listens
pub const UDP_MAX_PAYLOAD:     usize = 512;   // max UDP payload bytes

// ── Sensor Periods (milliseconds) ────────────────────────────────────
pub const THERMAL_PERIOD_MS:   u64 = 100;
pub const POWER_PERIOD_MS:     u64 = 200;
pub const IMU_PERIOD_MS:       u64 = 500;

// ── Jitter Limits ─────────────────────────────────────────────────────
pub const THERMAL_JITTER_LIMIT_US: u64 = 1_000; // 1ms — Hard RT

// ── Buffer ────────────────────────────────────────────────────────────
pub const SENSOR_BUFFER_CAPACITY:  usize = 64;
pub const BUFFER_DEGRADED_PCT:     f64   = 0.80; // 80% → degraded mode

// ── Downlink Timing ───────────────────────────────────────────────────
pub const DOWNLINK_WINDOW_MS:      u64 = 30;  // must send within 30ms
pub const DOWNLINK_INIT_TIMEOUT_MS: u64 = 5;  // socket init deadline

// ── Safety Alerts ─────────────────────────────────────────────────────
pub const THERMAL_MISS_ALERT:      u32 = 3;   // alert after 3 consecutive misses
pub const GCS_PACKET_LOSS_ALERT:   u32 = 3;   // loss of contact after 3 seq gaps
pub const TELEMETRY_DECODE_MS:     u64 = 3;   // GCS must decode within 3ms
pub const CMD_DISPATCH_MS:         u64 = 2;   // urgent cmd dispatch ≤ 2ms

// ── Fault Injection ───────────────────────────────────────────────────
pub const FAULT_INJECT_INTERVAL_S: u64 = 60;  // inject every 60 seconds
pub const FAULT_RECOVERY_LIMIT_MS: u64 = 200; // recovery must complete < 200ms
pub const GCS_INTERLOCK_LIMIT_MS:  u64 = 100; // interlock must apply < 100ms

// ── Watchdog ──────────────────────────────────────────────────────────
pub const WATCHDOG_CHECK_INTERVAL_S: u64 = 1;
pub const WATCHDOG_TIMEOUT_S:        u64 = 5;

// ── Scheduler (RMS) ───────────────────────────────────────────────────
pub const THERMAL_CTRL_PERIOD_MS:    u64 = 100;
pub const DATA_COMPRESS_PERIOD_MS:   u64 = 500;
pub const HEALTH_MONITOR_PERIOD_MS:  u64 = 1_000;

// ── Simulation ────────────────────────────────────────────────────────
pub const SIM_DURATION_S: u64 = 300; // 5-minute simulation run
