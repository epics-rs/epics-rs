/// The value of a `menu(menuScan)` field — the SCAN field's domain.
///
/// [`Self::Illegal`] is not a defensive variant, it is C's state. `dbPut`
/// stores the `epicsEnum16` the client wrote and only THEN calls `scanAdd`,
/// which tests the index against the menu and, when it is outside,
///
/// ```c
/// /* dbScan.c:248-251 */
/// if (scan < 0 || scan >= nPeriodic + SCAN_1ST_PERIODIC) {
///     recGblRecordError(-1, (void *)precord,
///         "scanAdd detected illegal SCAN value");
/// }
/// ```
///
/// logs and adds the record to **no scan list**. The field itself keeps the
/// written value: verified on the softIoc, `caput REC.SCAN 10` succeeds and
/// `caget REC.SCAN` answers `10`.
///
/// Modelling SCAN as the nine legal choices alone forced `from_u16` to *erase*
/// an out-of-menu index to `Passive`, which is wrong twice over: the field then
/// read back `0` instead of `10`, and the record became put-processable
/// (C tests `precord->scan == 0` literally in `dbPutField`, dbAccess.c:1263, so
/// an illegal SCAN does NOT process on a `pp(TRUE)` put — a `Passive` one does).
/// Carrying the index removes both.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum ScanType {
    #[default]
    Passive,
    Event,
    IoIntr,
    Sec10,
    Sec5,
    Sec2,
    Sec1,
    Sec05,
    Sec02,
    Sec01,
    /// An index outside `menuScan`. Stored, served, and scanned by nothing.
    Illegal(u16),
}

impl ScanType {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Passive,
            1 => Self::Event,
            2 => Self::IoIntr,
            3 => Self::Sec10,
            4 => Self::Sec5,
            5 => Self::Sec2,
            6 => Self::Sec1,
            7 => Self::Sec05,
            8 => Self::Sec02,
            9 => Self::Sec01,
            other => Self::Illegal(other),
        }
    }

    /// The `DBR_ENUM` index this value is served and stored as.
    pub fn to_u16(self) -> u16 {
        match self {
            Self::Passive => 0,
            Self::Event => 1,
            Self::IoIntr => 2,
            Self::Sec10 => 3,
            Self::Sec5 => 4,
            Self::Sec2 => 5,
            Self::Sec1 => 6,
            Self::Sec05 => 7,
            Self::Sec02 => 8,
            Self::Sec01 => 9,
            Self::Illegal(v) => v,
        }
    }

    /// The scan list this SCAN value names, if it names one — see [`ScanList`].
    pub fn scan_list(self) -> Option<ScanList> {
        ScanList::of(self)
    }

    /// Return the interval duration for periodic scan types.
    pub fn interval(&self) -> Option<std::time::Duration> {
        match self {
            Self::Sec10 => Some(std::time::Duration::from_secs(10)),
            Self::Sec5 => Some(std::time::Duration::from_secs(5)),
            Self::Sec2 => Some(std::time::Duration::from_secs(2)),
            Self::Sec1 => Some(std::time::Duration::from_secs(1)),
            Self::Sec05 => Some(std::time::Duration::from_millis(500)),
            Self::Sec02 => Some(std::time::Duration::from_millis(200)),
            Self::Sec01 => Some(std::time::Duration::from_millis(100)),
            _ => None,
        }
    }
}

/// The key of a scan list: a `SCAN` value that actually names one.
///
/// C `scanAdd` (`dbScan.c:241-251`) is the sole gate on scan-list membership,
/// and it admits neither of the two SCAN values that name no list:
///
/// ```c
/// if (scan == menuScanPassive) return;                       /* no list */
/// if (scan < 0 || scan >= nPeriodic + SCAN_1ST_PERIODIC) {   /* no list */
///     recGblRecordError(-1, precord, "scanAdd detected illegal SCAN value");
/// } else if (scan == menuScanEvent) { ... }
/// ```
///
/// The scan index is keyed by this type rather than by [`ScanType`], so those
/// two cases are refused by construction at every insert site — a `Passive` or
/// [`ScanType::Illegal`] record cannot be put in a bucket, and no consumer of a
/// bucket has to re-check. Before [`ScanType::Illegal`] existed the index was
/// keyed by `ScanType` and each site spelled out `!= ScanType::Passive`, which
/// admitted exactly the illegal index C refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ScanList(ScanType);

impl ScanList {
    /// `None` when this SCAN names no list — `Passive`, or an index outside
    /// `menuScan`.
    pub fn of(scan: ScanType) -> Option<Self> {
        match scan {
            ScanType::Passive | ScanType::Illegal(_) => None,
            scanned => Some(Self(scanned)),
        }
    }

    /// The SCAN value this list holds the records of.
    pub fn scan(self) -> ScanType {
        self.0
    }

    /// How many scan lists exist: the `menuScan` choices minus `Passive`,
    /// i.e. `Event`, `IoIntr` and the seven periodic rates. C sizes its own
    /// list array the same way — `nPeriodic + SCAN_1ST_PERIODIC`
    /// (`dbScan.c:250`) over `papPeriodic` plus the two special lists.
    pub const COUNT: usize = 9;

