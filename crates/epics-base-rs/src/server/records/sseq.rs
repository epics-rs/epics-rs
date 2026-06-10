use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue, PvString};

/// Choice labels for the SELM step-selection menu, in index order.
/// C `menu(sseqSELM)` (synApps sseqRecord.dbd): 0=All, 1=Specified, 2=Mask.
const SSEQ_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

/// Choice labels for the per-step wait-mode menu, in index order.
/// C `menu(sseqWAIT)` (synApps sseqRecord.dbd): 0=NoWait, 1=Wait, then
/// "After1".."AfterA" (wait for step 1..10 to finish before this one).
/// Shared by every `WAIT1`..`WAITA` field.
const SSEQ_WAIT_CHOICES: &[&str] = &[
    "NoWait", "Wait", "After1", "After2", "After3", "After4", "After5", "After6", "After7",
    "After8", "After9", "AfterA",
];

/// Choice labels for the per-step DOL/LNK link-status menu, in index
/// order. C `menu(sseqLNKV)` (sseqRecord.dbd:20): 0=Ext PV NC, 1=Ext PV
/// OK, 2=Local PV, 3=Constant. Shared by every `DOLnV`/`LNKnV` field.
const SSEQ_LNKV_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

const NUM_STEPS: usize = 10;

/// A single step in the string sequence.
#[derive(Clone, Default)]
struct SseqStep {
    dly: f64,          // Delay before executing this step
    dol: String,       // Input link (DOLn)
    dov: f64,          // Numeric value (DOn)
    lnk: String,       // Output link (LNKn)
    str_val: PvString, // String value (STRn)
    wait: i16,         // Wait mode: 0=NoWait, 1=Wait, 2..=After1..After9
}

/// Sseq record — string sequence record.
///
/// Executes up to 10 steps, each with an optional delay, input link,
/// numeric value, string value, and output link. Steps are selected
/// by SELM (All, Specified, Mask) with SELN as the selection value.
pub struct SseqRecord {
    pub val: i32,
    pub selm: i16, // 0=All, 1=Specified, 2=Mask
    pub seln: u16,
    pub sell: String,
    pub prec: i16,
    pub abort: i16,
    pub busy: i16,
    steps: [SseqStep; NUM_STEPS],
}

impl Default for SseqRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            seln: 1,
            sell: String::new(),
            prec: 0,
            abort: 0,
            busy: 0,
            steps: Default::default(),
        }
    }
}

impl SseqRecord {
    pub fn new() -> Self {
        Self::default()
    }

    fn step_index_from_suffix(name: &str) -> Option<(usize, &str)> {
        // Parse step index from field name suffix: 1-9 or A (=10)
        if name.len() < 2 {
            return None;
        }
        let last = name.as_bytes()[name.len() - 1];
        let prefix = &name[..name.len() - 1];
        match last {
            b'1'..=b'9' => Some(((last - b'1') as usize, prefix)),
            b'A' => Some((9, prefix)),
            _ => None,
        }
    }

    /// Parse a `DOLnV` / `LNKnV` link-status field name into its 0-based
    /// step index (suffix `1`..`9` or `A`). These `menu(sseqLNKV)` fields
    /// (sseqRecord.dbd:118,125) carry the step digit *before* the trailing
    /// `V`, so the generic `step_index_from_suffix` (which keys on the last
    /// character) does not recognise them.
    fn link_status_index(name: &str) -> Option<usize> {
        let mid = name
            .strip_prefix("DOL")
            .or_else(|| name.strip_prefix("LNK"))?
            .strip_suffix('V')?;
        if mid.len() != 1 {
            return None;
        }
        match mid.as_bytes()[0] {
            c @ b'1'..=b'9' => Some((c - b'1') as usize),
            b'A' => Some(9),
            _ => None,
        }
    }

    pub fn should_execute_step(&self, step_idx: usize) -> bool {
        match self.selm {
            0 => true, // All
            1 => {
                // Specified — SELN is the 1-based step number. The 10
                // steps are numbered 1..=10 (synApps sseq DLY1..DLYA),
                // so step number `seln` selects index `seln - 1`.
                // `seln == 0` or `seln > 10` selects no step.
                let sel = self.seln as usize;
                (1..=NUM_STEPS).contains(&sel) && step_idx == sel - 1
            }
            2 => {
                // Mask — SELN is a bitmask
                (self.seln & (1 << step_idx)) != 0
            }
            _ => false,
        }
    }
}

