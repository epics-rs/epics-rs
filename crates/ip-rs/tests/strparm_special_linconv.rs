//! ai/ao `special(SPC_LINCONV)` through devXxStrParm's convert slot
//! (`aiRecord.c:187-198`, `aoRecord.c:254-265`). The expected ESLO/EOFF/ROFF
//! are a C IOC built from base 8f5015b66, asyn and ip d00f687, with the same
//! records and puts.

use std::collections::HashMap;

use epics_base_rs::runtime::task::test_block_on;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use ip_rs::strparm::{DEVICE_ROWS, device_support_factory};

const FIELDS: &str = r#"field(EGUL, "5") field(EGUF, "10") field(ESLO, "2") field(EOFF, "3") field(ROFF, "4") field(LOPR, "-7") field(ASLO, "1.1")"#;

/// ESLO, EOFF, ROFF, LOPR, ASLO.
type Fields = [f64; 5];

/// `<p>:L` (LINR LINEAR) after `EGUF 20`, `<p>:N` (NO CONVERSION) after
/// `LINR LINEAR`, then `<p>:L` after `EGUL 1`. No port `P` exists, so every
/// init fails and leaves PACT set, as C's does; the puts are `dbPutField`s
/// (`dbpf`), which a put-notify would instead hold until PACT clears.
fn linconv(prefix: &str, rtype: &str) -> [Fields; 3] {
    let dtyp = format!("asyn {} stringParm", &rtype[..2]);
    let io = if rtype == "ao" { "OUT" } else { "INP" };
    let mut db = String::new();
    for (suffix, linr) in [("L", "LINEAR"), ("N", "NO CONVERSION")] {
        db += &format!(
            "record({rtype}, \"{prefix}:{suffix}\") {{ field(DTYP, \"{dtyp}\") \
             field({io}, \"@asyn(P 1)\") field(LINR, \"{linr}\") {FIELDS} }}\n"
        );
    }
    test_block_on(async move {
        let (db, _) = IocBuilder::new()
            .register_dynamic_device_support(&DEVICE_ROWS, device_support_factory())
            .db_string(&db, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        let fields = |name: &str| -> Fields {
            let rec = db.get_record(name).unwrap();
            let inst = rec.read();
            ["ESLO", "EOFF", "ROFF", "LOPR", "ASLO"]
                .map(|f| inst.record.get_field(f).and_then(|v| v.to_f64()).unwrap())
        };
        let (l, n) = (format!("{prefix}:L"), format!("{prefix}:N"));
        let put = async |name: &str, field: &str, value: EpicsValue| {
            db.put_record_field_from_ca_no_notify(name, field, value)
                .await
                .unwrap();
        };
        put(&l, "EGUF", EpicsValue::Double(20.0)).await;
        put(&n, "LINR", EpicsValue::Short(2)).await;
        let first = [fields(&l), fields(&n)];
        put(&l, "EGUL", EpicsValue::Double(1.0)).await;
        [first[0], first[1], fields(&l)]
    })
}

/// `strParmConvert` is `MPCConvert`'s body: ESLO 1 and ROFF 0 on an ai. On an
/// ao its `aiRecord *` cast writes LOPR 1 and zeroes ASLO's low word (C reads
/// ASLO 1.0999994278 afterwards); only the EOFF rebase is kept there.
#[test]
fn strparm_ai_takes_a_unit_slope_and_ao_rebases_eoff_only() {
    assert_eq!(
        linconv("LC:SPAI", "ai"),
        [
            [1.0, 5.0, 0.0, -7.0, 1.1],
            [1.0, 5.0, 0.0, -7.0, 1.1],
            [1.0, 1.0, 0.0, -7.0, 1.1],
        ]
    );
    assert_eq!(
        linconv("LC:SPAO", "ao"),
        [
            [2.0, 5.0, 4.0, -7.0, 1.1],
            [2.0, 5.0, 4.0, -7.0, 1.1],
            [2.0, 1.0, 4.0, -7.0, 1.1],
        ]
    );
}
