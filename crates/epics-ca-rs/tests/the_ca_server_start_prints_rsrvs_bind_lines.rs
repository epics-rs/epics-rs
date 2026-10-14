//! What `rsrv_grab_tcp` and `rsrv_init` print while the server binds, byte
//! for byte: the `Skipping a.b.c.d:port` line for an address that is no
//! interface (`caservertask.c:222`, `osiSock.c:168-170`), the five
//! "Configured TCP port was unavailable" lines for the restart that follows
//! (`:585-590`), and the missing-broadcast warning naming the final TCP port
//! (`:683-685`).
//!
//! The lines go to the process's own stdout and stderr, which the test harness
//! captures in-process, so the server is built in a child run of this binary.
#![cfg(tokio_backend)]

use std::process::Command;

use epics_ca_rs::server::CaServer;

const CHILD: &str = "CA_BIND_LINES_CHILD";

/// The child: build a server on `127.0.0.2` beside an address no interface
/// owns, asking for a TCP port that is free, and report the one it got.
#[tokio::test]
async fn child_builds_the_server() {
    let Ok(requested) = std::env::var(CHILD) else {
        return;
    };
    let server = CaServer::builder()
        .port(0)
        .tcp_port(requested.parse().unwrap())
        .build()
        .await
        .expect("build");
    println!("FINAL-TCP-PORT {}", server.tcp_port());
}

#[test]
fn skip_restart_and_missing_broadcast_lines_are_rsrvs() {
    let requested = {
        let l = std::net::TcpListener::bind(("127.0.0.2", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_builds_the_server", "--nocapture"])
        .env(CHILD, requested.to_string())
        .env("EPICS_CAS_INTF_ADDR_LIST", "192.0.2.1 127.0.0.2")
        .env_remove("TERM")
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");

    let bound: u16 = stdout
        .lines()
        .find_map(|l| l.strip_prefix("FINAL-TCP-PORT "))
        .expect("the child reports its TCP port")
        .parse()
        .unwrap();
    assert_ne!(bound, requested, "the skip restarts on an OS-picked port");

    let skip = format!("Skipping 192.0.2.1:{requested} which is not an interface address\n");
    assert!(stdout.contains(&skip), "stdout lacks {skip:?}:\n{stdout}");

    let restart = format!(
        "cas WARNING: Configured TCP port was unavailable.\n\
         cas WARNING: Using dynamically assigned TCP port {bound},\n\
         cas WARNING: but now two or more servers share the same UDP port.\n\
         cas WARNING: Depending on your IP kernel this server may not be\n\
         cas WARNING: reachable with UDP unicast (a host's IP in EPICS_CA_ADDR_LIST)\n"
    );
    assert!(
        stderr.contains(&restart),
        "stderr lacks the restart warning:\n{stderr}"
    );

    // Without the broadcast socket there is nothing to warn about.
    #[cfg(ca_udp_bcast)]
    {
        let no_bcast = format!(
            "Warning: Can't find broadcast address of interface 127.0.0.2:{bound}\n         \
             Name lookup may not work on this interface\n"
        );
        assert!(
            stderr.contains(&no_bcast),
            "stderr lacks the missing-broadcast warning:\n{stderr}"
        );
    }
}
