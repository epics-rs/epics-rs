//! `fast_pid_control.db` loaded as shipped: the "Fast Epid" record connects
//! its asyn ports from `INP` and `DESC` (devEpidFast.c `init_record`), and a
//! Float64 interrupt on the input port drives the PID, which writes the
//! output port.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use asyn_rs::manager::PortManager;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{DrvUserRequest, InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

struct Port {
    base: PortDriverBase,
}

impl PortDriver for Port {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// A Float64 parameter-library port holding `params` at addr 0.
fn port(manager: &PortManager, name: &str, params: &[(&str, f64)]) {
    let claim = asyn_rs::manager::claim_port_driver(name).unwrap();
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags::default(),
        InterfaceMask::FLOAT64 | InterfaceMask::DRV_USER,
        InterfaceMask::FLOAT64,
    );
    for &(param, value) in params {
        let reason = base.create_param(param, ParamType::Float64).unwrap();
        base.set_float64_param(reason, 0, value).unwrap();
    }
    manager
        .register_port(claim, Port { base }, &asyn_rs::registry::open_owner())
        .unwrap();
}

fn get(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_base_rs::epics_test]
async fn fast_epid_record_reads_input_and_writes_output() {
    let manager = PortManager::new();
    port(&manager, "EPF_IN", &[("DATA", 0.0), ("INTERVAL", 0.001)]);
    port(&manager, "EPF_OUT", &[("OUTPUT", 0.0)]);
    let input = manager.find_port_handle("EPF_IN").unwrap();
    let output = manager.find_port_handle("EPF_OUT").unwrap();

    let macros: HashMap<String, String> = [
        ("P", "EPF:"),
        ("PID", "PID"),
        ("INPUT", "EPF_IN"),
        ("ICHAN", "0"),
        ("INPUT_DATA", "DATA"),
        ("INPUT_INTERVAL", "INTERVAL"),
        ("OUTPUT", "EPF_OUT"),
        ("OCHAN", "0"),
        ("OUTPUT_DATA", "OUTPUT"),
        ("SCAN", "Passive"),
        ("KP", "2"),
        ("KI", "0"),
        ("KD", "0"),
        ("DT", "0.0025"),
        ("LOPR", "-100"),
        ("HOPR", "100"),
        ("DRVL", "-100"),
        ("DRVH", "100"),
        ("PREC", "3"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    // The db's transform records come from calc, as in C.
    let mut builder = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()));
    for (record_type, factory) in std_rs::std_record_factories() {
        builder = builder.register_record_type(record_type, factory);
    }
    for (record_type, devsup, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(record_type, devsup, factory);
    }
    let path = format!("{}/fast_pid_control.db", std_rs::STD_DB_DIR);
    let (db, _) = builder
        .db_file(&path, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    // DT reads back timePerPointActual: numAverage = (int)(0.5 + 2.5 ms /
    // the 1 ms INTERVAL read at init) = 3, times 1 ms.
    let dt = get(&db, "EPF:PID.DT");
    assert!((dt - 0.003).abs() < 1e-12, "DT {dt}");

    db.put_record_field_from_ca("EPF:PID", "FBON", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("EPF:PID", "VAL", EpicsValue::Double(10.0))
        .await
        .unwrap();

    let data = input.drv_user_create(&DrvUserRequest::new("DATA", 0)).await;
    let data = data.unwrap();
    for sample in [3.0, 4.0, 5.0] {
        input
            .set_params_and_notify(
                0,
                vec![ParamSetValue::Value {
                    reason: data,
                    addr: 0,
                    value: ParamValue::Float64(sample),
                }],
            )
            .await
            .unwrap();
    }

    // CVAL is the mean of the three, 4; KP * (VAL - CVAL) = 2 * (10 - 4),
    // written by the epidFast thread.
    let written = output
        .drv_user_create(&DrvUserRequest::new("OUTPUT", 0))
        .await;
    let written = written.unwrap();
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(Duration::from_secs(5));
    loop {
        let v = output.read_float64(written, 0).await.unwrap();
        if v == 12.0 {
            break;
        }
        assert!(Instant::now() < deadline, "EPF_OUT OUTPUT stayed {v}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("EPF:PID", &mut visited)
        .await
        .unwrap();
    assert_eq!(get(&db, "EPF:PID.CVAL"), 4.0);
    assert_eq!(get(&db, "EPF:PID.OVAL"), 12.0);
}

/// Where C leaves the record with bare PACT=1 (`goto bad`), init also sets
/// LINK/INVALID, as devAsynInt32's `bad:` arm does: an input port that does
/// not exist, and a missing token.
#[epics_base_rs::epics_test]
async fn fast_epid_record_that_cannot_connect_raises_link_alarm() {
    let db = r#"
record(epid, "EPF:NOPORT") {
    field(DTYP, "Fast Epid")
    field(INP, "@EPF_NO_SUCH_PORT 0 DATA INTERVAL")
    field(DESC, "EPF_NO_SUCH_OUT 0 OUTPUT")
}
record(epid, "EPF:SHORT") {
    field(DTYP, "Fast Epid")
    field(INP, "@EPF_NO_SUCH_PORT 0 DATA INTERVAL")
    field(DESC, "EPF_NO_SUCH_OUT 0")
}
"#;
    let mut builder = IocBuilder::new();
    for (record_type, factory) in std_rs::std_record_factories() {
        builder = builder.register_record_type(record_type, factory);
    }
    for (record_type, devsup, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(record_type, devsup, factory);
    }
    let (db, _) = builder
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for name in ["EPF:NOPORT", "EPF:SHORT"] {
        assert_eq!(
            get(&db, &format!("{name}.NSTA")) as u16,
            epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
            "{name}"
        );
        assert_eq!(get(&db, &format!("{name}.NSEV")), 3.0, "{name} INVALID");
        assert_eq!(get(&db, &format!("{name}.PACT")), 1.0, "{name} PACT");
    }
}