    /// Dense slot in `0..COUNT`, so a per-list table can be a fixed array
    /// rather than a map behind its own lock.
    ///
    /// Total by construction: [`Self::of`] is the only constructor and it
    /// refuses `Passive` (menu index 0) and every out-of-menu index, so the
    /// wrapped [`ScanType`] is always one of the nine indices `1..=9`.
    pub fn slot(self) -> usize {
        self.0.to_u16() as usize - 1
    }

    /// Every scan list, in menu order — the enumeration a per-list table is
    /// built from.
    pub const ALL: [Self; Self::COUNT] = [
        Self(ScanType::Event),
        Self(ScanType::IoIntr),
        Self(ScanType::Sec10),
        Self(ScanType::Sec5),
        Self(ScanType::Sec2),
        Self(ScanType::Sec1),
        Self(ScanType::Sec05),
        Self(ScanType::Sec02),
        Self(ScanType::Sec01),
    ];
}

/// `SSCN` — the simulation-mode scan field (`DBF_MENU`, `menu(menuScan)`).
///
/// The same domain as `SCAN` — it is the same menu — so it is the same type,
/// and an out-of-menu index is carried, not erased. Its dbd default is the
/// out-of-range `65535` (`field(SSCN,DBF_MENU){ menu(menuScan) initial("65535")
/// }`, identical across all 21 records that carry SSCN), which C reads as "not
/// set — keep scanning at SCAN while in simulation mode".
///
/// That sentinel is `65535` and *only* `65535`. Both recGbl helpers test it
/// literally —
///
/// ```c
/// /* recGbl.c: recGblSaveSimm and recGblCheckSimm both open with */
/// if (*psscn == USHRT_MAX) return;
/// ```
///
/// — so an SSCN of, say, `10` is NOT "unset": C performs the swap, lands the
/// illegal `10` in SCAN, and `scanAdd` then leaves the record in no scan list.
/// Treating every illegal index as the sentinel would be a different behaviour
/// from C, so the distinction is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SimModeScan(ScanType);

impl Default for SimModeScan {
    fn default() -> Self {
        Self(ScanType::Illegal(Self::DO_NOT_USE))
    }
}

impl SimModeScan {
    /// C's dbd default, and the one index the recGbl simulation helpers bail on.
    pub const DO_NOT_USE: u16 = 65535;

    pub fn from_u16(v: u16) -> Self {
        Self(ScanType::from_u16(v))
    }

    pub fn from_scan(s: ScanType) -> Self {
        Self(s)
    }

    /// The `DBR_ENUM`/wire index — whatever was written, sentinel included.
    pub fn to_u16(self) -> u16 {
        self.0.to_u16()
    }

    /// C's `*psscn == USHRT_MAX` test.
    pub fn is_unset(self) -> bool {
        self.to_u16() == Self::DO_NOT_USE
    }

    /// The scan SSCN swaps SCAN to. `None` only for the unset sentinel — an
    /// illegal-but-not-sentinel index still swaps, exactly as it does in C.
    pub fn scan(self) -> Option<ScanType> {
        (!self.is_unset()).then_some(self.0)
    }
}

impl std::fmt::Display for SimModeScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for ScanType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passive => write!(f, "Passive"),
            Self::Event => write!(f, "Event"),
            Self::IoIntr => write!(f, "I/O Intr"),
            Self::Sec10 => write!(f, "10 second"),
            Self::Sec5 => write!(f, "5 second"),
            Self::Sec2 => write!(f, "2 second"),
            Self::Sec1 => write!(f, "1 second"),
            Self::Sec05 => write!(f, ".5 second"),
            Self::Sec02 => write!(f, ".2 second"),
            Self::Sec01 => write!(f, ".1 second"),
            // C has no label for an out-of-menu index; `caget` renders the
            // number itself (measured: `caget REC.SCAN` -> `10`).
            Self::Illegal(v) => write!(f, "{v}"),
        }
    }
}

#[cfg(test)]
mod sim_mode_scan_tests {
    use super::*;

    #[test]
    fn default_is_the_65535_sentinel() {
        // C dbd `field(SSCN,DBF_MENU){ initial("65535") }`.
        assert!(SimModeScan::default().is_unset());
        assert_eq!(SimModeScan::default().to_u16(), 65535);
        assert_eq!(SimModeScan::default().scan(), None);
    }

    #[test]
    fn sentinel_round_trips_through_u16() {
        assert!(SimModeScan::from_u16(65535).is_unset());
        assert_eq!(SimModeScan::from_u16(65535).to_u16(), 65535);
    }

    #[test]
    fn valid_menu_indices_map_to_scan_choices() {
        for v in 0u16..=9 {
            assert_eq!(SimModeScan::from_u16(v).scan(), Some(ScanType::from_u16(v)));
            assert_eq!(SimModeScan::from_u16(v).to_u16(), v);
            assert!(!SimModeScan::from_u16(v).is_unset());
        }
    }

