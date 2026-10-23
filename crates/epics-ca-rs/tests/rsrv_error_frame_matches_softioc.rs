//! Whole `CA_PROTO_ERROR` frames, pinned from softIoc R7.0.10.1 (8f5015b66)
//! answering the same raw frames on [`DB`] with
//! `EPICS_CA_MAX_ARRAY_BYTES=1000000`. `send_err` (`camessage.c:142-255`)
//! echoes the request header and appends the context string with its NUL,
//! padded to 8; the error's `m_cid` is the channel's cid for a channel
//! request that names a live channel and `0xffffffff` otherwise.
//!
//! Three refusals are this port's own limits, which C has no counterpart for;
//! they keep their status and carry the text of C's refusal with the same
//! status at the same stage.

#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_DO_REPLY, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO,
    CA_PROTO_ERROR, CA_PROTO_EVENT_ADD, CA_PROTO_EVENT_CANCEL, CA_PROTO_HOST_NAME,
    CA_PROTO_NOT_FOUND, CA_PROTO_READ, CA_PROTO_READ_NOTIFY, CA_PROTO_READ_SYNC, CA_PROTO_SEARCH,
    CA_PROTO_VERSION, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY, CaHeader, ECA_16KARRAYCLIENT,
    ECA_ALLOCMEM, ECA_BADMONID, ECA_INTERNAL, ECA_PUTCBINPROG, ECA_TOLARGE,
};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const DB: &str = "record(longout,\"LO\"){}
record(waveform,\"WF\"){ field(FTVL,\"DOUBLE\") field(NELM,\"10000\") }
";

const DBR_DOUBLE: u16 = 6;
const NO_CID: u32 = 0xffff_ffff;

fn hdr(cmmd: u16, size: u16, dt: u16, count: u16, p1: u32, p2: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&cmmd.to_be_bytes());
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(&dt.to_be_bytes());
    b.extend_from_slice(&count.to_be_bytes());
    b.extend_from_slice(&p1.to_be_bytes());
    b.extend_from_slice(&p2.to_be_bytes());
    b
}

fn padded(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b.resize(b.len().next_multiple_of(8), 0);
    b
}

/// The bytes after the context string's NUL zeroed. `cas_commit_msg`
/// (`caserverio.c:350-365`) only rounds the size up, so C's padding is
/// whatever the send buffer last held there; a pin compares through the NUL.
fn mask_padding(mut frame: Vec<u8>) -> Vec<u8> {
    // The echo is 24 bytes when it carries the extended size and count.
    let ctx = if frame[18..20] == [0xff, 0xff] {
        40
    } else {
        32
    };
    if let Some(nul) = frame[ctx..].iter().position(|&b| b == 0) {
        frame[ctx + nul..].fill(0);
    }
    frame
}

/// The frame softIoc sent: `send_err(request, status, ctx)`.
fn error_frame(cid: u32, status: u32, request: &[u8], ctx: &str) -> Vec<u8> {
    let mut payload = request[..16].to_vec();
    payload.extend_from_slice(&padded(ctx));
    let mut f = hdr(CA_PROTO_ERROR, payload.len() as u16, 0, 0, cid, status);
    f.extend_from_slice(&payload);
    f
}

fn event_add(sid: u32, sub_id: u32, count: u16) -> Vec<u8> {
    let mut f = hdr(CA_PROTO_EVENT_ADD, 16, DBR_DOUBLE, count, sid, sub_id);
    f.extend_from_slice(&[0u8; 12]);
    f.extend_from_slice(&5u16.to_be_bytes());
    f.extend_from_slice(&[0u8; 2]);
    f
}

struct Client {
    sock: TcpStream,
    acc: Vec<u8>,
    minor: u16,
}

impl Client {
    fn connect(port: u16, minor: u16) -> Self {
        let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = Self {
            sock,
            acc: Vec::new(),
            minor,
        };
        let mut out = hdr(CA_PROTO_VERSION, 0, 0, minor, 0, 0);
        out.extend(hdr(CA_PROTO_CLIENT_NAME, 8, 0, 0, 0, 0));
        out.extend(padded("u"));
        out.extend(hdr(CA_PROTO_HOST_NAME, 8, 0, 0, 0, 0));
        out.extend(padded("h"));
        c.send(&out);
        c
    }

    fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).unwrap();
    }

    /// The next whole frame, `Ok(None)` once the server closed the circuit.
    fn try_next(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if let Ok((h, size)) = CaHeader::from_bytes_extended(&self.acc) {
                let end = size + h.actual_postsize();
                if self.acc.len() >= end {
                    return Ok(Some(self.acc.drain(..end).collect()));
                }
            }
            let mut buf = [0u8; 65536];
            match self.sock.read(&mut buf) {
                Ok(0) => return Ok(None),
                Ok(n) => self.acc.extend_from_slice(&buf[..n]),
                Err(e) => return Err(e),
            }
        }
    }

    fn next(&mut self) -> Option<Vec<u8>> {
        self.try_next()
            .unwrap_or_else(|e| panic!("no frame from the server: {e}"))
    }

    fn create(&mut self, cid: u32, pv: &str) -> u32 {
        let name = padded(pv);
        let mut f = hdr(
            CA_PROTO_CREATE_CHAN,
            name.len() as u16,
            0,
            0,
            cid,
            self.minor as u32,
        );
        f.extend(name);
        self.send(&f);
        loop {
            let frame = self.next().expect("CREATE_CHAN reply");
            if u16::from_be_bytes([frame[0], frame[1]]) == CA_PROTO_CREATE_CHAN {
                return u32::from_be_bytes(frame[12..16].try_into().unwrap());
            }
        }
    }

    /// The first `CA_PROTO_ERROR` through [`mask_padding`], or the headers
    /// that came instead.
    fn try_error(&mut self) -> Result<Vec<u8>, String> {
        let mut before = Vec::new();
        loop {
            match self.try_next() {
                Ok(Some(f)) if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_ERROR => {
                    return Ok(mask_padding(f));
                }
                Ok(Some(f)) => before.push(f[..16].to_vec()),
                Ok(None) => return Err(format!("closed; headers before: {before:?}")),
                Err(e) => return Err(format!("{e}; headers before: {before:?}")),
            }
        }
    }

    /// The headers of every frame left before the server closes, less the
    /// earlier subscriptions' updates, which the event task may still send.
    fn rest_until_closed(&mut self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.next().map(|f| f[..16].to_vec()))
            .filter(|h| u16::from_be_bytes([h[0], h[1]]) != CA_PROTO_EVENT_ADD)
            .collect()
    }

    fn error(&mut self) -> Vec<u8> {
        self.try_error()
            .unwrap_or_else(|e| panic!("no CA_PROTO_ERROR: {e}"))
    }
}

async fn server() -> u16 {
    serve(CaServer::builder()).await
}

