//! `tableRecord`'s eight `special(SPC_DBADDR)` fields serve the arrays C's
//! `cvt_dbaddr` describes.
//!
//! ```c
//! /* tableRecord.c:727-745 */
//! static long cvt_dbaddr(DBADDR *paddr) {
//!     ...
//!     if (fieldIndex == tableRecordA) {
//!         paddr->pfield = ptbl->a[0];  paddr->no_elements = 9;
//!     } else if (fieldIndex == tableRecordB) {
//!         paddr->pfield = ptbl->b[0];  paddr->no_elements = 9;
//!     } else {
//!         paddr->pfield = pp0[pfield - pp0];  paddr->no_elements = 3;
//!     }
//!     paddr->field_type = DBF_DOUBLE;
//!     ...
//! }
//! ```
//!
//! `tableRecord.dbd:828-883` declares all eight `DBF_NOACCESS` — the standard
//! idiom (`waveformRecord.dbd.pod:401-405` does the same for `VAL`) where the
//! declaration carries no type and `cvt_dbaddr` supplies it. The two branches
//! of that `if` are the count boundary this file tests: nine for the matrices,
//! three for each pivot point.
//!
//! The port has no `Special::DbAddr` inspection and needs none for the VALUE
//! half: an array `EpicsValue` already carries the element type `cvt_dbaddr`
//! sets and the count it reports, and `project_to_declared_type` short-circuits
//! on it because `DoubleArray::db_field_type()` is `Double`. The redirect is
//! the record's own `get_field` arm, which is where every other `SPC_DBADDR`
//! field in the workspace already does it (`acalcout.AVAL`, `asyn.BOUT`).
//!
//! `cvt_dbaddr` runs in `dbNameToAddr`, so the redirect it installs is the
//! channel — it is not read-only. The port had the read arms and no write arms,
//! so all eight fields the `.dbd` declares writable (`read_only: false` in
//! `dbd_generated.rs`) rejected every put with `FieldNotFound`. The second
//! group below is the write half.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

/// The geometry the C golden harness uses, with `rx=ry=rz=0` so `fx=sx`,
/// `fy=sy`, `fz=sz` (`tableRecord.c:1153-1155`).
fn initialised() -> TableRecord {
    let mut t = TableRecord::default();
    t.lx = 200.0;
    t.lz = 300.0;
    t.sx = 100.0;
    t.sy = 50.0;
    t.sz = 150.0;
    t.init_record(0).unwrap();
    t.init_record(1).unwrap();
    t
}

fn doubles(rec: &TableRecord, field: &str) -> Vec<f64> {
    match rec.get_field(field) {
        Some(EpicsValue::DoubleArray(v)) => v,
        other => panic!("{field} served {other:?}, not a DoubleArray"),
    }
}

/// `cvt_dbaddr`'s first two arms: `no_elements = 9`, the 3x3 flattened.
#[test]
fn the_two_matrices_serve_nine_elements() {
    let t = initialised();
    assert_eq!(doubles(&t, "A").len(), 9);
    assert_eq!(doubles(&t, "B").len(), 9);
}

/// `cvt_dbaddr`'s `else` arm: every pivot point is `no_elements = 3`.
#[test]
fn every_pivot_point_serves_three_elements() {
    let t = initialised();
    for f in ["PP0", "PP1", "PP2", "PPO0", "PPO1", "PPO2"] {
        assert_eq!(doubles(&t, f).len(), 3, "{f}");
    }
}

/// The pivot values C's SRI branch computes (`tableRecord.c:1194-1205`), with
/// `pp` and `ppo` seeded to the same vectors by the same statements.
#[test]
fn the_pivots_hold_the_vectors_c_computes_for_sri() {
    let t = initialised();
    // lx - fx, -fy, -fz  /  -fx, -fy, -fz  /  lx/2 - fx, -fy, lz - fz
    assert_eq!(doubles(&t, "PP0"), vec![100.0, -50.0, -150.0]);
    assert_eq!(doubles(&t, "PP1"), vec![-100.0, -50.0, -150.0]);
    assert_eq!(doubles(&t, "PP2"), vec![0.0, -50.0, 150.0]);
    assert_eq!(doubles(&t, "PPO0"), doubles(&t, "PP0"));
    assert_eq!(doubles(&t, "PPO1"), doubles(&t, "PP1"));
    assert_eq!(doubles(&t, "PPO2"), doubles(&t, "PP2"));
}

