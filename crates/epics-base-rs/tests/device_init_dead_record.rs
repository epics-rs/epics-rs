//! A device support that fails its `init_record` the C way — `pr->pact = 1` —
//! must leave the record DEAD, not merely flagged.
//!
//! `devAsynXXXTimeSeries.h::initRecord` rejects an FTVL that is not the signed
//! or unsigned EPICS type of its interface:
//!
//! ```c
//!     if ((pwf->ftvl != SIGNED_TYPE) && (pwf->ftvl != UNSIGNED_TYPE)) {
//!         errlogPrintf("%s::initCommon, %s field type must be ...", ...);
//!         goto bad;
//!     }
//!     ...
//! bad:
//!    pr->pact=1;
//!    return -1;
//! ```
//!
//! Nothing clears that PACT: `iocInit.c::doInitRecord0` resets it BEFORE
//! `init_record` runs (`:519-520`), and the only later release is a process
//! cycle's tail, which `dbProcess` never reaches because its already-active
//! branch turns every entry away first (`dbAccess.c:536-556`). So a user sees
//! PACT=1 forever, BUSY=0, and `caput REC.RARM 1` doing nothing — the put lands
//! in the field and sets RPRO, and no processing follows
//! (`dbAccess.c:1267-1271`).
//!
//! The port had no way for device support to say that. `init()` returned
//! `CaResult<()>`, whose `Err` is C's *other* failure shape —
//! `recGblRecordError(status, prec, ...); return status` with PACT untouched
//! (`devBiDbState.c:28-31`, `devGeneralTime.c:60-63`) — so a misconfigured
//! record kept processing with PACT=0 and drivers worked around it with an
//! inert `read()`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

const DEAD: &str = "TEST:DEAD";
const LIVE: &str = "TEST:LIVE";

/// Counts its reads and writes a value, so "never processed again" is an
/// observation rather than an assumption.
struct Probe {
    outcome: DeviceInitOutcome,
    reads: Arc<AtomicU32>,
}

impl DeviceSupport for Probe {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Ok(self.outcome)
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        record.put_field("VAL", EpicsValue::Double(7.0))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "probe"
    }
}

/// One dead record and one live one, from the same device support, so every
/// assertion below has its control.
async fn build() -> (Arc<PvDatabase>, Arc<AtomicU32>, Arc<AtomicU32>) {
    let dead_reads = Arc::new(AtomicU32::new(0));
    let live_reads = Arc::new(AtomicU32::new(0));
    let (d, l) = (Arc::clone(&dead_reads), Arc::clone(&live_reads));
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| match ctx.dtyp {
            "probeDead" => Some(Box::new(Probe {
                outcome: DeviceInitOutcome::dead(),
                reads: Arc::clone(&d),
            }) as Box<dyn DeviceSupport>),
            "probeLive" => Some(Box::new(Probe {
                outcome: DeviceInitOutcome::Live,
                reads: Arc::clone(&l),
            }) as Box<dyn DeviceSupport>),
            _ => None,
        })
        .db_string(
            &format!(
                r#"record(ai, "{DEAD}") {{ field(DTYP, "probeDead") }}
                   record(ai, "{LIVE}") {{ field(DTYP, "probeLive") }}"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    (db, dead_reads, live_reads)
}

/// What a `caget REC.<field>` reads.
fn field(db: &Arc<PvDatabase>, rec: &str, name: &str) -> EpicsValue {
    db.get_record(rec)
        .unwrap()
        .read()
        .resolve_field(name)
        .unwrap_or_else(|| panic!("{rec}.{name} exists"))
}

async fn process(db: &Arc<PvDatabase>, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// C's `bad:` arm as a client sees it: PACT reads 1 straight out of iocInit,
/// before anything has processed.
#[epics_macros_rs::epics_test]
async fn a_dead_init_leaves_pact_set_at_iocinit() {
    let (db, _, _) = build().await;

    assert_eq!(
        field(&db, DEAD, "PACT"),
        EpicsValue::Char(1),
        "C `goto bad; pr->pact = 1` — a client reads PACT=1 with nothing in flight"
    );
    assert_eq!(
        field(&db, LIVE, "PACT"),
        EpicsValue::Char(0),
        "the same device support with a Live init leaves PACT clear"
    );
}

/// The consequence, which is the whole point: `dbProcess` refuses the record,
/// so device support is never asked to read and VAL never changes. Repeated
/// attempts do not wear the state down either — the record is not "busy until
/// something completes", it is dead.
#[epics_macros_rs::epics_test]
async fn a_dead_record_never_processes_again() {
    let (db, dead_reads, live_reads) = build().await;

    for _ in 0..3 {
        process(&db, DEAD).await;
    }
    assert_eq!(
        dead_reads.load(Ordering::SeqCst),
        0,
        "dbProcess takes its already-active branch and never reaches the device"
    );
    assert_eq!(
        field(&db, DEAD, "VAL"),
        EpicsValue::Double(0.0),
        "nothing was read, so VAL stands at its init value"
    );
    assert_eq!(
        field(&db, DEAD, "PACT"),
        EpicsValue::Char(1),
        "and PACT is still set — a refused entry releases nothing"
    );

    process(&db, LIVE).await;
    assert_eq!(live_reads.load(Ordering::SeqCst), 1);
    assert_eq!(field(&db, LIVE, "VAL"), EpicsValue::Double(7.0));
}

/// The symptom a user reports — `caput REC.RARM 1` doing nothing. VAL is
/// `pp(TRUE)` on a Passive ai, so C's `dbPutField` would process the record;
/// with PACT set it sets RPRO instead and calls no `dbProcess`
/// (`dbAccess.c:1267-1271`). The put still lands in the field: only processing
/// is refused, which is exactly why the record looks alive to a client and
/// still does nothing.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_dead_record_processes_nothing() {
    let (db, dead_reads, live_reads) = build().await;

    // `dbPutField`, not `dbPutNotify`: plain `caput` builds no wait-set.
    db.put_record_field_from_ca_no_notify(DEAD, "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(
        dead_reads.load(Ordering::SeqCst),
        0,
        "the put must not bring the record back to life"
    );
    assert_eq!(
        field(&db, DEAD, "VAL"),
        EpicsValue::Double(3.0),
        "the value still lands — only processing is refused"
    );
    assert_eq!(
        field(&db, DEAD, "RPRO"),
        EpicsValue::UChar(1),
        "C records the refused request in RPRO instead of processing"
    );

    // Control: the same put on the live twin processes, and the device read
    // overwrites the put value with its own reading.
    db.put_record_field_from_ca_no_notify(LIVE, "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(live_reads.load(Ordering::SeqCst), 1);
    assert_eq!(field(&db, LIVE, "VAL"), EpicsValue::Double(7.0));
}
