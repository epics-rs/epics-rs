//! A relink completes the old link's put callback still waiting on the
//! server: `dbCaRemoveLink`'s `CA_CLEAR_CHANNEL` reaches `caLinkDec`, which
//! calls `putCallback` (`dbCa.c:184-197`), and the server's later answer
//! completes nothing, because `putComplete` finds `plink` cleared
//! (`dbCa.c:1063`). So that answer cannot end the record's next cycle, whose
//! put went through the new link.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, PutAdmission};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// A put to `P1` waits for `release`; a put to any other name never ends.
struct Remote {
    puts: Arc<Mutex<Vec<String>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[epics_base_rs::async_trait]
impl LinkSet for Remote {
    fn is_connected(&self, _name: LinkRef<'_>) -> bool {
        true
    }
    fn put_admission(&self, _name: LinkRef<'_>) -> PutAdmission {
        PutAdmission::Connected
    }
    async fn get_value(&self, _name: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn put_value(
        &self,
        link: LinkRef<'_>,
        _value: EpicsValue,
        _op: LinkPutOp,
    ) -> Result<(), String> {
        let name = link.name;
        let name = String::from_utf8_lossy(name).into_owned();
        self.puts.lock().unwrap().push(name.clone());
        let held = (name == "P1")
            .then(|| self.release.lock().unwrap().take())
            .flatten();
        match held {
            Some(rx) => {
                let _ = rx.await;
            }
            None => std::future::pending::<()>().await,
        }
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn the_old_links_late_answer_does_not_complete_the_next_cycle() {
    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"record(ao,"A"){field(DTYP,"Async Soft Channel") field(OUT,"P1 CA")}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let puts = Arc::new(Mutex::new(Vec::new()));
    let (release, rx) = tokio::sync::oneshot::channel();
    db.register_link_set(
        "ca",
        Arc::new(Remote {
            puts: puts.clone(),
            release: Mutex::new(Some(rx)),
        }),
    )
    .await;
    // iocInit's link pass, then dbCaRun.
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
    let a = db.get_record("A").unwrap();
    let sent = |n: usize| {
        let puts = puts.clone();
        async move {
            while puts.lock().unwrap().len() < n {
                epics_base_rs::runtime::task::yield_now().await;
            }
        }
    };

    db.put_pv("A", EpicsValue::Double(1.0)).await.unwrap();
    db.process_record("A").await.unwrap();
    sent(1).await;
    db.put_record_field_from_ca("A", "OUT", EpicsValue::String("P2 CA".into()))
        .await
        .unwrap();
    // The relink drops PACT (`dbAccess.c:1194`), and the put callback it
    // completes runs `process()` later, from a callback (`dbLink.c:450-457`):
    // in C as here, a cycle started before that pass is the one it ends. The
    // pass commits cycle 1's alarm, so A leaves its init UDF/INVALID there.
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while db.get_pv("A.SEVR").unwrap().to_f64() != Some(0.0) {
        assert!(
            Instant::now() < deadline,
            "the relink left P1's put callback waiting"
        );
        epics_base_rs::runtime::task::yield_now().await;
    }
    db.put_pv("A", EpicsValue::Double(2.0)).await.unwrap();
    db.process_record("A").await.unwrap();
    sent(2).await;
    assert_eq!(*puts.lock().unwrap(), ["P1", "P2"]);
    assert!(a.read().is_processing(), "A waits for its put to P2");

    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            a.read().is_processing(),
            "P1's answer completed the cycle waiting on P2"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
}