/// Build the full `sseq` field table. The 7 base record fields plus the
/// top-level `ABORTING` status, then the C "struct linkGroup" — 13 fields
/// per step (sseqRecord.dbd) for each of the 10 steps (suffixes `1`..`9`,
/// `A`), in DBD declaration order. `concat!` materialises each per-step
/// field name as a `&'static str`, keeping the table a compile-time
/// `static` while the per-step shape is generated once (no copy-paste
/// drift across 130 entries).
///
/// `DTn`/`LTn` (DOL/LNK link field type), `WERRn` (wait-config error),
/// `WTGn` (outstanding callback), `IXn` (step index), `DOLnV`/`LNKnV`
/// (link connection status, `menu(sseqLNKV)`) and top-level `ABORTING`
/// are read-only diagnostics: C `sseqRecord.c` updates them from the
/// record's async sequence owner (checkLinks / processNextLink), which is
/// not yet ported, so they expose their DBD init defaults here.
macro_rules! sseq_fields {
    ($($s:literal),+ $(,)?) => {
        &[
            FieldDesc { name: "VAL", dbf_type: DbFieldType::Long, read_only: false },
            // SELM is DBF_MENU menu(sseqSELM) (sseqRecord.dbd:34) — served
            // as DBR_ENUM with the menu's choice labels (SSEQ_SELM_CHOICES).
            FieldDesc { name: "SELM", dbf_type: DbFieldType::Enum, read_only: false },
            FieldDesc { name: "SELN", dbf_type: DbFieldType::UShort, read_only: false },
            FieldDesc { name: "SELL", dbf_type: DbFieldType::String, read_only: false },
            FieldDesc { name: "PREC", dbf_type: DbFieldType::Short, read_only: false },
            FieldDesc { name: "ABORT", dbf_type: DbFieldType::Short, read_only: false },
            FieldDesc { name: "ABORTING", dbf_type: DbFieldType::Short, read_only: true },
            FieldDesc { name: "BUSY", dbf_type: DbFieldType::Short, read_only: true },
            $(
                FieldDesc { name: concat!("DLY", $s), dbf_type: DbFieldType::Double, read_only: false },
                FieldDesc { name: concat!("DOL", $s), dbf_type: DbFieldType::String, read_only: false },
                FieldDesc { name: concat!("DO", $s), dbf_type: DbFieldType::Double, read_only: false },
                FieldDesc { name: concat!("LNK", $s), dbf_type: DbFieldType::String, read_only: false },
                FieldDesc { name: concat!("STR", $s), dbf_type: DbFieldType::String, read_only: false },
                FieldDesc { name: concat!("DT", $s), dbf_type: DbFieldType::Short, read_only: true },
                FieldDesc { name: concat!("LT", $s), dbf_type: DbFieldType::Short, read_only: true },
                // WAITn is DBF_MENU menu(sseqWAIT) — see SSEQ_WAIT_CHOICES.
                FieldDesc { name: concat!("WAIT", $s), dbf_type: DbFieldType::Short, read_only: false },
                FieldDesc { name: concat!("WERR", $s), dbf_type: DbFieldType::Short, read_only: true },
                FieldDesc { name: concat!("WTG", $s), dbf_type: DbFieldType::Short, read_only: true },
                FieldDesc { name: concat!("IX", $s), dbf_type: DbFieldType::Short, read_only: true },
                // DOLnV / LNKnV are DBF_MENU menu(sseqLNKV) — served as
                // DBR_ENUM with SSEQ_LNKV_CHOICES.
                FieldDesc { name: concat!("DOL", $s, "V"), dbf_type: DbFieldType::Enum, read_only: true },
                FieldDesc { name: concat!("LNK", $s, "V"), dbf_type: DbFieldType::Enum, read_only: true },
            )+
        ]
    };
}

