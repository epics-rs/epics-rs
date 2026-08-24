//! An `mca` channel is `NMAX` wide even when the record serves fewer channels.
//!
//! C keeps the two counts in two different hooks, and they answer differently
//! on purpose:
//!
//! ```c
//! /* cvt_dbaddr (mcaRecord.c:846-863) — the CHANNEL's capacity */
//! paddr->no_elements = pmca->nmax;
//!
//! /* get_array_info (mcaRecord.c:865-873) — the CURRENT valid length */
//! *no_elements =  pmca->nord;
//! if (*no_elements == 0) *no_elements = 1;
//! ```
//!
//! `cvt_dbaddr` sets `no_elements` outside the `fieldIndex` branch that picks
//! `bptr` vs `pbg`, so `NMAX` governs `VAL` and `BG` alike — and those two are
//! exactly the `special(SPC_DBADDR)` fields that reach `cvt_dbaddr` at all.
//!
//! `mca` implemented neither hook's capacity half, so the channel was sized
//! from the served count. `ca_element_count` is settled once at create-channel
//! time, so a client that connected before the first acquisition fixed its
//! buffer at the floored single channel and never saw the spectrum widen.

use std::time::Duration;

use epics_base_rs::server::record::FieldDeclaration;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use mca_rs::McaRecord;

fn mca_with_nmax(nmax: i32) -> McaRecord {
    McaRecord {
        nmax,
        ..Default::default()
    }
}

/// The capacity hook itself: `NMAX` for the two `SPC_DBADDR` fields, `None`
/// everywhere else so those channels keep their value's own count.
#[test]
fn only_val_and_bg_advertise_nmax_as_their_native_count() {
    let rec = mca_with_nmax(2048);

    assert_eq!(rec.field_native_count("VAL"), Some(2048));
    assert_eq!(rec.field_native_count("BG"), Some(2048));

    for field in ["NORD", "NMAX", "NUSE", "VERS", "DWEL", "ACQG"] {
        assert_eq!(
            rec.field_native_count(field),
            None,
            "{field} is not special(SPC_DBADDR), so it has no separate capacity"
        );
    }
}

/// The observable: connect before any acquisition, when `NORD` is 0 and the
/// record therefore serves the single floored channel, and the channel must
/// still advertise all 8.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_connecting_before_acquisition_sees_the_full_nmax_capacity() {
    let server = CaServer::builder()
        .port(0)
        .record("MCA:CAP:TEST", mca_with_nmax(8))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel("MCA:CAP:TEST");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to the mca VAL channel");

    assert_eq!(
        ch.element_count().expect("native element count"),
        8,
        "the channel must advertise NMAX (C cvt_dbaddr), not the served count"
    );
}
