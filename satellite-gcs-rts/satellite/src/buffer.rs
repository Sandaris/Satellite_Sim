use std::cmp::{Ordering, Reverse};
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
    heap:     BinaryHeap<SensorReading>,
    min_heap: BinaryHeap<Reverse<SensorReading>>,
    capacity: usize,
    pub stats: BufferStats,
}

impl SensorBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            min_heap: BinaryHeap::with_capacity(capacity),
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
            self.heap.push(reading.clone());
            self.min_heap.push(Reverse(reading));
            self.update_stats();
            return None;
        }

        // Peak min_heap for lowest priority item (Reverse heap -> peek is min of original)
        if let Some(lowest_item_rev) = self.min_heap.peek() {
            let lowest_item = &lowest_item_rev.0;
            if reading.cmp(lowest_item) > Ordering::Equal {
                // Incoming reading has HIGHER priority than the current lowest.
                // Replace the lowest-priority item.
                let target_to_remove = lowest_item.clone();
                let dropped_reading = Some(target_to_remove.clone());
                
                // Remove from both heaps
                let mut h_vec = self.heap.drain().collect::<Vec<_>>();
                if let Some(pos) = h_vec.iter().position(|r| r == &target_to_remove) {
                    h_vec.swap_remove(pos);
                }
                self.heap = BinaryHeap::from(h_vec);
                
                let mut m_vec = self.min_heap.drain().collect::<Vec<_>>();
                if let Some(pos) = m_vec.iter().position(|r| r.0 == target_to_remove) {
                    m_vec.swap_remove(pos);
                }
                self.min_heap = BinaryHeap::from(m_vec);

                // Push new reading to both
                self.heap.push(reading.clone());
                self.min_heap.push(Reverse(reading.clone()));

                self.update_stats();
                self.stats.total_dropped += 1;
                return dropped_reading;
            }
        }
        
        self.stats.total_dropped += 1;
        self.update_stats();
        
        Some(reading)
    }

    pub fn pop(&mut self) -> Option<SensorReading> {
        let popped = self.heap.pop()?;
        
        // Remove from min_heap to keep in sync
        let mut m_vec = self.min_heap.drain().collect::<Vec<_>>();
        if let Some(pos) = m_vec.iter().position(|r| r.0 == popped) {
            m_vec.swap_remove(pos);
        }
        self.min_heap = BinaryHeap::from(m_vec);

        self.update_stats();
        Some(popped)
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
