//! `IocBuilder::build` starts the `dbCaLink` worker before any
//! `init_record`, as `iocBuild`'s `dbCaLinkInit` precedes `initDatabase`.

#[path = "ca_link_init_precedes_init_record/probe.rs"]
mod probe;

#[epics_macros_rs::epics_test]
async fn build_starts_the_worker_before_init_record() {
    let _ = probe::builder().build().await.unwrap();
    assert_eq!(*probe::SEEN.lock().unwrap(), [true]);
}
