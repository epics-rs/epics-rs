//! Each forward link holds its own `caLink` (`dbCa.c:398-414`), so two fanout
//! `LNKn` naming one remote `.PROC` each `dbCaPutLink` a 1 through
//! `scanForward` (`dbCa.c:643-648`) and neither supersedes the other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkPutOp, LinkSet, ProcStack, PutAdmission};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct Remote {
    puts: Arc<Mutex<Vec<String>>>,
}

#[epics_base_rs::async_trait]
impl LinkSet for Remote {
    fn is_connected(&self, _name: &[u8]) -> bool {
        true
    }
    fn put_admission(&self, _name: &[u8]) -> PutAdmission {
        PutAdmission::Connected
    }
    async fn get_value(&self, _name: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(
        &self,
        name: &[u8],
        _value: EpicsValue,
        _op: LinkPutOp,
    ) -> Result<(), String> {
        self.puts
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(name).into_owned());
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn two_lnkn_rows_naming_one_pv_put_twice() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(fanout, "F") { field(LNK1, "R:X.PROC CA") field(LNK2, "R:X.PROC CA") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let puts = Arc::new(Mutex::new(Vec::new()));
    db.register_link_set("ca", Arc::new(Remote { puts: puts.clone() }))
        .await;
    db.process_record_with_links("F", &mut ProcStack::new())
        .await
        .unwrap();
    db.sync_external_link_puts().await;
    assert_eq!(*puts.lock().unwrap(), ["R:X.PROC", "R:X.PROC"]);
    assert_eq!(db.external_link_puts_coalesced(), 0);
}
