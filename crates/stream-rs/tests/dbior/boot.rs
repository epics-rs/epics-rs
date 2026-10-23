//! A stream IOC booted on `tests/oracle/dbior`.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

/// streamApp's bus interfaces, as `dev::register_app` registers them, and
/// `db` built and run.
pub async fn boot(db: &str) -> Arc<PvDatabase> {
    // SAFETY: set before the build, which is the only reader.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", super::harness::DIR) };
    stream_rs::debug_interface::register();
    stream_rs::dummy::register();
    stream_rs::asyn::register();
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}
