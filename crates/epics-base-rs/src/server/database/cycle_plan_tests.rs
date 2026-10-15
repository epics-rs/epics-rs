//! Differential harness for [`CyclePlan`](crate::server::record::record_instance::CyclePlan):
//! a scripted scenario runs once with every record forced to the General
//! arm and once under whatever plan the record resolves, and both runs must
//! observe the same record values, alarms, posts, forward-link and CP
//! consequences. A plain arm is correct exactly when no scenario tells them
//! apart.

use std::collections::HashMap;

use crate::server::ioc_builder::IocBuilder;
use crate::server::recgbl::EventMask;
use crate::server::record::record_instance::CyclePlan;
use crate::types::{DbFieldType, EpicsValue};

use crate::server::record::FieldDeclaration;

use super::PvDatabase;

/// One step: puts applied first, then the record `T` is processed once.
type Step = &'static [(&'static str, &'static str)];

struct Scenario {
    name: &'static str,
    db: &'static str,
    steps: &'static [Step],
    /// Device support attached to `T` before the first cycle.
    device: bool,
    /// Wait after each cycle for continuations the cycle scheduled.
    settle_ms: u64,
    /// The arm each step's cycle ran, `P` for the plain arm and `G` for
    /// General — pinned, so a scenario that stops selecting its arm fails.
    arms: &'static str,
}

/// A device-backed `ai`: every read bumps RVAL.
struct CountDev(i32);

impl crate::server::device_support::DeviceSupport for CountDev {
    fn read(
        &mut self,
        record: &mut dyn crate::server::record::Record,
    ) -> crate::error::CaResult<crate::server::device_support::DeviceReadOutcome> {
        self.0 += 1;
        record.put_field("RVAL", EpicsValue::Long(self.0))?;
        Ok(crate::server::device_support::DeviceReadOutcome::ok())
    }
    fn write(&mut self, _: &mut dyn crate::server::record::Record) -> crate::error::CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "CountDev"
    }
}

const MASK: u16 = EventMask::VALUE.bits()
    | EventMask::LOG.bits()
    | EventMask::ALARM.bits()
    | EventMask::PROPERTY.bits();

/// The records every scenario shares: a forward-link target `F` and a CP
/// consumer `C` of `T`.
macro_rules! with_fixtures {
    ($t:literal) => {
        concat!(
            $t,
            r#"
record(calc, "F") { field(CALC, "VAL+1") }
record(calc, "C") { field(INPA, "T CP") field(INPB, "C") field(CALC, "B+1") }
"#
        )
    };
}

const BASE: Scenario = Scenario {
    name: "",
    db: "",
    steps: &[],
    device: false,
    settle_ms: 0,
    arms: "",
};

