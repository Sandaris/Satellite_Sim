use serde::{Deserialize, Serialize};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PacketType {
    SensorData    = 0,
    FaultNotify   = 1,
    Heartbeat     = 2,
    CommandUplink = 3,
    CommandAck    = 4,
    LossOfContact = 5,
    MissionAbort  = 6,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash, Serialize, Deserialize)]
pub enum SensorId {
    Thermal = 0,   // Priority 1 — Hard RT, period 100ms, jitter < 1ms
    Power   = 1,   // Priority 2 — Firm RT, period 200ms
    Imu     = 2,   // Priority 3 — Soft RT, period 500ms
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TelemetryPacket {
    pub seq_no:          u32,    // monotonically incrementing, per-sensor
    pub timestamp_us:    u64,    // Instant at sender, microseconds since sim start
    pub packet_type:     PacketType,
    pub sensor_id:       SensorId,
    pub priority:        u8,     // 1=highest, 3=lowest
    pub value:           f64,    // sensor reading (temperature C, watts, rad/s)
    pub is_corrupted:    bool,   // set true when fault injection corrupts this packet
    #[serde(with = "serde_arrays")]
    pub payload:         [u8; 64], // fixed padding / future use — initialise to 0
}

impl TelemetryPacket {
    pub fn new(seq_no: u32, ts_us: u64, sensor_id: SensorId, value: f64) -> Self {
        let priority = match sensor_id {
            SensorId::Thermal => 1,
            SensorId::Power   => 2,
            SensorId::Imu     => 3,
        };
        Self { seq_no, timestamp_us: ts_us, packet_type: PacketType::SensorData,
               sensor_id, priority, value, is_corrupted: false, payload: [0u8; 64] }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FaultPacket {
    pub seq_no:          u32,
    pub timestamp_us:    u64,
    pub fault_type:      FaultType,
    pub affected_sensor: SensorId,
    pub severity:        u8,     // 1=low, 2=medium, 3=critical
    #[serde(with = "serde_arrays")]
    pub payload:         [u8; 32],
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FaultType {
    DelayedSensor  = 0,   // sensor data arrived late
    CorruptedData  = 1,   // CRC-like check failed
    MissedDeadline = 2,   // task missed its deadline
    BufferOverflow = 3,   // sensor buffer was full
    SensorOffline  = 4,   // 3+ consecutive misses
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CommandPacket {
    pub seq_no:       u32,
    pub timestamp_us: u64,
    pub cmd_type:     CommandType,
    pub priority:     u8,   // 1=emergency, 2=urgent, 3=routine
    #[serde(with = "serde_arrays")]
    pub payload:      [u8; 32],
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandType {
    NoOp            = 0,
    EmergencyStop   = 1,   // priority 1, dispatch <= 2ms
    SafeMode        = 2,   // priority 1, dispatch <= 2ms
    ResetSensor     = 3,   // priority 2, dispatch <= 2ms
    AdjustAntenna   = 4,   // priority 2
    RequestTelemetry = 5,  // priority 3, re-request for missed packets
    Heartbeat       = 6,   // priority 3, periodic GCS alive signal
}
