#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemState {
    Nominal,          // all systems go
    Degraded,         // buffer > 80%, downgraded transmission
    SafeMode,         // ground-commanded safe — distinct from autonomous Fault
    Fault,            // autonomous / sensor fault — block non-essential ops
    MissionAbort,     // recovery > 200ms — total shutdown
}
