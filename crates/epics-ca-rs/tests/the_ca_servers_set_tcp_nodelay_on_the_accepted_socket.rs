//! **Both CA servers set `TCP_NODELAY` on the circuit they accept.**
//!
//! rsrv does it right after `accept` (`caservertask.c:1444`). Without it a
//! reply written behind an unanswered one waits for the client's delayed ACK
//! (~40 ms), which no reply-order or content check can see.
//!
//! The option is on the server's end of the connection, which the client
//! cannot read. Both ends live in this process, so the test finds the server's
//! descriptor in `/proc/self/fd` by its address pair and reads the option from
//! a duplicate (the option belongs to the open file description, not the fd).
#![cfg(target_os = "linux")]

use std::net::{SocketAddr, TcpStream};
use std::os::fd::BorrowedFd;

use raw_ca::connect_and_handshake;

/// `TCP_NODELAY` as the server end of `client`'s connection has it.
fn server_end_nodelay(client: &TcpStream) -> bool {
    let (client_addr, server_addr): (SocketAddr, SocketAddr) = (
        client.local_addr().expect("client local addr"),
        client.peer_addr().expect("client peer addr"),
    );
    for entry in std::fs::read_dir("/proc/self/fd").expect("list fds") {
        let Ok(fd) = entry.unwrap().file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // SAFETY: the fd is only borrowed for the duplicate; a descriptor that
        // closes under us makes the duplicate fail, which is skipped.
        let Ok(dup) = (unsafe { BorrowedFd::borrow_raw(fd) }).try_clone_to_owned() else {
            continue;
        };
        let sock = socket2::Socket::from(dup);
        if sock.local_addr().ok().and_then(|a| a.as_socket()) == Some(server_addr)
            && sock.peer_addr().ok().and_then(|a| a.as_socket()) == Some(client_addr)
        {
            return sock.nodelay().expect("read TCP_NODELAY");
        }
    }
    panic!("no descriptor in this process is the server's end of {client_addr} -> {server_addr}");
}

#[test]
fn the_blocking_server_sets_nodelay() {
    use std::sync::Arc;

    use epics_base_rs::server::database::PvDatabase;

    let (server, addr, accept) = raw_ca::start_server(Arc::new(PvDatabase::new()));
    let c = connect_and_handshake(addr);
    assert!(
        server_end_nodelay(c.stream()),
        "the blocking driver must set TCP_NODELAY on the accepted socket \
         (rsrv caservertask.c:1444)"
    );
    drop(c);
    server.shutdown();
    accept.join().unwrap();
}

#[cfg(tokio_backend)]
mod async_server {
    use super::*;

    #[test]
    fn the_async_server_sets_nodelay() {
        use epics_ca_rs::server::CaServer;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        let (server, port) = rt.block_on(async {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .build()
                .await
                .expect("build CA server");
            let port = server.tcp_port();
            (server, port)
        });
        let run = rt.spawn(async move {
            let _ = server.run().await;
        });
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let c = connect_and_handshake(addr);
        assert!(
            server_end_nodelay(c.stream()),
            "the async server must set TCP_NODELAY on the accepted socket \
             (rsrv caservertask.c:1444)"
        );
        drop(c);
        run.abort();
    }
}