async fn serve(builder: epics_ca_rs::server::CaServerBuilder) -> u16 {
    serve_db(builder, DB).await
}

async fn serve_db(builder: epics_ca_rs::server::CaServerBuilder, db: &str) -> u16 {
    let server = builder
        .port(0)
        .db_string(db, &Default::default())
        .expect("db")
        .pv("SP", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    tokio::spawn(async move { server.run().await });
    port
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// `bad_tcp_cmd_action` (`camessage.c:343-357`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_illegal_command_names_the_damaged_request() {
    let port = server().await;
    let got = blocking(move || {
        let mut c = Client::connect(port, 13);
        c.send(&hdr(100, 0, 0, 0, 0, 0));
        c.error()
    })
    .await;
    let req = hdr(100, 0, 0, 0, 0, 0);
    assert_eq!(
        got,
        error_frame(
            NO_CID,
            ECA_INTERNAL,
            &req,
            "invalid (damaged?) request code from TCP"
        )
    );
}

/// softIoc's one VERSION is the greeting `create_tcp_client` sends
/// (`caservertask.c:1525`); `tcp_version_action` (`camessage.c:362`) answers
/// a VERSION with nothing, so after a VERSION and an ECHO, twice, these
/// three headers are all softIoc sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_client_version_draws_no_reply() {
    let port = server().await;
    let got = blocking(move || {
        let mut c = Client::connect(port, 13);
        for _ in 0..2 {
            let mut out = hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0);
            out.extend(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            c.send(&out);
        }
        (0..3)
            .map(|_| c.next().expect("frame")[..16].to_vec())
            .collect::<Vec<_>>()
    })
    .await;
    assert_eq!(
        got,
        [
            hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0),
            hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0),
            hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0),
        ]
    );
}

/// `tcp_version_action` (`camessage.c:369-373`) takes an extended VERSION's
/// version from its annex `m_count`, all 32 bits (`server.h:100`): softIoc
/// keeps the circuit for an annex of 13, 0x10000 and 0x10003 and closes it
/// for 3, whatever the 16-bit `m_count` says.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_extended_version_is_its_annex_count() {
    let port = server().await;
    let kept = blocking(move || {
        [(13u32, 0), (3, 13), (0x10000, 0), (0x10003, 13)].map(|(count, raw)| {
            let mut c = Client::connect(port, 13);
            let mut out = hdr(CA_PROTO_VERSION, 0xFFFF, 0, raw, 0, 0);
            out.extend(0u32.to_be_bytes());
            out.extend(count.to_be_bytes());
            out.extend(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            c.send(&out);
            std::iter::from_fn(|| c.next())
                .any(|f| u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_ECHO)
        })
    })
    .await;
    assert_eq!(kept, [true, false, true, true]);
}

/// `cmmd` carrying `count` in the extended annex when `annex`, else in the
/// 16-bit `m_count`.
fn counted(cmmd: u16, dt: u16, count: u32, annex: bool, p1: u32, p2: u32, body: &[u8]) -> Vec<u8> {
    let mut f = if annex {
        let mut f = hdr(cmmd, 0xFFFF, dt, 0, p1, p2);
        f.extend((body.len() as u32).to_be_bytes());
        f.extend(count.to_be_bytes());
        f
    } else {
        hdr(cmmd, body.len() as u16, dt, count as u16, p1, p2)
    };
    f.extend_from_slice(body);
    f
}

/// READ_SYNC, search-fail and CLEAR_CHANNEL echo the request's `msg.m_count`,
/// which an extended request carries in its annex (`camessage.c:2464-2471`),
/// through `cas_copy_in_header`, extended once it reaches 0xffff
/// (`caserverio.c:302-316`). softIoc's frames for each request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_header_echo_carries_the_request_s_count() {
    let port = server().await;
    let (got, want) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let name = padded("NOPE");
        let (mut got, mut want) = (Vec::new(), Vec::new());
        for (count, annex) in [(5, true), (0x10000, true), (0xffff, false)] {
            let wide = count >= 0xffff;
            let cid = got.len() as u32 + 1;
            let sid = c.create(cid, "LO");
            for (request, reply) in [
                (
                    counted(CA_PROTO_READ_SYNC, 3, count, annex, 0x11, 0x22, &[]),
                    counted(CA_PROTO_READ_SYNC, 3, count, wide, 0x11, 0x22, &[]),
                ),
                (
                    counted(
                        CA_PROTO_SEARCH,
                        CA_DO_REPLY,
                        count,
                        annex,
                        0x33,
                        0x33,
                        &name,
                    ),
                    counted(
                        CA_PROTO_NOT_FOUND,
                        CA_DO_REPLY,
                        count,
                        wide,
                        0x33,
                        0x33,
                        &[],
                    ),
                ),
                (
                    counted(CA_PROTO_CLEAR_CHANNEL, 3, count, annex, sid, cid, &[]),
                    counted(CA_PROTO_CLEAR_CHANNEL, 3, count, wide, sid, cid, &[]),
                ),
            ] {
                c.send(&request);
                got.push(c.next().expect("echo"));
                want.push(reply);
            }
        }
        (got, want)
    })
    .await;
    assert_eq!(got, want);
}

/// A count of 0xffff needs the extended form, which a pre-V49 client cannot
/// take (`caserverio.c:266-269`). softIoc ends the circuit for READ_SYNC and
/// CLEAR_CHANNEL (`camessage.c:2114-2116`, `:1969-1971`), and logs a search
/// fail reply it cannot send and keeps the circuit (`:2135-2139`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_pre_v49_client_draws_no_0xffff_echo() {
    const LOST: &str =
        "../rsrv/camessage.c at 2138: should always get sufficient space for search fail reply?\n";
    let port = server().await;
    let (closed, search) = blocking(move || {
        // Each circuit opens a channel first, which also reads past the
        // greeting VERSION.
        let closed = [false, true].map(|clear| {
            let mut c = Client::connect(port, 8);
            let sid = c.create(1, "LO");
            let request = if clear {
                hdr(CA_PROTO_CLEAR_CHANNEL, 0, 3, 0xffff, sid, 1)
            } else {
                hdr(CA_PROTO_READ_SYNC, 0, 3, 0xffff, 0x11, 0x22)
            };
            let peer = c.sock.local_addr().unwrap();
            let mut rest = Vec::new();
            let lines = errlog_lines(|| {
                c.send(&request);
                rest = c.rest_until_closed();
            });
            (
                rest,
                lines == [format!("CAS: forcing disconnect from {peer}\n")],
            )
        });
        let mut c = Client::connect(port, 8);
        c.create(1, "LO");
        let name = padded("NOPE");
        let mut after = None;
        let lines = errlog_lines(|| {
            let mut out = hdr(
                CA_PROTO_SEARCH,
                name.len() as u16,
                CA_DO_REPLY,
                0xffff,
                0x33,
                0x33,
            );
            out.extend(name);
            out.extend(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            c.send(&out);
            after = c.next().map(|f| f[..16].to_vec());
        });
        (closed, (after, lines))
    })
    .await;
    assert_eq!(closed, [(vec![], true), (vec![], true)]);
    assert_eq!(
        search,
        (
            Some(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0)),
            vec![LOST.to_string()]
        )
    );
}

