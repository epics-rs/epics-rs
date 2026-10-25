//! The ip Keithley programs, each driving a [`mock::Keithley`] through its
//! template's devXxStrParm records.

#[path = "../support/mod.rs"]
mod support;

mod harness;
mod mock;

mod keithley2k_dmm_mf40;
mod keithley65xx_em;
