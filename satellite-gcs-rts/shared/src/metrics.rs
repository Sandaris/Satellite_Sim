#[derive(Debug, Clone)]
pub struct TaskMetrics {
    pub task_name:        String,
    pub expected_start_us: u64,
    pub actual_start_us:   u64,
    pub execution_time_us: u64,
    pub deadline_us:       u64,
    pub deadline_missed:   bool,
}

impl TaskMetrics {
    pub fn scheduling_drift_us(&self) -> i64 {
        self.actual_start_us as i64 - self.expected_start_us as i64
    }
    pub fn deadline_violation_us(&self) -> Option<u64> {
        let finish = self.actual_start_us + self.execution_time_us;
        if finish > self.deadline_us { Some(finish - self.deadline_us) } else { None }
    }
}

#[derive(Debug, Clone)]
pub struct PacketMetrics {
    pub seq_no:          u32,
    pub send_timestamp:  u64,
    pub recv_timestamp:  u64,
    pub one_way_latency: u64,  // recv - send, microseconds
}