/// The same refusal for an ECHO sends nothing and keeps the circuit
/// (`camessage.c:416-424`); for the EVENT_CANCEL echo of a subscription's
/// count it ends the circuit (`:2089-2095`). softIoc on a CA_V48 client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_pre_v49_0xffff_echo_is_dropped_and_its_cancel_closes() {
    const WC_DB: &str = "record(waveform,\"WC\"){ field(FTVL,\"CHAR\") field(NELM,\"70000\") }\n";
    const DBR_CHAR: u16 = 4;
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let port = serve_db(CaServer::builder(), WC_DB).await;
    let (echo, cancel) = blocking(move || {
        let mut c = Client::connect(port, 8);
        c.create(1, "WC");
        let mut out = hdr(CA_PROTO_ECHO, 0, 3, 0xffff, 0x44, 0x55);
        out.extend(hdr(CA_PROTO_ECHO, 0, 0, 0, 0x99, 0x99));
        c.send(&out);
        let echo = c.next().map(|f| f[..16].to_vec());

        let mut c = Client::connect(port, 8);
        let sid = c.create(1, "WC");
        let mut add = hdr(CA_PROTO_EVENT_ADD, 16, DBR_CHAR, 0xffff, sid, 7);
        add.extend([0u8; 12]);
        add.extend(5u16.to_be_bytes());
        add.extend([0u8; 2]);
        c.send(&add);
        // The first update draws the same refusal (`camessage.c:519-528`).
        let update = c.error()[12..16].to_vec();
        let peer = c.sock.local_addr().unwrap();
        let mut rest = Vec::new();
        let lines = errlog_lines(|| {
            c.send(&hdr(CA_PROTO_EVENT_CANCEL, 0, DBR_CHAR, 0xffff, sid, 7));
            rest = c.rest_until_closed();
        });
        let closed = lines == [format!("CAS: forcing disconnect from {peer}\n")];
        (echo, (update, rest, closed))
    })
    .await;
    assert_eq!(echo, Some(hdr(CA_PROTO_ECHO, 0, 0, 0, 0x99, 0x99)));
    assert_eq!(
        cancel,
        (ECA_16KARRAYCLIENT.to_be_bytes().to_vec(), vec![], true)
    );
}

/// `logBadIdWithFileAndLineno` (`camessage.c:312-325`): `__FILE__` as the
/// rsrv build spells it and the line of each `logBadId` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_bad_resource_id_names_the_logbadid_site() {
    let port = server().await;
    let write_body = |cmmd| {
        let mut f = hdr(cmmd, 8, DBR_DOUBLE, 1, 0xdead, 7);
        f.extend_from_slice(&[0u8; 8]);
        f
    };
    let cases: Vec<(Vec<u8>, u32)> = vec![
        (hdr(CA_PROTO_READ, 0, DBR_DOUBLE, 1, 0xdead, 7), 614),
        (hdr(CA_PROTO_READ_NOTIFY, 0, DBR_DOUBLE, 1, 0xdead, 7), 709),
        (write_body(CA_PROTO_WRITE), 749),
        (write_body(CA_PROTO_WRITE_NOTIFY), 1674),
        (event_add(0xdead, 9, 1), 1825),
        (hdr(CA_PROTO_CLEAR_CHANNEL, 0, 0, 0, 0xdead, 1), 1939),
        (
            hdr(CA_PROTO_EVENT_CANCEL, 0, DBR_DOUBLE, 1, 0xdead, 9),
            2048,
        ),
    ];
    let requests: Vec<Vec<u8>> = cases.iter().map(|(r, _)| r.clone()).collect();
    let got = blocking(move || {
        requests
            .into_iter()
            .map(|r| {
                let mut c = Client::connect(port, 13);
                c.send(&r);
                c.error()
            })
            .collect::<Vec<_>>()
    })
    .await;
    let want: Vec<Vec<u8>> = cases
        .iter()
        .map(|(r, line)| {
            error_frame(
                NO_CID,
                ECA_INTERNAL,
                r,
                &format!("Bad Resource ID at ../rsrv/camessage.c.{line}"),
            )
        })
        .collect();
    assert_eq!(got, want);
}

/// `event_cancel_reply` (`camessage.c:2070-2074`): `RECORD_NAME`, not the
/// channel name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancelling_an_unknown_subscription_names_the_record() {
    let port = server().await;
    let (sid, got) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(1, "LO.DESC");
        c.send(&hdr(CA_PROTO_EVENT_CANCEL, 0, DBR_DOUBLE, 1, sid, 0x77));
        (sid, c.error())
    })
    .await;
    let req = hdr(CA_PROTO_EVENT_CANCEL, 0, DBR_DOUBLE, 1, sid, 0x77);
    assert_eq!(got, error_frame(1, ECA_BADMONID, &req, "LO"));
}

/// softIoc takes a second EVENT_ADD with a sub_id already in use: both get
/// their initial update and no error is sent. `event_add_action`
/// (`camessage.c:1812-1906`) has no duplicate check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_duplicate_sub_id_is_installed_as_softioc_does() {
    let port = server().await;
    let replies = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(1, "LO");
        let mut f = event_add(sid, 9, 1);
        f.extend(event_add(sid, 9, 1));
        c.send(&f);
        let mut replies = Vec::new();
        while replies.len() < 2 {
            let frame = c.next().expect("an EVENT_ADD reply");
            let cmmd = u16::from_be_bytes([frame[0], frame[1]]);
            assert_ne!(cmmd, CA_PROTO_ERROR, "softIoc sends no error: {frame:?}");
            if cmmd == CA_PROTO_EVENT_ADD {
                replies.push(frame[..16].to_vec());
            }
        }
        replies
    })
    .await;
    let reply = hdr(CA_PROTO_EVENT_ADD, 8, DBR_DOUBLE, 1, 1, 9);
    assert_eq!(replies, [reply.clone(), reply]);
}

