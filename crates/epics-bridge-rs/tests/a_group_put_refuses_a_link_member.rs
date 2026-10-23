//! A group put that writes a link member is refused in every process mode,
//! atomic or not, and the link keeps its text.
//!
//! pvxs's prep pass means to refuse it: `throw "Links not supported for put"`
//! (`ioc/groupsource.cpp:603-605`). It tests `dbChannelFinalFieldType`, which
//! `ioc/channel.cpp:69-74` has already made `DBR_CHAR` for every link field,
//! so the throw never fires and pvxs goes on to `doDbPut`
//! (`iocsource.cpp:456-462`) and rewires the link. That wrong accessor is a
//! pvxs defect this port does not copy. A group can carry `FLNK`: the type
//! builder refuses only `DBF_INLINK..=DBF_OUTLINK` (`iocsource.cpp:634-635`).
#![cfg(feature = "qsrv-core")]

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::qsrv::{BridgeProvider, ProcessMode, PutOptions, group::GroupChannel};
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.
#[tokio::test]
async fn a_flnk_member_put_is_refused_in_every_process_mode() {
    for atomic in [true, false] {
        for process in [
            ProcessMode::Inhibit,
            ProcessMode::Passive,
            ProcessMode::Force,
        ] {
            let what = format!("atomic={atomic} {process:?}");
            let (db, _) = IocBuilder::new()
                .db_string(
                    r#"
                    record(ai, "FL:SRC") { field(FLNK, "FL:A") }
                    record(calc, "FL:A") { field(CALC, "VAL+1") }
                    record(calc, "FL:B") { field(CALC, "VAL+1") }
                    "#,
                    &HashMap::new(),
                )
                .unwrap()
                .build()
                .await
                .unwrap();
            let provider = BridgeProvider::new(db.clone());
            provider
                .load_group_config(&format!(
                    r#"{{ "FG": {{
                        "+atomic": {atomic},
                        "lnk": {{ "+channel": "FL:SRC.FLNK", "+type": "plain", "+putorder": 0 }}
                    }} }}"#
                ))
                .expect("load");
            assert_eq!(provider.process_groups().await, 1, "FG is created");
            let flnk = db.get_pv("FL:SRC.FLNK").unwrap();

            let def = provider.groups().get("FG").cloned().expect("FG");
            let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
            put.fields.push((
                "lnk".into(),
                PvField::Scalar(ScalarValue::String("FL:B".into())),
            ));
            let opts = PutOptions {
                process,
                block: false,
            };
            let err = GroupChannel::new(db.clone(), def)
                .put_with_options(&put, opts, None, &Default::default())
                .await
                .expect_err(&what);
            assert!(
                err.to_string().contains("Links not supported for put"),
                "{what}: {err}"
            );
            assert_eq!(db.get_pv("FL:SRC.FLNK").unwrap(), flnk, "{what}");
            assert_eq!(
                db.get_pv("FL:B").unwrap(),
                EpicsValue::Double(0.0),
                "{what}: nothing processed"
            );
        }
    }
}