const SCENARIOS: &[Scenario] = &[
    Scenario {
        // An output's `writeValue` (`aoRecord.c:560-590`): SIMM YES/RAW
        // writes SIOL instead of OUT, NO writes OUT.
        name: "ao SIMM toggled across cycles",
        arms: "GGGGGGGG",
        db: with_fixtures!(
            r#"record(ai, "SS") { field(VAL, "42") }
               record(ao, "T") { field(DTYP, "Soft Channel") field(OUT, "F.B") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T", "3")],
            &[("T.SIMM", "1")],
            &[("T", "4")],
            &[("T.SIMM", "0")],
            &[("T", "5")],
            &[("T.SIMM", "2")],
            &[("T", "6")],
        ],
        ..BASE
    },
    Scenario {
        name: "const INP, alarm limits, runtime INP put",
        arms: "PPPPPP",
        db: with_fixtures!(
            r#"record(ai, "T") { field(INP, "3.5") field(DTYP, "Soft Channel") field(FLNK, "F")
                field(HIHI, "5") field(HHSV, "MAJOR") field(HIGH, "4") field(HSV, "MINOR")
                field(MDEL, "0.1") field(ADEL, "0.1") }"#
        ),
        steps: &[
            &[],
            &[],
            &[("T.INP", "4.5")],
            &[("T.INP", "6")],
            &[("T.INP", "6")],
            &[("T.INP", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "DB link INP, source changes between cycles",
        arms: "DDDDP",
        db: with_fixtures!(
            r#"record(ai, "S") { field(VAL, "1") }
               record(ai, "T") { field(INP, "S") field(DTYP, "Soft Channel") field(FLNK, "F")
                field(HIHI, "5") field(HHSV, "MAJOR") field(MDEL, "0.5") }"#
        ),
        steps: &[
            &[],
            &[("S", "2")],
            &[("S", "2.1")],
            &[("S", "9")],
            &[("T.INP", "3")],
        ],
        ..BASE
    },
    Scenario {
        name: "SIMM enabled and disabled across cycles",
        arms: "PGGPGG",
        db: with_fixtures!(
            r#"record(ai, "SS") { field(VAL, "42") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T.SIMM", "1")],
            &[("SS", "43")],
            &[("T.SIMM", "0")],
            &[("T.SIMM", "2")],
            &[],
        ],
        ..BASE
    },
    Scenario {
        name: "SIML and SDIS link text edited at runtime",
        arms: "PGGPGGP",
        db: with_fixtures!(
            r#"record(ai, "SM") { field(VAL, "1") }
               record(ai, "SS") { field(VAL, "42") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T.SIML", "SM")],
            &[("SM", "0")],
            &[("T.SIML", "")],
            &[("T.SDIS", "SM")],
            &[("SM", "1")],
            &[("T.SDIS", "")],
        ],
        ..BASE
    },
    Scenario {
        name: "SDIS disable by link value and by DISA",
        arms: "GGGGGG",
        db: with_fixtures!(
            r#"record(ai, "DS") { field(VAL, "0") }
               record(ai, "T") { field(INP, "2") field(DTYP, "Soft Channel") field(SDIS, "DS")
                field(DISV, "1") field(DISS, "MAJOR") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("DS", "1")],
            &[],
            &[("DS", "0")],
            &[("T.DISA", "1")],
            &[("T.DISA", "0")],
        ],
        ..BASE
    },
    Scenario {
        name: "TSEL source time and TSE changes",
        arms: "PPPP",
        db: with_fixtures!(
            r#"record(ai, "TS") { field(VAL, "1") }
               record(ai, "T") { field(INP, "2") field(DTYP, "Soft Channel") field(TSEL, "TS.TIME")
                field(TSE, "-2") field(FLNK, "F") }"#
        ),
        steps: &[&[], &[("TS", "2")], &[("T.TSE", "0")], &[("T.TSE", "-2")]],
        ..BASE
    },
    Scenario {
        // The dset is bound at init (`iocInit.c:529`): the DTYP puts change
        // the name only, so every cycle runs the Raw Soft dset.
        name: "DB link INP, Raw Soft Channel at load, DTYP put at runtime",
        arms: "GGGGGGG",
        db: with_fixtures!(
            r#"record(ai, "S") { field(VAL, "1.5") }
               record(ai, "T") { field(INP, "S NPP") field(DTYP, "Raw Soft Channel") field(FLNK, "F")
                field(ASLO, "2") field(AOFF, "1") }"#
        ),
        steps: &[
            &[],
            &[("S", "2.7")],
            &[("T.DTYP", "Soft Channel")],
            &[("S", "3.7")],
            &[],
            &[("T.DTYP", "Raw Soft Channel")],
            &[("S", "4.7")],
        ],
        ..BASE
    },
    Scenario {
        name: "DTYP and FLNK edited at runtime",
        arms: "PPPPPPPPP",
        db: with_fixtures!(
            r#"record(ai, "T") { field(INP, "2") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T.FLNK", "")],
            &[],
            &[("T.FLNK", "F")],
            &[("T.DTYP", "Raw Soft Channel")],
            &[],
            &[("T.DTYP", "Async Soft Channel")],
            &[],
            &[("T.DTYP", "Soft Channel")],
        ],
        ..BASE
    },
    Scenario {
        name: "constant SDIS, DISA put by a client",
        arms: "PPPPP",
        db: with_fixtures!(
            r#"record(ai, "T") { field(INP, "5") field(SDIS, "1") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(DISS, "MAJOR") }"#
        ),
        steps: &[&[], &[("T.DISA", "1")], &[], &[("T.DISA", "0")], &[]],
        ..BASE
    },
    Scenario {
        name: "SSCN set, SIMM toggled",
        arms: "PGGGGPP",
        db: with_fixtures!(
            r#"record(ai, "SS") { field(VAL, "42") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T.SSCN", "2")],
            &[("T.SIMM", "1")],
            &[("SS", "43")],
            &[("T.SIMM", "0")],
            &[("T.SSCN", "65535")],
            &[],
        ],
        ..BASE
    },
    Scenario {
        name: "device-backed DTYP",
        arms: "GGGGG",
        db: with_fixtures!(r#"record(ai, "T") { field(FLNK, "F") field(MDEL, "0.5") }"#),
        steps: &[&[], &[], &[("T.SIMM", "1")], &[("T.SIMM", "0")], &[]],
        device: true,
        ..BASE
    },
    Scenario {
        name: "SDLY continuation",
        arms: "PGGPP",
        db: with_fixtures!(
            r#"record(ai, "SS") { field(VAL, "42") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(SDLY, "0.01") field(FLNK, "F") }"#
        ),
        steps: &[&[], &[("T.SIMM", "1")], &[], &[("T.SIMM", "0")], &[]],
        settle_ms: 80,
        ..BASE
    },
    Scenario {
        name: "TPRO set",
        arms: "PPP",
        db: with_fixtures!(
            r#"record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(TPRO, "1")
                field(FLNK, "F") }"#
        ),
        steps: &[&[], &[("T.INP", "8")], &[]],
        ..BASE
    },
    Scenario {
        name: "OUT link writes T.SIML",
        arms: "PPPPP",
        db: with_fixtures!(
            r#"record(ai, "SM") { field(VAL, "1") }
               record(ai, "SS") { field(VAL, "42") }
               record(stringout, "W") { field(OUT, "T.SIML PP") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("W.VAL", "SM"), ("W.PROC", "1")],
            &[("SM", "0")],
            &[("W.VAL", ""), ("W.PROC", "1")],
            &[],
        ],
        ..BASE
    },
    Scenario {
        name: "SIML stored by post_fields and post_property",
        arms: "PGGGG",
        db: with_fixtures!(
            r#"record(ai, "SM") { field(VAL, "1") }
               record(ai, "SS") { field(VAL, "42") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T.SIML@fields", "SM")],
            &[("T.SIML@fields", "")],
            &[("T.SIML@prop", "SM")],
            &[("T.SIML@prop", "")],
        ],
        ..BASE
    },
    Scenario {
        name: "SIOL text and INP const-to-link at runtime",
        arms: "PDGGDPP",
        db: with_fixtures!(
            r#"record(ai, "SS") { field(VAL, "42") }
               record(ai, "S2") { field(VAL, "43") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("T.INP", "SS")],
            &[("T.SIMM", "1")],
            &[("T.SIOL", "S2")],
            &[("T.SIMM", "0")],
            &[("T.INP", "9")],
            &[],
        ],
        ..BASE
    },
    Scenario {
        name: "DB link INP variants",
        // `NoSuch` names no record here: `dbAddLink` makes it CA.
        arms: "DDDGGDGDD",
        db: with_fixtures!(
            r#"record(ai, "S") { field(VAL, "1") field(HIHI, "5") field(HHSV, "MAJOR") }
               record(ai, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("S", "9")],
            &[("S", "2")],
            &[("T.INP", "S PP")],
            &[("S", "3")],
            &[("T.INP", "T")],
            &[("T.INP", "NoSuch")],
            &[("T.INP", "S.HIHI NPP")],
            &[("T.INP", "S")],
        ],
        ..BASE
    },
    Scenario {
        name: "D arm, link severity MSS, source MAJOR then INVALID then clear",
        arms: "DDDDDDD",
        db: with_fixtures!(
            r#"record(ai, "S") { field(INP, "1") field(DTYP, "Soft Channel") field(HIHI, "5") field(HHSV, "MAJOR") field(HSV, "MINOR") field(HIGH, "3") }
               record(ai, "T") { field(INP, "S MSS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("S.PROC", "1")],
            &[("S.INP", "9"), ("S.PROC", "1")],
            &[("S.INP", "4"), ("S.PROC", "1")],
            &[("S.INP", "NoSuch"), ("S.PROC", "1")],
            &[("S.INP", "2"), ("S.PROC", "1")],
            &[("S.INP", "1"), ("S.PROC", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "D arm, link severity MSI, source MAJOR then INVALID then clear",
        arms: "DDDDDDD",
        db: with_fixtures!(
            r#"record(ai, "S") { field(INP, "1") field(DTYP, "Soft Channel") field(HIHI, "5") field(HHSV, "MAJOR") field(HSV, "MINOR") field(HIGH, "3") }
               record(ai, "T") { field(INP, "S MSI") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("S.PROC", "1")],
            &[("S.INP", "9"), ("S.PROC", "1")],
            &[("S.INP", "4"), ("S.PROC", "1")],
            &[("S.INP", "NoSuch"), ("S.PROC", "1")],
            &[("S.INP", "2"), ("S.PROC", "1")],
            &[("S.INP", "1"), ("S.PROC", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "D arm, link severity NMS, source MAJOR then INVALID then clear",
        arms: "DDDDDDD",
        db: with_fixtures!(
            r#"record(ai, "S") { field(INP, "1") field(DTYP, "Soft Channel") field(HIHI, "5") field(HHSV, "MAJOR") field(HSV, "MINOR") field(HIGH, "3") }
               record(ai, "T") { field(INP, "S NMS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("S.PROC", "1")],
            &[("S.INP", "9"), ("S.PROC", "1")],
            &[("S.INP", "4"), ("S.PROC", "1")],
            &[("S.INP", "NoSuch"), ("S.PROC", "1")],
            &[("S.INP", "2"), ("S.PROC", "1")],
            &[("S.INP", "1"), ("S.PROC", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "D arm, MS with source HIHI and the target's own HIHI",
        arms: "DDDDDDD",
        db: with_fixtures!(
            r#"record(ai, "S") { field(INP, "1") field(DTYP, "Soft Channel") field(HIHI, "5") field(HHSV, "MAJOR") field(HSV, "MINOR") field(HIGH, "3") }
               record(ai, "T") { field(INP, "S MS") field(DTYP, "Soft Channel") field(FLNK, "F") field(HIHI, "5") field(HHSV, "MINOR") field(HIGH, "2") field(HSV, "MINOR") }"#
        ),
        steps: &[
            &[],
            &[("S.PROC", "1")],
            &[("S.INP", "9"), ("S.PROC", "1")],
            &[("S.INP", "4"), ("S.PROC", "1")],
            &[("S.INP", "NoSuch"), ("S.PROC", "1")],
            &[("S.INP", "2"), ("S.PROC", "1")],
            &[("S.INP", "1"), ("S.PROC", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "D arm, AMSG passes through MS and clears on the next good cycle",
        arms: "DDDDD",
        db: with_fixtures!(
            r#"record(ai, "S") { field(INP, "1") field(DTYP, "Soft Channel") field(HIHI, "5") field(HHSV, "MAJOR") field(HSV, "MINOR") field(HIGH, "3") }
               record(ai, "T") { field(INP, "S MS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("S.INP", "NoSuch"), ("S.PROC", "1")],
            &[],
            &[("S.INP", "2"), ("S.PROC", "1")],
            &[],
        ],
        ..BASE
    },
    Scenario {
        name: "D arm, failed conversion from a string source",
        arms: "DDDDD",
        db: with_fixtures!(
            r#"record(stringin, "SI") { field(VAL, "3.5") }
               record(ai, "T") { field(INP, "SI MS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[&[], &[("SI.VAL", "word")], &[], &[("SI.VAL", "7")], &[]],
        ..BASE
    },
    Scenario {
        name: "longin INP from an ai, DTYP switched to Raw Soft Channel and back",
        arms: "GGGGGGGGG",
        db: with_fixtures!(
            r#"record(ai, "S") { field(VAL, "1.5") }
               record(longin, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(HIHI, "3") field(HHSV, "MAJOR") field(MDEL, "1") }"#
        ),
        steps: &[
            &[],
            &[("S", "2.7")],
            &[("S", "-3e9")],
            &[("S", "3e9")],
            &[("T.DTYP", "Raw Soft Channel")],
            &[("S", "3.7")],
            &[],
            &[("T.DTYP", "Soft Channel")],
            &[("S", "4.7")],
        ],
        ..BASE
    },
    Scenario {
        name: "longin INP from a calc VAL slot, values past i32",
        arms: "GGGGGG",
        db: with_fixtures!(
            r#"record(calc, "S") { field(VAL, "1.5") }
               record(longin, "T") { field(INP, "S NPP") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("S", "2.7")],
            &[("S", "-2.7")],
            &[("S", "-3e9")],
            &[("S", "3e9")],
            &[("S", "1e300")],
        ],
        ..BASE
    },
    Scenario {
        name: "longin INP from a string source, failed conversion",
        arms: "GGGGG",
        db: with_fixtures!(
            r#"record(stringin, "SI") { field(VAL, "12") }
               record(longin, "T") { field(INP, "SI MS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[&[], &[("SI.VAL", "word")], &[], &[("SI.VAL", "7")], &[]],
        ..BASE
    },
    Scenario {
        name: "bi INP from a calc VAL slot, DTYP switched to Raw and Async and back",
        arms: "GGGGGGGGGGGG",
        db: with_fixtures!(
            r#"record(calc, "S") { field(VAL, "1") }
               record(bi, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(OSV, "MAJOR") field(MASK, "6") }"#
        ),
        steps: &[
            &[],
            &[("S", "0")],
            &[("S", "2.7")],
            &[("S", "-1")],
            &[("S", "70000")],
            &[("T.DTYP", "Raw Soft Channel")],
            &[("S", "7")],
            &[("T.DTYP", "Async Soft Channel")],
            &[("S", "1")],
            &[("T.DTYP", "Soft Channel")],
            &[("S", "1e300")],
            &[("S", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "mbbi INP from a calc VAL slot, DTYP switched to Raw and Async and back",
        arms: "GGGGGGGGGGG",
        db: with_fixtures!(
            r#"record(calc, "S") { field(VAL, "1") }
               record(mbbi, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(ONST, "one") field(TWST, "two") field(TWSV, "MINOR")
                field(ONVL, "1") field(TWVL, "2") field(UNSV, "MAJOR") }"#
        ),
        steps: &[
            &[],
            &[("S", "2.7")],
            &[("S", "-1")],
            &[("S", "70000")],
            &[("T.DTYP", "Raw Soft Channel")],
            &[("S", "2")],
            &[("T.DTYP", "Async Soft Channel")],
            &[("S", "1")],
            &[("T.DTYP", "Soft Channel")],
            &[("S", "1e300")],
            &[("S", "2")],
        ],
        ..BASE
    },
    Scenario {
        name: "mbbi INP from a string source, failed conversion",
        arms: "GGGGG",
        db: with_fixtures!(
            r#"record(stringin, "SI") { field(VAL, "2") }
               record(mbbi, "T") { field(INP, "SI MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(TWST, "two") field(TWSV, "MINOR") }"#
        ),
        steps: &[&[], &[("SI.VAL", "word")], &[], &[("SI.VAL", "1")], &[]],
        ..BASE
    },
    Scenario {
        name: "mbbiDirect INP from a calc VAL slot, DTYP switched to Raw and Async and back",
        arms: "GGGGGGGGGGGG",
        db: with_fixtures!(
            r#"record(calc, "S") { field(VAL, "5") }
               record(mbbiDirect, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(NOBT, "4") field(SHFT, "1") }"#
        ),
        steps: &[
            &[],
            &[("S", "6.7")],
            &[("S", "-1")],
            &[("S", "3e9")],
            &[("S", "5e9")],
            &[("T.DTYP", "Raw Soft Channel")],
            &[("S", "12")],
            &[("T.DTYP", "Async Soft Channel")],
            &[("S", "9")],
            &[("T.DTYP", "Soft Channel")],
            &[("S", "1e300")],
            &[("S", "3")],
        ],
        ..BASE
    },
    Scenario {
        name: "mbbiDirect INP from a longin and a string source",
        arms: "GGGGGG",
        db: with_fixtures!(
            r#"record(longin, "L") { field(VAL, "-2") }
               record(stringin, "SI") { field(VAL, "12") }
               record(mbbiDirect, "T") { field(INP, "L MS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("L", "7")],
            &[("T.INP", "SI MS")],
            &[("SI.VAL", "word")],
            &[("SI.VAL", "4000000000")],
            &[],
        ],
        ..BASE
    },
    Scenario {
        name: "int64in INP from a calc VAL slot, DTYP switched to Async and back",
        arms: "GGGGGGGGGG",
        db: with_fixtures!(
            r#"record(calc, "S") { field(VAL, "1.5") }
               record(int64in, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(HIHI, "3") field(HHSV, "MAJOR") field(MDEL, "1") }"#
        ),
        steps: &[
            &[],
            &[("S", "2.7")],
            &[("S", "-3e9")],
            &[("S", "1e19")],
            &[("S", "-1e300")],
            &[("T.DTYP", "Async Soft Channel")],
            &[("S", "4.7")],
            &[("T.DTYP", "Soft Channel")],
            &[("S", "5e15")],
            &[("S", "1")],
        ],
        ..BASE
    },
    Scenario {
        name: "int64in INP from a longin and a string source",
        arms: "GGGGGG",
        db: with_fixtures!(
            r#"record(longin, "L") { field(VAL, "-2") }
               record(stringin, "SI") { field(VAL, "12") }
               record(int64in, "T") { field(INP, "L MS") field(DTYP, "Soft Channel") field(FLNK, "F") }"#
        ),
        steps: &[
            &[],
            &[("L", "7")],
            &[("T.INP", "SI MS")],
            &[("SI.VAL", "word")],
            &[("SI.VAL", "9000000000000")],
            &[],
        ],
        ..BASE
    },
    // Their request is not `DBR_DOUBLE`, so the DB-link arm is not theirs.
    Scenario {
        name: "bi DB link INP",
        arms: "GGG",
        db: with_fixtures!(
            r#"record(bo, "S") { field(VAL, "0") }
               record(bi, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(OSV, "MAJOR") }"#
        ),
        steps: &[&[], &[("S", "1")], &[("S", "0")]],
        ..BASE
    },
    Scenario {
        name: "mbbi DB link INP",
        arms: "GGG",
        db: with_fixtures!(
            r#"record(longin, "S") { field(VAL, "1") }
               record(mbbi, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(ONST, "one") field(TWST, "two") field(TWSV, "MINOR") }"#
        ),
        steps: &[&[], &[("S", "2")], &[("S", "1")]],
        ..BASE
    },
    Scenario {
        name: "longin DB link INP",
        arms: "GGGG",
        db: with_fixtures!(
            r#"record(longin, "S") { field(VAL, "1") field(HIHI, "5") field(HHSV, "MAJOR") }
               record(longin, "T") { field(INP, "S NPP MS") field(DTYP, "Soft Channel")
                field(FLNK, "F") field(MDEL, "1") }"#
        ),
        steps: &[&[], &[("S", "9")], &[("S", "2")], &[("T.INP", "4")]],
        ..BASE
    },
];

/// Fields posted to a monitor: the type's own plus the `dbCommon` ones a cycle
/// can touch.
const COMMON_POSTED: &[&str] = &[
    "SEVR", "STAT", "AMSG", "NSEV", "NSTA", "UDF", "TIME", "ACKS", "SIMM", "OLDSIMM", "SSCN",
    "SCAN", "PACT", "LCNT",
];

fn describe(db: &PvDatabase, name: &str) -> String {
    let cell = db.get_record(name).expect("scenario record");
    let inst = cell.read();
    let val = inst.record.get_field("VAL");
    // The stamp value is the clock's, so it is compared by what it can be
    // told from: unset, and equal to the TSEL source's.
    let same_as_ts = db
        .get_record("TS")
        .map(|ts| ts.read().common.time == inst.common.time);
    format!(
        "{name}: val={val:?} stat={} sevr={:?} amsg={:?} nsta={} nsev={:?} udf={} pact={} lcnt={} \
         simm={:?} oldsimm={} sscn={:?} scan={:?} tse={} stamped={} time=ts:{same_as_ts:?}",
        inst.common.stat,
        inst.common.sevr,
        inst.common.amsg,
        inst.common.nsta,
        inst.common.nsev,
        inst.common.udf,
        inst.is_processing(),
        inst.common.lcnt,
        inst.record.get_field("SIMM"),
        inst.common.oldsimm,
        inst.common.sscn,
        inst.common.scan,
        inst.common.tse(),
        inst.common.time != crate::types::WallTime::UNIX_EPOCH,
    )
}

/// Run `sc` and return everything it observed, one line per fact, and which
/// arm each step's cycle ran.
async fn observe(sc: &Scenario, general: bool) -> (Vec<String>, String) {
    let (db, _) = IocBuilder::new()
        .db_string(sc.db, &HashMap::new())
        .expect("scenario db")
        .build()
        .await
        .expect("scenario ioc");
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    let mut readers = Vec::new();
    for name in ["T", "F", "C"] {
        let cell = db.get_record(name).unwrap();
        let mut inst = cell.write();
        if name == "T" {
            inst.plan_forced_general = general;
            if sc.device {
                inst.common.dtyp = "CountDev".into();
                inst.device = Some(Box::new(CountDev(0)));
            }
        }
        let own: Vec<&str> = inst.record.field_list().iter().map(|f| f.name).collect();
        let mut sid = 0u32;
        for field in own.iter().copied().chain(COMMON_POSTED.iter().copied()) {
            sid += 1;
            if let Ok(rx) = inst.add_subscriber(field, sid, DbFieldType::String, MASK) {
                readers.push((name, field, rx));
            }
        }
    }
    let mut last_time = crate::types::WallTime::UNIX_EPOCH;
    let mut out = Vec::new();
    let mut arms = String::new();
    for (i, step) in sc.steps.iter().enumerate() {
        for (pv, v) in *step {
            let field = pv
                .rsplit('.')
                .next()
                .map(|f| f.split('@').next().unwrap_or(f));
            let value = match field {
                Some("INP" | "FLNK" | "DTYP" | "SIML" | "SIOL" | "SDIS") => {
                    EpicsValue::String((*v).into())
                }
                Some("SSCN") => EpicsValue::Enum(v.parse().expect("index")),
                Some("VAL") if pv.starts_with("W.") || pv.starts_with("SI.") => {
                    EpicsValue::String((*v).into())
                }
                _ => EpicsValue::Double(v.parse().expect("number")),
            };
            // A refused put is part of what both runs must agree on. Link
            // fields change only through `dbPutField`'s analogue.
            let put = match pv.split_once('.') {
                // `post_fields` and `post_property` store a field of the
                // record directly, bypassing the put funnels.
                Some((rec, spec)) if spec.ends_with("@fields") => db
                    .post_fields(
                        rec,
                        vec![(
                            spec.trim_end_matches("@fields").to_string(),
                            EpicsValue::String((*v).into()),
                        )],
                    )
                    .map(|_| ()),
                Some((rec, spec)) if spec.ends_with("@prop") => db
                    .post_property(
                        rec,
                        crate::server::device_support::PropertyPost {
                            writes: vec![(
                                spec.trim_end_matches("@prop").to_string(),
                                EpicsValue::String((*v).into()),
                            )],
                            post_field: "VAL".into(),
                        },
                    )
                    .map(|_| ()),
                Some((rec, field @ ("INP" | "FLNK" | "SIML" | "SIOL" | "SDIS"))) => db
                    .put_record_field_from_ca(rec, field, value)
                    .await
                    .map(|_| ()),
                _ => db.put_pv(pv, value).await,
            };
            out.push(format!(
                "step {i} put {pv}={v}: {:?}",
                put.map_err(|e| e.to_string())
            ));
        }
        let mut visited = super::ProcStack::new();
        let r = db.process_record_with_links("T", &mut visited).await;
        out.push(format!(
            "step {i} process: {:?}",
            r.map_err(|e| e.to_string())
        ));
        if sc.settle_ms > 0 {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(sc.settle_ms);
            while std::time::Instant::now() < until {
                tokio::task::yield_now().await;
            }
        }
        arms.push(match db.get_record("T").unwrap().read().ran_arm() {
            CyclePlan::General => 'G',
            CyclePlan::ConstSoftIn => 'P',
            CyclePlan::DbSoftIn => 'D',
        });
        for n in ["T", "F", "C", "SS"] {
            if n == "SS" && db.get_record(n).is_none() {
                continue;
            }
            out.push(format!("step {i} {}", describe(&db, n)));
        }
        // The stamp's value is the clock's, so each run is compared by how it
        // moved: unchanged, or later than the cycle before.
        let time = db.get_record("T").unwrap().read().common.time;
        out.push(format!("step {i} T time moved={}", time != last_time));
        assert!(time >= last_time, "T's stamp went backwards");
        last_time = time;
        for (rec, field, rx) in &mut readers {
            while let Ok(ev) = rx.try_recv() {
                // A wide field's log is read at delivery, as a client reads it.
                let value = ev.value().as_value().cloned().or_else(|| {
                    let cell = db.get_record(&**rec).unwrap();
                    let inst = cell.read();
                    inst.channel_field_value(field, false)
                });
                out.push(format!(
                    "step {i} post {rec}.{field} {:?} mask={:?}",
                    value.unwrap(),
                    ev.mask()
                ));
            }
        }
    }
    (out, arms)
}

/// The harness itself: General against General, so a scenario that is not
/// deterministic fails here and not as a false arm mismatch later.
#[epics_macros_rs::epics_test]
async fn general_against_itself_is_deterministic() {
    for sc in SCENARIOS {
        let a = observe(sc, true).await.0;
        let (b, arms) = observe(sc, true).await;
        assert!(arms.chars().all(|c| c == 'G'), "forced General ran {arms}");
        assert_eq!(
            a, b,
            "scenario `{}` differs between two General runs",
            sc.name
        );
        assert!(
            a.len() > sc.steps.len() * 4,
            "scenario `{}` observed too little",
            sc.name
        );
    }
}

/// Every record's plan against General, scenario by scenario. With no plain
/// arm yet the two runs take the same path; each arm added later is covered
/// by this test without change.
#[epics_macros_rs::epics_test]
async fn the_resolved_plan_is_indistinguishable_from_general() {
    for sc in SCENARIOS {
        let general = observe(sc, true).await.0;
        let (planned, arms) = observe(sc, false).await;
        assert_eq!(arms, sc.arms, "scenario `{}`: arm per step", sc.name);
        assert_eq!(
            general, planned,
            "scenario `{}`: plan differs from General",
            sc.name
        );
    }
}

/// A SIML the record type holds itself (`ai.rs`) reaches the plan stamp: a
/// record on the plain arm that gains a SIML link simulates on its next cycle.
#[epics_macros_rs::epics_test]
async fn a_runtime_siml_put_leaves_the_plain_arm() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai, "SM") { field(VAL, "1") }
               record(ai, "SS") { field(VAL, "42") }
               record(ai, "T") { field(INP, "7") field(DTYP, "Soft Channel") field(SIOL, "SS") }"#,
            &HashMap::new(),
        )
        .expect("db")
        .build()
        .await
        .expect("ioc");
    let mut visited = super::ProcStack::new();
    db.process_record_with_links("T", &mut visited)
        .await
        .unwrap();
    db.put_record_field_from_ca("T", "SIML", EpicsValue::String("SM".into()))
        .await
        .unwrap();
    db.process_record_with_links("T", &mut visited)
        .await
        .unwrap();
    let t = db.get_record("T").unwrap();
    let t = t.read();
    assert_eq!(t.record.get_field("SIMM"), Some(EpicsValue::Short(1)));
    assert_eq!(t.record.get_field("VAL"), Some(EpicsValue::Double(42.0)));
}

/// An ao skips the simulation block only while SIMM is NO: softIoc, caput by
/// caput, routes YES and RAW to SIOL and NO to OUT (values read back with
/// caget from `SS F.B F` after each put).
#[epics_macros_rs::epics_test]
async fn an_ao_writes_siol_only_while_simulating() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai, "SS") { field(VAL, "42") }
               record(calc, "F") { field(CALC, "VAL+1") }
               record(ao, "T") { field(DTYP, "Soft Channel") field(OUT, "F.B") field(SIOL, "SS")
                field(FLNK, "F") }"#,
            &HashMap::new(),
        )
        .expect("db")
        .build()
        .await
        .expect("ioc");
    let value = |name: &str, field: &str| {
        db.get_record(name)
            .unwrap()
            .read()
            .record
            .get_field(field)
            .and_then(|v| v.to_f64())
    };
    for (simm, val, ss, b, f) in [
        (0.0, 3.0, 42.0, 3.0, 1.0),
        (1.0, 4.0, 4.0, 3.0, 2.0),
        (0.0, 5.0, 4.0, 5.0, 3.0),
        (2.0, 6.0, 6.0, 5.0, 4.0),
    ] {
        db.put_pv("T.SIMM", EpicsValue::Double(simm)).await.unwrap();
        db.put_pv("T", EpicsValue::Double(val)).await.unwrap();
        db.process_record_with_links("T", &mut super::ProcStack::new())
            .await
            .unwrap();
        assert_eq!(
            (value("SS", "VAL"), value("F", "B"), value("F", "VAL")),
            (Some(ss), Some(b), Some(f)),
            "SIMM={simm} VAL={val}"
        );
    }
}

/// The D-arm severity scenarios above compare against General, whose soft DB
/// read shares `soft_db_inp_read`; these pin C's `recGblInheritSevrMsg` table
/// itself (`dbDbLink.c:228-232`) on the D arm.
#[epics_macros_rs::epics_test]
async fn the_d_arm_inherits_link_severity_as_c_does() {
    use crate::server::record::AlarmSeverity::{Invalid, Major, NoAlarm};
    // (mode, source MAJOR -> T, source INVALID -> T)
    let table = [
        ("NMS", NoAlarm, NoAlarm),
        ("MS", Major, Invalid),
        ("MSS", Major, Invalid),
        ("MSI", NoAlarm, Invalid),
    ];
    for (mode, at_major, at_invalid) in table {
        let src = format!(
            r#"record(ai, "S") {{ field(VAL, "1") field(DTYP, "Soft Channel") field(HIHI, "5") field(HHSV, "MAJOR") }}
               record(ai, "T") {{ field(INP, "S {mode}") field(DTYP, "Soft Channel") }}"#
        );
        let (db, _) = IocBuilder::new()
            .db_string(&src, &HashMap::new())
            .expect("db")
            .build()
            .await
            .expect("ioc");
        let mut visited = super::ProcStack::new();
        for (s_inp, want) in [("9", at_major), ("NoSuch", at_invalid)] {
            if s_inp == "9" {
                db.put_pv("S", EpicsValue::Double(9.0)).await.unwrap();
            } else {
                db.put_record_field_from_ca("S", "INP", EpicsValue::String(s_inp.into()))
                    .await
                    .unwrap();
            }
            db.process_record_with_links("S", &mut visited)
                .await
                .unwrap();
            db.process_record_with_links("T", &mut visited)
                .await
                .unwrap();
            let t = db.get_record("T").unwrap();
            let t = t.read();
            assert_eq!(t.ran_arm(), CyclePlan::DbSoftIn, "{mode} S.INP={s_inp}");
            assert_eq!(t.common.sevr, want, "{mode} S.INP={s_inp}");
        }
    }
}

/// An ao `T` whose OUT names `A`, with the lent OUT target the frame takes
/// set to the decoy `D` (on `scan`) at the name revision plus `skew`.
async fn ao_with_lent_out_target(
    out: &str,
    field: &str,
    scan: &str,
    skew: u64,
) -> std::sync::Arc<PvDatabase> {
    use crate::server::record::record_instance::{LinkTargetResolver, OutTarget, PutAddr};
    let (db, _) = IocBuilder::new()
        .db_string(
            &format!(
                r#"record(calc, "A") {{ field(CALC, "B+1") }}
                   record(calc, "D") {{ field(CALC, "B+1") field(SCAN, "{scan}") }}
                   record(ao, "T") {{ field(DTYP, "Soft Channel") field(OUT, "{out}")
                    field(HIHI, "5") field(HHSV, "MAJOR") }}"#
            ),
            &HashMap::new(),
        )
        .expect("db")
        .build()
        .await
        .expect("ioc");
    let decoy = db.get_record("D").unwrap();
    let target = OutTarget::new(
        &decoy,
        PutAddr::resolve_in(&decoy, field),
        db.name_revision() + skew,
    );
    let t = db.get_record("T").unwrap();
    t.write().parsed_out.set_out_target(Some(target));
    db.put_pv("T", EpicsValue::Double(6.0)).await.unwrap();
    db.process_record_with_links("T", &mut super::ProcStack::new())
        .await
        .unwrap();
    db
}

/// `(B, VAL, SEVR)` of a calc.
fn calc_state(db: &PvDatabase, name: &str) -> (f64, f64, crate::server::record::AlarmSeverity) {
    let r = db.get_record(name).unwrap();
    let r = r.read();
    let f = |n| r.record.get_field(n).and_then(|v| v.to_f64()).unwrap();
    (f("B"), f("VAL"), r.common.sevr)
}

/// A lent target at the current revision is C's `dbAddr`: the frame puts
/// through it, then inherits MS and processes the Passive target for PP
/// from the link's facts, never asking the link.
#[epics_macros_rs::epics_test]
async fn a_current_out_target_is_where_the_frame_puts() {
    use crate::server::record::AlarmSeverity::{Invalid, Major};
    let db = ao_with_lent_out_target("A.B PP MS", "B", "Passive", 0).await;
    assert_eq!(calc_state(&db, "D"), (6.0, 7.0, Major));
    // Never processed: still UDF.
    assert_eq!(calc_state(&db, "A"), (0.0, 0.0, Invalid));
}

/// T's OUT target: the cell it lends and the revision it was lent at.
fn out_target_of(db: &PvDatabase) -> Option<(bool, bool)> {
    use crate::server::record::record_instance::LinkTargetResolver;
    let a = db.get_record("A").unwrap();
    let t = db.get_record("T").unwrap();
    let target = t.read().parsed_out.out_target();
    target.map(|t| (t.is_cell(&a), t.revision() == db.name_revision()))
}

/// A target lent at another name revision is not dereferenced: the frame
/// lends the link's target again, writes it, and keeps that one.
#[epics_macros_rs::epics_test]
async fn a_stale_out_target_is_not_written() {
    use crate::server::record::AlarmSeverity::{Invalid, Major};
    let db = ao_with_lent_out_target("A.B PP MS", "B", "Passive", 1).await;
    assert_eq!(calc_state(&db, "A"), (6.0, 7.0, Major));
    assert_eq!(calc_state(&db, "D"), (0.0, 0.0, Invalid));
    assert_eq!(out_target_of(&db), Some((true, true)));
}

/// An OUT link with no target yet lends one on its first cycle, and the
/// next cycle writes through it at the same revision.
#[epics_macros_rs::epics_test]
async fn the_first_cycle_lends_the_out_target_the_next_one_writes() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(calc, "A") { field(CALC, "B+1") }
               record(ao, "T") { field(DTYP, "Soft Channel") field(OUT, "A.B PP") }"#,
            &HashMap::new(),
        )
        .expect("db")
        .build()
        .await
        .expect("ioc");
    assert_eq!(out_target_of(&db), None);
    for val in [3.0, 4.0] {
        db.put_pv("T", EpicsValue::Double(val)).await.unwrap();
        db.process_record_with_links("T", &mut super::ProcStack::new())
            .await
            .unwrap();
        assert_eq!(out_target_of(&db), Some((true, true)), "VAL={val}");
        assert_eq!(calc_state(&db, "A").1, val + 1.0, "VAL={val}");
    }
}

/// A lent target at `.PROC` processes its record through C's first
/// `dbDbPutValue` gate (`dbDbLink.c:387`): no PP on the link, and a target
/// that is not Passive.
#[epics_macros_rs::epics_test]
async fn a_current_out_target_at_proc_processes_its_record() {
    let db = ao_with_lent_out_target("A.PROC", "PROC", "10 second", 0).await;
    assert_eq!(calc_state(&db, "D").1, 1.0);
    assert_eq!(calc_state(&db, "A").1, 0.0);
}

/// A stale target whose link no longer names a local record is dropped,
/// not kept beside the link's own route.
#[epics_macros_rs::epics_test]
async fn a_stale_out_target_with_no_local_answer_is_dropped() {
    use crate::server::record::AlarmSeverity::Invalid;
    let db = ao_with_lent_out_target("NOSUCH.B PP", "B", "Passive", 1).await;
    assert_eq!(calc_state(&db, "D"), (0.0, 0.0, Invalid));
    let t = db.get_record("T").unwrap();
    assert!(t.read().parsed_out.out_target().is_none());
}
