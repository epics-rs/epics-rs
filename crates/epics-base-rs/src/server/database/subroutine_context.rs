//! What a [`Subroutine::Context`](crate::server::record::Subroutine) function
//! reaches besides its record's fields: C's `prec->name`, `prec->dpvt`, and
//! the IOC it takes `dbScanLock` and `scanOnce` against.

use std::any::Any;
use std::sync::{Arc, Weak};

use super::{ProcStack, PvDatabase, PvDatabaseInner};
use crate::types::RecordName;

/// The database a record lives in, held weakly so the record never keeps it
/// alive. Unset (dead) on a record no database owns.
#[derive(Clone, Default)]
pub(crate) struct RecordDb(Weak<PvDatabaseInner>);

impl RecordDb {
    pub(crate) fn of(db: &PvDatabase) -> Self {
        Self(Arc::downgrade(&db.inner))
    }
}

/// Handed to a context subroutine for one call.
pub struct SubroutineContext<'a> {
    pub(crate) name: &'a RecordName,
    pub(crate) db: &'a RecordDb,
    pub(crate) pvt: &'a mut Option<Box<dyn Any + Send + Sync>>,
}

impl SubroutineContext<'_> {
    /// C `prec->name`.
    pub fn name(&self) -> &RecordName {
        self.name
    }

    /// The record's database, or `None` once it is gone.
    pub fn db(&self) -> Option<PvDatabase> {
        self.db.0.upgrade().map(|inner| PvDatabase { inner })
    }

    /// C `prec->dpvt` as the type the INAM routine stored, or `None` when it
    /// holds nothing or something else.
    pub fn dpvt<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.pvt.as_mut()?.downcast_mut()
    }

    /// C `prec->dpvt = ...`.
    pub fn set_dpvt<T: Any + Send + Sync>(&mut self, value: T) {
        *self.pvt = Some(Box::new(value));
    }
}

impl PvDatabase {
    /// C `scanOnce(prec)`: process `name` once, after the caller lets go of
    /// it. Unlike [`crate::server::record::ProcessAction::ScanOnce`] there is
    /// no Passive gate, because no put of the caller's processes the record.
    ///
    /// Queued on the libcom `scanOnce` ring and processed on its one thread,
    /// so a full ring drops the request with C's overflow line. The frame
    /// runs to its end on that thread, as C's `onceTask` holds `dbScanLock`
    /// through `dbProcess`: the next request cannot start before it.
    pub fn scan_once(&self, name: &[u8]) {
        let db = self.clone();
        let record = RecordName::from(name);
        let _ = crate::runtime::task::background_scan_once(
            &String::from_utf8_lossy(name),
            Box::new(move || {
                let mut visited = ProcStack::new();
                let _ = db.process_record_with_links_sync(record.as_bytes(), &mut visited);
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::server::ioc_builder::IocBuilder;

    static RAN_ON: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// C `onceTask` processes the record on the `scanOnce` thread
    /// (`dbScan.c:715-717`), not on a callback band.
    #[epics_macros_rs::epics_test]
    async fn scan_once_processes_on_the_scan_once_thread() {
        let db = IocBuilder::new()
            .register_context_subroutine(
                "whereAmI",
                Box::new(|_rec, _ctx| {
                    let name = std::thread::current().name().unwrap_or("").to_string();
                    RAN_ON.lock().unwrap().push(name);
                    Ok(0)
                }),
            )
            .db_string(
                r#"record(aSub, "ONCE") { field(SNAM, "whereAmI") }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.scan_once(b"ONCE");
        for _ in 0..200 {
            if !RAN_ON.lock().unwrap().is_empty() {
                break;
            }
            crate::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*RAN_ON.lock().unwrap(), ["scanOnce"]);
    }
}
