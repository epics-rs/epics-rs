//! A client put to `NDAttributesFile` reaches the next frame: C
//! `asynNDArrayDriver::writeOctet` loads the file (asynNDArrayDriver.cpp:526)
//! and `simTask` attaches the set to each image (simDetector.cpp:828), so
//! the NTNDArray an NDPva plugin serves carries the attribute.

#![cfg(all(feature = "ioc", tokio_backend))]

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A base client tool, from the reference tree; a missing tree fails.
fn base_bin(name: &str) -> PathBuf {
    use epics_base_rs::reference::{ReferenceTree, reference_path};
    let arch = std::env::var("EPICS_HOST_ARCH").unwrap_or_else(|_| "linux-x86_64".into());
    reference_path(ReferenceTree::Base, &format!("bin/{arch}/{name}"))
}

struct Clients {
    ca: u16,
    pva: u16,
}

impl Clients {
    fn run(&self, tool: &str, args: &[&str]) -> Output {
        Command::new(base_bin(tool))
            .args(args)
            .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", self.ca.to_string())
            .env("EPICS_PVA_ADDR_LIST", "127.0.0.1")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_BROADCAST_PORT", self.pva.to_string())
            .output()
            .unwrap_or_else(|e| panic!("run {tool}: {e}"))
    }

    fn put(&self, args: &[&str]) {
        let out = self.run("caput", &[&["-w", "5"], args].concat());
        assert!(out.status.success(), "caput {args:?}: {out:?}");
    }
}

#[test]
#[ignore = "needs caput/pvget from $EPICS_BASE; runs in --profile interop"]
fn a_ca_put_to_the_attributes_file_reaches_the_next_frame() {
    let dir = tempfile::tempdir().unwrap();
    let attrs = dir.path().join("attrs.xml");
    std::fs::write(
        &attrs,
        r#"<Attributes>
    <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
</Attributes>
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("st.cmd"),
        r#"epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(ADCORE)/db")
simDetectorConfig("SIMT", 8, 8, 1000000)
dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=T:,R=cam1:,PORT=SIMT,ADDR=0,TIMEOUT=1")
NDPvaConfigure("PVA1", 5, 0, "SIMT", 0, "T:Pva1:Image")
dbLoadRecords("NDPva.template", "P=T:,R=Pva1:,PORT=PVA1,NDARRAY_PORT=SIMT")
iocInit()
"#,
    )
    .unwrap();
    // The UDP numbers are the ones the clients search on and the IOC must get
    // (a taken TCP number only moves a server to a dynamic port, which the
    // search reply carries), so they are held for the child, whose fanout
    // binds join the holds.
    let (ca, pva) = (named_port::HeldPort::udp(), named_port::HeldPort::udp());
    let clients = Clients {
        ca: ca.port,
        pva: pva.port,
    };
    let _ioc = Reaped(
        Command::new(env!("CARGO_BIN_EXE_sim-ioc"))
            .arg("st.cmd")
            .current_dir(dir.path())
            .env("EPICS_CAS_SERVER_PORT", clients.ca.to_string())
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .env("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO")
            .env("EPICS_PVAS_SERVER_PORT", clients.pva.to_string())
            .env("EPICS_PVAS_BROADCAST_PORT", clients.pva.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sim-ioc"),
    );

    let deadline = Instant::now() + Duration::from_secs(60);
    while !clients
        .run("caget", &["-w", "1", "T:cam1:Acquire"])
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "sim-ioc never served CA");
    }

    clients.put(&["T:Pva1:EnableCallbacks", "1"]);
    clients.put(&["-S", "T:cam1:NDAttributesFile", attrs.to_str().unwrap()]);
    let status = clients.run("caget", &["-t", "-n", "T:cam1:NDAttributesStatus"]);
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "0",
        "attributes file status: {status:?}"
    );
    clients.put(&["T:cam1:ImageMode", "0"]);
    clients.put(&["T:cam1:AcquireTime", "0.001"]);
    clients.put(&["-c", "T:cam1:Acquire", "1"]);

    let image = clients.run(
        "pvget",
        &["-w", "5", "-r", "field(attribute)", "T:Pva1:Image"],
    );
    let text = String::from_utf8_lossy(&image.stdout);
    assert!(image.status.success(), "pvget: {image:?}");
    assert!(text.contains("Counter"), "frame attributes:\n{text}");
}
