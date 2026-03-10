pub mod backup;
pub mod error;
pub mod format;
pub mod iocsh;
pub mod macros;
pub mod manager;
pub mod request;
pub mod save_file;
pub mod save_set;
pub mod verify;

pub use backup::BackupConfig;
pub use error::{AutosaveError, AutosaveResult};
pub use manager::{AutosaveBuilder, AutosaveManager};
pub use save_set::{RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, TriggerMode};

// --- Legacy API (backward-compatible with the old single-file autosave.rs) ---

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{CaError, CaResult};
use crate::server::database::PvDatabase;
use crate::types::EpicsValue;

/// Autosave configuration (legacy API).
#[derive(Clone, Debug)]
pub struct AutosaveConfig {
    pub save_path: PathBuf,
    pub period: Duration,
    pub request_pvs: Vec<String>,
}

/// Parse a .req file (one PV name per line, # comments).
pub fn parse_request_file(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

/// Serialize an EpicsValue to a savefile string.
fn value_to_save_str(value: &EpicsValue) -> String {
    match value {
        EpicsValue::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        EpicsValue::DoubleArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::LongArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::CharArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// Parse a savefile value string back to EpicsValue.
fn parse_save_value(s: &str, template: &EpicsValue) -> Option<EpicsValue> {
    let s = s.trim();
    match template {
        EpicsValue::String(_) => {
            if s.starts_with('"') && s.ends_with('"') {
                let inner = &s[1..s.len()-1];
                let unescaped = inner.replace("\\\"", "\"").replace("\\\\", "\\");
                Some(EpicsValue::String(unescaped))
            } else {
                Some(EpicsValue::String(s.to_string()))
            }
        }
        EpicsValue::Double(_) => s.parse::<f64>().ok().map(EpicsValue::Double),
        EpicsValue::Float(_) => s.parse::<f32>().ok().map(EpicsValue::Float),
        EpicsValue::Long(_) => s.parse::<i32>().ok().map(EpicsValue::Long),
        EpicsValue::Short(_) => s.parse::<i16>().ok().map(EpicsValue::Short),
        EpicsValue::Enum(_) => s.parse::<u16>().ok().map(EpicsValue::Enum),
        EpicsValue::Char(_) => s.parse::<u8>().ok().map(EpicsValue::Char),
        EpicsValue::ShortArray(_) => parse_array(s, |v| v.parse::<i16>().ok()).map(EpicsValue::ShortArray),
        EpicsValue::FloatArray(_) => parse_array(s, |v| v.parse::<f32>().ok()).map(EpicsValue::FloatArray),
        EpicsValue::EnumArray(_) => parse_array(s, |v| v.parse::<u16>().ok()).map(EpicsValue::EnumArray),
        EpicsValue::DoubleArray(_) => parse_array(s, |v| v.parse::<f64>().ok()).map(EpicsValue::DoubleArray),
        EpicsValue::LongArray(_) => parse_array(s, |v| v.parse::<i32>().ok()).map(EpicsValue::LongArray),
        EpicsValue::CharArray(_) => parse_array(s, |v| v.parse::<u8>().ok()).map(EpicsValue::CharArray),
    }
}

fn parse_array<T, F>(s: &str, parse_elem: F) -> Option<Vec<T>>
where
    F: Fn(&str) -> Option<T>,
{
    let inner = s.trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() { return Some(Vec::new()); }
    inner.split(',')
        .map(|v| parse_elem(v.trim()))
        .collect()
}

/// Save PV values to a file (atomic write via temp file + rename).
pub async fn save_to_file(db: &PvDatabase, pvs: &[String], path: &Path) -> CaResult<()> {
    let mut content = String::new();
    for pv in pvs {
        match db.get_pv(pv).await {
            Ok(val) => {
                content.push_str(pv);
                content.push(' ');
                content.push_str(&value_to_save_str(&val));
                content.push('\n');
            }
            Err(_) => {}
        }
    }

    let tmp_path = path.with_extension("tmp");
    tokio::fs::write(&tmp_path, content.as_bytes()).await
        .map_err(CaError::Io)?;
    tokio::fs::rename(&tmp_path, path).await
        .map_err(CaError::Io)?;
    Ok(())
}

/// Restore PV values from a file. Returns number of PVs restored.
pub async fn restore_from_file(db: &PvDatabase, path: &Path) -> CaResult<usize> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CaError::Io(e)),
    };

    let mut restored = 0;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (pv, val_str) = match line.find(' ') {
            Some(pos) => (&line[..pos], &line[pos+1..]),
            None => continue,
        };

        let current = match db.get_pv(pv).await {
            Ok(v) => v,
            Err(_) => {
                eprintln!("autosave: PV '{pv}' not found, skipping");
                continue;
            }
        };

        if let Some(value) = parse_save_value(val_str, &current) {
            if db.put_pv_no_process(pv, value).await.is_ok() {
                restored += 1;
            }
        }
    }

    Ok(restored)
}

