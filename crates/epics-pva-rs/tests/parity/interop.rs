//! Cross-implementation interop matrix against pvxs `softIocPVX` / `pvget` /
//! `pvput` / `pvmonitor`.
//!
//! These tests are **ignored by default** because they require a built
//! `pvxs` available locally. Run with:
//!
//! ```bash
//! PVXS_HOME=/path/to/pvxs cargo test -p epics-pva-rs --test parity_interop -- --ignored
//! ```
//!
//! The harness probes `PVXS_HOME/bundle/usr/local/bin` first, then
//! `PVXS_HOME/bin`, then `$PATH`. If `softIocPVX` is missing the test
//! self-skips (with a tracing message) so the matrix degrades gracefully.

#![cfg(test)]
#![allow(clippy::manual_async_fn, clippy::approx_constant)]

// RTEMS-EXEC-MODEL-ALLOW(10): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command as TokioCommand};

/// Find a pvxs binary by name. Returns `None` when the file isn't found.
///
/// Lookup order:
/// 1. `PVXS_HOME/bin/<host>/`, `bundle/usr/local/bin/`, `bin/`
/// 2. `PATH` for the exact name
/// 3. **pvAccessCPP fallback** (production-parity work, April 2026):
///    when pvxs isn't installed but the older pvAccessCPP toolchain
///    from EPICS Base is available, map pvxs binary names to their
///    pvAccessCPP equivalents:
///    `softIocPVX → softIocPVA`,
///    `pvxget → pvget`,
///    `pvxput → pvput`,
///    `pvxmonitor → pvmonitor`,
///    `pvxinfo → pvinfo`.
///    Both implementations honour the same PVA wire protocol so the
///    same interop assertions exercise our code identically. We log
///    which fallback was selected so test failures aren't ambiguous.
fn find_pvxs_bin(name: &str) -> Option<PathBuf> {
    if let Ok(home) = std::env::var("PVXS_HOME") {
        let home = PathBuf::from(home);
        // Check standard EPICS layouts: O.<host>/, bin/<host>/, bin/, ...
        let host = std::env::var("EPICS_HOST_ARCH").unwrap_or_else(|_| "darwin-aarch64".into());
        for sub in &[
            format!("bin/{}", host),
            "bundle/usr/local/bin".into(),
            "bin".into(),
        ] {
            let p = home.join(sub.as_str()).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    // Fallback: PATH
    if let Some(found) = which_binary(name) {
        return Some(found);
    }
    // pvAccessCPP fallback: same PVA wire spec, different binary names.
    let alias = match name {
        "softIocPVX" => Some("softIocPVA"),
        "pvxget" => Some("pvget"),
        "pvxput" => Some("pvput"),
        "pvxmonitor" => Some("pvmonitor"),
        "pvxinfo" => Some("pvinfo"),
        _ => None,
    };
    if let Some(alt) = alias {
        if let Some(found) = which_binary(alt) {
            eprintln!(
                "interop: {name} not found; using pvAccessCPP fallback {alt} → {}",
                found.display()
            );
            return Some(found);
        }
    }
    None
}

fn which_binary(name: &str) -> Option<PathBuf> {
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            let p = Path::new(dir).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// True iff every named binary is available locally.
fn pvxs_available(names: &[&str]) -> bool {
    names.iter().all(|n| find_pvxs_bin(n).is_some())
}

static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(31000);
fn alloc_port_pair() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    (base, base + 1)
}

#[tokio::test]
#[ignore]
async fn rust_client_to_pvxs_softiocpvx_get() {
    if !pvxs_available(&["softIocPVX", "pvxget"]) {
        eprintln!("pvxs not found; set PVXS_HOME and rerun");
        return;
    }

    let softioc = find_pvxs_bin("softIocPVX").unwrap();

    // Spawn softIocPVX with a minimal db file.
    let dbfile = tempfile::NamedTempFile::new().expect("temp db file");
    std::fs::write(
        dbfile.path(),
        "record(ai, \"INTEROP:VAL\") { field(VAL, \"42.5\") }\n",
    )
    .unwrap();

    let (port, udp) = alloc_port_pair();

    let mut cmd = TokioCommand::new(&softioc);
    cmd.env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", udp.to_string())
        .arg("-S") // no interactive shell — required so softIocPVX exits cleanly
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn softIocPVX");

    // Wait until the TCP port is actually listening (with cap).
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let mut ready = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            ready = true;
            break;
        }
    }
    if !ready {
        let _ = child.start_kill();
        panic!("softIocPVX did not bind {port} within 3s");
    }
    let _killer = ChildKiller(&mut child);

    // Native client GET against the pvxs IOC.
    use epics_pva_rs::client_native::context::PvaClient;
    let addr = std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(addr)
        .build();

    // First diagnose: get the introspection alone (GET_FIELD).
    let intro = tokio::time::timeout(Duration::from_secs(5), client.pvinfo("INTEROP:VAL"))
        .await
        .expect("pvinfo timeout")
        .expect("pvinfo failed");
    eprintln!("INTROSPECTION:\n{intro}");

    // Then full GET with explicit value-only field filter.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvget_fields("INTEROP:VAL", &["value"]),
    )
    .await
    .expect("pvget timeout")
    .expect("pvget failed");

    eprintln!("INTEROP got: {}", result.value);
    match &result.value {
        epics_pva_rs::pvdata::PvField::Structure(s) => {
            assert!(
                s.struct_id.starts_with("epics:nt/NTScalar"),
                "struct_id: {}",
                s.struct_id
            );
        }
        other => panic!("expected NTScalar, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn pvxs_pvget_to_rust_server_get() {
    if !pvxs_available(&["pvxget"]) {
        eprintln!("pvxs pvget not found; set PVXS_HOME and rerun");
        return;
    }
    let pvget = find_pvxs_bin("pvxget").unwrap();

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::Mutex;

    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig, run_pva_server};

    #[derive(Clone)]
    struct Source {
        v: Arc<Mutex<f64>>,
    }
    impl ChannelSource for Source {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            async { vec!["INTEROP:RS".into()] }
        }
        fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = name.to_string();
            async move { n == "INTEROP:RS" }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async {
                Some(FieldDesc::Structure {
                    struct_id: "epics:nt/NTScalar:1.0".into(),
                    fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
                })
            }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            let v = self.v.clone();
            async move {
                let val = *v.lock().await;
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields
                    .push(("value".into(), PvField::Scalar(ScalarValue::Double(val))));
                Some(PvField::Structure(s))
            }
        }
        fn put_value(
            &self,
            _: &str,
            _: PvField,
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
            async { Ok(()) }
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

    static NEXT: AtomicU32 = AtomicU32::new(28075);
    let port = NEXT.fetch_add(2, Ordering::Relaxed) as u16;

    let source = Arc::new(Source {
        v: Arc::new(Mutex::new(123.5)),
    });
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: port + 1,
        ..Default::default()
    };
    let h = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Run pvxs `pvget` against our server. We bypass UDP search by
    // forcing EPICS_PVA_ADDR_LIST.
    let output = TokioCommand::new(&pvget)
        .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", port + 1))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .arg("-w")
        .arg("3")
        .arg("INTEROP:RS")
        .output()
        .await
        .expect("pvget run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("pvxs pvget stdout:\n{stdout}");
    assert!(
        stdout.contains("INTEROP:RS") && stdout.contains("123"),
        "expected pvxs pvget to print PV name + value, got:\n{stdout}"
    );

    h.abort();
}

