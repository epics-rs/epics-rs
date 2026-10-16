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
    pub(crate) fn scan_once(&self, name: &str) {
        let db = self.clone();
        let name = RecordName::from(name);
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move {
                let mut visited = ProcStack::new();
                let _ = db.process_record_with_links(&name, &mut visited).await;
            },
        );
    }
}
