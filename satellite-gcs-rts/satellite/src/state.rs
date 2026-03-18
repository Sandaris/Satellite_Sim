#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemState {
    Nominal,          // all systems go
    Degraded,         // buffer > 80%, downgraded transmission
    Fault,            // active fault — block non-essential ops
    MissionAbort,     // recovery > 200ms — total shutdown
}