/// `event_cancel_reply` (`camessage.c:2057-2063`) takes the first install of a
/// duplicated sub_id. softIoc, after two EVENT_ADDs with sub 9 on `LO`: the
/// first EVENT_CANCEL is confirmed, a write of 5 draws one update, the second
/// is confirmed, and the third draws ECA_BADMONID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn each_cancel_of_a_duplicate_sub_id_takes_one_install() {
    let port = server().await;
    let (sid, got) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(1, "LO");
        let mut f = event_add(sid, 9, 1);
        f.extend(event_add(sid, 9, 1));
        c.send(&f);
        let mut got = vec![
            c.next().unwrap()[..16].to_vec(),
            c.next().unwrap()[..16].to_vec(),
        ];
        let cancel = hdr(CA_PROTO_EVENT_CANCEL, 0, DBR_DOUBLE, 1, sid, 9);
        c.send(&cancel);
        got.push(c.next().unwrap());
        let mut write = hdr(CA_PROTO_WRITE, 8, 5, 1, sid, 0);
        write.extend_from_slice(&5i32.to_be_bytes());
        write.extend_from_slice(&[0; 4]);
        c.send(&write);
        got.push(c.next().unwrap()[..16].to_vec());
        c.send(&cancel);
        got.push(c.next().unwrap());
        c.send(&cancel);
        got.push(c.error());
        (sid, got)
    })
    .await;
    let update = hdr(CA_PROTO_EVENT_ADD, 8, DBR_DOUBLE, 1, 1, 9);
    let confirm = hdr(CA_PROTO_EVENT_ADD, 0, DBR_DOUBLE, 1, sid, 9);
    let cancel = hdr(CA_PROTO_EVENT_CANCEL, 0, DBR_DOUBLE, 1, sid, 9);
    assert_eq!(
        got,
        [
            update.clone(),
            update.clone(),
            confirm.clone(),
            update,
            confirm,
            error_frame(1, ECA_BADMONID, &cancel, "LO"),
        ]
    );
}

/// `read_reply` (`camessage.c:519-528`) for a subscription, and through it a
/// READ_NOTIFY, and `read_action` (`camessage.c:632-638`) for a READ, on a
/// CA_V48 client: `cas_copy_in_header` refuses a payload past 0xffff with
/// `ECA_16KARRAYCLIENT`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_16k_refusal_names_the_pv_and_the_sizes() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let port = server().await;
    let got = blocking(move || {
        let requests: [fn(u32) -> Vec<u8>; 3] = [
            |sid| event_add(sid, 9, 10000),
            |sid| hdr(CA_PROTO_READ, 0, DBR_DOUBLE, 10000, sid, 7),
            |sid| hdr(CA_PROTO_READ_NOTIFY, 0, DBR_DOUBLE, 10000, sid, 8),
        ];
        requests
            .into_iter()
            .map(|r| {
                let mut c = Client::connect(port, 8);
                let r = r(c.create(1, "WF"));
                c.send(&r);
                let got = c.try_error();
                (r, got)
            })
            .collect::<Vec<_>>()
    })
    .await;
    let update = |avail| {
        format!(
            "server unable to load read (or subscription update) response into protocol \
             buffer PV=\"WF\" dbf=6 count=10000 avail={avail} max bytes=1000024"
        )
    };
    let read = "server unable to load read response into protocol buffer PV=\"WF\" dbf=6 \
                count=10000 avail=7 max bytes=1000024";
    let ctx = [update(9), read.to_string(), update(8)];
    let want: Vec<_> = got
        .iter()
        .zip(ctx)
        .map(|((req, _), ctx)| Ok(error_frame(1, ECA_16KARRAYCLIENT, req, &ctx)))
        .collect();
    let got: Vec<_> = got.into_iter().map(|(_, frame)| frame).collect();
    assert_eq!(got, want);
}

/// `read_reply` sizes an autosize request at the field's `no_elements` before
/// `cas_copy_in_header` (`camessage.c:509-518`), so a CA_V48 client is refused
/// for the whole array rather than sent an empty update or reply. softIoc
/// answers an EVENT_ADD and a READ_NOTIFY of count 0 on `WF` with these.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_zero_count_subscription_is_refused_for_the_whole_array() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let port = server().await;
    let got = blocking(move || {
        let requests: [fn(u32) -> Vec<u8>; 2] = [
            |sid| event_add(sid, 10, 0),
            |sid| hdr(CA_PROTO_READ_NOTIFY, 0, DBR_DOUBLE, 0, sid, 7),
        ];
        requests
            .map(|request| {
                let mut c = Client::connect(port, 8);
                let req = request(c.create(1, "WF"));
                c.send(&req);
                (req, c.try_error())
            })
            .to_vec()
    })
    .await;
    let ctx = |avail| {
        format!(
            "server unable to load read (or subscription update) response into protocol \
             buffer PV=\"WF\" dbf=6 count=10000 avail={avail} max bytes=1000024"
        )
    };
    let want: Vec<_> = got
        .iter()
        .zip([10, 7])
        .map(|((req, _), avail)| {
            (
                req.clone(),
                Ok(error_frame(1, ECA_16KARRAYCLIENT, req, &ctx(avail))),
            )
        })
        .collect();
    assert_eq!(got, want);
}

/// `read_action` and `read_reply` refuse the header before they check access
/// (`camessage.c:629-645`, `:516-535`). softIoc with an ACF granting nobody
/// read answers these on `WF` to a CA_V48 client, not ECA_NORDACCESS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_16k_refusal_precedes_the_access_check() {
    const ACF: &str =
        "UAG(nobody) { nosuchuser }\nASG(DEFAULT) {\n RULE(1, READ) { UAG(nobody) }\n}\n";
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let acf = epics_base_rs::server::access_security::parse_acf(ACF).expect("acf");
    let port = serve(CaServer::builder().acf(acf)).await;
    let got = blocking(move || {
        let requests: [fn(u32) -> Vec<u8>; 3] = [
            |sid| event_add(sid, 9, 0),
            |sid| hdr(CA_PROTO_READ_NOTIFY, 0, DBR_DOUBLE, 0, sid, 7),
            |sid| hdr(CA_PROTO_READ, 0, DBR_DOUBLE, 10000, sid, 7),
        ];
        requests
            .map(|request| {
                let mut c = Client::connect(port, 8);
                let req = request(c.create(1, "WF"));
                c.send(&req);
                (req, c.try_error())
            })
            .to_vec()
    })
    .await;
    let update = |avail| {
        format!(
            "server unable to load read (or subscription update) response into protocol \
             buffer PV=\"WF\" dbf=6 count=10000 avail={avail} max bytes=1000024"
        )
    };
    let read = "server unable to load read response into protocol buffer PV=\"WF\" dbf=6 \
                count=10000 avail=7 max bytes=1000024"
        .to_string();
    let want: Vec<_> = got
        .iter()
        .zip([update(9), update(7), read])
        .map(|((req, _), ctx)| {
            (
                req.clone(),
                Ok(error_frame(1, ECA_16KARRAYCLIENT, req, &ctx)),
            )
        })
        .collect();
    assert_eq!(got, want);
}

