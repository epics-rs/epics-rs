//! Every INP/OUT link in `scopeSimulator.db` names a driver parameter.

mod common;

use std::sync::Arc;

use asyn_rs::runtime::sync::Notify;
use common::records;
use scope_ioc::driver::ScopeSimulator;

#[test]
fn every_link_names_a_driver_parameter() {
    let driver = ScopeSimulator::new("scopeDbParams", Arc::new(Notify::new()));
    let missing: Vec<_> = records(include_str!("../db/scopeSimulator.db"))
        .into_values()
        .flat_map(|(_, fields)| fields)
        .filter(|(name, _)| name == "INP" || name == "OUT")
        .map(|(_, link)| link.rsplit(')').next().unwrap().to_owned())
        .filter(|drv_info| driver.base.find_param(drv_info).is_none())
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
}
