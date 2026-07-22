//! Per-channel `ReportInfo` wiring.
//!
//! pvxs lets a `Source` stash an opaque `ReportInfo` on the channel
//! control it is handed during `onCreate`
//! (`ServerChannelControl::updateInfo`, `source.h:192`); `Server::report()`
//! then surfaces that pointer in `Report::Channel::info` (`netcommon.h:75`,
//! `server.cpp`). The Rust server simplifies the opaque base to
//! `Option<String>` and queries it once at CREATE_CHANNEL from the bound
//! owner via `ChannelSource::channel_report_info`.
//!
//! This test drives the path end-to-end: a source that returns per-channel
//! info, a real in-process server+client opening a channel, and an
//! assertion that the info reaches the server report.

#![cfg(test)]
// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

// `ChannelSource` trait methods return `impl Future` (RPITIT); test impls
// mirror that shape rather than `async fn`, as in the sibling test files.
#![allow(clippy::manual_async_fn)]

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer};

/// The contextual string a source attaches to every channel it serves.
const INFO: &str = "owner=test-source pid=4242";

/// Minimal source serving one PV that attaches per-channel report info.
#[derive(Clone)]
struct InfoSource;

impl ChannelSource for InfoSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        async { vec!["dut".into()] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    fn get_introspection(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        async {
            Some(FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        async {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Int(7))));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        _: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async { Err("read-only".into()) }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        async { None }
    }
    /// The hook under test: attach per-channel info for the served PV.
    fn channel_report_info(
        &self,
        name: &str,
        _ctx: epics_pva_rs::server_native::ChannelContext,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        let attach = name == "dut";
        async move { attach.then(|| INFO.to_string()) }
    }
}

/// A source that attaches `channel_report_info` has that info surface in
/// the server report's per-channel `info` field after a channel opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_report_info_reaches_server_report() {
    let server =
        PvaServer::isolated(Arc::new(InfoSource)).expect("isolated test server must start");
    let client = server.client_config();

    // Open a channel (CREATE_CHANNEL + GET). The channel stays open on the
    // server until the client disconnects, so it is present in the report.
    let _ = tokio::time::timeout(Duration::from_secs(5), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");

    let report = server.report();
    let infos: Vec<Option<String>> = report
        .peers
        .iter()
        .flat_map(|(_addr, snap)| snap.channels_detail.iter())
        .filter(|c| c.name == "dut")
        .map(|c| c.report_info.clone())
        .collect();

    assert!(
        !infos.is_empty(),
        "the opened `dut` channel must appear in the server report — got peers {:?}",
        report.peers
    );
    assert!(
        infos.iter().any(|i| i.as_deref() == Some(INFO)),
        "source-supplied report info must reach the report; got {infos:?}"
    );

    // Hold the client until the assertions run so the channel is not torn
    // down out from under the report.
    drop(client);
}

/// Control: a default source (no `channel_report_info` override) leaves the
/// report's per-channel `info` field `None`, matching pvxs leaving
/// `chan->reportInfo` null.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_source_leaves_report_info_none() {
    #[derive(Clone)]
    struct PlainSource;
    impl ChannelSource for PlainSource {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            async { vec!["dut".into()] }
        }
        fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = n.to_string();
            async move { n == "dut" }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async {
                Some(FieldDesc::Structure {
                    struct_id: "epics:nt/NTScalar:1.0".into(),
                    fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
                })
            }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            async {
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields
                    .push(("value".into(), PvField::Scalar(ScalarValue::Int(7))));
                Some(PvField::Structure(s))
            }
        }
        fn put_value(
            &self,
            _: &str,
            _: PvField,
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
            async { Err("read-only".into()) }
        }
        fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
            async { false }
        }
        fn subscribe(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
            async { None }
        }
    }

    let server =
        PvaServer::isolated(Arc::new(PlainSource)).expect("isolated test server must start");
    let client = server.client_config();

    let _ = tokio::time::timeout(Duration::from_secs(5), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");

    let report = server.report();
    let any_dut = report
        .peers
        .iter()
        .flat_map(|(_addr, snap)| snap.channels_detail.iter())
        .any(|c| c.name == "dut");
    assert!(
        any_dut,
        "the opened `dut` channel must appear in the report"
    );

    let all_none = report
        .peers
        .iter()
        .flat_map(|(_addr, snap)| snap.channels_detail.iter())
        .filter(|c| c.name == "dut")
        .all(|c| c.report_info.is_none());
    assert!(
        all_none,
        "a source with no channel_report_info override must leave report_info None"
    );

    drop(client);
}