/// `send_err` stamps `pciu->cid`, the client's id, never the sid, on every
/// refusal (`camessage.c:162-185`). softIoc with `LO` on cid 5 and `WF` on
/// cid 3 answers these four on `WF` with cid 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_16k_refusal_carries_the_client_cid() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let port = server().await;
    let got = blocking(move || {
        let requests: [fn(u32) -> Vec<u8>; 4] = [
            |sid| event_add(sid, 9, 0),
            |sid| event_add(sid, 9, 10000),
            |sid| hdr(CA_PROTO_READ_NOTIFY, 0, DBR_DOUBLE, 0, sid, 7),
            |sid| hdr(CA_PROTO_READ, 0, DBR_DOUBLE, 10000, sid, 7),
        ];
        requests
            .map(|request| {
                let mut c = Client::connect(port, 8);
                c.create(5, "LO");
                let req = request(c.create(3, "WF"));
                c.send(&req);
                (req, c.try_error())
            })
            .to_vec()
    })
    .await;
    let update = |avail| {
        format!(
            "server unable to load read (or subscription update) response into protocol \
             buffer PV=\"WF\" dbf=6 count=10000 avail={avail} max bytes=1000024"
        )
    };
    let read = "server unable to load read response into protocol buffer PV=\"WF\" dbf=6 \
                count=10000 avail=7 max bytes=1000024"
        .to_string();
    let want: Vec<_> = got
        .iter()
        .zip([update(9), update(9), update(7), read])
        .map(|((req, _), ctx)| {
            (
                req.clone(),
                Ok(error_frame(3, ECA_16KARRAYCLIENT, req, &ctx)),
            )
        })
        .collect();
    assert_eq!(got, want);
}

/// The receive gate's refusals go through `send_err` too, so a channel
/// request naming a live channel carries its cid (`camessage.c:162-178`).
/// softIoc with `EPICS_CA_AUTO_ARRAY_BYTES=NO`, `LO` on cid 5: a declared
/// body past the ceiling for each of the six channel commands, then a
/// misaligned WRITE_NOTIFY, all answered with cid 5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_gate_refusal_carries_the_client_cid() {
    // SAFETY: before the server reads them, in a test of its own.
    unsafe {
        std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000");
        std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", "NO");
    }
    let port = server().await;
    let commands = [
        CA_PROTO_EVENT_ADD,
        CA_PROTO_EVENT_CANCEL,
        CA_PROTO_READ,
        CA_PROTO_WRITE,
        CA_PROTO_READ_NOTIFY,
        CA_PROTO_WRITE_NOTIFY,
    ];
    let got = blocking(move || {
        let mut got: Vec<_> = commands
            .map(|cmmd| {
                let mut c = Client::connect(port, 13);
                let sid = c.create(5, "LO");
                let mut req = hdr(cmmd, 0xffff, DBR_DOUBLE, 0, sid, 0x66);
                req.extend_from_slice(&0x0200_0000u32.to_be_bytes());
                req.extend_from_slice(&1u32.to_be_bytes());
                c.send(&req);
                c.send(&[0; 64]);
                (req, c.try_error())
            })
            .to_vec();
        let mut c = Client::connect(port, 13);
        let sid = c.create(5, "LO");
        let req = hdr(CA_PROTO_WRITE_NOTIFY, 4, 5, 1, sid, 7);
        c.send(&req);
        c.send(&[0; 4]);
        got.push((req, c.try_error()));
        got
    })
    .await;
    let too_large = |req: &[u8]| {
        let mut payload = req[..24].to_vec();
        payload.extend_from_slice(&padded(
            "CAS: Server unable to load large request message. Max bytes=1000024",
        ));
        let mut f = hdr(CA_PROTO_ERROR, payload.len() as u16, 0, 0, 5, ECA_TOLARGE);
        f.extend_from_slice(&payload);
        f
    };
    let mut want: Vec<_> = got[..6]
        .iter()
        .map(|(req, _)| (req.clone(), Ok(too_large(req))))
        .collect();
    let req = &got[6].0;
    want.push((
        req.clone(),
        Ok(error_frame(
            5,
            ECA_INTERNAL,
            req,
            "CAS: Missaligned protocol rejected",
        )),
    ));
    assert_eq!(got, want);
}

/// `EPICS_CAS_MAX_CHANNELS` refuses where `claim_ciu_action` refuses a
/// channel it could not allocate (`camessage.c:1260-1268`): `RECORD_NAME`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_channel_limit_names_the_record() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CAS_MAX_CHANNELS", "1") };
    let port = server().await;
    let got = blocking(move || {
        let mut c = Client::connect(port, 13);
        c.create(1, "LO");
        let name = padded("LO.DESC");
        let mut f = hdr(CA_PROTO_CREATE_CHAN, name.len() as u16, 0, 0, 2, 13);
        f.extend(name);
        c.send(&f);
        (f, c.error())
    })
    .await;
    assert_eq!(got.1, error_frame(NO_CID, ECA_ALLOCMEM, &got.0, "LO"));
}

/// `EPICS_CAS_MAX_SUBS_PER_CHAN` refuses where `event_add_action` refuses a
/// subscription block it could not allocate (`camessage.c:1840-1851`):
/// `RECORD_NAME`, then `RSRV_ERROR`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_per_channel_subscription_cap_names_the_record() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CAS_MAX_SUBS_PER_CHAN", "1") };
    let port = server().await;
    let (sid, got) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(1, "LO.DESC");
        c.send(&event_add(sid, 9, 1));
        c.send(&event_add(sid, 10, 1));
        (sid, (c.error(), c.rest_until_closed()))
    })
    .await;
    assert_eq!(
        got,
        (
            error_frame(1, ECA_ALLOCMEM, &event_add(sid, 10, 1), "LO"),
            vec![]
        )
    );
}

/// `EPICS_CAS_MAX_SUBSCRIBERS_PER_PV` (at least 8) refuses where
/// `db_add_event` fails in `event_add_action` (`camessage.c:1868-1877`), on a
/// record field and on a builder PV, which C has no counterpart for, then
/// `RSRV_ERROR`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_per_pv_subscriber_cap_names_the_failed_install() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CAS_MAX_SUBSCRIBERS_PER_PV", "8") };
    let port = server().await;
    let got = blocking(move || {
        ["LO.DESC", "SP"].map(|pv| {
            let mut c = Client::connect(port, 13);
            let sid = c.create(1, pv);
            for sub_id in 1..=9 {
                c.send(&event_add(sid, sub_id, 1));
            }
            (sid, (c.error(), c.rest_until_closed()))
        })
    })
    .await;
    let want = [("LO", got[0].0), ("SP", got[1].0)].map(|(name, sid)| {
        (
            error_frame(
                1,
                ECA_ALLOCMEM,
                &event_add(sid, 9, 1),
                &format!("subscription install into record {name} failed"),
            ),
            vec![],
        )
    });
    assert_eq!(got.map(|(_, refusal)| refusal), want);
}

