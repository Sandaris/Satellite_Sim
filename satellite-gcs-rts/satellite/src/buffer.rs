use std::cmp::Ordering;
use std::collections::BinaryHeap;
use tokio::time::Instant;
use shared::packets::TelemetryPacket;

#[derive(Debug, Clone)]
pub struct SensorReading {
    pub packet:          TelemetryPacket,
    pub buffer_insert_us: u64,
}

impl Ord for SensorReading {
    fn cmp(&self, other: &Self) -> Ordering {
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
        self.packet.timestamp_us == other.packet.timestamp_us
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
    heap:     BinaryHeap<SensorReading>,
    capacity: usize,
    pub stats: BufferStats,
}

impl SensorBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
            stats: BufferStats {
                total_inserted: 0,
                total_dropped: 0,
                peak_fill: 0,
                degraded_mode: false,
            },
        }
    }

    pub fn push(&mut self, reading: SensorReading, _sim_start: &Instant) -> Option<SensorReading> {
        self.stats.total_inserted += 1;
        
        if self.heap.len() < self.capacity {
            self.heap.push(reading);
            self.update_stats();
            return None;
        }

        let mut handle_drop = false;
        let mut dropped_reading = None;
        if let Some(mut min) = self.heap.peek_mut() {
            if reading.cmp(&min) > Ordering::Equal {
                let dropped = min.clone();
                *min = reading.clone();
                handle_drop = true;
                dropped_reading = Some(dropped);
            }
        }
        
        if handle_drop {
            self.stats.total_dropped += 1;
            self.update_stats();
            return dropped_reading;
        }

        self.stats.total_dropped += 1;
        self.update_stats();
        Some(reading)
    }

    pub fn pop(&mut self) -> Option<SensorReading> {
        let res = self.heap.pop();
        self.update_stats();
        res
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn fill_pct(&self) -> f64 {
        self.heap.len() as f64 / self.capacity as f64
    }

    pub fn is_degraded(&self) -> bool {
        self.stats.degraded_mode
    }

    fn update_stats(&mut self) {
        if self.heap.len() > self.stats.peak_fill {
            self.stats.peak_fill = self.heap.len();
        }
        self.stats.degraded_mode = self.fill_pct() >= shared::config::BUFFER_DEGRADED_PCT;
    }
}