/// C flattens through `ptbl->a[0]` with `a[i] = a[i-1] + 3`
/// (`tableRecord.c:358-361`), i.e. ROW-major: `bb[r][c]` lands at `3*r + c`.
///
/// The three distinct magnitudes at flat 5, 6 and 7 are what make this an
/// order test and not just a value test — a column-major flatten would move
/// `bb[1][2]` from index 5 to index 7.
#[test]
fn the_inverse_matrix_is_flattened_row_major() {
    let t = initialised();
    let b = doubles(&t, "B");
    // det = 3.6e9 for this geometry; every entry is C's bb[r][c] closed form
    // at `tableRecord.c:1231-1238`.
    let expect = [
        -0.005,
        0.0,
        0.0,
        0.0,
        0.0,
        1.666_666_666_666_666_7e-5,
        0.001_666_666_666_666_666_8,
        0.003_333_333_333_333_333_5,
        -0.0,
    ];
    for (i, want) in expect.iter().enumerate() {
        assert!(
            (b[i] - want).abs() < 1e-18,
            "B[{i}] = {}, want {want}",
            b[i]
        );
    }
}

/// The declared type is a `DBF_DOUBLE` SCALAR — `cvt_dbaddr` is what makes the
/// channel an array — so the array must survive
/// `project_to_declared_type`, which `client_field_value` runs on every CA
/// GET/MONITOR. It does, because `DoubleArray::db_field_type()` is `Double`
/// and `convert_to` short-circuits an equal type.
#[test]
fn the_scalar_declaration_does_not_collapse_the_served_array() {
    let inst = RecordInstance::new("T".to_string(), initialised());
    for (f, n) in [("A", 9), ("B", 9), ("PP0", 3), ("PPO2", 3)] {
        match inst.client_field_value(f) {
            Some(EpicsValue::DoubleArray(v)) => assert_eq!(v.len(), n, "{f}"),
            other => panic!("{f} reached the client as {other:?}"),
        }
    }
}

/// Every one of the eight accepts a put and stores it where `get_field` reads
/// it back from. All eight rejected the put with `FieldNotFound` before the
/// write arms existed.
#[test]
fn all_eight_dbaddr_channels_take_a_put() {
    let mut t = initialised();
    for (f, n) in [
        ("A", 9),
        ("B", 9),
        ("PP0", 3),
        ("PP1", 3),
        ("PP2", 3),
        ("PPO0", 3),
        ("PPO1", 3),
        ("PPO2", 3),
    ] {
        let want: Vec<f64> = (0..n).map(|i| i as f64 + 0.5).collect();
        t.put_field(f, EpicsValue::DoubleArray(want.clone()))
            .unwrap_or_else(|e| panic!("{f}: {e:?}"));
        assert_eq!(doubles(&t, f), want, "{f}");
    }
}

/// The matrix put is row-major, the same flatten `get_field` serves: C hands
/// the client `ptbl->a[0]` and `a[i] = a[i-1] + 3` (`tableRecord.c:358-361`),
/// so flat index `3*r + c` is `a[r][c]` on the way in as well as out.
#[test]
fn a_matrix_put_lands_row_major() {
    let mut t = initialised();
    let flat: Vec<f64> = (1..=9).map(f64::from).collect();
    t.put_field("A", EpicsValue::DoubleArray(flat.clone()))
        .unwrap();
    assert_eq!(doubles(&t, "A"), flat);
    // The `else` arm of `cvt_dbaddr` must not have been touched: a put to one
    // channel writes only that channel's memory.
    assert_eq!(doubles(&t, "B"), doubles(&initialised(), "B"));
}

/// `dbAccess.c:1360` — `if (no_elements < nRequest) nRequest = no_elements;`.
/// A put longer than the channel writes the first `no_elements` and drops the
/// rest; it is not an error.
#[test]
fn a_put_longer_than_the_channel_is_truncated() {
    let mut t = initialised();
    let long: Vec<f64> = (1..=12).map(f64::from).collect();
    t.put_field("A", EpicsValue::DoubleArray(long)).unwrap();
    assert_eq!(doubles(&t, "A"), (1..=9).map(f64::from).collect::<Vec<_>>());
}

/// The other end of the same copy: C writes `nRequest` elements from index 0
/// and leaves the tail of the field alone — there is no `put_array_info`
/// (`tableRecord.c:172`) to shorten the channel, so element 2 keeps the value
/// `init_record` computed.
#[test]
fn a_put_shorter_than_the_channel_leaves_the_tail_standing() {
    let mut t = initialised();
    let before = doubles(&t, "PP1");
    t.put_field("PP1", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();
    assert_eq!(doubles(&t, "PP1"), vec![7.0, 8.0, before[2]]);
}

/// `dbAccess.c:1350` — `if (nRequest > 1 || special == SPC_DBADDR)`. A
/// one-element put to an `SPC_DBADDR` field takes the ARRAY row, so it writes
/// element 0 rather than being handled as a scalar store.
#[test]
fn a_one_element_put_writes_element_zero() {
    let mut t = initialised();
    let before = doubles(&t, "PP2");
    t.put_field("PP2", EpicsValue::Double(42.0)).unwrap();
    assert_eq!(doubles(&t, "PP2"), vec![42.0, before[1], before[2]]);
}