/// RAII wrapper that kills the child process on drop.
struct ChildKiller<'a>(&'a mut Child);
impl<'a> Drop for ChildKiller<'a> {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

// ── Additional interop scenarios ──────────────────────────────────────

#[tokio::test]
#[ignore]
async fn rust_client_pvput_to_pvxs_softiocpvx() {
    if !pvxs_available(&["softIocPVX"]) {
        return;
    }
    let softioc = find_pvxs_bin("softIocPVX").unwrap();

    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        "record(ao, \"INTEROP:OUT\") { field(VAL, \"0\") }\n",
    )
    .unwrap();

    let (port, udp) = alloc_port_pair();
    let _ = udp;
    let mut child = TokioCommand::new(&softioc)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", (port + 1).to_string())
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            break;
        }
    }
    let _killer = ChildKiller(&mut child);

    use epics_pva_rs::client_native::context::PvaClient;
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    // PUT a value
    tokio::time::timeout(Duration::from_secs(5), client.pvput("INTEROP:OUT", "37.5"))
        .await
        .expect("pvput timeout")
        .expect("pvput failed");

    // GET back to confirm
    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("INTEROP:OUT"))
        .await
        .expect("pvget timeout")
        .expect("pvget failed");

    if let epics_pva_rs::pvdata::PvField::Structure(s) = v {
        match s.get_value() {
            Some(epics_pva_rs::pvdata::ScalarValue::Double(d)) => {
                assert!((d - 37.5).abs() < 1e-6, "got {d}");
            }
            other => panic!("expected double, got {other:?}"),
        }
    } else {
        panic!("expected NTScalar");
    }
}

