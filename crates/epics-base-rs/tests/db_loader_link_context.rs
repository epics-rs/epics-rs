//! A record type that declares `INP`/`OUT` in its own `field_list` — mirroring
//! its C `.dbd`, as `scalerRecord`, `motorRecord` and `acalcout` do — must still
//! hand the link text to device-support init.
//!
//! C parity: device support dereferences the link itself at init, regardless of
//! which layer "owns" the field. `devScalerAsyn.c::scaler_init_record` reads
//! `prec->out` to pick the board a `scalerRecord` instance talks to; the record
//! owning `OUT` in its dbd changes nothing about that. The Rust loader used to
//! route a record-declared field to `Record::put_field` *only*, leaving
//! `RecordCommon.out` — the single source of truth `DeviceSupportContext` is
//! built from — empty. A dynamic factory therefore saw `ctx.out == ""` and could
//! not disambiguate two boards by their OUT link.
//!
//! The two halves of the invariant are pinned separately below, because they are
//! deliberately distinct: `common.out` is the link *text* (always populated),
//! while `parsed_out` is the *framework's* dispatch of that link (armed only for
//! a record type that does not declare the field, so a record driving its own
//! link is not driven twice per cycle).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ParsedLink, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Stand-in for a C-faithful custom record type: it declares `OUT` in its own
/// field list, exactly as `scalerRecord.dbd` does.
#[derive(Default)]
struct OwnsOutRecord {
    val: i32,
    out: String,
}

static OWNS_OUT_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Long, false),
    FieldDesc::new("OUT", DbFieldType::String, false),
];

impl Record for OwnsOutRecord {
    fn record_type(&self) -> &'static str {
        "ownsOut"
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        OWNS_OUT_FIELDS
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Long(v)) => self.val = v,
            ("OUT", EpicsValue::String(s)) => self.out = s.as_str_lossy().into_owned(),
            _ => {}
        }
        Ok(())
    }
}

struct NoopDevice;

impl DeviceSupport for NoopDevice {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "ownsOutDev"
    }
}

const OUT_LINK: &str = "#C0 S0 @scaler1";

/// The db file's `OUT` value must reach the dynamic device-support factory.
/// Before the fix `ctx.out` was `""` and a factory keying on the link (C
/// `scaler_init_record`) could not tell two boards apart.
#[epics_macros_rs::epics_test]
async fn record_owned_out_link_reaches_device_support_context() {
    let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);

    let db = format!(
        r#"
record(ownsOut, "SCALER:1") {{
    field(DTYP, "ownsOutDev")
    field(OUT, "{OUT_LINK}")
}}
"#
    );

    let (database, _) = IocBuilder::new()
        .register_record_type("ownsOut", || Box::new(OwnsOutRecord::default()))
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| {
            captured.lock().unwrap().push((
                ctx.dtyp.to_string(),
                ctx.inp.to_string(),
                ctx.out.to_string(),
            ));
            Some(Box::new(NoopDevice) as Box<dyn DeviceSupport>)
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let contexts = std::mem::take(&mut *seen.lock().unwrap());
    assert_eq!(contexts.len(), 1, "dynamic factory should run once");
    let (dtyp, inp, out) = &contexts[0];
    assert_eq!(dtyp, "ownsOutDev");
    assert_eq!(inp, "", "record declares no INP");
    assert_eq!(
        out, OUT_LINK,
        "record-owned OUT must reach DeviceSupportContext.out"
    );

    // The record still stores the field itself — the mirror into the common
    // fields adds a reader, it does not move ownership.
    let instance = database.get_record("SCALER:1").expect("record loaded");
    let instance = instance.read();
    assert_eq!(
        instance.record.get_field("OUT"),
        Some(EpicsValue::String(OUT_LINK.into()))
    );
    assert_eq!(instance.common.out, OUT_LINK);
}

/// The other half of the invariant: mirroring the link *text* must not arm the
/// framework's generic single-OUT dispatch for a record that owns its OUT. If
/// `parsed_out` were populated here, a record driving its own link (`acalcout`
/// and `scalcout` via `multi_output_links`, `motorRecord`/`scalerRecord` via
/// device support) would write that link twice per process cycle.
#[epics_macros_rs::epics_test]
async fn record_owned_out_link_does_not_arm_framework_dispatch() {
    let db = r#"
record(ownsOut, "SCALER:2") {
    field(DTYP, "ownsOutDev")
    field(OUT, "TARGET:PV PP")
}
"#;

    let (database, _) = IocBuilder::new()
        .register_record_type("ownsOut", || Box::new(OwnsOutRecord::default()))
        .register_dynamic_device_support(|_ctx: &DeviceSupportContext| {
            Some(Box::new(NoopDevice) as Box<dyn DeviceSupport>)
        })
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let instance = database.get_record("SCALER:2").expect("record loaded");
    let instance = instance.read();

    // Link text: populated, so device support sees it.
    assert_eq!(instance.common.out, "TARGET:PV PP");
    // Framework dispatch: unarmed, because the record type owns the field.
    assert!(
        matches!(instance.parsed_out, ParsedLink::None),
        "framework OUT dispatch must stay unarmed for a record that owns OUT, \
         got {:?}",
        instance.parsed_out
    );
}
