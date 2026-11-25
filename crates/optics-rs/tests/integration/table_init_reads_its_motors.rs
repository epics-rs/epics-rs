//! table's `init_record(1)` against oracleTableIoc (optics a750b958): C reads
//! each motor at `iocInit` through `GetReadback`, `ReadEncoders` and
//! `GetMotorLimits` (tableRecord.c:386-401) before `CalcUserLimits`, so the
//! user coordinates, readbacks and limits hold the motors' values before any
//! `process()`. `F` holds the failure arms: a failed readback or encoder read
//! stops its loop with the later axes as loaded (`:1109`, `:1133`), so R0Y
//! and E0Y keep 3 and 6; a failed limit read zeroes that limit and the loop
//! goes on (`:1026`), so H0X is 0, not the loaded 10; and the failed reads
//! leave LINK/INVALID, which `monitor()`'s `recGblResetAlarms` (`:405`)
//! makes the record's alarm. `dbgf` on each field after `iocInit`:

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_condition_string;
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(ao, "T:M0") { field(VAL, "1.5") field(DRVH, "11") field(DRVL, "-9") }
record(ao, "T:M1") { field(VAL, "-2.25") field(DRVH, "12") field(DRVL, "-8") }
record(ao, "T:M2") { field(VAL, "3") field(DRVH, "13") field(DRVL, "-7") }
record(ao, "T:M3") { field(VAL, "0.75") field(DRVH, "14") field(DRVL, "-6") }
record(ao, "T:M4") { field(VAL, "-1") field(DRVH, "15") field(DRVL, "-5") }
record(ao, "T:M5") { field(VAL, "7") field(DRVH, "16") field(DRVL, "-4") }
record(ao, "T:E0") { field(VAL, "1.25") }
record(ao, "T:E1") { field(VAL, "-2") }
record(ao, "T:E2") { field(VAL, "2.5") }
record(ao, "T:E3") { field(VAL, "1") }
record(ao, "T:E4") { field(VAL, "-0.5") }
record(ao, "T:E5") { field(VAL, "6.5") }
record(table, "T") {
  field(LX, "100") field(LZ, "200") field(SX, "10") field(SY, "20") field(SZ, "30")
  field(M0XL, "T:M0 PP") field(R0XI, "T:M0") field(E0XI, "T:E0") field(H0XL, "T:M0.DRVH") field(L0XL, "T:M0.DRVL")
  field(M0YL, "T:M1 PP") field(R0YI, "T:M1") field(E0YI, "T:E1") field(H0YL, "T:M1.DRVH") field(L0YL, "T:M1.DRVL")
  field(M1YL, "T:M2 PP") field(R1YI, "T:M2") field(E1YI, "T:E2") field(H1YL, "T:M2.DRVH") field(L1YL, "T:M2.DRVL")
  field(M2XL, "T:M3 PP") field(R2XI, "T:M3") field(E2XI, "T:E3") field(H2XL, "T:M3.DRVH") field(L2XL, "T:M3.DRVL")
  field(M2YL, "T:M4 PP") field(R2YI, "T:M4") field(E2YI, "T:E4") field(H2YL, "T:M4.DRVH") field(L2YL, "T:M4.DRVL")
  field(M2ZL, "T:M5 PP") field(R2ZI, "T:M5") field(E2ZI, "T:E5") field(H2ZL, "T:M5.DRVH") field(L2ZL, "T:M5.DRVL")
}
record(stringin, "F:S") { field(VAL, "abc") }
record(ao, "F:M0") { }
record(ao, "F:M1") { field(VAL, "4") field(DRVH, "8") field(DRVL, "-8") }
record(ao, "F:L0") { field(VAL, "-5") }
record(table, "F") {
  field(LX, "100") field(LZ, "200")
  field(M0XL, "F:M0 PP") field(R0XI, "F:S") field(E0XI, "F:S") field(H0XL, "F:S") field(L0XL, "F:L0")
  field(M0YL, "F:M1 PP") field(R0YI, "F:M1") field(E0YI, "F:M1") field(H0YL, "F:M1.DRVH") field(L0YL, "F:M1.DRVL")
  field(R0Y, "3") field(E0Y, "6") field(H0X, "10") field(L0X, "-10")
}
"#;