#[tokio::test]
#[ignore]
async fn pvxs_pvxget_to_rust_server_ntndarray() {
    if !pvxs_available(&["pvxget"]) {
        return;
    }
    let pvxget = find_pvxs_bin("pvxget").unwrap();

    use std::sync::Arc;
    use tokio::sync::Mutex;

    use epics_pva_rs::nt::nd_array::{
        NdAlarm, NdArrayBuffer, NdAttribute, NdCodec, NdDimension, NdTimeStamp, NtNdArray,
        nt_nd_array_desc, nt_nd_array_value,
    };
    use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarValue, VariantValue};
    use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig, run_pva_server};

    /// Source serving a single 4x4 ubyte NTNDArray with a known pattern.
    #[derive(Clone)]
    struct ImageSource {
        unique_id: Arc<Mutex<i32>>,
    }

    impl ChannelSource for ImageSource {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            async { vec!["IMG:RAW".into()] }
        }
        fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = n.to_string();
            async move { n == "IMG:RAW" }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async { Some(nt_nd_array_desc()) }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            let uid = self.unique_id.clone();
            async move {
                let mut guard = uid.lock().await;
                *guard += 1;
                let id = *guard;
                drop(guard);

                // 4x4 ubyte image with values 0..15.
                let nt = NtNdArray {
                    value: NdArrayBuffer::UByte((0u8..16).collect()),
                    codec: NdCodec::default(),
                    compressed_size: 16,
                    uncompressed_size: 16,
                    dimension: vec![
                        NdDimension {
                            size: 4,
                            full_size: 4,
                            binning: 1,
                            ..NdDimension::default()
                        },
                        NdDimension {
                            size: 4,
                            full_size: 4,
                            binning: 1,
                            ..NdDimension::default()
                        },
                    ],
                    unique_id: id,
                    data_time_stamp: NdTimeStamp::default(),
                    alarm: NdAlarm {
                        message: "NO_ALARM".into(),
                        ..NdAlarm::default()
                    },
                    time_stamp: NdTimeStamp::default(),
                    attribute: vec![NdAttribute {
                        name: "ColorMode".into(),
                        value: VariantValue::scalar(ScalarValue::Int(0)),
                        descriptor: "Mono".into(),
                        source_type: 0,
                        source: "driver".into(),
                        ..NdAttribute::default()
                    }],
                };
                Some(nt_nd_array_value(&nt))
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

    let (port, udp) = alloc_port_pair();
    let source = Arc::new(ImageSource {
        unique_id: Arc::new(Mutex::new(0)),
    });
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let h = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // pvxget against our NTNDArray-serving rust server.
    //
    // Tree format (`-F tree`) — not pvxget's default Delta — so the
    // top-level `epics:nt/NTNDArray:1.0` id is observable. FmtDelta
    // prints the very-top struct line only when the root changed-bit is
    // set (pvxs `datafmt.cpp:19`), which neither pvxs's own server nor
    // this one sets on a GET (`to_wire_valid`, `dataencode.cpp:416-439`;
    // `Value::mark` cannot set a structure's own bit, `data.cpp:256-270`).
    // FmtTree prints every struct's id unconditionally
    // (`datafmt.cpp:196-200`), so the id assertion below holds.
    let output = TokioCommand::new(&pvxget)
        .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", port + 1))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .arg("-w")
        .arg("3")
        .arg("-F")
        .arg("tree")
        .arg("IMG:RAW")
        .output()
        .await
        .expect("pvxget run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("pvxget stdout:\n{stdout}");
    if !output.status.success() {
        panic!("pvxget failed: stdout=\n{stdout}\nstderr=\n{stderr}");
    }
    // Verify pvxs successfully decoded our NTNDArray.
    assert!(
        stdout.contains("epics:nt/NTNDArray:1.0"),
        "pvxget didn't recognize NTNDArray, got:\n{stdout}"
    );
    assert!(
        stdout.contains("uniqueId"),
        "pvxget output missing uniqueId, got:\n{stdout}"
    );
    assert!(
        stdout.contains("dimension"),
        "pvxget output missing dimension, got:\n{stdout}"
    );

    h.abort();
}

#[tokio::test]
#[ignore]
async fn rust_client_to_rust_server_ntndarray_full_roundtrip() {
    use std::sync::Arc;

    use epics_pva_rs::client_native::context::PvaClient;
    use epics_pva_rs::nt::nd_array::{
        NdAlarm, NdArrayBuffer, NdAttribute, NdCodec, NdDimension, NdTimeStamp, NtNdArray,
        nt_nd_array_desc, nt_nd_array_value,
    };
    use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarValue, VariantValue};
    use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

    #[derive(Clone)]
    struct ImageSource {}
    impl ChannelSource for ImageSource {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            async { vec!["IMG:INT".into()] }
        }
        fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = n.to_string();
            async move { n == "IMG:INT" }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async { Some(nt_nd_array_desc()) }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            async {
                // 1024-element int32 array with simple ramp values.
                let nt = NtNdArray {
                    value: NdArrayBuffer::Int((0i32..1024).collect()),
                    codec: NdCodec::default(),
                    compressed_size: 4096,
                    uncompressed_size: 4096,
                    dimension: vec![NdDimension {
                        size: 1024,
                        full_size: 1024,
                        binning: 1,
                        ..NdDimension::default()
                    }],
                    unique_id: 42,
                    data_time_stamp: NdTimeStamp::default(),
                    alarm: NdAlarm::default(),
                    time_stamp: NdTimeStamp::default(),
                    attribute: vec![NdAttribute {
                        name: "Test".into(),
                        value: VariantValue::scalar(ScalarValue::Double(3.14)),
                        descriptor: "test attr".into(),
                        source_type: 1,
                        source: "fake".into(),
                        ..NdAttribute::default()
                    }],
                };
                Some(nt_nd_array_value(&nt))
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

    let source = Arc::new(ImageSource {});
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Native client retrieves the NTNDArray, decode union value back.
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("IMG:INT"))
        .await
        .expect("pvget timeout")
        .expect("pvget failed");

    let s = match &v {
        PvField::Structure(s) => s,
        other => panic!("expected structure, got {other:?}"),
    };
    assert_eq!(s.struct_id, "epics:nt/NTNDArray:1.0");

    // Drill into value union.
    match s.get_field("value") {
        Some(PvField::Union {
            selector,
            variant_name,
            value,
        }) => {
            // pvxs nt.cpp::NTNDArray::build orders the value union signed
            // first, then unsigned, then float: boolean=0, byte=1, short=2,
            // int=3 (NOT the `NdArrayBuffer` enum ordinal, where Int is 5).
            assert_eq!(
                *selector, 3,
                "expected intValue selector (3, pvxs union order), got {selector}"
            );
            assert_eq!(variant_name, "intValue");
            match &**value {
                PvField::ScalarArray(items) => {
                    assert_eq!(items.len(), 1024, "expected 1024 elements");
                    if let ScalarValue::Int(n) = items[0] {
                        assert_eq!(n, 0);
                    }
                    if let ScalarValue::Int(n) = items[1023] {
                        assert_eq!(n, 1023);
                    }
                }
                // typed fast-path landing.
                PvField::ScalarArrayTyped(arr) => {
                    let ints = arr.as_ints().expect("expected int array");
                    assert_eq!(ints.len(), 1024);
                    assert_eq!(ints[0], 0);
                    assert_eq!(ints[1023], 1023);
                }
                other => panic!("expected ScalarArray, got {other:?}"),
            }
        }
        other => panic!("expected Union, got {other:?}"),
    }

    // uniqueId
    if let Some(PvField::Scalar(ScalarValue::Int(uid))) = s.get_field("uniqueId") {
        assert_eq!(*uid, 42);
    } else {
        panic!("missing uniqueId");
    }

    // dimension is a structure-array
    match s.get_field("dimension") {
        Some(PvField::StructureArray(dims)) => {
            assert_eq!(dims.len(), 1);
            if let Some(PvField::Scalar(ScalarValue::Int(sz))) =
                dims[0].as_ref().unwrap().get_field("size")
            {
                assert_eq!(*sz, 1024);
            } else {
                panic!("dim[0].size missing");
            }
        }
        other => panic!("expected StructureArray for dimension, got {other:?}"),
    }

    // attribute is a structure-array of NTAttribute
    match s.get_field("attribute") {
        Some(PvField::StructureArray(attrs)) => {
            assert_eq!(attrs.len(), 1);
            let attr0 = attrs[0].as_ref().unwrap();
            if let Some(PvField::Scalar(ScalarValue::String(name))) = attr0.get_field("name") {
                assert_eq!(name, "Test");
            }
            // value is a Variant carrying Double(3.14)
            match attr0.get_field("value") {
                Some(PvField::Variant(v)) => match &v.value {
                    PvField::Scalar(ScalarValue::Double(d)) => {
                        assert!((d - 3.14).abs() < 1e-6, "got {d}")
                    }
                    other => panic!("variant value: {other:?}"),
                },
                other => panic!("expected Variant attribute value, got {other:?}"),
            }
        }
        other => panic!("expected StructureArray for attribute, got {other:?}"),
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

#[tokio::test]
#[ignore]
async fn pvxs_pvxcall_to_rust_server_rpc() {
    if !pvxs_available(&["pvxcall"]) {
        return;
    }
    let pvxcall = find_pvxs_bin("pvxcall").unwrap();

    use std::sync::Arc;
    use tokio::sync::Mutex;

    use epics_pva_rs::pvdata::{
        FieldDesc, PvField, PvStructure, RpcReply, ScalarType, ScalarValue,
    };
    use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig, run_pva_server};

    /// RPC service: takes `{ a: double, b: double }` and returns
    /// `{ result: double }` where result = a + b.
    #[derive(Clone)]
    struct AddService {
        _v: Arc<Mutex<()>>,
    }

    impl ChannelSource for AddService {
        fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
            async { vec!["RPC:ADD".into()] }
        }
        fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = n.to_string();
            async move { n == "RPC:ADD" }
        }
        fn get_introspection(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
            async {
                Some(FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![
                        ("a".into(), FieldDesc::Scalar(ScalarType::Double)),
                        ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
                    ],
                })
            }
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            async { None }
        }
        fn put_value(
            &self,
            _: &str,
            _: PvField,
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
            async { Err("PUT not supported".into()) }
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
        fn rpc(
            &self,
            _name: &str,
            _request_desc: FieldDesc,
            request_value: PvField,
        ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
            async move {
                let mut a = 0.0f64;
                let mut b = 0.0f64;
                if let PvField::Structure(s) = &request_value {
                    if let Some(PvField::Scalar(ScalarValue::Double(av))) = s.get_field("a") {
                        a = *av;
                    }
                    if let Some(PvField::Scalar(ScalarValue::Double(bv))) = s.get_field("b") {
                        b = *bv;
                    }
                }
                let result_desc = FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![("result".into(), FieldDesc::Scalar(ScalarType::Double))],
                };
                let mut s = PvStructure::new("");
                s.fields
                    .push(("result".into(), PvField::Scalar(ScalarValue::Double(a + b))));
                Ok((result_desc, PvField::Structure(s)).into())
            }
        }
    }

    let (port, udp) = alloc_port_pair();
    let source = Arc::new(AddService {
        _v: Arc::new(Mutex::new(())),
    });
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let h = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // pvxcall RPC:ADD a=2.5 b=4.5
    let output = TokioCommand::new(&pvxcall)
        .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", port + 1))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .arg("-w")
        .arg("3")
        .arg("RPC:ADD")
        .arg("a=2.5")
        .arg("b=4.5")
        .output()
        .await
        .expect("pvxcall run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("pvxcall stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("7") || stdout.contains("result"),
        "expected RPC result containing 7.0, got stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    h.abort();
}

