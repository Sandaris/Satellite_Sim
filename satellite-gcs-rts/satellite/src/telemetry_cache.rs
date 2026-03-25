//! Ring of recently sent telemetry per sensor for GCS RequestTelemetry retransmit.

use std::collections::{HashMap, VecDeque};
use shared::packets::{SensorId, TelemetryPacket};

const PER_SENSOR_CAP: usize = 512;

pub struct TelemetryCache {
    rings: HashMap<SensorId, VecDeque<TelemetryPacket>>,
}

impl TelemetryCache {
    pub fn new() -> Self {
        Self {
            rings: HashMap::new(),
        }
    }

    /// Call after a packet is successfully placed on the wire (downlink).
    pub fn record(&mut self, pkt: TelemetryPacket) {
        let ring = self
            .rings
            .entry(pkt.sensor_id)
            .or_insert_with(|| VecDeque::with_capacity(PER_SENSOR_CAP.min(64)));
        if let Some(pos) = ring.iter().position(|p| p.seq_no == pkt.seq_no) {
            ring.remove(pos);
        }
        ring.push_back(pkt);
        while ring.len() > PER_SENSOR_CAP {
            ring.pop_front();
        }
    }

    pub fn get(&self, sensor: SensorId, seq: u32) -> Option<TelemetryPacket> {
        self.rings
            .get(&sensor)?
            .iter()
            .find(|p| p.seq_no == seq)
            .cloned()
    }
}
