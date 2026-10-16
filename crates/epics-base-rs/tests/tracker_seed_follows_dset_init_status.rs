//! `longinRecord.c:114-120` returns a nonzero dset `init_record` status before
//! `prec->mlst = prec->val`; `mbbiRecord.c:133-137` keeps the status and seeds
//! anyway. Both shapes of a port failure (`Err`, `Dead`) are a nonzero status.

mod probe_rows;

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

struct Probe(Option<DeviceInitOutcome>);

impl DeviceSupport for Probe {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        self.0
            .ok_or_else(|| CaError::InvalidValue("probe init fails".into()))
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "probe"
    }
}

async fn ioc() -> Arc<PvDatabase> {
    let mut db = String::new();
    for rtype in ["longin", "mbbi"] {
        for dtyp in ["probeLive", "probeDead", "probeErr"] {
            db += &format!(
                "record({rtype}, \"{rtype}:{dtyp}\") {{ field(DTYP, \"{dtyp}\") field(VAL, \"3\") }}\n"
            );
        }
    }
    probe_ioc(&db).await
}

/// One record of `rtype` per probe outcome, named `<rtype>:<dtyp>`, each
/// carrying `fields`.
async fn probe_ioc_of(rtype: &str, fields: &str) -> Arc<PvDatabase> {
    let mut db = String::new();
    for dtyp in ["probeLive", "probeDead", "probeErr"] {
        db += &format!(
            "record({rtype}, \"{rtype}:{dtyp}\") {{ field(DTYP, \"{dtyp}\") {fields} }}\n"
        );
    }
    probe_ioc(&db).await
}

async fn probe_ioc(db: &str) -> Arc<PvDatabase> {
    let devices = probe_rows::devices(&[
        "ai",
        "ao",
        "bi",
        "bo",
        "longin",
        "longout",
        "mbbi",
        "mbbo",
        "mbbiDirect",
        "mbboDirect",
        "stringin",
        "stringout",
        "lsi",
        "lso",
    ]);
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            let outcome = match ctx.dtyp {
                "probeLive" => Some(DeviceInitOutcome::ok()),
                "probeDead" => Some(DeviceInitOutcome::dead()),
                "probeErr" => None,
                _ => return None,
            };
            Some(Box::new(Probe(outcome)) as Box<dyn DeviceSupport>)
        })
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// `field` of each probe outcome's record, as `[Live, Dead, Err]`.
fn by_outcome(db: &PvDatabase, rtype: &str, field: &str) -> [Option<EpicsValue>; 3] {
    ["probeLive", "probeDead", "probeErr"].map(|dtyp| {
        db.get_record(format!("{rtype}:{dtyp}"))
            .unwrap()
            .read()
            .record
            .get_field(field)
    })
}

fn mlst(db: &PvDatabase, name: &str) -> f64 {
    db.get_record(name)
        .unwrap()
        .read()
        .record
        .get_field("MLST")
        .and_then(|v| v.to_f64())
        .unwrap()
}

#[epics_macros_rs::epics_test]
async fn longin_seeds_only_after_a_successful_dset_init_and_mbbi_always() {
    let db = ioc().await;
    for (name, want) in [
        ("longin:probeLive", 3.0),
        ("longin:probeDead", 0.0),
        ("longin:probeErr", 0.0),
        ("mbbi:probeLive", 3.0),
        ("mbbi:probeDead", 3.0),
        ("mbbi:probeErr", 3.0),
    ] {
        assert_eq!(mlst(&db, name), want, "{name}.MLST");
    }
}

/// `biRecord.c:112` returns a nonzero dset status before `:114-116` seed MLST,
/// LALM and ORAW.
#[epics_macros_rs::epics_test]
async fn bi_seeds_mlst_lalm_oraw_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("bi", r#"field(VAL, "1") field(RVAL, "7")"#).await;
    for field in ["MLST", "LALM"] {
        let got = by_outcome(&db, "bi", field).map(|v| v.and_then(|v| v.to_f64()));
        assert_eq!(got, [Some(1.0), Some(0.0), Some(0.0)], "bi.{field}");
    }
    let oraw = by_outcome(&db, "bi", "ORAW").map(|v| v.and_then(|v| v.to_f64()));
    assert_eq!(oraw, [Some(7.0), Some(0.0), Some(0.0)], "bi.ORAW");
}

/// `aoRecord.c:152` returns `S_dev_badInitRet` for a dset status other than
/// 0 or 2, before `:156` (`oval = pval = val`) and `:160-161` (ORAW, ORBV).
/// The Live probe returns 0, so `:132-146` converts RVAL into VAL first.
#[epics_macros_rs::epics_test]
async fn ao_seeds_oval_pval_oraw_orbv_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("ao", r#"field(RVAL, "3")"#).await;
    let num = |f| by_outcome(&db, "ao", f).map(|v| v.and_then(|v| v.to_f64()));
    let [val, ..] = num("VAL");
    for field in ["OVAL", "PVAL", "ORAW", "ORBV"] {
        let [live, dead, err] = num(field);
        let want_live = if matches!(field, "OVAL" | "PVAL") {
            val
        } else {
            live
        };
        assert_eq!(
            [live, dead, err],
            [want_live, Some(0.0), Some(0.0)],
            "ao.{field}"
        );
    }
    assert_ne!(
        val,
        Some(0.0),
        "precondition: the Live ao keeps a nonzero VAL"
    );
}

