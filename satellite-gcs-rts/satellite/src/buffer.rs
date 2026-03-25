use std::cmp::Ordering;
use tokio::time::Instant;
use shared::packets::TelemetryPacket;

#[derive(Debug, Clone)]
pub struct SensorReading {
    pub packet:          TelemetryPacket,
    pub buffer_insert_us: u64,
}

impl Ord for SensorReading {
    fn cmp(&self, other: &Self) -> Ordering {
        // Lower priority number (e.g. 1) = Higher priority in System
        // BinaryHeap is a MAX-heap, so we want the "highest" item to be the one with the lowest priority number.
        other.packet.priority.cmp(&self.packet.priority)
            .then_with(|| other.packet.timestamp_us.cmp(&self.packet.timestamp_us))
    }
}

impl PartialOrd for SensorReading {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SensorReading {
    fn eq(&self, other: &Self) -> bool {
        self.packet.priority == other.packet.priority &&
        self.packet.timestamp_us == other.packet.timestamp_us &&
        self.packet.seq_no == other.packet.seq_no &&
        self.packet.sensor_id == other.packet.sensor_id &&
        self.buffer_insert_us == other.buffer_insert_us
    }
}

impl Eq for SensorReading {}

pub struct BufferStats {
    pub total_inserted:  u64,
    pub total_dropped:   u64,
    pub peak_fill:       usize,
    pub degraded_mode:   bool,
}

pub struct SensorBuffer {
    data:     Vec<SensorReading>,
    capacity: usize,
    pub stats: BufferStats,
}

impl SensorBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            capacity,
            stats: BufferStats {
                total_inserted: 0,
                total_dropped: 0,
                peak_fill: 0,
                degraded_mode: false,
            },
        }
    }

    /// Pushes a new reading into the buffer.
    /// If the buffer is full, drops the lowest priority packet to make room.
    pub fn push(&mut self, reading: SensorReading, _sim_start: &Instant) -> Option<SensorReading> {
        self.stats.total_inserted += 1;
        
        if self.data.len() < self.capacity {
            self.data.push(reading);
            self.refresh_stats();
            return None;
        }

        // Buffer is full: evict the lowest-criticality reading (highest priority number).
        // Ord: lower numeric priority = Greater — so the "worst" buffer entry is the min_by Ord.
        let worst_idx = self.data.iter().enumerate()
            .min_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(idx, _)| idx);

        if let Some(idx) = worst_idx {
            // If the incoming reading is BETTER (lower priority number) than the worst one, replace it.
            // Our Ord implementation ranks lower priority numbers as GREATER than higher ones.
            if reading.cmp(&self.data[idx]) == Ordering::Greater {
                let dropped = self.data.swap_remove(idx);
                self.data.push(reading);
                self.stats.total_dropped += 1;
                self.refresh_stats();
                return Some(dropped);
            }
        }
        
        // Incoming packet is lower priority than anything in the buffer: drop it.
        self.stats.total_dropped += 1;
        self.refresh_stats();
        Some(reading)
    }

    /// Returns the highest priority reading from the buffer.
    pub fn pop(&mut self) -> Option<SensorReading> {
        // Dequeue the highest-criticality reading (lowest priority number).
        // Ord: lower numeric priority = Greater — so the "best" entry is the max_by Ord.
        let best_idx = self.data.iter().enumerate()
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(idx, _)| idx);

        if let Some(idx) = best_idx {
            let popped = self.data.swap_remove(idx);
            self.refresh_stats();
            return Some(popped);
        }
        None
    }

    fn refresh_stats(&mut self) {
        if self.data.len() > self.stats.peak_fill {
            self.stats.peak_fill = self.data.len();
        }
        self.stats.degraded_mode = self.fill_pct() >= shared::config::BUFFER_DEGRADED_PCT;
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn fill_pct(&self) -> f64 {
        self.data.len() as f64 / self.capacity as f64
    }

    pub fn is_degraded(&self) -> bool {
        self.stats.degraded_mode
    }
}