/// Run autosave as a periodic task.
pub async fn run_autosave(db: Arc<PvDatabase>, config: AutosaveConfig) {
    let mut interval = tokio::time::interval(config.period);
    loop {
        interval.tick().await;
        if let Err(e) = save_to_file(&db, &config.request_pvs, &config.save_path).await {
            eprintln!("autosave error: {e}");
        }
    }
}

/// Bridge: convert legacy config to new SaveSetConfig.
pub fn from_legacy_config(config: &AutosaveConfig) -> SaveSetConfig {
    SaveSetConfig {
        name: "legacy".to_string(),
        save_path: config.save_path.clone(),
        strategy: SaveStrategy::Periodic {
            interval: config.period,
        },
        request_file: None,
        request_pvs: config.request_pvs.clone(),
        backup: BackupConfig::default(),
        macros: std::collections::HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use crate::server::records::ao::AoRecord;
    use crate::server::records::stringin::StringinRecord;
    #[test]
    fn test_parse_request_file() {
        let content = "# Comment\nTEMP.VAL\nPRESSURE.VAL\n\n# Another\nSWITCH.VAL\n";
        let pvs = parse_request_file(content);
        assert_eq!(pvs, vec!["TEMP.VAL", "PRESSURE.VAL", "SWITCH.VAL"]);
    }

    #[tokio::test]
    async fn test_save_and_restore_roundtrip() {
        let db = PvDatabase::new();
        db.add_record("TEMP", Box::new(AoRecord::new(25.5))).await;
        db.add_record("MSG", Box::new(StringinRecord::new("hello world"))).await;

        let tmp = std::env::temp_dir().join("epics_test_autosave.sav");
        let pvs = vec!["TEMP".to_string(), "MSG".to_string()];

        save_to_file(&db, &pvs, &tmp).await.unwrap();

        db.put_pv_no_process("TEMP", EpicsValue::Double(0.0)).await.unwrap();
        db.put_pv_no_process("MSG", EpicsValue::String("modified".into())).await.unwrap();

        let count = restore_from_file(&db, &tmp).await.unwrap();
        assert_eq!(count, 2);

        match db.get_pv("TEMP").await.unwrap() {
            EpicsValue::Double(v) => assert!((v - 25.5).abs() < 1e-10),
            other => panic!("expected Double(25.5), got {:?}", other),
        }
        match db.get_pv("MSG").await.unwrap() {
            EpicsValue::String(s) => assert_eq!(s, "hello world"),
            other => panic!("expected String, got {:?}", other),
        }

        let _ = tokio::fs::remove_file(&tmp).await;
    }

    #[tokio::test]
    async fn test_restore_missing_file() {
        let db = PvDatabase::new();
        let path = Path::new("/tmp/epics_test_nonexistent_file_12345.sav");
        let count = restore_from_file(&db, path).await.unwrap();
        assert_eq!(count, 0);
    }
}