/// `claim_ciu_action`, `search_reply_udp` and `search_reply_tcp`
/// (`camessage.c:1235`, `:2210`, `:2297`) each `log_header` a name of
/// `m_postsize <= 1` and keep going. softIoc prints these for a UDP SEARCH,
/// then a TCP SEARCH and a CREATE_CHAN, all with an empty payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_empty_pv_name_is_logged_at_each_refusal() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &Default::default())
        .expect("db")
        .build()
        .await
        .expect("build CA server");
    let (tcp, udp) = (server.tcp_port(), server.udp_port());
    tokio::spawn(async move { server.run().await });
    let (udp_peer, tcp_peer, lines) = blocking(move || {
        let u = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        u.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = Client::connect(tcp, 13);
        let lines = errlog_lines(|| {
            let mut dg = hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0);
            dg.extend(hdr(CA_PROTO_SEARCH, 0, 5, 13, 0x77, 0x88));
            u.send_to(&dg, ("127.0.0.1", udp)).unwrap();
            // A reply to a later datagram proves the first was handled.
            let name = padded("LO");
            let mut found = hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0);
            found.extend(hdr(CA_PROTO_SEARCH, name.len() as u16, 10, 13, 1, 1));
            found.extend(name);
            u.send_to(&found, ("127.0.0.1", udp)).unwrap();
            u.recv(&mut [0; 512]).expect("SEARCH reply");
            c.send(&hdr(CA_PROTO_SEARCH, 0, 5, 13, 0x71, 0x72));
            c.send(&hdr(CA_PROTO_CREATE_CHAN, 0, 0, 0, 0x73, 13));
            c.send(&hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            while c.next().expect("ECHO")[..2] != CA_PROTO_ECHO.to_be_bytes() {}
        });
        (u.local_addr().unwrap(), c.sock.local_addr().unwrap(), lines)
    })
    .await;
    let lines: Vec<_> = lines
        .into_iter()
        .filter(|l| l.contains(&udp_peer.to_string()) || l.contains(&tcp_peer.to_string()))
        .collect();
    let empty = |peer, cmmd, cid, avail| {
        [
            format!("CAS: request from {peer} => empty PV name in UDP search request?\n"),
            format!(
                "CAS: Request from {peer} => cmmd={cmmd} cid={cid:#x} type={} count={} \
                 postsize=0\n",
                if cmmd == 6 { 5 } else { 0 },
                if cmmd == 6 { 13 } else { 0 },
            ),
            format!("CAS: Request from {peer} =>   available={avail:#x} \tN=0 paddr=(nil)\n"),
        ]
    };
    let want: Vec<_> = [
        empty(udp_peer, 6, 0x77, 0x88),
        empty(tcp_peer, 6, 0x71, 0x72),
        empty(tcp_peer, 18, 0x73, 0xd),
    ]
    .concat();
    assert_eq!(lines, want);
}

/// Every errlog line `f` produces, in order.
fn errlog_lines(f: impl FnOnce()) -> Vec<String> {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    // A listener hears every message still queued when it is added, as C's
    // live list does (`errlog.c:801-815`): the server's start-up line first.
    epics_base_rs::runtime::log::errlog_flush();
    let listener = epics_base_rs::runtime::log::errlog_add_listener(move |line| {
        sink.lock().unwrap().push(line.to_string());
    });
    f();
    epics_base_rs::runtime::log::errlog_flush();
    epics_base_rs::runtime::log::errlog_remove_listener(listener);
    seen.lock().unwrap().clone()
}

/// `putNotifyErrorReply` cannot frame a count of 0xffff for a pre-V49
/// client, so it sends nothing and logs (`camessage.c:1525-1529`). softIoc
/// with `WR` in a read-only ASG prints this line and keeps the circuit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_unframeable_put_notify_error_reply_is_logged_not_sent() {
    const WN_DB: &str =
        "record(waveform,\"WR\"){ field(FTVL,\"CHAR\") field(NELM,\"70000\") field(ASG,\"RO\") }
";
    const ACF: &str =
        "ASG(DEFAULT) {\n RULE(1, READ)\n RULE(1, WRITE)\n}\nASG(RO) {\n RULE(1, READ)\n}\n";
    const LOST: &str = "../rsrv/camessage.c at 1528: should always get sufficient space for put notify error reply\n";
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let acf = epics_base_rs::server::access_security::parse_acf(ACF).expect("acf");
    let port = serve_db(CaServer::builder().acf(acf), WN_DB).await;
    let (denied, denied_lines) = blocking(move || {
        let mut c = Client::connect(port, 8);
        let sid = c.create(3, "WR");
        let mut after = Vec::new();
        let lines = errlog_lines(|| {
            c.send(&hdr(CA_PROTO_WRITE_NOTIFY, 0, 4, 0xffff, sid, 7));
            c.send(&hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            after.push(c.next().map(|f| f[..16].to_vec()));
        });
        (after, lines)
    })
    .await;
    assert_eq!(denied, [Some(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0))]);
    assert_eq!(denied_lines, [LOST]);
}

/// `db_add_event` refuses mask 0 (`dbEvent.c:437`): softIoc logs the
/// header, sends the install failure and ends the circuit. `paddr` is masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_failed_db_install_logs_the_header_and_closes() {
    let port = server().await;
    let (got, lines, peer, sid) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(3, "LO");
        let peer = c.sock.local_addr().unwrap();
        let mut req = event_add(sid, 9, 1);
        req[28..30].copy_from_slice(&0u16.to_be_bytes());
        let mut got = None;
        let lines = errlog_lines(|| {
            c.send(&req);
            got = Some((req.clone(), c.error(), c.rest_until_closed()));
        });
        (got.unwrap(), lines, peer, sid)
    })
    .await;
    let (req, frame, rest) = got;
    assert_eq!(
        (frame, rest),
        (
            error_frame(
                3,
                ECA_ALLOCMEM,
                &req,
                "subscription install into record LO failed"
            ),
            vec![]
        )
    );
    let masked: Vec<String> = lines
        .iter()
        .filter(|l| l.contains(&peer.to_string()))
        .map(|l| match l.find("paddr=0x") {
            Some(at) => format!("{}paddr=<p>\n", &l[..at]),
            None => l.clone(),
        })
        .collect();
    assert_eq!(
        masked,
        [
            format!("CAS: request from {peer} => no memory to add subscription to db\n"),
            format!("CAS: Request from {peer} => cmmd=1 cid={sid:#x} type=6 count=1 postsize=16\n"),
            format!("CAS: Request from {peer} =>   available=0x9 \tN=0 paddr=<p>\n"),
            format!("CAS: forcing disconnect from {peer}\n"),
        ]
    );
}

