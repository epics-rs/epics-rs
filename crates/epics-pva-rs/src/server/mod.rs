//! PVA server components — protocol bridge, server wrapper, protocol runner.
//!
//! [`native_source`] is the database→pvAccess bridge and is target-neutral.
//! The wrapper (`pva_server`), its iocsh command (`iocsh`) and `run_pva_ioc`
//! all drive `crate::server_native::runtime`, which is reactor-only — see
//! that module for why a build without one (RTEMS, VxWorks, or a host
//! `exec_backend` build) stops at the protocol layer.

#[cfg(tokio_backend)]
pub mod iocsh;
pub mod native_source;
#[cfg(tokio_backend)]
pub mod pva_server;

pub use native_source::PvDatabaseSource;
#[cfg(tokio_backend)]
pub use pva_server::{PvaServer, PvaServerBuilder};

#[cfg(tokio_backend)]
use epics_base_rs::error::CaResult;
#[cfg(tokio_backend)]
use epics_base_rs::server::ioc_app::IocRunConfig;

/// Stand `app` up on pvAccess — [`run_pva_ioc`] with pvxs's
/// `pvxsBaseRegistrar` already run.
///
/// pvxs registers `pvxsr` from an `epicsExportRegistrar`
/// (`ioc/iochooks.cpp:461-476`), so it answers on the first `st.cmd` line.
/// This port has no link-time registrar, so the pairing here is what keeps a
/// head from getting the runner without it; `IocApplication::run(run_pva_ioc)`
/// still reaches the interactive shell's copy.
#[cfg(tokio_backend)]
pub async fn run_pva_ioc_app(app: epics_base_rs::server::ioc_app::IocApplication) -> CaResult<()> {
    iocsh::register_pvxs_commands(app).run(run_pva_ioc).await
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
#[cfg(tokio_backend)]
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
