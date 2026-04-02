#![allow(clippy::approx_constant)]

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use asyn_rs::interrupt::{InterruptManager, InterruptValue};
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::sync_io::SyncIOHandle;

// -- Minimal test driver for benchmarking --

struct BenchPort {
    base: PortDriverBase,
}

impl BenchPort {
    fn new(name: &str) -> Self {
        let mut base = PortDriverBase::new(name, 1, PortFlags::default());
        base.create_param("INT_VAL", ParamType::Int32).unwrap();
        base.create_param("F64_VAL", ParamType::Float64).unwrap();
        base.create_param("OCT_VAL", ParamType::Octet).unwrap();
        Self { base }
    }
}

impl PortDriver for BenchPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

// -- Helpers --

fn make_sync_io(name: &str) -> SyncIOHandle {
    let (handle, _jh) = create_port_runtime(BenchPort::new(name), RuntimeConfig::default());
    SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1))
}

// -- Benchmarks --

fn bench_actor_int32_read(c: &mut Criterion) {
    let sio = make_sync_io("bench_int32_r");
    sio.write_int32(0, 42).unwrap();

    c.bench_function("actor_int32_read", |b| {
        b.iter(|| {
            let _ = sio.read_int32(0).unwrap();
        });
    });
}

fn bench_actor_float64_write(c: &mut Criterion) {
    let sio = make_sync_io("bench_f64_w");

    c.bench_function("actor_float64_write", |b| {
        b.iter(|| {
            sio.write_float64(1, 3.14).unwrap();
        });
    });
}

fn bench_actor_octet_roundtrip(c: &mut Criterion) {
    let sio = make_sync_io("bench_oct_rt");
    let data = b"benchmark test data";

    c.bench_function("actor_octet_roundtrip", |b| {
        b.iter(|| {
            sio.write_octet(2, data).unwrap();
            let _ = sio.read_octet(2, 64).unwrap();
        });
    });
}

fn bench_concurrent_32_producers(c: &mut Criterion) {
    let sio = make_sync_io("bench_concurrent");

    // Warm up
    sio.write_int32(0, 0).unwrap();

    c.bench_function("concurrent_32_producers", |b| {
        b.iter(|| {
            let sio_ref = &sio;
            std::thread::scope(|s| {
                for i in 0..32 {
                    s.spawn(move || {
                        sio_ref.write_int32(0, i).unwrap();
                        let _ = sio_ref.read_int32(0).unwrap();
                    });
                }
            });
        });
    });
}

fn bench_interrupt_event_throughput(c: &mut Criterion) {
    let im = InterruptManager::new(4096);
    let _rx = im.subscribe_sync().unwrap();

    c.bench_function("interrupt_event_throughput", |b| {
        b.iter(|| {
            for i in 0..1000 {
                im.notify(InterruptValue {
                    reason: 0,
                    addr: 0,
                    value: ParamValue::Int32(i),
                    timestamp: std::time::SystemTime::now(),
                });
            }
        });
    });
}

criterion_group!(
    benches,
    bench_actor_int32_read,
    bench_actor_float64_write,
    bench_actor_octet_roundtrip,
    bench_concurrent_32_producers,
    bench_interrupt_event_throughput,
);
criterion_main!(benches);
