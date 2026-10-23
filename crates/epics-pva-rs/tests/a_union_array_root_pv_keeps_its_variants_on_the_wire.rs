//! A `UnionArray`-root PV's introspection carries the variants it was opened
//! with: `PvField::descriptor` cannot recover them from the items.
#![cfg(feature = "client")]

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_union_array_root_pv_keeps_its_variants_on_the_wire() {
    use std::sync::Arc;
    use std::time::Duration;

    use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue, UnionItem};
    use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

    let canonical = FieldDesc::UnionArray {
        struct_id: String::new(),
        variants: vec![
            ("as_int".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("as_double".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    };
    let pv = SharedPV::new();
    pv.open(
        canonical.clone(),
        PvField::UnionArray(vec![Some(UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(ScalarValue::Int(7)),
        })]),
    )
    .unwrap();
    let source = SharedSource::new();
    source.add("TEST:WIRE:UARR", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let got = tokio::time::timeout(
        Duration::from_secs(5),
        server.client_config().pvinfo("TEST:WIRE:UARR"),
    )
    .await
    .expect("pvinfo timeout")
    .expect("pvinfo failed");
    assert_eq!(got, canonical);
}