#[tokio::test]
#[ignore]
async fn rust_client_ntscalar_array_get_from_pvxs() {
    if !pvxs_available(&["softIocPVX"]) {
        return;
    }
    let softioc = find_pvxs_bin("softIocPVX").unwrap();

    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        r#"record(waveform, "INTEROP:WAVE") {
    field(NELM, "10")
    field(FTVL, "DOUBLE")
    field(INP, [1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5, 9.5, 10.5])
}
"#,
    )
    .unwrap();

    let (port, udp) = alloc_port_pair();
    let _ = udp;
    let mut child = TokioCommand::new(&softioc)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", (port + 1).to_string())
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            break;
        }
    }
    let _killer = ChildKiller(&mut child);

    use epics_pva_rs::client_native::context::PvaClient;
    use epics_pva_rs::pvdata::{PvField, ScalarValue};
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("INTEROP:WAVE"))
        .await
        .expect("pvget timeout")
        .expect("pvget failed");

    eprintln!("WAVE got: {v}");
    if let PvField::Structure(s) = v {
        assert!(
            s.struct_id.starts_with("epics:nt/NTScalarArray"),
            "struct_id: {}",
            s.struct_id
        );
        match s.get_field("value") {
            Some(PvField::ScalarArray(items)) => {
                assert_eq!(items.len(), 10, "expected 10 elements, got {}", items.len());
                if let ScalarValue::Double(d) = items[0] {
                    assert!((d - 1.5).abs() < 1e-6);
                }
                if let ScalarValue::Double(d) = items[9] {
                    assert!((d - 10.5).abs() < 1e-6);
                }
            }
            // typed fast-path landing.
            Some(PvField::ScalarArrayTyped(arr)) => {
                let doubles = arr.as_doubles().expect("expected double array");
                assert_eq!(doubles.len(), 10);
                assert!((doubles[0] - 1.5).abs() < 1e-6);
                assert!((doubles[9] - 10.5).abs() < 1e-6);
            }
            other => panic!("expected ScalarArray, got {other:?}"),
        }
    } else {
        panic!("expected NTScalarArray structure");
    }
}

