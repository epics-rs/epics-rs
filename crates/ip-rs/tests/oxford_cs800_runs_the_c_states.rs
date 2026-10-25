//! The compiled `Oxford_CS800.st` on the shipped `Oxford_CS800_status.db`
//! and `Oxford_CS800.db`, booted as ip's two iocsh files boot them: the
//! status record reads CS800 datagrams from a UDP `drvAsynIPPort`, and the
//! command records are StreamDevice's. The values each datagram leaves are
//! the ones a C IOC (seq 2.2.9, asyn, StreamDevice) posted for the same
//! datagrams.

mod support;

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;

use support::{eventually, sh};

const P: &str = "x:CS0:";
const SP: &str = "x:CS:status_packet";

const CTRLNUM: u16 = 1028;
const TEMP: u16 = 1051;

struct Ioc {
    db: Arc<PvDatabase>,
    /// The status port's local port, where the controller's datagrams go.
    status: u16,
    _scan: ScanOwner,
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn macros(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Both templates past `iocInit`, the controller's with `disable_comms`
/// as `DISABLE_COMMS` when given, then
/// `seq &Oxford_CS800, "name=cs0,CID=<cid>,P=x:,R=CS0:,SP=x:CS:status_packet"`.
async fn boot(cid: u32, disable_comms: Option<&str>) -> Ioc {
    ip_rs::register_ip_dbd();
    stream_rs::asyn::register();
    let status = free_udp_port();
    let no_db = Arc::new(PvDatabase::new());
    for line in [
        format!(
            r#"epicsEnvSet("STREAM_PROTOCOL_PATH", "{}")"#,
            ip_rs::IP_DB_DIR
        ),
        format!(r#"drvAsynIPPortConfigure("CS_UDP", "127.0.0.1:{status}:{status} UDP")"#),
        format!(
            r#"drvAsynIPPortConfigure("CS_CMD", "127.0.0.1:{} UDP")"#,
            free_udp_port()
        ),
    ] {
        sh(&no_db, &line).await;
    }
    let mut controller = vec![("P", "x:"), ("R", "CS0:"), ("CP", "CS_CMD")];
    controller.extend(disable_comms.map(|d| ("DISABLE_COMMS", d)));
    let (asyn_name, asyn_factory) = asyn_rs::asyn_record::asyn_record_factory();
    let builder = IocBuilder::new().register_record_type(asyn_name, move || asyn_factory());
    let (db, _) = stream_rs::dev::register(builder)
        .db_file(
            &format!("{}/Oxford_CS800_status.db", ip_rs::IP_DB_DIR),
            &macros(&[("P", "x:"), ("R", "CS:"), ("SP", "CS_UDP")]),
        )
        .unwrap()
        .db_file(
            &format!("{}/Oxford_CS800.db", ip_rs::IP_DB_DIR),
            &macros(&controller),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let scan = ScanOwner::start(db.clone());
    let seq = format!(r#"seq &Oxford_CS800, "name=cs0,CID={cid},P=x:,R=CS0:,SP={SP}""#);
    sh(&db, &seq).await;
    Ioc {
        db,
        status,
        _scan: scan,
    }
}

/// A CS800 status datagram: 0xaaab, its length, 230 (id, value) pairs, a
/// checksum the program does not check, and 0xabaa. `pairs` fill slots 1
/// on, and every other slot carries a filler id, 0x7000 plus the slot, that
/// no parameter has.
struct Packet([u8; 928]);

impl Packet {
    fn new(pairs: &[(u16, u16)]) -> Self {
        let mut p = [0; 928];
        p[..4].copy_from_slice(&[0xaa, 0xab, 0x03, 0xa0]);
        p[926..].copy_from_slice(&[0xab, 0xaa]);
        let mut packet = Packet(p);
        for slot in 1..=230 {
            packet = packet.at(slot, 0x7000 + slot as u16, 0);
        }
        for (slot, &(id, value)) in (1..).zip(pairs) {
            packet = packet.at(slot, id, value);
        }
        packet
    }

    fn at(mut self, slot: usize, id: u16, value: u16) -> Self {
        let i = 4 * slot;
        self.0[i..i + 2].copy_from_slice(&id.to_be_bytes());
        self.0[i + 2..i + 4].copy_from_slice(&value.to_be_bytes());
        self
    }
}

impl Ioc {
    /// The first `len` bytes of `packet` to the status port, once the
    /// status record has read them and the program has had a quarter
    /// second to take them.
    async fn deliver(&self, packet: &Packet, len: usize) {
        let sent = &packet.0[..len];
        let mut binp = DbSubscription::subscribe(&self.db, &format!("{SP}.BINP"))
            .await
            .unwrap();
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(sent, ("127.0.0.1", self.status))
            .unwrap();
        let read = eventually(|| {
            std::iter::from_fn(|| binp.try_recv_event().ok()).any(|e| {
                matches!(e.value().as_value(), Some(EpicsValue::CharArray(a)) if a[..] == *sent)
            })
        })
        .await;
        assert!(read, "the status record never read the packet");
        task::sleep(Duration::from_millis(250)).await;
    }

    fn f64_of(&self, pv: &str) -> f64 {
        self.db
            .get_pv(format!("{P}{pv}"))
            .unwrap()
            .to_f64()
            .unwrap()
    }

    fn str_of(&self, pv: &str) -> String {
        match self.db.get_pv(format!("{P}{pv}")).unwrap() {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            v => panic!("{pv}: {v:?}"),
        }
    }
}

/// The template boots with comms disabled (`DISABLE_COMMS` defaults to 1),
/// so the program parks in `ignore` and a packet from its controller lands
/// in no record.
#[epics_base_rs::epics_test]
async fn a_disabled_program_takes_no_packet() {
    let ioc = boot(113, None).await;
    assert_eq!(ioc.f64_of("DISABLE"), 1.0);
    ioc.deliver(&Packet::new(&[(CTRLNUM, 113), (TEMP, 29315)]), 928)
        .await;
    assert_eq!(ioc.f64_of("TEMP.RVAL"), 0.0);
    assert_eq!(ioc.str_of("ALARM"), "unset");
}
