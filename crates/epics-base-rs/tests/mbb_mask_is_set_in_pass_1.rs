//! mbbi, mbbo, mbbiDirect and mbboDirect derive MASK from NOBT in
//! `init_record` pass 1, after the pass-0 return (`mbbiRecord.c:129-130`,
//! `mbboRecord.c:137-138`, `mbbiDirectRecord.c:112-113`,
//! `mbboDirectRecord.c:123-124`).

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

#[test]
fn mask_is_derived_in_pass_1() {
    for rtype in ["mbbi", "mbbo", "mbbiDirect", "mbboDirect"] {
        let mut rec = create_record(rtype).unwrap();
        rec.load_field("NOBT", EpicsValue::Short(4)).unwrap();
        rec.init_record(0).unwrap();
        let mask = |rec: &dyn Record| rec.get_field("MASK").and_then(|v| v.to_f64());
        assert_eq!(mask(&*rec), Some(0.0), "{rtype}.MASK after pass 0");
        rec.init_record(1).unwrap();
        assert_eq!(mask(&*rec), Some(15.0), "{rtype}.MASK after pass 1");
    }
}