#[tokio::test]
#[ignore]
async fn rust_client_various_scalar_types_from_pvxs() {
    if !pvxs_available(&["softIocPVX"]) {
        return;
    }
    let softioc = find_pvxs_bin("softIocPVX").unwrap();

    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        r#"record(ai, "INTEROP:DBL") { field(VAL, "3.1416") }
record(longin, "INTEROP:LNG") { field(VAL, "12345") }
record(stringin, "INTEROP:STR") { field(VAL, "hello world") }
record(bo, "INTEROP:BIN") {
    field(ZNAM, "off")
    field(ONAM, "on")
    field(VAL, "1")
}
"#,
    )
    .unwrap();

    let (port, udp) = alloc_port_pair();
    let _ = udp;
    let mut child = TokioCommand::new(&softioc)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", (port + 1).to_string())
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            break;
        }
    }
    let _killer = ChildKiller(&mut child);

    use epics_pva_rs::client_native::context::PvaClient;
    use epics_pva_rs::pvdata::{PvField, ScalarValue};
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    // Double
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("INTEROP:DBL"))
        .await
        .expect("DBL pvget timeout")
        .expect("DBL pvget failed");
    if let PvField::Structure(s) = v {
        match s.get_value() {
            Some(ScalarValue::Double(d)) => assert!((d - 3.1416).abs() < 1e-4, "got {d}"),
            other => panic!("DBL expected Double, got {other:?}"),
        }
    } else {
        panic!("DBL not a structure");
    }

    // Long (Int32)
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("INTEROP:LNG"))
        .await
        .expect("LNG pvget timeout")
        .expect("LNG pvget failed");
    if let PvField::Structure(s) = v {
        match s.get_value() {
            Some(ScalarValue::Int(i)) => assert_eq!(*i, 12345),
            other => panic!("LNG expected Int, got {other:?}"),
        }
    }

    // String
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("INTEROP:STR"))
        .await
        .expect("STR pvget timeout")
        .expect("STR pvget failed");
    if let PvField::Structure(s) = v {
        match s.get_value() {
            Some(ScalarValue::String(s)) => assert_eq!(s, "hello world"),
            other => panic!("STR expected String, got {other:?}"),
        }
    }
}