/// `longoutRecord.c:117-120` returns a nonzero dset status before `:123-126`,
/// which seed PVAL with the trackers.
#[epics_macros_rs::epics_test]
async fn longout_seeds_pval_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("longout", r#"field(VAL, "5")"#).await;
    for field in ["PVAL", "MLST"] {
        let got = by_outcome(&db, "longout", field).map(|v| v.and_then(|v| v.to_f64()));
        assert_eq!(got, [Some(5.0), Some(0.0), Some(0.0)], "longout.{field}");
    }
}

/// `lsoRecord.c:82` loads DOL before the dset runs, but the OVAL/OLEN copy
/// sits after `:84-88`'s `if (status) return status`. softIoc with
/// `DOL {const:"xyz"}` reads OLEN 4.
#[epics_macros_rs::epics_test]
async fn lso_seeds_oval_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("lso", r#"field(DOL, {const:"xyz"}) field(SIZV, "40")"#).await;
    let olen = by_outcome(&db, "lso", "OLEN").map(|v| v.and_then(|v| v.to_f64()));
    assert_eq!(olen, [Some(4.0), Some(0.0), Some(0.0)], "lso.OLEN");
    let len = by_outcome(&db, "lso", "LEN").map(|v| v.and_then(|v| v.to_f64()));
    assert_eq!(len, [Some(4.0); 3], "the DOL load itself is ungated");
}

/// `lsoRecord.c:84-95` sets OVAL and OLEN below the dset status check.
/// softIoc with `DOL "5"`, a number constant that loads LEN 1 and no text,
/// reads OLEN 1 and OVAL "".
#[epics_macros_rs::epics_test]
async fn lso_seeds_olen_from_a_number_dol_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("lso", r#"field(DOL, "5") field(SIZV, "40")"#).await;
    let olen = by_outcome(&db, "lso", "OLEN").map(|v| v.and_then(|v| v.to_f64()));
    assert_eq!(olen, [Some(1.0), Some(0.0), Some(0.0)], "lso.OLEN");
    let oval = by_outcome(&db, "lso", "OVAL").map(|v| v.map(|v| v.to_string()));
    assert_eq!(oval, [const { Some(String::new()) }; 3], "lso.OVAL");
}

/// `stringinRecord.c:117` copies VAL into OVAL after `:112-115` returns a
/// nonzero dset status. softIoc with `field(VAL,"abc")` reads OVAL "abc".
#[epics_macros_rs::epics_test]
async fn stringin_seeds_oval_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("stringin", r#"field(VAL, "abc")"#).await;
    let s = |v: &str| Some(EpicsValue::String(v.into()));
    assert_eq!(
        by_outcome(&db, "stringin", "OVAL"),
        [s("abc"), s(""), s("")]
    );
}

/// `stringoutRecord.c:122` copies VAL into OVAL after `:117-120` returns a
/// nonzero dset status. softIoc with `field(VAL,"abc")` reads OVAL "abc".
#[epics_macros_rs::epics_test]
async fn stringout_seeds_oval_only_after_a_successful_dset_init() {
    let db = probe_ioc_of("stringout", r#"field(VAL, "abc")"#).await;
    let s = |v: &str| Some(EpicsValue::String(v.into()));
    assert_eq!(
        by_outcome(&db, "stringout", "OVAL"),
        [s("abc"), s(""), s("")]
    );
}

/// `mbbiRecord.c:132-140` seeds ORAW after the dset init has loaded RVAL,
/// whatever its status. softIoc with `DTYP "Raw Soft Channel"` and INP "5"
/// reads ORAW 5.
#[epics_macros_rs::epics_test]
async fn mbbi_seeds_oraw_after_the_dset_init_regardless_of_status() {
    let raw =
        probe_ioc(r#"record(mbbi, "MI") { field(DTYP, "Raw Soft Channel") field(INP, "5") }"#)
            .await;
    let rec = raw.get_record("MI").unwrap();
    assert_eq!(
        rec.read().record.get_field("ORAW"),
        Some(EpicsValue::ULong(5))
    );

    let db = probe_ioc_of("mbbi", r#"field(RVAL, "7")"#).await;
    assert_eq!(
        by_outcome(&db, "mbbi", "ORAW"),
        [const { Some(EpicsValue::ULong(7)) }; 3]
    );
}

/// `mbbiDirectRecord.c:128-129` seeds MLST and ORAW after the dset init has
/// loaded RVAL, whatever its status. softIoc with `DTYP "Raw Soft Channel"`
/// and INP "5" reads ORAW 5.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_seeds_oraw_after_the_dset_init_regardless_of_status() {
    let raw = probe_ioc(
        r#"record(mbbiDirect, "MD") { field(DTYP, "Raw Soft Channel") field(INP, "5") }"#,
    )
    .await;
    let rec = raw.get_record("MD").unwrap();
    assert_eq!(
        rec.read().record.get_field("ORAW"),
        Some(EpicsValue::ULong(5))
    );

    let db = probe_ioc_of("mbbiDirect", r#"field(RVAL, "7")"#).await;
    assert_eq!(
        by_outcome(&db, "mbbiDirect", "ORAW"),
        [const { Some(EpicsValue::ULong(7)) }; 3]
    );
}
