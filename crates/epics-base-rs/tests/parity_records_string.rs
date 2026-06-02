//! C-parity integration tests for the string / array / subroutine
//! record types reviewed in `doc/parity-review/08-records-string.md`.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::lsi::LsiRecord;
use epics_base_rs::server::records::lso::LsoRecord;
use epics_base_rs::server::records::printf::PrintfRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

// ---------------------------------------------------------------
// compress put_field("VAL", DoubleArray) must not corrupt the
// circular buffer or panic linearise_val.
// ---------------------------------------------------------------

#[test]
fn c1_compress_val_array_put_does_not_panic_or_desync() {
    // NSAM=4, Circular Buffer. A client writes a SHORTER array to VAL.
    let mut rec = CompressRecord::new(4, 4);
    rec.put_field("VAL", EpicsValue::DoubleArray(vec![1.0, 2.0]))
        .unwrap();
    // The array was ingested through the algorithm, NUSE tracks it.
    assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::Long(2)));
    // Reading VAL linearises NUSE elements — must not index OOB.
    match rec.get_field("VAL").unwrap() {
        EpicsValue::DoubleArray(v) => assert_eq!(v, vec![1.0, 2.0]),
        other => panic!("expected DoubleArray, got {other:?}"),
    }
    // A second, longer write keeps the buffer consistent.
    rec.put_field("VAL", EpicsValue::DoubleArray(vec![3.0, 4.0, 5.0]))
        .unwrap();
    assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::Long(4)));
    match rec.get_field("VAL").unwrap() {
        EpicsValue::DoubleArray(v) => assert_eq!(v.len(), 4),
        other => panic!("expected DoubleArray, got {other:?}"),
    }
}

// ---------------------------------------------------------------
// histogram counter must wrap at UINT_MAX, never panic.
// ---------------------------------------------------------------

#[test]
fn c2_histogram_counter_wraps_no_panic() {
    let mut rec = HistogramRecord::new(2, 0.0, 10.0);
    rec.val[0] = u32::MAX as i32; // UINT_MAX bit pattern
    rec.put_field("SGNL", EpicsValue::Double(1.0)).unwrap();
    // SGNL put triggers add_count (C SPC_MOD); counter wraps to 0.
    assert_eq!(rec.val[0], 0);
}

// ---------------------------------------------------------------
// lsi/lso SIZV clamps to [16, 0x7fff]; LEN initialises 0.
// ---------------------------------------------------------------

#[test]
fn h8_lsi_sizv_clamps_to_c_range() {
    let mut rec = LsiRecord::default();
    rec.put_field("SIZV", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("SIZV"), Some(EpicsValue::Short(16)));
    rec.put_field("SIZV", EpicsValue::Short(i16::MAX)).unwrap();
    assert_eq!(rec.get_field("SIZV"), Some(EpicsValue::Short(0x7fff)));
}

#[test]
fn m8_lsi_lso_len_initialises_zero() {
    let lsi = LsiRecord::default();
    assert_eq!(lsi.get_field("LEN"), Some(EpicsValue::Long(0)));
    assert_eq!(lsi.get_field("OLEN"), Some(EpicsValue::Long(0)));
    let lso = LsoRecord::default();
    assert_eq!(lso.get_field("LEN"), Some(EpicsValue::Long(0)));
    assert_eq!(lso.get_field("OLEN"), Some(EpicsValue::Long(0)));
}

// ---------------------------------------------------------------
// lsi/lso process() copies OVAL/OLEN only on change.
// ---------------------------------------------------------------

#[test]
fn h9_lso_process_olen_tracks_last_posted_length() {
    let mut rec = LsoRecord::default();
    rec.put_field("VAL", EpicsValue::String("first".into()))
        .unwrap();
    rec.process().unwrap();
    let len_after_first = rec.get_field("LEN");
    let olen_after_first = rec.get_field("OLEN");
    assert_eq!(len_after_first, olen_after_first);
    // A no-op process (no value change) must NOT move OLEN.
    rec.process().unwrap();
    assert_eq!(rec.get_field("OLEN"), olen_after_first);
    assert_eq!(rec.get_field("LEN"), len_after_first);
}

// ---------------------------------------------------------------
// stringin/stringout VAL truncates at MAX_STRING_SIZE (40).
// ---------------------------------------------------------------

#[test]
fn h10_stringin_stringout_truncate_at_40() {
    let mut si = StringinRecord::default();
    si.put_field("VAL", EpicsValue::String("a".repeat(100).into()))
        .unwrap();
    if let Some(EpicsValue::String(s)) = si.get_field("VAL") {
        assert_eq!(s.len(), 39);
    } else {
        panic!("expected String");
    }

    let mut so = StringoutRecord::default();
    so.put_field("VAL", EpicsValue::String("b".repeat(60).into()))
        .unwrap();
    if let Some(EpicsValue::String(s)) = so.get_field("VAL") {
        assert_eq!(s.len(), 39);
    } else {
        panic!("expected String");
    }
}