/// softIoc's `log_header` lines for `req` and the `camsgtask.c` disconnect;
/// `paddr` is masked when C found a channel and `(nil)` when it did not.
fn refusal_lines(peer: std::net::SocketAddr, ctx: &str, req: &[u8], found: bool) -> Vec<String> {
    let u16_at = |i: usize| u16::from_be_bytes([req[i], req[i + 1]]);
    let u32_at = |i: usize| u32::from_be_bytes(req[i..i + 4].try_into().unwrap());
    let paddr = if found { "<p>" } else { "(nil)" };
    vec![
        format!("CAS: request from {peer} => {ctx}\n"),
        format!(
            "CAS: Request from {peer} => cmmd={} cid={:#x} type={} count={} postsize={}\n",
            u16_at(0),
            u32_at(8),
            u16_at(4),
            u16_at(6),
            u16_at(2)
        ),
        format!(
            "CAS: Request from {peer} =>   available={:#x} \tN=0 paddr={paddr}\n",
            u32_at(12)
        ),
        format!("CAS: forcing disconnect from {peer}\n"),
    ]
}

/// Each handler refusal C logs with `log_header` before `RSRV_ERROR`
/// (`logBadIdWithFileAndLineno`, `write_action`, `write_notify_action`,
/// `host_name_action`, `client_name_action`); lines are softIoc's for these
/// frames, `paddr` masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn each_handler_refusal_logs_the_request_header() {
    type Case = (Option<&'static str>, fn(u32) -> Vec<u8>, &'static str);
    let cases: [Case; 11] = [
        (
            None,
            |_| hdr(CA_PROTO_READ, 0, 6, 1, 0xdead, 7),
            "bad resource ID",
        ),
        (
            None,
            |_| {
                let mut f = hdr(CA_PROTO_WRITE, 8, 6, 1, 0xdead, 7);
                f.extend([0; 8]);
                f
            },
            "bad resource ID",
        ),
        (None, |_| event_add(0xdead, 9, 1), "bad resource ID"),
        (
            None,
            |_| hdr(CA_PROTO_EVENT_CANCEL, 0, 6, 1, 0xdead, 9),
            "bad resource ID",
        ),
        (
            None,
            |_| hdr(CA_PROTO_READ_NOTIFY, 0, 6, 1, 0xdead, 7),
            "bad resource ID",
        ),
        (
            None,
            |_| {
                let mut f = hdr(CA_PROTO_WRITE_NOTIFY, 8, 6, 1, 0xdead, 7);
                f.extend([0; 8]);
                f
            },
            "bad resource ID",
        ),
        (
            None,
            |_| hdr(CA_PROTO_CLEAR_CHANNEL, 0, 0, 0, 0xdead, 1),
            "bad resource ID",
        ),
        (
            Some("LO"),
            |sid| {
                let mut f = hdr(CA_PROTO_WRITE, 8, 99, 1, sid, 7);
                f.extend([0; 8]);
                f
            },
            "bad put data type",
        ),
        (
            Some("LO"),
            |sid| {
                let mut f = hdr(CA_PROTO_WRITE_NOTIFY, 8, 99, 1, sid, 7);
                f.extend([0; 8]);
                f
            },
            "bad put notify data type",
        ),
        (
            None,
            |_| {
                let mut f = hdr(CA_PROTO_HOST_NAME, 520, 0, 0, 0, 0);
                f.extend([b'h'; 520]);
                f
            },
            "bad (very long) host name",
        ),
        (
            None,
            |_| {
                let mut f = hdr(CA_PROTO_CLIENT_NAME, 520, 0, 0, 0, 0);
                f.extend([b'u'; 520]);
                f
            },
            "a very long user name was specified",
        ),
    ];
    let port = server().await;
    let got = blocking(move || {
        cases
            .map(|(pv, request, ctx)| {
                let mut c = Client::connect(port, 13);
                let sid = pv.map(|pv| c.create(3, pv));
                let peer = c.sock.local_addr().unwrap();
                let req = request(sid.unwrap_or(0));
                let lines = errlog_lines(|| {
                    c.send(&req);
                    c.rest_until_closed();
                });
                let lines: Vec<String> = lines
                    .iter()
                    .filter(|l| l.contains(&peer.to_string()))
                    .map(|l| match l.find("paddr=0x") {
                        Some(at) => format!("{}paddr=<p>\n", &l[..at]),
                        None => l.clone(),
                    })
                    .collect();
                (lines, refusal_lines(peer, ctx, &req, sid.is_some()))
            })
            .to_vec()
    })
    .await;
    for (got, want) in got {
        assert_eq!(got, want);
    }
}

/// A payload shorter than `dbr_size_n`, or a scalar string with no NUL, is
/// `RSRV_ERROR` with no frame and no `log_header` on both opcodes
/// (`camessage.c:761-771`, `:1692-1702`); softIoc prints only the disconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_short_put_payload_is_dropped_without_a_log_header() {
    type Case = (&'static str, fn(u32) -> Vec<u8>);
    let cases: [Case; 4] = [
        ("WF", |sid| {
            let mut f = hdr(CA_PROTO_WRITE, 8, DBR_DOUBLE, 2, sid, 7);
            f.extend([0; 8]);
            f
        }),
        ("WF", |sid| {
            let mut f = hdr(CA_PROTO_WRITE_NOTIFY, 8, DBR_DOUBLE, 2, sid, 7);
            f.extend([0; 8]);
            f
        }),
        ("LO", |sid| {
            let mut f = hdr(CA_PROTO_WRITE, 8, 0, 1, sid, 7);
            f.extend([b'a'; 8]);
            f
        }),
        ("LO", |sid| {
            let mut f = hdr(CA_PROTO_WRITE_NOTIFY, 8, 0, 1, sid, 7);
            f.extend([b'a'; 8]);
            f
        }),
    ];
    let port = server().await;
    let got = blocking(move || {
        cases
            .map(|(pv, request)| {
                let mut c = Client::connect(port, 13);
                let sid = c.create(3, pv);
                let peer = c.sock.local_addr().unwrap();
                let mut rest = Vec::new();
                let lines = errlog_lines(|| {
                    c.send(&request(sid));
                    rest = c.rest_until_closed();
                });
                let lines: Vec<String> = lines
                    .into_iter()
                    .filter(|l| l.contains(&peer.to_string()))
                    .collect();
                (
                    (rest, lines),
                    (
                        vec![],
                        vec![format!("CAS: forcing disconnect from {peer}\n")],
                    ),
                )
            })
            .to_vec()
    })
    .await;
    for (got, want) in got {
        assert_eq!(got, want);
    }
}

