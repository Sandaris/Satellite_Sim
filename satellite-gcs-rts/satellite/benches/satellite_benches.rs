use criterion::{black_box, criterion_group, criterion_main, Criterion};
use shared::packets::{TelemetryPacket, SensorId, FaultPacket, FaultType};
use tokio::time::Instant;

#[path = "../src/buffer.rs"]
#[allow(dead_code)]
mod buffer;

use buffer::{SensorBuffer, SensorReading};

fn bench_buffer_push_pop(c: &mut Criterion) {
    c.bench_function("buffer_push_pop", |b| {
        let sim_start = Instant::now();
        b.iter(|| {
            let mut buf = SensorBuffer::new(100);
            let packet = TelemetryPacket::new(1, 1000, SensorId::Thermal, 25.5);
            let reading = SensorReading {
                packet,
                buffer_insert_us: 1050,
            };
            
            buf.push(black_box(reading), &sim_start);
            let _ = black_box(buf.pop());
        });
    });
}

fn bench_telemetry_serialization(c: &mut Criterion) {
    let packet = TelemetryPacket::new(1, 1000, SensorId::Thermal, 25.5);
    c.bench_function("bincode_serialize_telemetry", |b| {
        b.iter(|| {
            let _encoded = bincode::serialize(black_box(&packet)).unwrap();
        });
    });
}

fn bench_fault_serialization(c: &mut Criterion) {
    let fault = FaultPacket {
        seq_no: 1,
        timestamp_us: 1500,
        fault_type: FaultType::DelayedSensor,
        affected_sensor: SensorId::Thermal,
        severity: 2,
        payload: [0u8; 32],
    };
    c.bench_function("bincode_serialize_fault", |b| {
        b.iter(|| {
            let _encoded = bincode::serialize(black_box(&fault)).unwrap();
        });
    });
}

criterion_group!(benches, bench_buffer_push_pop, bench_telemetry_serialization, bench_fault_serialization);
criterion_main!(benches);
