//! PVA server components — protocol bridge, server wrapper, protocol runner.
//!
//! [`native_source`] is the database→pvAccess bridge and is target-neutral.
//! The wrapper (`pva_server`) serves through `crate::server_native::runtime`
//! on tokio and `crate::server_native::serving` on the exec backend, so one
//! IOC runner stands up on either. Its iocsh command (`iocsh`) reads a report
//! every serving path publishes.

pub mod iocsh;
pub mod native_source;
pub mod pva_server;

pub use native_source::PvDatabaseSource;
pub use pva_server::{PvaServer, PvaServerBuilder};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocRunConfig;

/// Stand `app` up on pvAccess — [`run_pva_ioc`] with pvxs's
/// `pvxsBaseRegistrar` already run.
///
/// pvxs registers `pvxsr` and binds its server from an `epicsExportRegistrar`
/// (`ioc/iochooks.cpp:461-511`), so the command answers on the first `st.cmd`
/// line and a held port is reported before it. This port has no link-time
/// registrar, so the pairing here is what keeps a head from getting the
/// runner without it; `IocApplication::run(run_pva_ioc)` still reaches the
/// interactive shell's copy and binds at the runner. The exec backend's
/// native server binds with its source, so there it binds at the runner too.
pub async fn run_pva_ioc_app(app: epics_base_rs::server::ioc_app::IocApplication) -> CaResult<()> {
    iocsh::pvxs_base_registrar(app).run(run_pva_ioc).await
}

/// Run an IOC with the pvAccess protocol.
///
/// This is the standard protocol runner for [`IocApplication::run`](epics_base_rs::server::ioc_app::IocApplication::run).
/// It creates a [`PvaServer`] from the provided configuration and
/// starts the PVA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// epics_pva_rs::server::run_pva_ioc_app(
///     IocApplication::new().startup_script("st.cmd"),
/// )
/// .await
/// ```
///
/// [`run_pva_ioc_app`] rather than `.run(run_pva_ioc)`, for the reason given
/// there: this runner is dispatched after the startup script, so `pvxsr`
/// registered from here reaches only the interactive shell.
///
/// The TCP port is pvxs's server port (`EPICS_PVAS_SERVER_PORT`, then
/// `EPICS_PVA_SERVER_PORT`, then 5075, `config.cpp:402-408`), not
/// `IocRunConfig::port`, which is the CA server port.
pub async fn run_pva_ioc(mut config: IocRunConfig) -> CaResult<()> {
    let server = PvaServer::from_parts(
        config.db,
        None,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    )
    .bound_at_registrar(&mut config.servers);
    server
        .run_with_shell(move |shell| {
            for cmd in config.shell_commands {
                shell.register(cmd);
            }
        })
        .await
}