static SSEQ_FIELDS: &[FieldDesc] = sseq_fields!("1", "2", "3", "4", "5", "6", "7", "8", "9", "A");

impl Record for SseqRecord {
    fn record_type(&self) -> &'static str {
        "sseq"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.busy = 1;
        // For each selected step, prepare the output value.
        // DOL reads are handled by pre_process_actions().
        //
        // SINGLE-OWNER INVARIANT: `LNKn` dispatch (per-step value
        // write + target forward-link processing) is owned solely by
        // `PvDatabase::dispatch_multi_output`'s `MultiOut::Sseq` arm —
        // the only path with SELL/SELM/SELN selection, DOLn input
        // fetch, and STR/DO value precedence. `SseqRecord` therefore
        // MUST NOT implement `Record::multi_output_links`: doing so
        // would make the generic `multi_output_links` block in
        // `processing.rs` §4.6 dispatch every `LNKn` a second time per
        // cycle. C `sseqRecord.c::processNextLink` drives each step's
        // `LNKn` via `dbPutLink` exactly once.
        self.busy = 0;
        Ok(ProcessOutcome::complete())
    }

    fn pre_process_actions(&mut self) -> Vec<crate::server::record::ProcessAction> {
        use crate::server::record::ProcessAction;

        static DOL_DOV: [(&str, &str); NUM_STEPS] = [
            ("DOL1", "DO1"),
            ("DOL2", "DO2"),
            ("DOL3", "DO3"),
            ("DOL4", "DO4"),
            ("DOL5", "DO5"),
            ("DOL6", "DO6"),
            ("DOL7", "DO7"),
            ("DOL8", "DO8"),
            ("DOL9", "DO9"),
            ("DOLA", "DOA"),
        ];

        let mut actions = Vec::new();
        for i in 0..NUM_STEPS {
            if self.should_execute_step(i) && !self.steps[i].dol.is_empty() {
                actions.push(ProcessAction::ReadDbLink {
                    link_field: DOL_DOV[i].0,
                    target_field: DOL_DOV[i].1,
                });
            }
        }
        actions
    }

    // NOTE: `SseqRecord` deliberately does NOT implement
    // `Record::multi_output_links`. Sseq `LNKn` dispatch is owned
    // solely by `dispatch_multi_output`'s `MultiOut::Sseq` arm — see
    // the single-owner invariant in `process()` above. Re-adding the
    // override here would re-introduce double LNKn writes per cycle.

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            // SELM is DBF_MENU (sseqRecord.dbd:34): served as DBR_ENUM,
            // labels from menu_field_choices.
            "SELM" => Some(EpicsValue::Enum(self.selm as u16)),
            "SELN" => Some(EpicsValue::UShort(self.seln)),
            "SELL" => Some(EpicsValue::String(self.sell.clone().into())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "ABORT" => Some(EpicsValue::Short(self.abort)),
            // ABORTING (sseqRecord.dbd:820) is a machine-driven status:
            // C `sseqRecord.c:special`/`asyncFinish` toggle it across an
            // abort. That async sequence owner is not yet ported, so the
            // record always reads the inactive default — exposed read-only
            // so clients can still open REC.ABORTING.
            "ABORTING" => Some(EpicsValue::Short(0)),
            "BUSY" => Some(EpicsValue::Short(self.busy)),
            _ => {
                // DOLnV / LNKnV link-status menu (menu(sseqLNKV),
                // sseqRecord.dbd:118,125). The step digit sits before the
                // trailing `V`, so `step_index_from_suffix` (which keys on
                // the last char) does not recognise these. C init value is
                // sseqLNKV_EXT (1); the live connection status is computed
                // by `sseqRecord.c:checkLinks` (part of the async sequence
                // machine not yet ported).
                if Self::link_status_index(name).is_some() {
                    return Some(EpicsValue::Enum(1));
                }
                if let Some((idx, prefix)) = Self::step_index_from_suffix(name) {
                    let step = &self.steps[idx];
                    return match prefix {
                        "DLY" => Some(EpicsValue::Double(step.dly)),
                        "DOL" => Some(EpicsValue::String(step.dol.clone().into())),
                        "DO" => Some(EpicsValue::Double(step.dov)),
                        "LNK" => Some(EpicsValue::String(step.lnk.clone().into())),
                        "STR" => Some(EpicsValue::String(step.str_val.clone())),
                        "WAIT" => Some(EpicsValue::Short(step.wait)),
                        // Per-step diagnostics (sseqRecord.dbd). Read-only;
                        // their live values come from the async sequence
                        // owner (`sseqRecord.c:processNextLink`/`checkLinks`),
                        // not yet ported — exposed at their C init defaults
                        // so REC.DTn/LTn/WERRn/WTGn/IXn can be opened.
                        "DT" => Some(EpicsValue::Short(0)),
                        "LT" => Some(EpicsValue::Short(0)),
                        "WERR" => Some(EpicsValue::Short(0)),
                        "WTG" => Some(EpicsValue::Short(0)),
                        // IXn holds the step's own 0-based index
                        // (sseqRecord.dbd initial: IX1=0 .. IXA=9).
                        "IX" => Some(EpicsValue::Short(idx as i16)),
                        _ => None,
                    };
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = match value {
                    EpicsValue::Long(v) => v,
                    _ => value
                        .to_f64()
                        .map(|v| v as i32)
                        .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?,
                };
                Ok(())
            }
            // SELM is DBF_MENU (sseqRecord.dbd:34): a client put arrives
            // converted to Enum; internal callers may still pass a Short
            // index. Store the menu index either way.
            "SELM" => match value {
                EpicsValue::Enum(v) => {
                    self.selm = v as i16;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.selm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELM".into())),
            },
            // SELN is `DBF_USHORT` (sseqRecord.dbd:40): client puts arrive
            // converted to UShort; internal SELL link reads pass a Short.
            "SELN" => match value {
                EpicsValue::UShort(v) => {
                    self.seln = v;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.seln = v as u16;
                    Ok(())
                }
                _ => {
                    let v = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch("SELN".into()))?;
                    self.seln = v as u16;
                    Ok(())
                }
            },
            "SELL" => match value {
                EpicsValue::String(s) => {
                    self.sell = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELL".into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PREC".into())),
            },
            "ABORT" => match value {
                EpicsValue::Short(v) => {
                    self.abort = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ABORT".into())),
            },
            _ => {
                if let Some((idx, prefix)) = Self::step_index_from_suffix(name) {
                    let step = &mut self.steps[idx];
                    return match prefix {
                        "DLY" => {
                            step.dly = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            Ok(())
                        }
                        "DOL" => match value {
                            EpicsValue::String(s) => {
                                step.dol = s.as_str_lossy().into_owned();
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "DO" => {
                            step.dov = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            Ok(())
                        }
                        "LNK" => match value {
                            EpicsValue::String(s) => {
                                step.lnk = s.as_str_lossy().into_owned();
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "STR" => match value {
                            EpicsValue::String(s) => {
                                step.str_val = s;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "WAIT" => match value {
                            EpicsValue::Short(v) => {
                                step.wait = v;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        _ => Err(CaError::FieldNotFound(name.to_string())),
                    };
                }
                Err(CaError::FieldNotFound(name.to_string()))
            }
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SSEQ_FIELDS
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SELM" => Some(SSEQ_SELM_CHOICES),
            // The per-step `WAIT1`..`WAITA` fields are `menu(sseqWAIT)`.
            _ if matches!(Self::step_index_from_suffix(field), Some((_, "WAIT"))) => {
                Some(SSEQ_WAIT_CHOICES)
            }
            // The per-step `DOLnV`/`LNKnV` link-status fields are
            // `menu(sseqLNKV)` (sseqRecord.dbd:118,125).
            _ if Self::link_status_index(field).is_some() => Some(SSEQ_LNKV_CHOICES),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sseq_default() {
        let rec = SseqRecord::new();
        assert_eq!(rec.record_type(), "sseq");
        assert_eq!(rec.val, 0);
        assert_eq!(rec.selm, 0);
        assert_eq!(rec.seln, 1);
    }

    #[test]
    fn test_sseq_put_get_val() {
        let mut rec = SseqRecord::new();
        rec.put_field("VAL", EpicsValue::Long(42)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Long(42)));
    }

    #[test]
    fn test_sseq_put_get_selm() {
        let mut rec = SseqRecord::new();
        // SELM is DBF_MENU (sseqRecord.dbd:34): served as DBR_ENUM. A Short
        // put is tolerated; the read-back is the native Enum index.
        rec.put_field("SELM", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(2)));
    }

    #[test]
    fn test_sseq_step_fields() {
        let mut rec = SseqRecord::new();
        rec.put_field("DLY1", EpicsValue::Double(1.5)).unwrap();
        rec.put_field("DO1", EpicsValue::Double(42.0)).unwrap();
        rec.put_field("STR1", EpicsValue::String("hello".into()))
            .unwrap();
        rec.put_field("LNK1", EpicsValue::String("target.VAL".into()))
            .unwrap();
        rec.put_field("DOL1", EpicsValue::String("source.VAL".into()))
            .unwrap();
        rec.put_field("WAIT1", EpicsValue::Short(1)).unwrap();

        assert_eq!(rec.get_field("DLY1"), Some(EpicsValue::Double(1.5)));
        assert_eq!(rec.get_field("DO1"), Some(EpicsValue::Double(42.0)));
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String("hello".into()))
        );
        assert_eq!(
            rec.get_field("LNK1"),
            Some(EpicsValue::String("target.VAL".into()))
        );
        assert_eq!(
            rec.get_field("DOL1"),
            Some(EpicsValue::String("source.VAL".into()))
        );
        assert_eq!(rec.get_field("WAIT1"), Some(EpicsValue::Short(1)));
    }

    #[test]
    fn test_sseq_step_a_suffix() {
        let mut rec = SseqRecord::new();
        rec.put_field("DLYA", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("DOA", EpicsValue::Double(99.0)).unwrap();
        rec.put_field("STRA", EpicsValue::String("step10".into()))
            .unwrap();
        rec.put_field("LNKA", EpicsValue::String("out10.VAL".into()))
            .unwrap();

        assert_eq!(rec.get_field("DLYA"), Some(EpicsValue::Double(2.0)));
        assert_eq!(rec.get_field("DOA"), Some(EpicsValue::Double(99.0)));
        assert_eq!(
            rec.get_field("STRA"),
            Some(EpicsValue::String("step10".into()))
        );
        assert_eq!(
            rec.get_field("LNKA"),
            Some(EpicsValue::String("out10.VAL".into()))
        );
    }

    #[test]
    fn test_sseq_all_steps() {
        let mut rec = SseqRecord::new();
        // Set all 10 steps
        for i in 1..=9 {
            let dly_name = format!("DLY{}", i);
            rec.put_field(&dly_name, EpicsValue::Double(i as f64))
                .unwrap();
        }
        rec.put_field("DLYA", EpicsValue::Double(10.0)).unwrap();

        for i in 1..=9 {
            let dly_name = format!("DLY{}", i);
            assert_eq!(rec.get_field(&dly_name), Some(EpicsValue::Double(i as f64)));
        }
        assert_eq!(rec.get_field("DLYA"), Some(EpicsValue::Double(10.0)));
    }

    #[test]
    fn test_sseq_selm_all() {
        let rec = SseqRecord::new();
        for i in 0..NUM_STEPS {
            assert!(rec.should_execute_step(i));
        }
    }

    #[test]
    fn test_sseq_selm_specified() {
        let mut rec = SseqRecord::new();
        rec.selm = 1; // Specified
        rec.seln = 3; // Select step 3
        assert!(!rec.should_execute_step(0));
        assert!(!rec.should_execute_step(1));
        assert!(rec.should_execute_step(2)); // step 3 is index 2
        assert!(!rec.should_execute_step(3));
    }

    #[test]
    fn test_sseq_selm_mask() {
        let mut rec = SseqRecord::new();
        rec.selm = 2; // Mask
        rec.seln = 0b0000_0101; // Steps 1 and 3
        assert!(rec.should_execute_step(0));
        assert!(!rec.should_execute_step(1));
        assert!(rec.should_execute_step(2));
        assert!(!rec.should_execute_step(3));
    }

    #[test]
    fn test_sseq_process() {
        let mut rec = SseqRecord::new();
        rec.process().unwrap();
        assert_eq!(rec.busy, 0);
    }

    #[test]
    fn test_sseq_field_not_found() {
        let mut rec = SseqRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_sseq_type_mismatch() {
        let mut rec = SseqRecord::new();
        assert!(
            rec.put_field("SELM", EpicsValue::String("x".into()))
                .is_err()
        );
        assert!(rec.put_field("STR1", EpicsValue::Double(1.0)).is_err());
    }

    #[test]
    fn test_sseq_field_list() {
        let rec = SseqRecord::new();
        let fields = rec.field_list();
        // 8 base (VAL/SELM/SELN/SELL/PREC/ABORT/ABORTING/BUSY) + 13 per
        // step * 10 steps = 138 fields (full sseqRecord.dbd surface).
        assert_eq!(fields.len(), 138);
        // The generated per-step table must not collide on a field name.
        let mut names: Vec<&str> = fields.iter().map(|f| f.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "field names must be unique");
    }

    #[test]
    fn test_sseq_status_fields_openable() {
        // The per-step diagnostics and top-level ABORTING must be
        // openable/readable (clients previously got field-not-found).
        // They expose their sseqRecord.dbd init defaults: the live values
        // are driven by the async sequence owner, not yet ported.
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("DT1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("LT1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WERR1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTG1"), Some(EpicsValue::Short(0)));
        // A-suffix step variants.
        assert_eq!(rec.get_field("DTA"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTGA"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WERRA"), Some(EpicsValue::Short(0)));
        // DOLnV / LNKnV menu init = sseqLNKV_EXT (1).
        assert_eq!(rec.get_field("DOL1V"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.get_field("LNK1V"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.get_field("DOLAV"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.get_field("LNKAV"), Some(EpicsValue::Enum(1)));
        // Top-level ABORTING.
        assert_eq!(rec.get_field("ABORTING"), Some(EpicsValue::Short(0)));
    }

    #[test]
    fn test_sseq_ix_is_step_index() {
        // IXn holds the step's own 0-based index (sseqRecord.dbd
        // initial: IX1=0 .. IXA=9).
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("IX1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("IX2"), Some(EpicsValue::Short(1)));
        assert_eq!(rec.get_field("IX9"), Some(EpicsValue::Short(8)));
        assert_eq!(rec.get_field("IXA"), Some(EpicsValue::Short(9)));
    }

    #[test]
    fn test_sseq_link_status_menu_choices() {
        // DOLnV / LNKnV are menu(sseqLNKV); served as DBR_ENUM with the
        // four connection-status labels. The link fields they shadow
        // (DOL1 / LNK1) are NOT menus.
        let rec = SseqRecord::new();
        let choices = rec
            .menu_field_choices("DOL1V")
            .expect("DOL1V is a menu field");
        assert_eq!(choices, &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"]);
        assert_eq!(rec.menu_field_choices("LNKAV"), Some(SSEQ_LNKV_CHOICES));
        assert!(rec.menu_field_choices("DOL1").is_none());
        assert!(rec.menu_field_choices("LNK1").is_none());
    }

    #[test]
    fn test_sseq_status_fields_read_only() {
        // Diagnostics are SPC_NOMOD in the DBD (sseqRecord.dbd); ABORTING's
        // live transition is machine-owned. All are read-only here. The
        // writable control/step fields stay writable.
        let rec = SseqRecord::new();
        let fields = rec.field_list();
        for ro_name in [
            "ABORTING", "DT1", "LT1", "WERR1", "WTG1", "IX1", "DOL1V", "LNK1V", "DTA", "LNKAV",
        ] {
            let f = fields
                .iter()
                .find(|f| f.name == ro_name)
                .unwrap_or_else(|| panic!("{ro_name} present in field_list"));
            assert!(f.read_only, "{ro_name} must be read-only");
        }
        for rw_name in ["VAL", "ABORT", "DLY1", "WAIT1", "LNK1", "STRA"] {
            let f = fields.iter().find(|f| f.name == rw_name).unwrap();
            assert!(!f.read_only, "{rw_name} stays writable");
        }
    }
}
