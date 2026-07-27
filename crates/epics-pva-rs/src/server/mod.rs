//! PVA server components — protocol bridge, server wrapper, protocol runner.
//!
//! [`native_source`] is the database→pvAccess bridge and is target-neutral.
//! The wrapper (`pva_server`), its iocsh command (`iocsh`) and
//! `run_pva_ioc` all drive `crate::server_native::runtime`, which is
//! host-only — see that module for why an `epics_embedded_target` build
//! (RTEMS or VxWorks) stops at the protocol layer.

#[cfg(not(epics_embedded_target))]
pub mod iocsh;
pub mod native_source;
#[cfg(not(epics_embedded_target))]
pub mod pva_server;

pub use native_source::PvDatabaseSource;
#[cfg(not(epics_embedded_target))]
pub use pva_server::{PvaServer, PvaServerBuilder};

#[cfg(not(epics_embedded_target))]
use epics_base_rs::error::CaResult;
#[cfg(not(epics_embedded_target))]
use epics_base_rs::server::ioc_app::IocRunConfig;

/// Run an IOC with the pvAccess protocol.
///
/// This is the standard protocol runner for [`IocApplication::run`](epics_base_rs::server::ioc_app::IocApplication::run).
/// It creates a [`PvaServer`] from the provided configuration and
/// starts the PVA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// IocApplication::new()
///     .startup_script("st.cmd")
///     .run(epics_pva_rs::server::run_pva_ioc)
///     .await
/// ```
#[cfg(not(epics_embedded_target))]
pub async fn run_pva_ioc(config: IocRunConfig) -> CaResult<()> {
    let server = PvaServer::from_parts(
        config.db,
        config.port,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    );
    server
        .run_with_shell(move |shell| {
            for cmd in config.shell_commands {
                shell.register(cmd);
            }
        })
        .await
}
