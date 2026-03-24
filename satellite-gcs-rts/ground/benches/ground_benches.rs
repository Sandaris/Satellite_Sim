use criterion::{black_box, criterion_group, criterion_main, Criterion};
use shared::packets::{TelemetryPacket, SensorId, CommandPacket, CommandType};
use std::collections::BinaryHeap;
use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub struct PrioritizedCommand {
    pub packet: CommandPacket,
    pub enqueue_us: u64,
}

impl Ord for PrioritizedCommand {
    fn cmp(&self, other: &Self) -> Ordering {
        other.packet.priority.cmp(&self.packet.priority)
            .then_with(|| other.enqueue_us.cmp(&self.enqueue_us))
    }
}

impl PartialOrd for PrioritizedCommand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PrioritizedCommand {
    fn eq(&self, other: &Self) -> bool {
        self.packet.priority == other.packet.priority &&
        self.enqueue_us == other.enqueue_us
    }
}

impl Eq for PrioritizedCommand {}

fn bench_command_queue(c: &mut Criterion) {
    c.bench_function("command_heap_push_pop", |b| {
        b.iter(|| {
            let mut heap = BinaryHeap::new();
            
            let cmd1 = PrioritizedCommand {
                packet: CommandPacket { seq_no: 1, timestamp_us: 100, cmd_type: CommandType::SafeMode, priority: 1, payload: [0u8; 32] },
                enqueue_us: 110,
            };
            let cmd2 = PrioritizedCommand {
                packet: CommandPacket { seq_no: 2, timestamp_us: 200, cmd_type: CommandType::Heartbeat, priority: 3, payload: [0u8; 32] },
                enqueue_us: 210,
            };
            let cmd3 = PrioritizedCommand {
                packet: CommandPacket { seq_no: 3, timestamp_us: 300, cmd_type: CommandType::ResetSensor, priority: 2, payload: [0u8; 32] },
                enqueue_us: 310,
            };

            heap.push(black_box(cmd1));
            heap.push(black_box(cmd2));
            heap.push(black_box(cmd3));

            black_box(heap.pop());
            black_box(heap.pop());
            black_box(heap.pop());
        });
    });
}

fn bench_telemetry_deserialization(c: &mut Criterion) {
    let packet = TelemetryPacket::new(1, 1000, SensorId::Thermal, 25.5);
    let encoded = bincode::serialize(&packet).unwrap();
    
    c.bench_function("bincode_deserialize_telemetry", |b| {
        b.iter(|| {
            let _decoded: TelemetryPacket = bincode::deserialize(black_box(&encoded)).unwrap();
        });
    });
}

criterion_group!(benches, bench_command_queue, bench_telemetry_deserialization);
criterion_main!(benches);
