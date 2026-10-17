//! A group interface naming an unknown `@iface` derives pvxs's empty
//! `SockAddr` (`IfaceMap::address_of`, `evhelper.cpp:889-896`), not the
//! wildcard, so the auto beacons find no address to expand. pvxs 1.5.3:
//!
//! ```text
//! $ EPICS_PVAS_INTF_ADDR_LIST=224.0.2.3@nosuch EPICS_PVAS_AUTO_BEACON_ADDR_LIST=YES pvxinfo -D
//!     EPICS_PVAS_BEACON_ADDR_LIST=
//!     EPICS_PVAS_INTF_ADDR_LIST=224.0.2.3:5075,-1@nosuch <>
//! ```

// Host-only: drives the built CLI binary out of process, and that is the
// tokio-backend build.
#![cfg(tokio_backend)]

#[test]
fn pvinfo_d_shows_the_empty_address_and_no_beacons() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pvinfo-rs"))
        .arg("-D")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "224.0.2.3@nosuch")
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "YES")
        .env_remove("EPICS_PVAS_BEACON_ADDR_LIST")
        .env_remove("EPICS_PVA_ADDR_LIST")
        .env_remove("EPICS_PVAS_SERVER_PORT")
        .env_remove("EPICS_PVA_SERVER_PORT")
        .output()
        .expect("run pvinfo-rs");
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.contains("EPICS_PVAS_BEACON_ADDR_LIST") || l.contains("EPICS_PVAS_INTF_ADDR_LIST")
        })
        .collect();
    assert_eq!(
        lines,
        [
            "    EPICS_PVAS_BEACON_ADDR_LIST=",
            "    EPICS_PVAS_INTF_ADDR_LIST=224.0.2.3:5075,-1@nosuch <>",
        ],
        "{text}"
    );
}
