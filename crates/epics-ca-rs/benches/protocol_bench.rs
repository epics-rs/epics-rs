use criterion::{Criterion, black_box, criterion_group, criterion_main};
use epics_ca_rs::protocol::*;

fn bench_header_encode(c: &mut Criterion) {
    c.bench_function("ca_header_to_bytes", |b| {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = 6; // DBR_DOUBLE
        hdr.cid = 42;
        hdr.available = 1;
        hdr.count = 1;
        b.iter(|| black_box(&hdr).to_bytes())
    });

    c.bench_function("ca_header_to_bytes_extended", |b| {
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.data_type = 20; // DBR_TIME_DOUBLE
        hdr.set_payload_size(100_000, 12500, epics_ca_rs::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        b.iter(|| black_box(&hdr).to_bytes_extended())
    });
}

fn bench_header_decode(c: &mut Criterion) {
    let hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
    let bytes = hdr.to_bytes();

    c.bench_function("ca_header_from_bytes", |b| {
        b.iter(|| CaHeader::from_bytes(black_box(&bytes)))
    });

    let mut ext_hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
    ext_hdr
        .set_payload_size(100_000, 12500, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    let ext_bytes = ext_hdr.to_bytes_extended();

    c.bench_function("ca_header_from_bytes_extended", |b| {
        b.iter(|| CaHeader::from_bytes_extended(black_box(&ext_bytes)))
    });
}

fn bench_pad_string(c: &mut Criterion) {
    c.bench_function("pad_string_short", |b| {
        b.iter(|| pad_string(black_box("TEMP:VAL")))
    });

    c.bench_function("pad_string_long", |b| {
        b.iter(|| pad_string(black_box("XF:31IDA-BI{Cam:Tbl}:image1:ArrayData")))
    });
}

fn bench_search_payload(c: &mut Criterion) {
    c.bench_function("search_payload_build", |b| {
        b.iter(|| {
            let mut hdr = CaHeader::new(CA_PROTO_SEARCH);
            let payload = pad_string(black_box("TEMP:ai1"));
            hdr.postsize = payload.len() as u16;
            hdr.data_type = 5; // DONT_REPLY
            hdr.count = CA_MINOR_VERSION;
            hdr.cid = 42;
            let mut frame = hdr.to_bytes().to_vec();
            frame.extend_from_slice(&payload);
            frame
        })
    });
}

fn bench_dbr_encode_decode(c: &mut Criterion) {
    use epics_base_rs::server::snapshot::Snapshot;
    use epics_base_rs::types::{EpicsValue, decode_dbr, encode_dbr};

    let snap = Snapshot::new(
        EpicsValue::Double(std::f64::consts::PI),
        0,
        0,
        std::time::SystemTime::now(),
    );

    c.bench_function("encode_dbr_time_double", |b| {
        b.iter(|| encode_dbr(black_box(20), black_box(&snap))) // DBR_TIME_DOUBLE
    });

    let encoded = encode_dbr(20, &snap).unwrap();
    c.bench_function("decode_dbr_time_double", |b| {
        b.iter(|| decode_dbr(black_box(20), black_box(&encoded), 1))
    });

    // Array encode/decode
    let array_snap = Snapshot::new(
        EpicsValue::DoubleArray(vec![1.0; 1024]),
        0,
        0,
        std::time::SystemTime::now(),
    );

    c.bench_function("encode_dbr_time_double_array_1k", |b| {
        b.iter(|| encode_dbr(black_box(20), black_box(&array_snap)))
    });

    let array_encoded = encode_dbr(20, &array_snap).unwrap();
    c.bench_function("decode_dbr_time_double_array_1k", |b| {
        b.iter(|| decode_dbr(black_box(20), black_box(&array_encoded), 1024))
    });
}

criterion_group!(
    benches,
    bench_header_encode,
    bench_header_decode,
    bench_pad_string,
    bench_search_payload,
    bench_dbr_encode_decode,
);
criterion_main!(benches);
