#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcsSystemState {
    Nominal,
    InterlockActive,   // fault received — blocking non-emergency commands
    LossOfContact,     // 3+ consecutive missing packets
    CriticalAlert,     // interlock latency > 100ms
}
