//! CA4-3: an array element that contains the punctuation of the array
//! text itself must survive a save/restore. The writer and the reader
//! share one owner of that text, so the round trip is total rather than
//! true-for-elements-without-commas.
//!
//! Correctness only: no synApps autosave source exists on this machine,
//! so nothing here is a claim about what C's `dbrestore.c` does.

use epics_base_rs::server::autosave::format::CompatMode;
use epics_base_rs::server::autosave::save_file::{
    SaveEntry, parse_save_value, read_save_file, value_to_save_str, value_to_save_str_c,
    write_save_file_with_mode,
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
    write_save_file_with_mode(
        &path,
        &[SaveEntry {
            pv_name: "SR:WFS".into(),
            value: value_to_save_str_c(&value),
            connected: true,
        }],
        CompatMode::CRead,
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
async fn every_element_survives_the_native_round_trip() {
    for a in NASTY {
        for b in NASTY {
            for elems in [vec![*a], vec![*a, *b], vec![*a, *b, "tail"]] {
                let value = string_array(&elems);
                let text = value_to_save_str(&value);
                let parsed = parse_save_value(&text, &string_array(&[]))
                    .unwrap_or_else(|| panic!("{elems:?} did not parse from {text}"));
                assert_eq!(
                    elements_of(&parsed),
                    elems.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    "round trip through {text}"
                );
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
    write_save_file_with_mode(
        &path,
        &[SaveEntry {
            pv_name: "SR:WFS".into(),
            value: value_to_save_str_c(&value),
            connected: true,
        }],
        CompatMode::CRead,
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

/// An empty array and an array of one empty element are different
/// things and must stay different.
#[epics_macros_rs::epics_test]
async fn an_empty_array_is_not_an_array_holding_an_empty_element() {
    let empty =
        parse_save_value(&value_to_save_str(&string_array(&[])), &string_array(&[])).unwrap();
    assert!(elements_of(&empty).is_empty());

    let one =
        parse_save_value(&value_to_save_str(&string_array(&[""])), &string_array(&[])).unwrap();
    assert_eq!(elements_of(&one), vec![String::new()]);
}

/// Numeric arrays keep the text they always had — the quoting rule
/// triggers on the array punctuation, which a number never contains.
#[epics_macros_rs::epics_test]
async fn numeric_array_text_is_unchanged() {
    assert_eq!(
        value_to_save_str(&EpicsValue::LongArray(vec![1, 2, 3])),
        "[1,2,3]"
    );
    let parsed = parse_save_value("[1,2,3]", &EpicsValue::LongArray(vec![])).unwrap();
    assert_eq!(parsed, EpicsValue::LongArray(vec![1, 2, 3]));
    // Hand-written spacing still reads.
    let spaced = parse_save_value("[1, 2, 3]", &EpicsValue::LongArray(vec![])).unwrap();
    assert_eq!(spaced, EpicsValue::LongArray(vec![1, 2, 3]));
}