/// A put-callback still busy after C's 60 s `blockSem` wait is cancelled and
/// answered ECA_PUTCBINPROG for its own ioid, after `log_header` of its saved
/// request (`camessage.c:1712-1745`). softIoc, `SQ.PROC` with DLY0 65, sends
/// that frame 60 s after the first put and prints these lines, `cid` being
/// its sid. Runs ~61 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_put_callback_past_the_wait_is_logged_and_answered_putcbinprog() {
    const SQ_DB: &str =
        "record(seq,\"SQ\"){ field(DLY0,\"65\") field(DO0,\"1\") field(LNK0,\"SQOUT PP\") }
record(ao,\"SQOUT\"){}
";
    let port = serve_db(CaServer::builder(), SQ_DB).await;
    let (first, elapsed, lines, peer, sid) = blocking(move || {
        let mut c = Client::connect(port, 13);
        c.sock
            .set_read_timeout(Some(Duration::from_secs(70)))
            .unwrap();
        let sid = c.create(3, "SQ.PROC");
        let peer = c.sock.local_addr().unwrap();
        let put = |ioid| {
            let mut f = hdr(CA_PROTO_WRITE_NOTIFY, 8, 1, 1, sid, ioid);
            f.extend([0, 1, 0, 0, 0, 0, 0, 0]);
            f
        };
        let mut first = Vec::new();
        let mut elapsed = Duration::ZERO;
        let lines = errlog_lines(|| {
            let start = std::time::Instant::now();
            c.send(&put(0x11));
            std::thread::sleep(Duration::from_millis(300));
            c.send(&put(0x22));
            first = c.next().expect("a reply")[..16].to_vec();
            elapsed = start.elapsed();
        });
        let lines: Vec<String> = lines
            .into_iter()
            .filter(|l| l.contains(&peer.to_string()))
            .map(|l| match l.find("paddr=0x") {
                Some(at) => format!("{}paddr=<p>\n", &l[..at]),
                None => l,
            })
            .collect();
        (first, elapsed, lines, peer, sid)
    })
    .await;
    assert_eq!(
        first,
        hdr(CA_PROTO_WRITE_NOTIFY, 0, 1, 1, ECA_PUTCBINPROG, 0x11)
    );
    assert!(
        elapsed >= Duration::from_secs(60) && elapsed < Duration::from_secs(62),
        "{elapsed:?}"
    );
    assert_eq!(
        lines,
        [
            format!("CAS: request from {peer} => put call back time out\n"),
            format!("CAS: Request from {peer} => cmmd=19 cid={sid:#x} type=1 count=1 postsize=8\n"),
            format!("CAS: Request from {peer} =>   available=0x11 \tN=0 paddr=<p>\n"),
        ]
    );
}

/// `read_reply` answers a read-denied monitor's initial update with
/// `no_read_access_event` (`camessage.c:516-535`, `:473-475`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_read_denied_monitor_gets_nordaccess_not_the_value() {
    const ACF: &str =
        "UAG(nobody) { nosuchuser }\nASG(DEFAULT) {\n RULE(1, READ) { UAG(nobody) }\n}\n";
    let acf = epics_base_rs::server::access_security::parse_acf(ACF).expect("acf");
    let port = serve(CaServer::builder().acf(acf)).await;
    let first = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(1, "LO");
        c.send(&event_add(sid, 9, 1));
        loop {
            let f = c.next().expect("frame");
            if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_EVENT_ADD {
                break f;
            }
        }
    })
    .await;
    assert_eq!(
        u32::from_be_bytes(first[8..12].try_into().unwrap()),
        epics_ca_rs::protocol::ECA_NORDACCESS,
        "initial update of a read-denied monitor: {first:02x?}"
    );
}

/// `cas_copy_in_header` frames with the client's minor as it stands when the
/// frame is built (`caserverio.c:266-270`), and a VERSION rewrites it
/// (`camessage.c:369`): softIoc sends a CA_V413 subscriber an extended update,
/// and refuses the next post once a VERSION 8 has arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_subscription_update_is_framed_at_the_minor_of_its_post() {
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let port = server().await;
    let (initial, add, refused) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let add = event_add(c.create(1, "WF"), 9, 10000);
        c.send(&add);
        let first = |c: &mut Client, cmmd: u16| {
            std::iter::from_fn(|| c.next()).find(|f| f[..2] == cmmd.to_be_bytes())
        };
        let initial = first(&mut c, CA_PROTO_EVENT_ADD).map(|f| f[2..4].to_vec());
        c.send(&hdr(CA_PROTO_VERSION, 0, 0, 8, 0, 0));
        c.send(&hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
        first(&mut c, CA_PROTO_ECHO).expect("the VERSION is dispatched");
        let mut w = Client::connect(port, 13);
        let mut put = hdr(CA_PROTO_WRITE, 8, DBR_DOUBLE, 1, w.create(1, "WF"), 0);
        put.extend(5f64.to_be_bytes());
        w.send(&put);
        (initial, add, c.try_error())
    })
    .await;
    assert_eq!(initial, Some(vec![0xff, 0xff]), "extended at minor 13");
    let ctx = "server unable to load read (or subscription update) response into protocol \
               buffer PV=\"WF\" dbf=6 count=10000 avail=9 max bytes=1000024";
    assert_eq!(refused, Ok(error_frame(1, ECA_16KARRAYCLIENT, &add, ctx)));
}

/// `write_notify_reply` frames the completion at the minor of its completion
/// (`camessage.c:1438-1449`): softIoc logs a put callback whose chain was
/// still running when a VERSION 8 arrived, and sends nothing for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_put_callback_completion_is_framed_at_the_minor_of_its_completion() {
    const WN_DB: &str =
        "record(waveform,\"WC\"){ field(FTVL,\"CHAR\") field(NELM,\"70000\") field(FLNK,\"CO\") }
record(calcout,\"CO\"){ field(CALC,\"1\") field(ODLY,\"0.5\") }
";
    // SAFETY: before the server reads it, in a test of its own.
    unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "1000000") };
    let port = serve_db(CaServer::builder(), WN_DB).await;
    let (after, lines) = blocking(move || {
        let mut c = Client::connect(port, 13);
        let sid = c.create(1, "WC");
        let mut put = hdr(CA_PROTO_WRITE_NOTIFY, 0xffff, 4, 0, sid, 7);
        put.extend(70000u32.to_be_bytes());
        put.extend(70000u32.to_be_bytes());
        put.extend([1u8; 70000]);
        let mut after = Vec::new();
        let lines = errlog_lines(|| {
            c.send(&put);
            c.send(&hdr(CA_PROTO_VERSION, 0, 0, 8, 0, 0));
            std::thread::sleep(Duration::from_secs(1));
            c.send(&hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            after.push(c.next().map(|f| f[..16].to_vec()));
        });
        (after, lines)
    })
    .await;
    assert_eq!(after, [Some(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0))]);
    assert_eq!(
        lines,
        ["CA server corrupted - put call back(s) discarded\n"]
    );
}
