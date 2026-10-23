//! A local DB link whose filter chain `dbChannelCreate` refuses becomes a CA
//! link (`dbDbLink.c:89-102`, `dbLink.c:118-130`), and the refusal prints
//! twice: once from `dbDbInitLink`, once when the CA link creates its
//! channel. softIoc R7.0.10-146-g8f5015b66 on the DB below:
//!
//! ```text
//! Starting iocInit
//! dbChannelCreate: Channel filter 'x' not found
//! dbChannelCreate: lexical error: invalid char in json text.
//!                                       {]
//!                      (right here) ------^
//!
//! dbChannelCreate: Channel filter 'x' not found
//! dbChannelCreate: lexical error: invalid char in json text.
//!                                       {]
//!                      (right here) ------^
//!
//! ```
//!
//! and iocInit then never returns: `dbCaRun` waits for the local link's first
//! event (a C bug). The port does not wait for it, so the link is observable:
//! never connected, and a read of it raises LINK/INVALID.

mod calink_ioc;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_strip_ansi};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ParsedLink;

const DB: &str = r#"
record(ai, "F") { field(VAL, "1") }
record(ai, "S") { field(INP, "F.{\"x\":1}") }
record(ai, "T") { field(INP, "F.{]") }
"#;

#[epics_macros_rs::epics_test]
async fn both_refusals_print_and_the_link_stays_unconnected() {
    let heard = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push_str(&errlog_strip_ansi(m)));
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // `ioc_app`'s `dbInitLink` pass, which `build` leaves to iocInit.
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.setup_external_link_opens().await;
    let once = "dbChannelCreate: Channel filter 'x' not found\n\
                dbChannelCreate: lexical error: invalid char in json text.\n                                      {]\n                     (right here) ------^\n\n";
    let twice = once.repeat(2);
    calink_ioc::until("both links opened", || {
        errlog_flush();
        heard.lock().unwrap().contains(&twice)
    })
    .await;
    assert_eq!(*heard.lock().unwrap(), twice);

    for rec in ["S", "T"] {
        let inp = db
            .record_link_fields(rec)
            .into_iter()
            .find(|(f, _, _)| f == "INP")
            .map(|(_, _, p)| p);
        assert!(matches!(inp, Some(ParsedLink::Ca(_))), "{rec}: {inp:?}");
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
        let num = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().to_f64().unwrap();
        // LINK (14), INVALID (3): C `dbCaGetLink` on a link never connected.
        assert_eq!((num("STAT"), num("SEVR")), (14.0, 3.0), "{rec}");
    }
}
