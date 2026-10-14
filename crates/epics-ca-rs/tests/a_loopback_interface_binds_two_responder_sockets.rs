//! rsrv binds a second UDP socket on 127.0.0.1 because
//! `osiSockDiscoverBroadcastAddresses` answers the loopback address with
//! itself (`osdNetIfAddrs.c:41-52`, `caservertask.c:680-703`). The two share
//! the address through datagram fanout, and a search sent to it is still
//! answered once: compiled softIoc, three searches, one reply each.
#![cfg(all(tokio_backend, target_os = "linux"))]

use std::collections::HashSet;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;

/// Inodes of the UDP sockets bound to 127.0.0.1:`port` in `proc_net_udp`,
/// split into ours (an `fd` link in `own_fds`) and everyone else's.
/// `/proc/net/udp` lists every process's sockets, and another one can be
/// handed the same ephemeral number.
fn holders(proc_net_udp: &str, port: u16, own_fds: &HashSet<String>) -> (usize, Vec<String>) {
    let want = format!("0100007F:{port:04X}");
    let (mut ours, mut foreign) = (0, Vec::new());
    for f in proc_net_udp
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f[1] == want)
    {
        if own_fds.contains(&format!("socket:[{}]", f[9])) {
            ours += 1;
        } else {
            foreign.push(f[9].to_string());
        }
    }
    (ours, foreign)
}

fn fd_links(dir: &str) -> HashSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .map(|l| l.to_string_lossy().into_owned())
        .collect()
}

/// This process's and other processes' sockets on 127.0.0.1:`port`.
fn holders_on(port: u16) -> (usize, Vec<String>) {
    let table = std::fs::read_to_string("/proc/net/udp").expect("read /proc/net/udp");
    holders(&table, port, &fd_links("/proc/self/fd"))
}

/// The pids holding socket `inode`, for the failure message.
fn pids_holding(inode: &str) -> Vec<String> {
    let link = format!("socket:[{inode}]");
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
        .filter(|pid| fd_links(&format!("/proc/{pid}/fd")).contains(&link))
        .collect()
}

/// CA_PROTO_VERSION followed by a CA_PROTO_SEARCH for `pv`.
fn search_datagram(pv: &str) -> Vec<u8> {
    let mut name = pv.as_bytes().to_vec();
    name.push(0);
    while !name.len().is_multiple_of(8) {
        name.push(0);
    }
    let mut d = Vec::new();
    for w in [0u16, 0, 0, 13] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&6u16.to_be_bytes());
    d.extend_from_slice(&(name.len() as u16).to_be_bytes());
    d.extend_from_slice(&5u16.to_be_bytes());
    d.extend_from_slice(&13u16.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    d.extend_from_slice(&name);
    d
}

/// One searcher's replies to a search sent to 127.0.0.1:`port`.
async fn replies_to_one_search(port: u16) -> usize {
    tokio::task::spawn_blocking(move || {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        s.send_to(&search_datagram("LOOP:A"), ("127.0.0.1", port))
            .unwrap();
        let (mut n, until) = (0, Instant::now() + Duration::from_millis(1500));
        let mut buf = [0u8; 2048];
        while Instant::now() < until {
            if s.recv(&mut buf).is_ok() {
                n += 1;
            }
        }
        n
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_loopback_interface_has_two_responder_sockets_and_one_reply() {
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
    }
    // A foreign socket on the port joins the responder's fanout group and can
    // take the one search, so both checks run only on a port nobody else
    // holds before or after the measurement.
    let mut last = Vec::new();
    for _ in 0..5 {
        let server = CaServer::builder()
            .port(0)
            .tcp_port(0)
            .pv("LOOP:A", EpicsValue::Double(1.0))
            .build()
            .await
            .expect("build");
        let port = server.udp_port();
        let (ours, foreign) = holders_on(port);
        if !foreign.is_empty() {
            last = foreign;
            continue;
        }
        assert_eq!(ours, 2, "rsrv holds two sockets on 127.0.0.1");
        let run = tokio::spawn(async move {
            let _ = server.run().await;
        });
        let replies = replies_to_one_search(port).await;
        run.abort();
        let _ = run.await;
        let (_, foreign) = holders_on(port);
        if !foreign.is_empty() {
            last = foreign;
            continue;
        }
        assert_eq!(replies, 1, "a search to 127.0.0.1 is answered exactly once");
        return;
    }
    let named: Vec<String> = last
        .iter()
        .map(|i| format!("inode {i} (pids {:?})", pids_holding(i)))
        .collect();
    panic!("every port the server was handed was also held by another process: {named:?}");
}

/// Two sockets on the port, one ours: only ours counts, the other is foreign.
#[test]
fn a_foreign_socket_on_the_port_is_not_counted_as_ours() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
  1: 0100007F:1F90 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 111 2 0000000000000000 0
  2: 0100007F:1F90 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 222 2 0000000000000000 0
  3: 0100007F:1F91 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 333 2 0000000000000000 0
";
    let own: HashSet<String> = ["socket:[111]".to_string(), "socket:[333]".to_string()].into();
    assert_eq!(holders(table, 0x1F90, &own), (1, vec!["222".to_string()]));
}
