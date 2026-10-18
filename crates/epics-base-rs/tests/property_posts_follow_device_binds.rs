//! A device's PROPERTY posts (C `registerInterruptUser(callbackEnum)`,
//! devAsynInt32.c:319) reach the record from whichever dset object is bound:
//! iocInit's, or the one a device-link put binds (`dbPutFieldLink`,
//! dbAccess.c:1183-1201).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::device_support::{DeviceSupport, PropertyPost};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

static POSTED: DevSup = DevSup {
    name: "devAiPosted",
    choice: "Posted",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

/// Each device object's post sender, by creation order.
type Made = Arc<Mutex<Vec<Option<mpsc::Sender<PropertyPost>>>>>;

struct Posted {
    id: usize,
    made: Made,
}

impl DeviceSupport for Posted {
    fn property_post_receiver(&mut self) -> Option<mpsc::Receiver<PropertyPost>> {
        let (tx, rx) = mpsc::channel(4);
        self.made.lock().unwrap()[self.id] = Some(tx);
        Some(rx)
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Posted"
    }
}

#[epics_macros_rs::epics_test]
async fn a_relinked_dset_posts_its_properties() {
    let made: Made = Arc::default();
    let factory = made.clone();
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &POSTED, move || {
            let mut made = factory.lock().unwrap();
            made.push(None);
            Box::new(Posted {
                id: made.len() - 1,
                made: factory.clone(),
            })
        })
        .db_string(
            r#"record(ai, "R") { field(DTYP, "Posted") field(INP, "@a") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let post = async |id: usize, egu: &str| {
        let tx = made.lock().unwrap()[id]
            .clone()
            .unwrap_or_else(|| panic!("device {id} never asked for its receiver"));
        tx.send(PropertyPost {
            writes: vec![("EGU".into(), EpicsValue::String(egu.into()))],
            post_field: "VAL".into(),
        })
        .await
        .unwrap();
        for _ in 0..200 {
            if db.get_pv("R.EGU").unwrap() == EpicsValue::String(egu.into()) {
                return true;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        false
    };

    assert!(post(0, "iocinit").await, "bound at iocInit");

    db.put_record_field_from_ca_no_notify("R", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    assert_eq!(made.lock().unwrap().len(), 2);
    assert!(post(1, "relinked").await, "bound by a device-link put");
}
