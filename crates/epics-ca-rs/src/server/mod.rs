//! CA server components — TCP handler, UDP search, beacon, monitor.

pub mod access_token;
pub mod addr_list;
pub mod beacon;
pub mod blocking;
pub mod ca_server;
pub mod introspection;
pub mod ioc_app;
pub mod iocsh;
pub mod monitor;
pub mod outbox;
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

/// Convert a long-string *record* field's snapshot value from
/// `EpicsValue::CharArray` to a scalar `EpicsValue::String`. C
/// `cvt_dbaddr` presents lsi/lso VAL & OVAL and printf VAL as a scalar
/// `DBF_STRING` with `no_elements = 1` (lsiRecord.c:141-143,
/// lsoRecord.c:183-185, printfRecord.c:411-413); the record stores the
/// value as a NUL-terminable CHAR array (the long-string carrier). This
/// is the inverse of [`apply_long_string`] — the conversion the CA
/// boundary applies for *plain* (non-`$`) access so the channel ships a
/// single `DBR_STRING` element. The buffer is decoded verbatim (no
/// UTF-8 validation, matching pvxs raw-byte storage) up to the first
/// NUL; the DBR_STRING encoder then truncates to `MAX_STRING_SIZE`
/// (= 40), so an over-long value clips on the wire exactly as C does.
/// Non-`CharArray` values pass through unchanged.
pub(super) fn apply_native_long_string(snap: &mut epics_base_rs::server::snapshot::Snapshot) {
    use epics_base_rs::types::{EpicsValue, PvString};
    let v = std::mem::replace(&mut snap.value, EpicsValue::Long(0));
    snap.value = match v {
        EpicsValue::CharArray(bytes) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            EpicsValue::String(PvString::from_bytes(&bytes[..end]))
        }
        other => other,
    };
}

/// How a channel presents a long-string field on the CA wire. `$`-access
/// and plain access to a long-string *record* field are mutually
/// exclusive boundary conversions, so they share one mode rather than two
/// booleans — the illegal "both at once" state cannot be constructed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LongStringMode {
    /// Ordinary field: deliver the value verbatim.
    Plain,
    /// Client appended `$`: a `DBF_STRING` field is delivered as a
    /// `DBR_CHAR` array of `MAX_STRING_SIZE` (= 40), per the C
    /// `dbChannel.c` long-string convention. See [`apply_long_string`].
    DollarChar,
    /// Plain access to a long-string *record* field (lsi/lso VAL & OVAL,
    /// printf VAL): C `cvt_dbaddr` presents it as a scalar `DBF_STRING`,
    /// so the CHAR-array carrier is decoded to a scalar string before
    /// encoding. See [`apply_native_long_string`].
    NativeString,
}

/// Apply the boundary conversion selected by `mode` to a delivery
/// snapshot before DBR encoding.
pub(super) fn apply_long_string_mode(
    snap: &mut epics_base_rs::server::snapshot::Snapshot,
    mode: LongStringMode,
) {
    match mode {
        LongStringMode::DollarChar => apply_long_string(snap),
        LongStringMode::NativeString => apply_native_long_string(snap),
        LongStringMode::Plain => {}
    }
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
    )
    .await?;
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
