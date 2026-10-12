//! C `dbPutLinkLS` (`dbLink.c:527-538`) over a CA link: `dbGetLinkDBFtype`
//! is the channel's native type (`dbCa.c:695-704`), so an lso into a
//! `DBF_DOUBLE` channel sends one `DBR_STRING` and into a `DBF_CHAR` one the
//! characters. While the channel is down the type is -1 (`pcaGetCheck`,
//! `dbCa.c:650-660`): nothing is put and no LINK alarm is raised. Both the
//! `ca://` form and a DB-spelled name this IOC does not hold.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{
    LinkDbfType, LinkMetadata, LinkPutOp, LinkSet, PutAdmission, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

/// AO is a `DBF_DOUBLE` channel, WF a `DBF_CHAR` one, DOWN disconnected.
struct Remote {
    puts: Arc<Mutex<Vec<(String, EpicsValue)>>>,
}

fn native(name: &str) -> Option<LinkDbfType> {
    match name {
        "AO" => Some(LinkDbfType::Double),
        "WF" => Some(LinkDbfType::Char),
        _ => None,
    }
}

#[epics_base_rs::async_trait]
impl LinkSet for Remote {
    fn is_connected(&self, name: &str) -> bool {
        native(name).is_some()
    }
    fn put_admission(&self, name: &str) -> PutAdmission {
        match native(name) {
            Some(_) => PutAdmission::Connected,
            None => PutAdmission::Refused,
        }
    }
    fn get_cached_value(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, name: &str, value: EpicsValue, _op: LinkPutOp) -> Result<(), String> {
        self.puts.lock().unwrap().push((name.to_string(), value));
        Ok(())
    }
    fn link_metadata(&self, name: &str) -> Option<LinkMetadata> {
        native(name).map(|t| LinkMetadata {
            dbf_type: Some(t),
            element_count: Some(1),
            ..Default::default()
        })
    }
}

const DB: &str = r#"
record(lso,"LA"){field(SIZV,"64") field(OUT,"ca://AO")}
record(lso,"LW"){field(SIZV,"64") field(OUT,"ca://WF")}
record(lso,"LD"){field(SIZV,"64") field(OUT,"ca://DOWN")}
record(lso,"NA"){field(SIZV,"64") field(OUT,"AO")}
record(lso,"NW"){field(SIZV,"64") field(OUT,"WF")}
record(lso,"ND"){field(SIZV,"64") field(OUT,"DOWN")}
"#;

fn chars(s: &str) -> EpicsValue {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    EpicsValue::CharArray(b.into())
}

fn sevr(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.SEVR")).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_long_string_over_ca_puts_one_string_unless_the_channel_is_char() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let puts = Arc::new(Mutex::new(Vec::new()));
    db.register_link_set("ca", Arc::new(Remote { puts: puts.clone() }))
        .await;
    for (rec, s) in [("LA", "12.5"), ("LW", "hello"), ("LD", "1")] {
        db.put_record_field_from_ca(rec, "VAL$", chars(s))
            .await
            .unwrap();
    }
    for (rec, s) in [("NA", "42"), ("NW", "hi"), ("ND", "2")] {
        db.put_record_field_from_ca(rec, "VAL$", chars(s))
            .await
            .unwrap();
    }
    db.sync_external_link_puts().await;

    let string = |s: &str| EpicsValue::String(PvString::from(s));
    assert_eq!(
        *puts.lock().unwrap(),
        vec![
            ("AO".to_string(), string("12.5")),
            ("WF".to_string(), chars("hello")),
            ("AO".to_string(), string("42")),
            ("WF".to_string(), chars("hi")),
        ],
        "nothing reaches DOWN"
    );
    for rec in ["LA", "LW", "LD", "NA", "NW", "ND"] {
        assert_eq!(sevr(&db, rec), 0.0, "{rec}: a down channel is status 0");
    }
}
