use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::verify::{MatchResult, format_verify_report, verify};

async fn setup_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("PV1", Box::new(AoRecord::new(10.0))).await;
    db.add_record("PV2", Box::new(AoRecord::new(20.0))).await;
    db.add_record("PV3", Box::new(AoRecord::new(30.0))).await;
    db
}

#[tokio::test]
async fn test_verify_all_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("match.sav");
    let db = setup_db().await;

    write_save_file(
        &path,
        &[
            SaveEntry {
                pv_name: "PV1".into(),
                value: "10".into(),
                connected: true,
            },
            SaveEntry {
                pv_name: "PV2".into(),
                value: "20".into(),
                connected: true,
            },
        ],
    )
    .await
    .unwrap();

    let results = verify(&db, &path).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(matches!(results[0].result, MatchResult::Match));
    assert!(matches!(results[1].result, MatchResult::Match));
}

#[tokio::test]
async fn test_verify_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mismatch.sav");
    let db = setup_db().await;

    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "PV1".into(),
            value: "99.9".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();

    let results = verify(&db, &path).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].result, MatchResult::Mismatch { .. }));
}

#[tokio::test]
async fn test_verify_pv_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notfound.sav");
    let db = setup_db().await;

    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "NONEXISTENT".into(),
            value: "1.0".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();

    let results = verify(&db, &path).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].result, MatchResult::PvNotFound));
}

#[tokio::test]
async fn test_verify_report_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.sav");
    let db = setup_db().await;

    write_save_file(
        &path,
        &[
            SaveEntry {
                pv_name: "PV1".into(),
                value: "10".into(),
                connected: true,
            },
            SaveEntry {
                pv_name: "PV2".into(),
                value: "99.9".into(),
                connected: true,
            },
            SaveEntry {
                pv_name: "MISSING".into(),
                value: "1.0".into(),
                connected: true,
            },
        ],
    )
    .await
    .unwrap();

    let results = verify(&db, &path).await.unwrap();
    let report = format_verify_report(&results);
    assert!(report.contains("MISMATCH: PV2"));
    assert!(report.contains("NOT_FOUND: MISSING"));
    assert!(report.contains("1 match"));
    assert!(report.contains("1 mismatch"));
    assert!(report.contains("1 not found"));
}
