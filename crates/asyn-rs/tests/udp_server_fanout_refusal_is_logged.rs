//! A UDP server whose fanout option the kernel refuses still comes up: C's
//! `epicsSocketEnableAddressUseForDatagramFanout` logs the option it could not
//! set and returns (osdSockAddrReuse.cpp:46-68), and `createServerSocket`
//! carries on to its own `SO_REUSEADDR` and the bind
//! (drvAsynIPServerPort.c:444-454).
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::drivers::ip_server_port::DrvAsynIPServerPort;
use asyn_rs::port::PortDriver;
use asyn_rs::user::AsynUser;
use epics_libcom_rs::runtime::log::{errlog_add_listener, errlog_flush};

/// Make `setsockopt(SOL_SOCKET, SO_REUSEPORT)` fail with EPERM on this thread.
fn refuse_reuse_port() {
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    let op = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let prog = [
        op(LD_W_ABS, 0, 0, 4), // seccomp_data.arch
        op(JEQ_K, 0, 7, AUDIT_ARCH_X86_64),
        op(LD_W_ABS, 0, 0, 0), // .nr
        op(JEQ_K, 0, 5, libc::SYS_setsockopt as u32),
        op(LD_W_ABS, 0, 0, 24), // .args[1], level
        op(JEQ_K, 0, 3, libc::SOL_SOCKET as u32),
        op(LD_W_ABS, 0, 0, 32), // .args[2], optname
        op(JEQ_K, 0, 1, libc::SO_REUSEPORT as u32),
        op(RET_K, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
        op(RET_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut _,
    };
    // SAFETY: `fprog` points at `prog`, live across both calls.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog),
            0
        );
    }
}

#[test]
fn a_refused_fanout_option_is_logged_and_the_udp_server_still_binds() {
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    errlog_add_listener(move |m: &str| sink.lock().unwrap().push(m.to_string()));
    // Held for the server, which has no ephemeral-port getter: its
    // SO_REUSEADDR bind on 127.0.0.1 joins the hold and outranks it for
    // unicast.
    let held = named_port::HeldPort::udp();
    let port = held.port;
    refuse_reuse_port();
    let mut srv =
        DrvAsynIPServerPort::new("fanout_refused", &format!("127.0.0.1:{port} UDP")).unwrap();
    srv.connect(&AsynUser::default())
        .expect("C goes on past a fanout option it could not set");
    assert!(srv.base().is_connected());

    // The server receives: the setup reached the bind.
    let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).unwrap();
    let user = AsynUser::default().with_timeout(Duration::from_secs(2));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 16];
    let n = loop {
        let n = srv.read_octet(&user, &mut buf).unwrap();
        if n > 0 || std::time::Instant::now() > deadline {
            break n;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(&buf[..n], b"alpha");

    errlog_flush();
    let lines = lines.lock().unwrap();
    assert!(
        lines.iter().any(
            |l| l == "epicsSocketEnablePortUseForDatagramFanout: unable to set SO_REUSEPORT?\n"
        ),
        "C's helper line is missing: {lines:?}"
    );
}
