//! CA server components — TCP handler, UDP search, beacon, monitor.

pub mod access_token;
pub mod addr_list;
pub mod beacon;
pub mod ca_server;
pub mod introspection;
pub mod ioc_app;
pub mod iocsh;
pub mod monitor;
pub mod rate_limit;
#[cfg(feature = "cap-tokens")]
pub mod signed_beacon;
pub mod tcp;
pub mod udp;

pub use ca_server::{AccessRightsNotifier, CaServer, CaServerBuilder, ServerStats};
pub use tcp::ServerConnectionEvent;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocRunConfig;

/// Convert a `$`-channel snapshot value from `EpicsValue::String` to
/// `EpicsValue::CharArray` of exactly `MAX_STRING_SIZE` (= 40) elements,
/// matching C `dbChannel.c:489` which sets `no_elements = field_size` (= 40)
/// and `dbr_field_type = DBR_CHAR`.  The string bytes are written first,
/// followed by a NUL terminator, and the remainder zero-padded to 40.
/// `DBF_STRING` guarantees `strlen <= 39`, so the string always fits.
/// Non-string values pass through unchanged.
pub(super) fn apply_long_string(snap: &mut epics_base_rs::server::snapshot::Snapshot) {
    use epics_base_rs::types::EpicsValue;
    const MAX_STRING_SIZE: usize = 40;
    let v = std::mem::replace(&mut snap.value, EpicsValue::Long(0));
    snap.value = match v {
        EpicsValue::String(s) => {
            let mut b = s.into_bytes();
            b.push(0); // NUL terminator
            b.resize(MAX_STRING_SIZE, 0); // zero-pad to field_size
            EpicsValue::CharArray(b)
        }
        other => other,
    };
}

/// Run an IOC with the Channel Access protocol.
///
/// This is the standard protocol runner for [`IocApplication::run`].
/// It creates a [`CaServer`] from the provided configuration and
/// starts the CA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// IocApplication::new()
///     .startup_script("st.cmd")
///     .run(epics_ca_rs::server::run_ca_ioc)
///     .await
/// ```
pub async fn run_ca_ioc(config: IocRunConfig) -> CaResult<()> {
    let mut server = CaServer::from_parts(
        config.db,
        config.port,
        config.tcp_port,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    );
    server.set_after_init_hooks(config.after_init_hooks);
    let casr = iocsh::casr_command(server.stats());
    server
        .run_with_shell(move |shell| {
            shell.register(casr);
            for cmd in config.shell_commands {
                shell.register(cmd);
            }
        })
        .await
}
