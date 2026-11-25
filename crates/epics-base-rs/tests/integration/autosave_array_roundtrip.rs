//! CA4-3: an array element that contains the punctuation of the array
//! text itself must survive a save/restore. The writer and the reader
//! share one owner of that text, so the round trip is total rather than
//! true-for-elements-without-commas.
//!
//! Correctness only: no synApps autosave source exists on this machine,
//! so nothing here is a claim about what C's `dbrestore.c` does.

use epics_base_rs::server::autosave::save_file::{
    SaveEntry, parse_save_value, read_save_file, value_to_save_str, write_save_file,
};
use epics_base_rs::types::EpicsValue;

/// Elements that collide with the array text's own punctuation.
const NASTY: &[&str] = &[
    "a,b", "a]b", "a\"b", "a\\b", "", " lead", "trail ", "plain", "[open", "a,]\"\\b",
];

fn string_array(elems: &[&str]) -> EpicsValue {
    EpicsValue::StringArray(elems.iter().map(|s| (*s).into()).collect())
}

fn elements_of(v: &EpicsValue) -> Vec<String> {
    match v {
        EpicsValue::StringArray(arr) => arr.iter().map(|s| s.as_str_lossy().into_owned()).collect(),
        other => panic!("expected StringArray, got {other:?}"),
    }
}

/// The reviewer's repro, end to end through the C `@array@` form: the
/// comma is inside one element, not between two.
#[epics_macros_rs::epics_test]
async fn a_comma_inside_a_c_array_element_is_not_a_separator() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfs.sav");

    let value = string_array(&["a,b", "c"]);
    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "SR:WFS".into(),
            value: value_to_save_str(&value),
            connected: true,
            array_fprintfs: Vec::new(),
        }],
    )
    .await
    .unwrap();

    let entries = read_save_file(&path).await.unwrap().unwrap().entries;
    let parsed = parse_save_value(&entries[0].value, &string_array(&[])).unwrap();

    assert_eq!(
        elements_of(&parsed),
        vec!["a,b".to_string(), "c".to_string()],
        "NORD and element 0 both change when the comma splits"
    );
}

/// Same shape for the character that ends the text: a `]` inside an
/// element must not be eaten as the closing bracket.
#[epics_macros_rs::epics_test]
async fn a_bracket_inside_an_element_is_not_the_terminator() {
    let value = string_array(&["a]b", "c]"]);
    let parsed = parse_save_value(&value_to_save_str(&value), &string_array(&[])).unwrap();
    assert_eq!(
        elements_of(&parsed),
        vec!["a]b".to_string(), "c]".to_string()]
    );
}

/// Every nasty element, alone and in every pair — the property the
/// round trip has to hold: what is written is what is read.
#[epics_macros_rs::epics_test]
async fn every_element_survives_the_round_trip() {
    for a in NASTY {
        for b in NASTY {
            for elems in [vec![*a], vec![*a, *b], vec![*a, *b, "tail"]] {
                let value = string_array(&elems);
                let text = value_to_save_str(&value);
                let parsed = parse_save_value(&text, &string_array(&[]))
                    .unwrap_or_else(|| panic!("{elems:?} did not parse from {text}"));
                // One element is written as a scalar, and C reads an empty
                // one back as no element (dbrestore.c:478-482).
                let expected: Vec<String> = match elems.as_slice() {
                    [""] => Vec::new(),
                    _ => elems.iter().map(|s| s.to_string()).collect(),
                };
                assert_eq!(elements_of(&parsed), expected, "round trip through {text}");
            }
        }
    }
}

/// The same set through the C `@array@` writer and a real file, so the
/// `.sav` line grammar is covered too and not just the value text.
#[epics_macros_rs::epics_test]
async fn every_element_survives_a_c_format_save_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nasty.sav");

    let value = string_array(NASTY);
    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "SR:WFS".into(),
            value: value_to_save_str(&value),
            connected: true,
            array_fprintfs: Vec::new(),
        }],
    )
    .await
    .unwrap();

    let entries = read_save_file(&path).await.unwrap().unwrap().entries;
    let parsed = parse_save_value(&entries[0].value, &string_array(&[])).unwrap();
    assert_eq!(
        elements_of(&parsed),
        NASTY.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    );
}

/// C writes an empty array and an array of one empty element alike, as
/// the scalar "" (save_restore.c:1853-1860), and reads that back as no
/// element (dbrestore.c:478-482).
#[epics_macros_rs::epics_test]
async fn an_empty_element_is_written_as_an_empty_array() {
    for elems in [&[][..], &[""][..]] {
        let text = value_to_save_str(&string_array(elems));
        assert_eq!(text, "");
        let parsed = parse_save_value(&text, &string_array(&[])).unwrap();
        assert!(elements_of(&parsed).is_empty(), "{elems:?}");
    }
}

/// A numeric array is written in C's form, and the `[..]` form of a file
/// an older autosave-rs wrote still reads.
#[epics_macros_rs::epics_test]
async fn numeric_array_text() {
    assert_eq!(
        value_to_save_str(&EpicsValue::LongArray(vec![1, 2, 3].into())),
        r#"@array@ { "1" "2" "3" }"#
    );
    let parsed = parse_save_value("[1,2,3]", &EpicsValue::LongArray(vec![].into())).unwrap();
    assert_eq!(parsed, EpicsValue::LongArray(vec![1, 2, 3].into()));
    // Hand-written spacing still reads.
    let spaced = parse_save_value("[1, 2, 3]", &EpicsValue::LongArray(vec![].into())).unwrap();
    assert_eq!(spaced, EpicsValue::LongArray(vec![1, 2, 3].into()));
}
