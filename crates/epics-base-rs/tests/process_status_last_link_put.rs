//! [`ProcessStatusRule::status_links`]: `process()` returns the status of the
//! last put to one of them, as scanparm's `status` is OGO's put and then
//! OLOAD's (`scanparmRecord.c:250-258`); a put to any other link is not it.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, ProcessStatusRule, Record,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

const fn outlink(name: &'static str) -> FieldDesc {
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new(name, DbFieldType::String, false)
    }
}

static FIELDS: &[FieldDesc] = &[outlink("OA"), outlink("OB"), outlink("OC")];

/// Puts 1 through OA, OB and OC in that order; OA and OB assign the status.
#[derive(Default)]
struct LastPut {
    links: [String; 3],
}

impl Record for LastPut {
    fn record_type(&self) -> &'static str {
        "last_put_test"
    }
    fn process_status_rule(&self) -> ProcessStatusRule {
        ProcessStatusRule::ZERO.with_status_links(&["OA", "OB"])
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete_with(
            ["OA", "OB", "OC"]
                .into_iter()
                .map(|link_field| ProcessAction::WriteDbLink {
                    link_field,
                    value: EpicsValue::Double(1.0),
                })
                .collect(),
        ))
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        let i = ["OA", "OB", "OC"].iter().position(|f| *f == name)?;
        Some(EpicsValue::String(self.links[i].clone().into()))
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let i = ["OA", "OB", "OC"]
            .iter()
            .position(|f| *f == name)
            .ok_or_else(|| CaError::FieldNotFound(name.into()))?;
        self.links[i] = value.to_string();
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

/// `R` with OA, OB, OC each to `T` (succeeds) or `FAIL` (process returns -1).
async fn ioc(oa: &str, ob: &str, oc: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("last_put_test", || Box::new(LastPut::default()))
        .register_subroutine("minusOne", |_| Ok(-1))
        .db_string(
            &format!(
                r#"
record(ao, "T") {{}}
record(sub, "FAIL") {{ field(SNAM, "minusOne") }}
record(last_put_test, "R") {{ field(OA, "{oa} PP") field(OB, "{ob} PP") field(OC, "{oc} PP") }}
"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

async fn put_ok(oa: &str, ob: &str, oc: &str) -> bool {
    ioc(oa, ob, oc)
        .await
        .put_record_field_from_ca_no_notify("R", "PROC", EpicsValue::Short(1))
        .await
        .is_ok()
}

#[epics_macros_rs::epics_test]
async fn a_later_status_put_that_succeeds_clears_the_failure() {
    assert!(put_ok("FAIL", "T", "T").await);
}

#[epics_macros_rs::epics_test]
async fn the_last_status_put_failing_is_returned() {
    assert!(!put_ok("T", "FAIL", "T").await);
    assert!(!put_ok("FAIL", "FAIL", "T").await);
}

#[epics_macros_rs::epics_test]
async fn a_put_to_another_link_is_not_the_status() {
    assert!(put_ok("T", "T", "FAIL").await);
}