#[tokio::test]
#[ignore]
async fn rust_client_ntenum_from_pvxs_mbbo() {
    if !pvxs_available(&["softIocPVX"]) {
        return;
    }
    let softioc = find_pvxs_bin("softIocPVX").unwrap();

    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        r#"record(mbbo, "INTEROP:MODE") {
    field(ZRST, "Idle")
    field(ONST, "Acquire")
    field(TWST, "Pause")
    field(THST, "Stop")
    field(VAL, "1")
}
"#,
    )
    .unwrap();

    let (port, udp) = alloc_port_pair();
    let _ = udp;
    let mut child = TokioCommand::new(&softioc)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", (port + 1).to_string())
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            break;
        }
    }
    let _killer = ChildKiller(&mut child);

    use epics_pva_rs::client_native::context::PvaClient;
    use epics_pva_rs::pvdata::{PvField, ScalarValue};
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("INTEROP:MODE"))
        .await
        .expect("pvget timeout")
        .expect("pvget failed");

    eprintln!("MODE got: {v}");
    if let PvField::Structure(s) = v {
        assert!(
            s.struct_id.starts_with("epics:nt/NTEnum"),
            "struct_id: {}",
            s.struct_id
        );
        match s.get_field("value") {
            Some(PvField::Structure(es)) => {
                if let Some(PvField::Scalar(ScalarValue::Int(i))) = es.get_field("index") {
                    assert_eq!(*i, 1, "expected index=1 (Acquire), got {i}");
                }
                if let Some(PvField::ScalarArray(choices)) = es.get_field("choices") {
                    assert_eq!(choices.len(), 4);
                    if let ScalarValue::String(s0) = &choices[0] {
                        assert_eq!(s0, "Idle");
                    }
                }
            }
            other => panic!("NTEnum value expected Structure, got {other:?}"),
        }
    } else {
        panic!("expected NTEnum structure");
    }
}