/// `dbgf` prints a double to 12 significant digits.
const ORACLE: &str = "\
T.AX -0.393914892561
T.AY 0.234713419721
T.AZ 3.00786944114
T.X 2.55130196777
T.Y 2.24069516691
T.Z 6.99785425824
T.AXRB -0.393914892561
T.AYRB 0.234713419721
T.AZRB 3.00786944114
T.XRB 2.55130196777
T.YRB 2.24069516691
T.ZRB 6.99785425824
T.EAX -0.214859919594
T.EAY 0.0861428436145
T.EAZ 2.57887616079
T.EX 2.19603963869
T.EY 1.91709896995
T.EZ 6.52433605351
T.AXL -0.393914892561
T.AYL 0.234713419721
T.AZL 3.00786944114
T.XL 2.55130196777
T.YL 2.24069516691
T.ZL 6.99785425824
T.HLAX 4.97254137251
T.HLAY 2.49813542576
T.HLAZ 6.75357504939
T.HLX 12.0513019678
T.HLY 12.2406951669
T.HLZ 15.9978542582
T.LLAX -1.7443344407
T.LLAY -4.27122390899
T.LLAZ -6.02589442233
T.LLX -4.19869803223
T.LLY -1.75930483309
T.LLZ -4.00214574176
T.M0X 1.5
T.M0Y -2.25
T.M1Y 3
T.M2X 0.75
T.M2Y -1
T.M2Z 7
T.R0X 1.5
T.R0Y -2.25
T.R1Y 3
T.R2X 0.75
T.R2Y -1
T.R2Z 7
T.E0X 1.25
T.E0Y -2
T.E1Y 2.5
T.E2X 1
T.E2Y -0.5
T.E2Z 6.5
T.H0X 11
T.H0Y 12
T.H1Y 13
T.H2X 14
T.H2Y 15
T.H2Z 16
T.L0X -9
T.L0Y -8
T.L1Y -7
T.L2X -6
T.L2Y -5
T.L2Z -4
T.SEVR NO_ALARM
T.STAT NO_ALARM
F.AX -0.429722377781
F.AY 0.00644804231433
F.AZ -1.71922804968
F.X 0.0450158273822
F.Y -2.22044604925e-16
F.Z -0.0112485744577
F.AXRB -0.429722377781
F.AYRB 0.00644804231433
F.AZRB -1.71922804968
F.XRB 0.0450158273822
F.YRB -2.22044604925e-16
F.ZRB -0.0112485744577
F.EAX -0.859469012109
F.EAY 0.0258194240576
F.EAZ -3.44058761249
F.EX 0.180253578209
F.EY 0
F.EZ -0.0449771088203
F.AXL -0.429722377781
F.AYL 0.00644804231433
F.AZL -1.71922804968
F.XL 0.0450158273822
F.YL -2.22044604925e-16
F.ZL -0.0112485744577
F.HLAX 89
F.HLAY 0.00644804231433
F.HLAZ -1.71922804968
F.HLX 0.0450158273821
F.HLY 5
F.HLZ -0.0112475744577
F.LLAX -89
F.LLAY 0.00644804231433
F.LLAZ -1.71922804968
F.LLX -4.95498417262
F.LLY -11
F.LLZ -0.0112495744577
F.M0X 0
F.M0Y 3
F.M1Y -2.22044604925e-16
F.M2X 0
F.M2Y 0
F.M2Z 0
F.R0X 0
F.R0Y 3
F.R1Y 0
F.R2X 0
F.R2Y 0
F.R2Z 0
F.E0X 0
F.E0Y 6
F.E1Y 0
F.E2X 0
F.E2Y 0
F.E2Z 0
F.H0X 0
F.H0Y 8
F.H1Y 0
F.H2X 0
F.H2Y 0
F.H2Z 0
F.L0X -5
F.L0Y -8
F.L1Y 0
F.L2X 0
F.L2Y 0
F.L2Z 0
F.SEVR INVALID
F.STAT LINK
";

#[epics_base_rs::epics_test]
async fn table_reads_its_motors_at_init_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("stringin", || Box::new(StringinRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut wrong = Vec::new();
    for line in ORACLE.lines() {
        let (pv, want) = line.split_once(' ').unwrap();
        let got = db.get_pv(pv).unwrap().to_f64().unwrap();
        let ok = match pv.rsplit_once('.').unwrap().1 {
            "SEVR" => MENU_ALARM_SEVR[got as usize] == want,
            "STAT" => alarm_condition_string(got as u16) == want,
            _ => (got - want.parse::<f64>().unwrap()).abs() <= 1e-9,
        };
        if !ok {
            wrong.push(format!("{pv} {got} (C {want})"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
