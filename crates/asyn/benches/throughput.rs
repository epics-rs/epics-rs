use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use parking_lot::Mutex;

use asyn_rs::interrupt::{InterruptManager, InterruptValue};
use asyn_rs::manager::PortManager;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::sync_io::SyncIO;

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

fn make_sync_io(name: &str) -> SyncIO {
    let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(BenchPort::new(name)));
    SyncIO::from_port(port, 0, Duration::from_secs(1))
}

fn make_actor_handle(name: &str) -> (PortManager, PortHandle) {
    let mgr = PortManager::new();
    let handle = mgr.register_port_actor(BenchPort::new(name));
    (mgr, handle)
}

// -- Benchmarks --

fn bench_local_int32_read(c: &mut Criterion) {
    let sio = make_sync_io("bench_int32_r");
    sio.write_int32(0, 42).unwrap();

    c.bench_function("local_int32_read", |b| {
        b.iter(|| {
            let _ = sio.read_int32(0).unwrap();
        });
    });
}

fn bench_local_float64_write(c: &mut Criterion) {
    let sio = make_sync_io("bench_f64_w");

    c.bench_function("local_float64_write", |b| {
        b.iter(|| {
            sio.write_float64(1, 3.14).unwrap();
        });
    });
}

fn bench_local_octet_roundtrip(c: &mut Criterion) {
    let sio = make_sync_io("bench_oct_rt");
    let data = b"benchmark test data";

    c.bench_function("local_octet_roundtrip", |b| {
        b.iter(|| {
            sio.write_octet(2, data).unwrap();
            let mut buf = [0u8; 64];
            let _ = sio.read_octet(2, &mut buf).unwrap();
        });
    });
}

fn bench_actor_int32_read(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (_mgr, handle) = rt.block_on(async {
        let (mgr, h) = make_actor_handle("bench_actor_r");
        h.write_int32(0, 0, 42).await.unwrap();
        (mgr, h)
    });

    c.bench_function("actor_int32_read", |b| {
        b.iter(|| {
            handle.read_int32_blocking(0, 0).unwrap();
        });
    });
}

fn bench_concurrent_32_producers(c: &mut Criterion) {
    let port: Arc<Mutex<dyn PortDriver>> =
        Arc::new(Mutex::new(BenchPort::new("bench_concurrent")));
    let sio = SyncIO::from_port(port, 0, Duration::from_secs(1));

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
    bench_local_int32_read,
    bench_local_float64_write,
    bench_local_octet_roundtrip,
    bench_actor_int32_read,
    bench_concurrent_32_producers,
    bench_interrupt_event_throughput,
);
criterion_main!(benches);
