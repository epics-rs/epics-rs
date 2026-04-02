use epics_base_rs::types::EpicsValue;

use epics_base_rs::server::autosave::save_file::{
    SaveEntry, parse_save_value, read_save_file, validate_save_file, value_to_save_str,
    write_save_file,
};

#[tokio::test]
async fn test_write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.sav");

    let entries = vec![
        SaveEntry {
            pv_name: "TEMP".into(),
            value: value_to_save_str(&EpicsValue::Double(25.5)),
            connected: true,
        },
        SaveEntry {
            pv_name: "MSG".into(),
            value: value_to_save_str(&EpicsValue::String("hello world".into())),
            connected: true,
        },
        SaveEntry {
            pv_name: "COUNT".into(),
            value: value_to_save_str(&EpicsValue::Long(42)),
            connected: true,
        },
        SaveEntry {
            pv_name: "ENUM_PV".into(),
            value: value_to_save_str(&EpicsValue::Enum(3)),
            connected: true,
        },
    ];

    write_save_file(&path, &entries).await.unwrap();

    let loaded = read_save_file(&path).await.unwrap().unwrap();
    assert_eq!(loaded.len(), 4);
    assert_eq!(loaded[0].pv_name, "TEMP");
    assert!(loaded[0].connected);
    assert_eq!(loaded[1].pv_name, "MSG");

    // Verify parse_save_value roundtrip
    let dbl_val = parse_save_value(&loaded[0].value, &EpicsValue::Double(0.0)).unwrap();
    match dbl_val {
        EpicsValue::Double(v) => assert!((v - 25.5).abs() < 1e-10),
        _ => panic!("expected Double"),
    }
}

#[tokio::test]
async fn test_end_marker_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("valid.sav");

    let entries = vec![SaveEntry {
        pv_name: "PV1".into(),
        value: "1.0".into(),
        connected: true,
    }];
    write_save_file(&path, &entries).await.unwrap();
    assert!(validate_save_file(&path).await.unwrap());
}

#[tokio::test]
async fn test_missing_end_marker_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.sav");

    // Write a file without <END>
    tokio::fs::write(&path, "PV1 1.0\nPV2 2.0\n").await.unwrap();

    assert!(!validate_save_file(&path).await.unwrap());
    let result = read_save_file(&path).await.unwrap();
    assert!(result.is_none()); // None = corrupt
}

#[tokio::test]
async fn test_c_autosave_array_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c_array.sav");

    let content = "ARRAY_PV @array@ { \"1.0\" \"2.0\" \"3.0\" }\n<END>\n";
    tokio::fs::write(&path, content).await.unwrap();

    let entries = read_save_file(&path).await.unwrap().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].pv_name, "ARRAY_PV");
    assert_eq!(entries[0].value, "[1.0,2.0,3.0]");
}

#[tokio::test]
async fn test_string_with_special_chars() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("special.sav");

    let entries = vec![SaveEntry {
        pv_name: "STR".into(),
        value: value_to_save_str(&EpicsValue::String(
            "test \"quoted\" with\\backslash".into(),
        )),
        connected: true,
    }];

    write_save_file(&path, &entries).await.unwrap();
    let loaded = read_save_file(&path).await.unwrap().unwrap();

    let parsed = parse_save_value(&loaded[0].value, &EpicsValue::String(String::new())).unwrap();
    match parsed {
        EpicsValue::String(s) => assert_eq!(s, "test \"quoted\" with\\backslash"),
        _ => panic!("expected String"),
    }
}

#[tokio::test]
async fn test_windows_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows.sav");

    // Write with \r\n
    let content = "# header\r\nPV1 42\r\nPV2 3.14\r\n<END>\r\n";
    tokio::fs::write(&path, content).await.unwrap();

    let entries = read_save_file(&path).await.unwrap().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].pv_name, "PV1");
    assert_eq!(entries[0].value, "42");
}

#[tokio::test]
async fn test_multiple_header_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi_header.sav");

    let content = "# autosave V1\n# extra header\n# another\nPV1 1.0\n<END>\n";
    tokio::fs::write(&path, content).await.unwrap();

    let entries = read_save_file(&path).await.unwrap().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].pv_name, "PV1");
}

#[tokio::test]
async fn test_disconnected_pv_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disconnected.sav");

    let entries = vec![
        SaveEntry {
            pv_name: "GOOD_PV".into(),
            value: "1.0".into(),
            connected: true,
        },
        SaveEntry {
            pv_name: "BAD_PV".into(),
            value: String::new(),
            connected: false,
        },
    ];

    write_save_file(&path, &entries).await.unwrap();
    let loaded = read_save_file(&path).await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
    assert!(loaded[0].connected);
    assert!(!loaded[1].connected);
    assert_eq!(loaded[1].pv_name, "BAD_PV");
}

#[test]
fn test_short_array_roundtrip() {
    let val = EpicsValue::ShortArray(vec![1, -2, 3, 0, -32768]);
    let s = value_to_save_str(&val);
    assert_eq!(s, "[1,-2,3,0,-32768]");
    let parsed = parse_save_value(&s, &EpicsValue::ShortArray(vec![])).unwrap();
    assert_eq!(parsed, val);
}

#[test]
fn test_float_array_roundtrip() {
    let val = EpicsValue::FloatArray(vec![1.0, -2.5, 0.0]);
    let s = value_to_save_str(&val);
    let parsed = parse_save_value(&s, &EpicsValue::FloatArray(vec![])).unwrap();
    match parsed {
        EpicsValue::FloatArray(arr) => {
            assert_eq!(arr.len(), 3);
            assert!((arr[0] - 1.0).abs() < 1e-6);
            assert!((arr[1] - (-2.5)).abs() < 1e-6);
            assert!((arr[2] - 0.0).abs() < 1e-6);
        }
        _ => panic!("expected FloatArray"),
    }
}

#[test]
fn test_enum_array_roundtrip() {
    let val = EpicsValue::EnumArray(vec![0, 1, 65535, 42]);
    let s = value_to_save_str(&val);
    assert_eq!(s, "[0,1,65535,42]");
    let parsed = parse_save_value(&s, &EpicsValue::EnumArray(vec![])).unwrap();
    assert_eq!(parsed, val);
}

#[test]
fn test_empty_new_array_types() {
    for template in &[
        EpicsValue::ShortArray(vec![]),
        EpicsValue::FloatArray(vec![]),
        EpicsValue::EnumArray(vec![]),
    ] {
        let s = value_to_save_str(template);
        assert_eq!(s, "[]");
        let parsed = parse_save_value(&s, template).unwrap();
        match (&parsed, template) {
            (EpicsValue::ShortArray(a), EpicsValue::ShortArray(_)) => assert!(a.is_empty()),
            (EpicsValue::FloatArray(a), EpicsValue::FloatArray(_)) => assert!(a.is_empty()),
            (EpicsValue::EnumArray(a), EpicsValue::EnumArray(_)) => assert!(a.is_empty()),
            _ => panic!("type mismatch"),
        }
    }
}
