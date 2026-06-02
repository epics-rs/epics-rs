#![allow(clippy::approx_constant)]

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use epics_base_rs::calc;
use epics_base_rs::types::EpicsValue;

fn bench_calc_compile(c: &mut Criterion) {
    c.bench_function("calc_compile_simple", |b| {
        b.iter(|| calc::compile(black_box("A+B*C")))
    });
    c.bench_function("calc_compile_complex", |b| {
        b.iter(|| calc::compile(black_box("(A>0?B+C*D:E-F)*SIN(G)+ABS(H-I)")))
    });
}

fn bench_calc_eval(c: &mut Criterion) {
    let simple = calc::compile("A+B*C").unwrap();
    let complex = calc::compile("(A>0?B+C*D:E-F)*SIN(G)+ABS(H-I)").unwrap();

    c.bench_function("calc_eval_simple", |b| {
        let mut inputs = calc::NumericInputs::default();
        inputs.vars[0] = 1.0; // A
        inputs.vars[1] = 2.0; // B
        inputs.vars[2] = 3.0; // C
        b.iter(|| calc::eval(black_box(&simple), &mut inputs))
    });

    c.bench_function("calc_eval_complex", |b| {
        let mut inputs = calc::NumericInputs::default();
        inputs.vars[0] = 1.0; // A
        inputs.vars[1] = 2.0; // B
        inputs.vars[2] = 3.0; // C
        inputs.vars[3] = 4.0; // D
        inputs.vars[4] = 5.0; // E
        inputs.vars[5] = 6.0; // F
        inputs.vars[6] = 0.5; // G
        inputs.vars[7] = 10.0; // H
        inputs.vars[8] = 3.0; // I
        b.iter(|| calc::eval(black_box(&complex), &mut inputs))
    });
}

fn bench_calc_one_shot(c: &mut Criterion) {
    c.bench_function("calc_one_shot", |b| {
        let mut inputs = calc::NumericInputs::default();
        inputs.vars[0] = 42.0; // A
        inputs.vars[1] = 3.14; // B
        b.iter(|| calc::calc(black_box("A*B+1.0"), &mut inputs))
    });
}

fn bench_epics_value(c: &mut Criterion) {
    c.bench_function("epics_value_double_to_bytes", |b| {
        let val = EpicsValue::Double(3.14159);
        b.iter(|| black_box(&val).to_bytes())
    });

    c.bench_function("epics_value_double_array_to_bytes", |b| {
        let val = EpicsValue::DoubleArray(vec![1.0; 1024]);
        b.iter(|| black_box(&val).to_bytes())
    });

    c.bench_function("epics_value_string_to_bytes", |b| {
        let val = EpicsValue::String("Hello EPICS".into());
        b.iter(|| black_box(&val).to_bytes())
    });
}

fn bench_link_parsing(c: &mut Criterion) {
    use epics_base_rs::server::record::parse_link_v2;

    c.bench_function("parse_link_db_simple", |b| {
        b.iter(|| parse_link_v2(black_box("TEMP.VAL")))
    });

    c.bench_function("parse_link_db_with_attrs", |b| {
        b.iter(|| parse_link_v2(black_box("MOTOR:RBV.VAL CP MS")))
    });

    c.bench_function("parse_link_constant", |b| {
        b.iter(|| parse_link_v2(black_box("3.14159")))
    });

    c.bench_function("parse_link_ca", |b| {
        b.iter(|| parse_link_v2(black_box("ca://REMOTE:PV.VAL")))
    });
}

criterion_group!(
    benches,
    bench_calc_compile,
    bench_calc_eval,
    bench_calc_one_shot,
    bench_epics_value,
    bench_link_parsing,
);
criterion_main!(benches);
