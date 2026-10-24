//! `NDAttrPlotConfig`'s positional-argument mapping, kept apart from the
//! command that installs it.
//!
//! The mapping is the parity-critical half of that command and it is pure:
//! `&[ArgValue]` in, seven fields out, no port manager, no IOC, no reactor. It
//! lives here rather than in `crate::ioc` because that module is gated on
//! `tokio_backend` — it stands an IOC up on `epics_ca_rs::server::ioc_app`,
//! which the reactor-free backend does not have — and a gate sized for the
//! `IocApplication` type surface took this parsing and its three boundary
//! cases down with it.
//!
//! The predicate here is `not(epics_embedded_target)` instead, which is what
//! [`ArgValue`] itself is gated on: the iocsh registry is absent on RTEMS,
//! not reactor-dependent, so a host `exec_backend` build
//! keeps both it and this.
//!
//! `crate::ioc` is named in a code span rather than linked for the same
//! reason this module exists: it is not there to link to in the very
//! configuration this paragraph is explaining.

use ad_core_rs::plugin::runtime::ArraySource;
use epics_base_rs::server::iocsh::registry::ArgValue;

/// Parsed `NDAttrPlotConfig` arguments.
pub struct AttrPlotArgs {
    pub port_name: String,
    pub n_attributes: usize,
    pub cache_size: usize,
    pub n_data_blocks: usize,
    pub source: ArraySource,
    pub queue_size: i32,
    pub blocking_callbacks: i32,
}

/// Parse `NDAttrPlotConfig` positional args in C order
/// (`NDPluginAttrPlot.cpp:308`): `port, n_attributes, cache_size,
/// n_selected_blocks, in_port, in_addr, queue_size, blockingCallbacks, ...`.
///
/// A present integer is honoured exactly — including an explicit `0`, which is
/// meaningful for `cache_size` (`0` = unlimited per-buffer cache). Fallbacks
/// apply only when an arg is absent; a real st.cmd always passes them, so the
/// fallbacks only affect malformed calls.
pub fn parse_attr_plot_args(args: &[ArgValue]) -> Result<AttrPlotArgs, String> {
    let port_name = match args.first() {
        Some(ArgValue::String(s)) if !s.is_empty() => s.to_str()?.to_owned(),
        _ => return Err("NDAttrPlotConfig: portName required".into()),
    };
    let usize_arg = |i: usize, default: usize| match args.get(i) {
        Some(ArgValue::Int(n)) => (*n).max(0) as usize,
        _ => default,
    };
    let port = match args.get(4) {
        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
        _ => String::new(),
    };
    let addr = match args.get(5) {
        Some(ArgValue::Int(n)) => *n as i32,
        _ => 0,
    };
    Ok(AttrPlotArgs {
        port_name,
        n_attributes: usize_arg(1, 8),
        cache_size: usize_arg(2, 1000),
        n_data_blocks: usize_arg(3, 4),
        source: ArraySource { port, addr },
        // As written, absent 0: the runtime floors it as C does.
        queue_size: match args.get(6) {
            Some(ArgValue::Int(n)) => *n as i32,
            _ => 0,
        },
        blocking_callbacks: match args.get(7) {
            Some(ArgValue::Int(n)) => *n as i32,
            _ => 0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_attr_plot_args_maps_c_positional_order() {
        // C NDAttrPlotConfig(port, n_attributes, cache_size, n_selected_blocks,
        // in_port, in_addr, queue_size, ...) — NDPluginAttrPlot.cpp:308. The
        // distinct order (n_attributes/cache/blocks before in_port, queue at
        // index 6) is the parity-critical mapping this guards.
        let args = vec![
            ArgValue::String("AP1".into()),
            ArgValue::Int(10),               // n_attributes
            ArgValue::Int(500),              // cache_size
            ArgValue::Int(3),                // n_selected_blocks
            ArgValue::String("DET1".into()), // in_port
            ArgValue::Int(0),                // in_addr
            ArgValue::Int(50),               // queue_size
            ArgValue::Int(0),                // blocking_callbacks
        ];
        let p = parse_attr_plot_args(&args).unwrap();
        assert_eq!(p.port_name, "AP1");
        assert_eq!(p.n_attributes, 10);
        assert_eq!(p.cache_size, 500);
        assert_eq!(p.n_data_blocks, 3);
        assert_eq!(p.source.port, "DET1");
        assert_eq!(p.queue_size, 50);
    }

    #[test]
    fn parse_attr_plot_args_requires_port_name() {
        assert!(parse_attr_plot_args(&[]).is_err());
        assert!(parse_attr_plot_args(&[ArgValue::String(Default::default())]).is_err());
        assert!(parse_attr_plot_args(&[ArgValue::Int(1)]).is_err());
    }

    #[test]
    fn parse_attr_plot_args_honours_explicit_zero_and_defaults_absent() {
        // Boundary: an explicit cache_size=0 is meaningful (unlimited) and must be
        // honoured; absent n_attributes/n_data_blocks fall back.
        let args = vec![
            ArgValue::String("AP2".into()),
            ArgValue::Missing, // n_attributes absent
            ArgValue::Int(0),  // cache_size = unlimited (explicit 0, not a fallback)
        ];
        let p = parse_attr_plot_args(&args).unwrap();
        assert_eq!(p.n_attributes, 8, "absent n_attributes -> fallback");
        assert_eq!(
            p.cache_size, 0,
            "explicit 0 cache_size honoured (unlimited)"
        );
        assert_eq!(p.n_data_blocks, 4, "absent n_data_blocks -> fallback");
        assert_eq!(p.source.port, "", "absent in_port -> empty");
        // iocsh hands C an absent int as 0, which the runtime floors to 1.
        assert_eq!(p.queue_size, 0, "absent queue_size is iocsh's 0");
    }
}
