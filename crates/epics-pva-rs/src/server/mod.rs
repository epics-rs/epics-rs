//! PVA server components — protocol bridge, server wrapper, protocol runner.

pub mod iocsh;
pub mod native_source;
pub mod pva_server;

pub use native_source::PvDatabaseSource;
pub use pva_server::{PvaServer, PvaServerBuilder};

use epics_base_rs::error::CaResult;
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
