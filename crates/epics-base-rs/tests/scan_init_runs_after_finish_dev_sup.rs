//! C `iocBuild_2` runs `scanInit` after `initDatabase`,
//! `initHookAfterInitDatabase` and `finishDevSup` (`iocInit.c:178-186`), so
//! an I/O Intr record's `get_ioint_info(0)` comes after autosave pass 1 and
//! after its dset's `init(after)`, on every build path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::interrupt_accept::InterruptGate;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, IoIntrCmd,
};
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};

static ORDERED: DevSup = DevSup {
    name: "devAiOrdered",
    choice: "Ordered",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

/// Whether `finishDevSup` had run at each `get_ioint_info(0)`.
type Adds = Arc<Mutex<Vec<bool>>>;

struct Ordered {
    dev_sup: Option<Arc<InterruptGate>>,
    adds: Adds,
}

impl DeviceSupport for Ordered {
    fn bind_dev_sup_gate(&mut self, gate: &Arc<InterruptGate>) {
        self.dev_sup = Some(gate.clone());
    }

    fn get_ioint_info(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut CommonFields,
        cmd: IoIntrCmd,
    ) -> bool {
        if cmd == IoIntrCmd::Add {
            let finished = self.dev_sup.as_ref().is_some_and(|g| g.is_open());
            self.adds.lock().unwrap().push(finished);
        }
        true
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        Some(mpsc::channel(1).1)
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Ordered"
    }
}

fn factory(adds: &Adds) -> impl Fn() -> Box<dyn DeviceSupport> + Send + Sync + 'static {
    let adds = adds.clone();
    move || {
        Box::new(Ordered {
            dev_sup: None,
            adds: adds.clone(),
        })
    }
}

const RECORD: &str =
    r#"record(ai, "INTR") { field(DTYP, "Ordered") field(INP, "@a") field(SCAN, "I/O Intr") }"#;

#[epics_macros_rs::epics_test]
async fn an_ioc_builder_build_joins_io_intr_after_finish_dev_sup() {
    let adds = Adds::default();
    let (_db, _) = IocBuilder::new()
        .register_device_support("ai", &ORDERED, factory(&adds))
        .db_string(RECORD, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(*adds.lock().unwrap(), [true]);
}

#[epics_macros_rs::epics_test]
async fn an_ioc_application_joins_io_intr_after_init_database_and_finish_dev_sup() {
    let adds = Adds::default();
    let at_init_database = Arc::new(Mutex::new(None));
    let seen = at_init_database.clone();
    let counted = adds.clone();
    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("intr.db");
    std::fs::write(&db_file, RECORD).unwrap();
    IocApplication::new()
        .port(0)
        .register_device_support("ai", &ORDERED, factory(&adds))
        .startup_line(&format!("dbLoadRecords(\"{}\")", db_file.display()))
        .register_after_init_database(move |_db, _acf| async move {
            *seen.lock().unwrap() = Some(counted.lock().unwrap().len());
        })
        .run(|_config| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(
        *at_init_database.lock().unwrap(),
        Some(0),
        "no get_ioint_info before initHookAfterInitDatabase"
    );
    assert_eq!(*adds.lock().unwrap(), [true]);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}
