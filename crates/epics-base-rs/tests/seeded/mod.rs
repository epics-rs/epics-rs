//! A record alone in its own database, seeded through the database put.
//!
//! The shared half of the tests that set metadata fields on a record and then
//! read what it serves: the database put reaches a field the record leaves to
//! the instance, refreshes the metadata the record serves from, and takes the
//! same gates a client's put does.

use std::sync::Arc;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, RecordCell};
use epics_base_rs::types::EpicsValue;

pub struct Seeded {
    db: PvDatabase,
    name: String,
}

impl Seeded {
    pub fn new(name: impl Into<String>, rec: Box<dyn Record>) -> Self {
        let name = name.into();
        let db = PvDatabase::new();
        block_on_sync(db.add_record(&name, rec))
            .expect("a plain test thread")
            .unwrap();
        Self { db, name }
    }

    pub fn cell(&self) -> Arc<RecordCell> {
        self.db.get_record(&self.name).unwrap()
    }

    pub fn put(&self, field: &str, value: EpicsValue) -> CaResult<()> {
        let pv = format!("{}.{field}", self.name);
        block_on_sync(self.db.put_pv_no_process(pv, value)).expect("a plain test thread")
    }
}
