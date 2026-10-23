//! C `recGblFwdLink` hands RPRO to `scanOnce` (`recGbl.c:297-300`), so the
//! reprocess runs on the scanOnce thread, not a callback band.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;

static RAN_ON: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[epics_macros_rs::epics_test]
async fn rpro_reprocesses_on_the_scan_once_thread() {
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
            r#"record(aSub, "R") { field(SNAM, "whereAmI") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    RAN_ON.lock().unwrap().clear();
    db.get_record("R").unwrap().write().common.rpro = 1;
    db.process_record_with_links("R", &mut ProcStack::new())
        .await
        .unwrap();
    for _ in 0..200 {
        if RAN_ON.lock().unwrap().len() >= 2 {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    let ran = RAN_ON.lock().unwrap();
    assert_eq!(ran.len(), 2, "{ran:?}");
    assert_eq!(ran[1], "scanOnce");
}
