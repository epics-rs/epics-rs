//! A site `menuScan.dbd` changes the IOC's scan rates — C `initPeriodic`
//! (`dbScan.c:856-918` at `R7.0.10`) parses the loaded menu's choice strings
//! rather than a fixed list of rates, and overriding the menu is documented,
//! supported base behaviour.
//!
//! ONE test in its own file on purpose. The menu is a process-global that the
//! first reader freezes (C's `nPeriodic`/`papPeriodic` are the same, sized once
//! at `iocInit`), so a second test in this binary would either race the freeze
//! or be testing a table the first test installed.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::db_loader::parse_db;
use epics_base_rs::server::record::{
    ScanList, ScanType, menu_scan, resolve_menu_field_string, shared_menu_choices,
};

/// A menu with rates base does not have, in units base's own menu never uses.
const SITE_MENU: &str = r#"
menu(menuScan) {
    choice(menuScanPassive,     "Passive")
    choice(menuScanEvent,       "Event")
    choice(menuScanI_O_Intr,    "I/O Intr")
    choice(menuScan1_hour,      "1 hour")
    choice(menuScan5_minutes,   "5 minutes")
    choice(menuScan60_Hz,       "60 Hz")
}
"#;

#[test]
fn a_site_menuscan_replaces_the_rates_the_ioc_scans_at() {
    // Registered BEFORE the table is frozen: C reports a rate it cannot keep
    // from inside `initPeriodic` itself (`dbScan.c:912`), so the message is
    // emitted at the freeze and there is no second chance to hear it.
    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });

    parse_db(SITE_MENU, &HashMap::new()).expect("the site menu loads");

    let menu = menu_scan();

    // 1/60 s is under two clock ticks on a 100 Hz kernel, which is C's own
    // definition of a rate it cannot keep — reported, then used anyway.
    epics_base_rs::runtime::log::errlog_flush();
    let heard = heard.lock().expect("sink").join("");
    assert!(
        heard.contains("initPeriodic: Scan rate '60 Hz' is not achievable."),
        "the unachievable rate must be reported: {heard:?}"
    );

    // Three rates, not base's seven — the count C takes as `nPeriodic`.
    assert_eq!(menu.n_periodic(), 3);
    assert_eq!(ScanList::count(), 5, "Event, I/O Intr and the three rates");

    // Every unit form, resolved from the menu rather than from a table of
    // seven hard-coded `Duration`s.
    assert_eq!(
        ScanType::from_u16(3).interval(),
        Some(Duration::from_secs(3600))
    );
    assert_eq!(
        ScanType::from_u16(4).interval(),
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        ScanType::from_u16(5).interval(),
        Some(Duration::from_secs_f64(1.0 / 60.0))
    );

    // The labels a client is served with `.SCAN`, and the reverse direction a
    // `.db` file's `field(SCAN,"60 Hz")` goes through.
    assert_eq!(menu.label_at(5), Some("60 Hz"));
    assert_eq!(menu.index_of("60 Hz"), Some(5));
    assert_eq!(
        epics_base_rs::server::record::shared_menu_choices("SCAN"),
        Some(menu.choices()),
        "the DBR_ENUM choice list is the loaded menu's"
    );
    assert_eq!(
        epics_base_rs::server::record::shared_menu_choices("SSCN"),
        Some(menu.choices()),
        "SSCN is the same menu, so it is the same list"
    );

    // Base's own rates are NOT in this IOC: `1 second` is index 6 in base's
    // menu and this menu has no index 6 at all, so it names no scan list —
    // C's `scan >= nPeriodic + SCAN_1ST_PERIODIC` refusal.
    assert_eq!(menu.index_of("1 second"), None);
    assert_eq!(ScanType::from_u16(6).scan_list(), None);
    assert_eq!(ScanType::from_u16(6).interval(), None);

    // Every rate this menu does have names a distinct scan-list slot.
    let lists = ScanList::all();
    assert_eq!(lists.len(), 5);
    for (i, list) in lists.iter().enumerate() {
        assert_eq!(list.slot(), i);
    }

    // The string→index put path: `caput REC.SCAN "5 minutes"` resolves to this
    // menu's index 4, and to nothing at all in base's menu.
    //
    // Through the FIELD's resolver, because a menu label is menu-specific:
    // "Specified" is index 1 of `menuFanout` and index 0 of `selSELM`, so a
    // field-blind converter could only guess which menu was meant.
    // `EpicsValue::parse` is field-blind and refuses menu labels for exactly
    // that reason; `shared_menu_choices("SCAN")` is what makes the answer this
    // IOC's loaded menu rather than base's compiled-in one.
    assert_eq!(
        resolve_menu_field_string(
            "SCAN",
            shared_menu_choices("SCAN").expect("SCAN is a shared menu field"),
            epics_base_rs::types::DbFieldType::Enum,
            "5 minutes",
        )
        .expect("a menu label resolves against its own field's menu"),
        epics_base_rs::types::EpicsValue::Enum(4)
    );
    assert!(
        epics_base_rs::types::EpicsValue::parse(
            epics_base_rs::types::DbFieldType::Enum,
            "5 minutes"
        )
        .is_err(),
        "the field-blind converter must not guess a menu"
    );

    // Loading the same menu again is not an error — a startup script that
    // includes its `.dbd` twice must not fail on the second.
    parse_db(SITE_MENU, &HashMap::new()).expect("the same menu re-loads");

    // A DIFFERENT menu after the table is in use is refused rather than
    // silently leaving records in lists keyed by the old one.
    let other = SITE_MENU.replace("60 Hz", "50 Hz");
    let err = parse_db(&other, &HashMap::new()).expect_err("a changed menu is refused");
    assert!(
        format!("{err:?}").contains("menuScan"),
        "the error must name the menu: {err:?}"
    );
}
