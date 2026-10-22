//! What snc-rs must emit for std's `delayDo.st` (`golden/delayDo.rs`),
//! included into a companion module the way `include_program!` includes
//! a build's output: it compiles against the contract, and its tables are
//! the ones snc R2-2-9 generates. The golden was transcribed from snc
//! R2-2-9's C output and must match snc-rs's output byte for byte.

mod delay_do {
    use epics_seq_rs::c::*;

    include!("golden/delayDo.rs");

    pub use self::seqg::seqg_vars;
}

use delay_do::{delayDo, seqg_vars};
use epics_seq_rs::*;

/// What an IOC calls to register the program, as C's dbd `registrar`.
const _: fn() = delay_do::delayDoRegistrar;

#[test]
fn the_golden_table_validates() {
    assert_eq!(delayDo.validate(), Ok(()));
}

/// `seqg_chans` of snc's `delayDo.c`, row for row.
#[test]
fn the_channel_table_is_snc_s() {
    let rows: Vec<_> = delayDo
        .chan
        .iter()
        .map(|c| {
            (
                c.ch_name.unwrap(),
                c.var_name,
                c.var_type,
                c.count,
                c.event_num,
                c.ef_id.0,
                c.monitored,
                c.queue_size,
                c.queue_index,
            )
        })
        .collect();
    use PrimType::*;
    assert_eq!(
        rows,
        [
            ("{P}{R}:debug.VAL", "debug_flag", Int, 1, 4, 0, true, 0, 0),
            ("{P}{R}:enable.VAL", "enable", Short, 1, 5, 1, true, 0, 0),
            ("{P}{R}:standbyCalc", "standby", Int, 1, 6, 2, true, 0, 0),
            ("{P}{R}:activeCalc", "active", Int, 1, 7, 3, true, 0, 0),
            ("{P}{R}:delay", "delayPeriod", Double, 1, 8, 0, true, 0, 0),
            ("{P}{R}:doSeq.PROC", "doSeq", Int, 1, 9, 0, false, 0, 0),
            ("{P}{R}:state", "seqState", String, 1, 10, 0, false, 0, 0),
        ]
    );
}

/// Each accessor reaches the variable its row names, not a neighbour of
/// the same type.
#[test]
fn each_channel_reaches_the_variable_it_names() {
    let v = <seqg_vars as ProgramVars>::zeroed();
    let fields: [(&str, &dyn std::any::Any); 7] = [
        ("debug_flag", &v.debug_flag),
        ("enable", &v.enable),
        ("standby", &v.standby),
        ("active", &v.active),
        ("delayPeriod", &v.delayPeriod),
        ("doSeq", &v.doSeq),
        ("seqState", &v.seqState),
    ];
    for (ch, (name, field)) in delayDo.chan.iter().zip(fields) {
        assert_eq!(ch.var_name, name);
        let cell = (ch.var)(&v).as_any();
        assert!(
            std::ptr::addr_eq(
                cell as *const dyn std::any::Any,
                field as *const dyn std::any::Any
            ),
            "{name}"
        );
    }
}

/// The state table and event masks of snc's `delayDo.c`.
#[test]
fn the_state_table_is_snc_s() {
    let [ss] = delayDo.ss else {
        panic!("one state set")
    };
    assert_eq!(ss.ss_name, "delayDoMain");
    let states: Vec<_> = ss
        .states
        .iter()
        .map(|s| {
            assert!(s.entry_func.is_none() && s.exit_func.is_none());
            assert_eq!(s.options, StateOptions::new(0));
            (s.state_name, s.event_mask)
        })
        .collect();
    assert_eq!(
        states,
        [
            ("init", &[0x0][..]),
            ("disable", &[0x22]),
            ("maybeStandby", &[0xc0]),
            ("idle", &[0xee]),
            ("standby", &[0x66]),
            ("maybeWait", &[0x88]),
            ("active", &[0xee]),
            ("waiting", &[0x1ee]),
            ("action", &[0x0]),
        ]
    );
}

/// `+d +r`, `+c` and `+e` by default: snc's `OPT_CONN | OPT_DEBUG |
/// OPT_NEWEF | OPT_REENT`.
#[test]
fn the_program_table_is_snc_s() {
    assert_eq!(delayDo.magic, SEQ_MAGIC);
    assert_eq!(delayDo.prog_name, "delayDo");
    assert_eq!(delayDo.params, "name=delayDo,P=xxx:,R=delayDo1");
    assert_eq!(delayDo.num_ev_flags, 3);
    assert_eq!(delayDo.num_queues, 0);
    assert_eq!(
        delayDo.options,
        ProgOptions::new(ProgOptions::CONN | ProgOptions::DEBUG | ProgOptions::NEWEF)
    );
    assert!(delayDo.vars.reentrant() && !delayDo.vars.safe());
    assert!(delayDo.entry_func.is_none() && delayDo.exit_func.is_none());
}

/// Escaped C reaches the companion by its exact text: an arm per
/// fragment, at item or statement position.
mod escaped {
    use epics_seq_rs::SeqVar;

    macro_rules! seqg_escaped {
        (r#"#include <math.h>"#) => {};
        ($env:ident, r#"pVar->x = 2*pVar->x;"#) => {
            $env.set($env.get() * 2)
        };
    }

    seqg_escaped!(r#"#include <math.h>"#);

    #[test]
    fn a_fragment_expands_to_the_arm_written_for_its_text() {
        let x = SeqVar::new(3);
        seqg_escaped!(x, r#"pVar->x = 2*pVar->x;"#);
        assert_eq!(x.get(), 6);
    }
}
