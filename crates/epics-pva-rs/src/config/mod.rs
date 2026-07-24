//! pvAccess environment-variable configuration.
//!
//! Mirrors pvxs's `Config::fromEnv()` for the standard variables:
//!
//! Client-side (`EPICS_PVA_*`):
//!
//! - `EPICS_PVA_ADDR_LIST`        — comma/whitespace-separated unicast
//!   targets for SEARCH
//! - `EPICS_PVA_AUTO_ADDR_LIST`   — `YES`/`NO` (default `YES`); broadcast
//!   to per-NIC limited broadcast addresses
//! - `EPICS_PVA_INTF_ADDR_LIST`   — interfaces to bind to (default: all)
//! - `EPICS_PVA_BROADCAST_PORT`   — UDP port (default 5076)
//! - `EPICS_PVA_SERVER_PORT`      — server's TCP port (default 5075)
//! - `EPICS_PVA_NAME_SERVERS`     — TCP-based name servers (host:port list)
//! - `EPICS_PVA_CONN_TMO`         — connection idle timeout in seconds
//!   (default 30)
//!
//! Server-side (`EPICS_PVAS_*` falling back to `EPICS_PVA_*`):
//!
//! - `EPICS_PVAS_INTF_ADDR_LIST`         — interfaces to bind UDP responder
//!   on (default: all)
//! - `EPICS_PVAS_BEACON_ADDR_LIST`       — addresses to send beacons to
//! - `EPICS_PVAS_AUTO_BEACON_ADDR_LIST`  — `YES`/`NO` (default `YES`);
//!   auto-discover per-NIC broadcasts
//! - `EPICS_PVAS_BEACON_PERIOD`          — seconds between beacons
//!   (default 15)
//! - `EPICS_PVAS_BROADCAST_PORT`         — UDP port (default 5076)
//! - `EPICS_PVAS_SERVER_PORT`            — TCP port (default 5075)

pub mod env;

pub use env::{
    Endpoint, PvaConfigDefs, auto_addr_list_enabled, auto_beacon_addr_list_enabled, beacon_period,
    broadcast_port, client_tls_keychain_password, conn_timeout_secs, expand_dollar_vars,
    list_intf_addresses, name_servers, parse_addr_list, parse_addr_list_with_port,
    parse_endpoints_with_port, seed_env_overrides, server_addr_list, server_beacon_addr_list,
    server_broadcast_port, server_intf_addr_list, server_port, server_tls_keychain_password,
};

// Both need live interface enumeration (`if-addrs`), which newlib cannot
// build — see [`env::Config`] for what closing that for real requires.
#[cfg(not(target_os = "rtems"))]
pub use env::{Config, list_broadcast_addresses};
