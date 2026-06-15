//! Runnable regression IOC.
//!
//! Boots the regression record set under live CA + PVA servers and stays up so
//! it can be poked by hand (`caget`/`caput`/`pvget`/`pvmonitor`). The e2e tests
//! boot the same records in-process via the `regression_ioc` library harness;
//! this binary exists so the identical IOC can also be run interactively.

use regression_ioc::RegressionIoc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ioc = RegressionIoc::boot().await?;
    println!("regression IOC up:");
    println!("  CA  : 127.0.0.1:{}", ioc.ca_port);
    println!("  PVA : {}", ioc.pva_addr);
    println!("records: see db/regression.db (REG:A:* .. REG:H:*)");
    println!("Ctrl-C to stop.");
    tokio::signal::ctrl_c().await?;
    println!("shutting down.");
    Ok(())
}