    /// The labels this type renders are `menuScan`'s, so the one menu
    /// converter (which owns every string→menu-index put, see
    /// `tests/menu_common_field_scan_pini.rs`) and this type agree on every
    /// choice — including the `".5 second"` spellings that the deleted
    /// `ScanType::from_str` used to accept a `"0.5 second"` alias for.
    #[test]
    fn labels_match_the_shared_menu_table() {
        for (i, label) in super::super::menu_choices::MENU_SCAN.iter().enumerate() {
            assert_eq!(ScanType::from_u16(i as u16).to_string(), *label);
        }
    }

    /// CORRECTED — this test used to assert `from_u16(10) == DoNotUse`, i.e.
    /// that an out-of-menu index collapses to the sentinel. It does not. C's
    /// recGbl simulation helpers bail on `*psscn == USHRT_MAX` and on nothing
    /// else, so `10` is an ordinary (illegal) index that still drives the swap;
    /// and the field itself reads back `10`, not `65535`.
    #[test]
    fn an_out_of_menu_index_is_carried_and_is_not_the_sentinel() {
        for v in [10u16, 40000] {
            let s = SimModeScan::from_u16(v);
            assert_eq!(s.to_u16(), v, "the written index is what reads back");
            assert!(!s.is_unset(), "{v} is illegal, but it is not USHRT_MAX");
            assert_eq!(
                s.scan(),
                Some(ScanType::Illegal(v)),
                "C swaps it into SCAN; scanAdd then scans nothing"
            );
        }
    }

    /// The SCAN field's own boundary: at the last legal choice, one past it,
    /// and the `-1` a client sends as `65535`.
    #[test]
    fn scan_carries_an_illegal_index_instead_of_erasing_it_to_passive() {
        assert_eq!(ScanType::from_u16(9), ScanType::Sec01);
        assert_eq!(ScanType::from_u16(9).to_u16(), 9);

        // One past the last choice. Measured: `caput REC.SCAN 10` succeeds and
        // `caget REC.SCAN` answers 10 — it does NOT become Passive.
        assert_eq!(ScanType::from_u16(10), ScanType::Illegal(10));
        assert_eq!(ScanType::from_u16(10).to_u16(), 10);
        assert_ne!(ScanType::from_u16(10), ScanType::Passive);

        // `caput REC.SCAN -1` reaches the field as the epicsEnum16 65535.
        assert_eq!(ScanType::from_u16(65535), ScanType::Illegal(65535));
        assert_eq!(ScanType::from_u16(65535).to_u16(), 65535);

        // An illegal index is in no scan list: not periodic, not I/O Intr.
        assert_eq!(ScanType::Illegal(10).interval(), None);
        assert_ne!(ScanType::Illegal(10), ScanType::IoIntr);
    }

    /// `scanAdd`'s gate, at its boundaries: the last legal index names a list,
    /// one past it names none, and `Passive` names none.
    #[test]
    fn only_a_menu_choice_other_than_passive_names_a_scan_list() {
        assert_eq!(ScanType::Passive.scan_list(), None);
        for v in 1u16..=9 {
            let scan = ScanType::from_u16(v);
            assert_eq!(
                scan.scan_list().map(ScanList::scan),
                Some(scan),
                "index {v} is a menuScan choice"
            );
        }
        for v in [10u16, 42, 65535] {
            assert_eq!(
                ScanType::from_u16(v).scan_list(),
                None,
                "index {v} is outside menuScan; scanAdd adds the record nowhere"
            );
        }
    }

    /// Every legal index survives the round trip, so nothing above is bought at
    /// the cost of the ordinary path.
    #[test]
    fn every_legal_index_round_trips() {
        for v in 0u16..=9 {
            assert_eq!(ScanType::from_u16(v).to_u16(), v);
        }
    }

    /// `slot()` is a total, collision-free index into a `COUNT`-sized array —
    /// the property the per-list scan-index table indexes by, without a
    /// bounds check or a fallible lookup.
    ///
    /// Boundary cases, not a narrative: every `of`-admissible SCAN maps into
    /// `0..COUNT`; `ALL` is exactly that set in slot order; and both values
    /// `of` refuses (`Passive`, out-of-menu) yield no slot at all.
    #[test]
    fn every_scan_list_has_a_distinct_slot() {
        let mut seen = [false; ScanList::COUNT];
        for v in 0u16..=9 {
            let Some(list) = ScanType::from_u16(v).scan_list() else {
                assert_eq!(v, 0, "only Passive names no list inside the menu");
                continue;
            };
            let slot = list.slot();
            assert!(slot < ScanList::COUNT, "slot {slot} out of the table");
            assert!(!seen[slot], "slot {slot} claimed twice");
            seen[slot] = true;
        }
        assert!(seen.iter().all(|s| *s), "every slot must be claimed");

        for (i, list) in ScanList::ALL.iter().enumerate() {
            assert_eq!(list.slot(), i, "ALL must be in slot order");
        }
        assert_eq!(ScanType::Illegal(65535).scan_list(), None);
    }
}