#[test]
fn h10_lsi_dbr_string_put_capped_at_40_even_with_large_sizv() {
    let mut rec = LsiRecord::default(); // SIZV=256
    // A DBR_STRING-typed put (EpicsValue::String) is capped at 40.
    rec.put_field("VAL", EpicsValue::String("c".repeat(100).into()))
        .unwrap();
    if let Some(EpicsValue::CharArray(bytes)) = rec.get_field("VAL") {
        assert_eq!(bytes.len(), 39, "DBR_STRING put capped at 40 (39+NUL)");
    } else {
        panic!("expected CharArray");
    }
    // A DBR_CHAR long-string put is bounded only by SIZV (256).
    rec.put_field("VAL", EpicsValue::CharArray(vec![b'd'; 100]))
        .unwrap();
    if let Some(EpicsValue::CharArray(bytes)) = rec.get_field("VAL") {
        assert_eq!(bytes.len(), 100, "DBR_CHAR put bounded by SIZV only");
    } else {
        panic!("expected CharArray");
    }
}

// ---------------------------------------------------------------
// histogram CMD start/stop semantics.
// ---------------------------------------------------------------

#[test]
fn h11_histogram_cmd_start_stop() {
    let mut rec = HistogramRecord::new(4, 0.0, 10.0);
    // CMD=3 stops counting.
    rec.put_field("CMD", EpicsValue::Short(3)).unwrap();
    rec.put_field("SGNL", EpicsValue::Double(2.0)).unwrap();
    assert_eq!(
        rec.get_field("VAL"),
        Some(EpicsValue::LongArray(vec![0, 0, 0, 0])),
        "stopped histogram does not count"
    );
    // CMD=2 resumes counting.
    rec.put_field("CMD", EpicsValue::Short(2)).unwrap();
    rec.put_field("SGNL", EpicsValue::Double(2.0)).unwrap();
    if let Some(EpicsValue::LongArray(v)) = rec.get_field("VAL") {
        assert_eq!(v.iter().sum::<i32>(), 1, "resumed histogram counts");
    } else {
        panic!("expected LongArray");
    }
}

// ---------------------------------------------------------------
// printf %s formats the link's STRING value.
// ---------------------------------------------------------------

#[test]
fn h6_printf_percent_s_uses_string_input() {
    let mut rec = PrintfRecord::default();
    rec.put_field("FMT", EpicsValue::String("device: %s".into()))
        .unwrap();
    // The framework delivers the INP0 link value into field A.
    rec.put_field("A", EpicsValue::String("ADC1".into()))
        .unwrap();
    rec.process().unwrap();
    if let Some(EpicsValue::CharArray(bytes)) = rec.get_field("VAL") {
        assert_eq!(String::from_utf8(bytes).unwrap(), "device: ADC1");
    } else {
        panic!("expected CharArray");
    }
}

// ---------------------------------------------------------------
// printf %*d / %ld / %ls / %c.
// ---------------------------------------------------------------

#[test]
fn h7_printf_star_width_and_modifiers() {
    let mut rec = PrintfRecord::default();
    rec.put_field("FMT", EpicsValue::String("[%*d] %ls %c".into()))
        .unwrap();
    rec.put_field("A", EpicsValue::Long(5)).unwrap(); // width for %*d
    rec.put_field("B", EpicsValue::Long(7)).unwrap(); // value for %*d
    rec.put_field("C", EpicsValue::String("tag".into()))
        .unwrap(); // %ls
    rec.put_field("D", EpicsValue::Long(33)).unwrap(); // %c -> '!'
    rec.process().unwrap();
    if let Some(EpicsValue::CharArray(bytes)) = rec.get_field("VAL") {
        assert_eq!(String::from_utf8(bytes).unwrap(), "[    7] tag !");
    } else {
        panic!("expected CharArray");
    }
}

// ---------------------------------------------------------------
// compress ILIL/IHIL skips only the leading out-of-limit run.
// ---------------------------------------------------------------

#[test]
fn m2_compress_ilil_ihil_leading_run_only() {
    let mut rec = CompressRecord::new(8, 2); // alg=Mean
    rec.n = 3;
    rec.ilil = 0.0;
    rec.ihil = 100.0;
    // [5, -1, 7]: 5 is in range so nothing is skipped; the mid-array
    // out-of-limit -1 is kept. Mean = (5 + -1 + 7)/3 = 11/3.
    rec.put_field("VAL", EpicsValue::DoubleArray(vec![5.0, -1.0, 7.0]))
        .unwrap();
    assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::Long(1)));
    if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
        assert!((v[0] - 11.0 / 3.0).abs() < 1e-9);
    } else {
        panic!("expected DoubleArray");
    }
}

// ---------------------------------------------------------------
// compress exposes CVB; scalar N-to-1 increments INX mid-cycle.
// ---------------------------------------------------------------

#[test]
fn m1_compress_scalar_cvb_and_inx() {
    let mut rec = CompressRecord::new(8, 1); // alg=High Value
    rec.n = 3;
    rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(9.0)).unwrap();
    // Two of three samples accumulated: INX=2, CVB tracks the running
    // high (9.0), nothing emitted yet.
    assert_eq!(rec.get_field("INX"), Some(EpicsValue::Long(2)));
    assert_eq!(rec.get_field("CVB"), Some(EpicsValue::Double(9.0)));
    assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::Long(0)));
    // Third sample completes the chunk: emit max(2,9,4)=9, INX resets.
    rec.put_field("VAL", EpicsValue::Double(4.0)).unwrap();
    assert_eq!(rec.get_field("INX"), Some(EpicsValue::Long(0)));
    assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::Long(1)));
    if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
        assert_eq!(v[0], 9.0);
    } else {
        panic!("expected DoubleArray");
    }
}
