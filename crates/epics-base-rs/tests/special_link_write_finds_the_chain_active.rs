//! A link a `special()` writes runs on the chain the put came from.
//!
//! C oracleScalerIoc (epics-base 8f5015b66, scaler 913e2e4) on
//! `oracle-ioc/iocBoot/iocOracle/scaler-c8.db`, TPRO on S, L, Z, SF,
//! `dbpf S.CNT 1`:
//!
//! ```text
//! _main_: dbProcess of 'L'
//! _main_: dbProcess of 'Z'
//! _main_: dbProcess of Active 'L' with RPRO=0
//! _main_: dbProcess of Active 'S' with RPRO=0
//! _main_: dbProcess of 'S'
//! _main_: dbProcess of 'SF'
//! ```
//!
//! then `S.CNT 0`, `S.LCNT 0`, `L.LCNT 1`, `L.SEVR NO_ALARM`, `Z.LCNT 0`,
//! `SF.VAL 1`. Z's NPP write of `S.CNT` fires COUTP again from inside L's
//! `processTarget(L, Z)`, which holds L PACT, and L's FLNK finds S PACT from
//! the outer `processTarget(S, L)`. The record here is the scaler's CNT/COUTP
//! `special()` and nothing else, so the chain needs no scaler driver.

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessAction, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

/// `scalerRecord.c` `special()`'s CNT arm, C:623-624: every CNT put writes
/// COUTP from inside `dbPut`.
#[derive(Default)]
struct CoutpRecord {
    cnt: i16,
    coutp: String,
    queued: Vec<ProcessAction>,
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Long, false),
    FieldDesc::new("CNT", DbFieldType::Short, false),
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("COUTP", DbFieldType::String, false)
    },
];

impl Record for CoutpRecord {
    fn record_type(&self) -> &'static str {
        "coutp_test"
    }
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["CNT"]
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "CNT" => Some(EpicsValue::Short(self.cnt)),
            "COUTP" => Some(EpicsValue::String(self.coutp.clone().into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "CNT" => {
                self.cnt = value.to_f64().unwrap_or(0.0) as i16;
                Ok(())
            }
            "COUTP" => {
                self.coutp = value.text_g();
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if after && field == "CNT" {
            self.queued.push(ProcessAction::WriteDbLink {
                link_field: "COUTP",
                value: EpicsValue::Short(self.cnt),
            });
        }
        Ok(())
    }
    fn take_special_actions(&mut self) -> Vec<ProcessAction> {
        std::mem::take(&mut self.queued)
    }
}

fn num(db: &epics_base_rs::server::database::PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

const CHILD: &str = "COUTP_TRACE_CHILD";

/// This test run again in a child whose stdout is not captured, and the
/// `dbProcess` lines it printed, context cut.
fn trace_of(test: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| {
            l.split_once(": dbProcess")
                .map(|(_, r)| format!("dbProcess{r}"))
        })
        .collect()
}

#[epics_macros_rs::epics_test]
async fn a_coutp_write_back_into_the_chain_stops_at_the_active_records() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("a_coutp_write_back_into_the_chain_stops_at_the_active_records"),
            [
                "dbProcess of 'L'",
                "dbProcess of 'Z'",
                "dbProcess of Active 'L' with RPRO=0",
                "dbProcess of Active 'S' with RPRO=0",
                "dbProcess of 'S'",
                "dbProcess of 'SF'",
            ]
        );
        return;
    }
    let (db, _) = IocBuilder::new()
        .register_record_type("coutp_test", || Box::new(CoutpRecord::default()))
        .db_string(
            r#"
record(coutp_test, "S") { field(COUTP, "L PP") field(FLNK, "SF") }
record(longout, "L") { field(OUT, "Z.PROC") field(FLNK, "S") }
record(longout, "Z") { field(VAL, "0") field(OUT, "S.CNT") }
record(calc, "SF") { field(CALC, "A+1") field(INPA, "SF") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    for r in ["S", "L", "Z", "SF"] {
        db.put_pv(&format!("{r}.TPRO"), EpicsValue::Char(1))
            .await
            .unwrap();
    }

    db.put_record_field_from_ca("S", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();

    let got: Vec<(&str, f64)> = ["S.CNT", "S.LCNT", "L.LCNT", "L.SEVR", "Z.LCNT", "SF.VAL"]
        .into_iter()
        .map(|pv| (pv, num(&db, pv)))
        .collect();
    assert_eq!(
        got,
        [
            ("S.CNT", 0.0),
            ("S.LCNT", 0.0),
            ("L.LCNT", 1.0),
            ("L.SEVR", 0.0),
            ("Z.LCNT", 0.0),
            ("SF.VAL", 1.0),
        ]
    );
}