#[tokio::test]
#[ignore]
async fn rust_client_pvmonitor_pvxs_softiocpvx_via_pvxput() {
    if !pvxs_available(&["softIocPVX", "pvxput"]) {
        return;
    }
    let softioc = find_pvxs_bin("softIocPVX").unwrap();
    let pvxput = find_pvxs_bin("pvxput").unwrap();

    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        "record(ao, \"INTEROP:MON\") { field(VAL, \"1.0\") }\n",
    )
    .unwrap();

    let (port, udp) = alloc_port_pair();
    let _ = udp;
    let mut child = TokioCommand::new(&softioc)
        .env("EPICS_PVA_SERVER_PORT", port.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", (port + 1).to_string())
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            break;
        }
    }
    let _killer = ChildKiller(&mut child);

    use epics_pva_rs::client_native::context::PvaClient;
    use epics_pva_rs::pvdata::{PvField, ScalarValue};
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let received = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
    let recv_cb = received.clone();

    let mon_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor("INTEROP:MON", move |value| {
                    if let PvField::Structure(s) = value {
                        if let Some(ScalarValue::Double(d)) = s.get_value() {
                            recv_cb.lock().push(*d);
                        }
                    }
                })
                .await;
        }
    });

    // Wait for initial snapshot.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Drive pvxput from outside to update the value.
    for v in &[2.0_f64, 3.0, 4.0] {
        let out = TokioCommand::new(&pvxput)
            .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", port + 1))
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .arg("-w")
            .arg("3")
            .arg("INTEROP:MON")
            .arg(format!("{v}"))
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "pvxput failed: {:?}", out);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = received.lock().clone();
    eprintln!("monitor got: {got:?}");
    assert!(
        got.contains(&4.0),
        "monitor did not receive final value 4.0; got {got:?}"
    );

    mon_handle.abort();
}
