//! Breakpoint-table linearisation — the `LINR >= 3` conversion for the `ai`
//! and `ao` records.
//!
//! Port of EPICS base `cvtBpt.c` (the runtime `raw <-> eng` piecewise-linear
//! lookup) and the breakpoint-table builder in `dbLexRoutines.c::dbBreakBody`
//! (slope computation + validation). A breakpoint table is an ordered list of
//! `(raw, eng)` points; the slope of each interval is precomputed so the
//! lookup is a single multiply once the bracketing interval is found.
//!
//! `LINR` (`menuConvert`) selects the conversion: `0 = NO_CONVERSION`,
//! `1 = SLOPE`, `2 = LINEAR` (all handled by the record's `ESLO`/`EOFF`), and
//! `>= 3` names a loaded breakpoint table. A table's index is assigned in
//! load (insertion) order and is STABLE — later loads never shift it — so a
//! resolved record always points at the same table (see
//! [`BreakTableRegistry`]).

use std::collections::BTreeMap;
use std::sync::Arc;

/// One breakpoint interval: the converted value `eng` at raw input `raw`, and
/// the `slope` (`d eng / d raw`) of the interval starting at this point. C
/// `brkInt` (`dbBase.h`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrkInt {
    /// Raw value at the start of the interval.
    pub raw: f64,
    /// Slope of the interval beginning at this point (`eng`/`raw`). The last
    /// point copies the previous slope so extrapolation past the end of the
    /// table continues with the final interval's slope.
    pub slope: f64,
    /// Converted (engineering) value at the start of the interval.
    pub eng: f64,
}

/// A named breakpoint table: an ordered array of [`BrkInt`]. C `brkTable`
/// (`dbBase.h`); `number` is `points.len()`.
#[derive(Debug, Clone, PartialEq)]
pub struct BrkTable {
    /// Table name (the `menuConvert` choice that selects it).
    pub name: String,
    /// Breakpoint intervals, in table order (raw monotonic up or down).
    pub points: Vec<BrkInt>,
}

impl BrkTable {
    /// Build a table from `(raw, eng)` pairs, computing the per-interval
    /// slopes. Mirrors C `dbBreakBody` (`dbLexRoutines.c:1046-1064`):
    ///
    /// - at least two points are required;
    /// - `slope[i] = (eng[i+1] - eng[i]) / (raw[i+1] - raw[i])`;
    /// - a zero slope is rejected (`"breaktable slope is zero"`);
    /// - the slope sign must not change (`"breaktable slope changes sign"`);
    /// - the final point copies the previous slope.
    ///
    /// C gates the zero/sign checks on `!dbBptNotMonotonic` (an opt-in global
    /// that allows non-monotonic tables). That flag defaults off and is not
    /// modelled here, so the strict (monotonic) rules always apply — the
    /// default EPICS behaviour.
    pub fn build(name: impl Into<String>, pairs: &[(f64, f64)]) -> Result<BrkTable, String> {
        let name = name.into();
        let number = pairs.len();
        if number < 2 {
            return Err(format!("breaktable {name}: Must have at least two points!"));
        }

        let mut points: Vec<BrkInt> = pairs
            .iter()
            .map(|&(raw, eng)| BrkInt {
                raw,
                slope: 0.0,
                eng,
            })
            .collect();

        // C: `down = (slope < 0)` at i == 0; every later slope must share the
        // sign, and no slope may be zero.
        let mut down = false;
        for i in 0..number - 1 {
            let denom = points[i + 1].raw - points[i].raw;
            let slope = (points[i + 1].eng - points[i].eng) / denom;
            if slope == 0.0 {
                return Err(format!("breaktable {name}: slope is zero"));
            }
            if i == 0 {
                down = slope < 0.0;
            } else if down != (slope < 0.0) {
                return Err(format!("breaktable {name}: slope changes sign"));
            }
            points[i].slope = slope;
        }
        // Continue with the last slope beyond the final point.
        points[number - 1].slope = points[number - 2].slope;

        Ok(BrkTable { name, points })
    }
}

/// Result of a breakpoint lookup. C returns `0` in range and `1` when the
/// input lies past either end of the table (the value is still produced by
/// extrapolating the nearest interval's slope, but the record raises
/// `SOFT_ALARM/MAJOR_ALARM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BptStatus {
    /// Input fell inside the table.
    InRange,
    /// Input fell past an end of the table; the value was extrapolated.
    OutOfRange,
}

/// Convert a raw value to engineering units through `table`, used by `ai`.
/// Port of C `cvtRawToEngBpt` (`cvtBpt.c:43-120`). `lbrk` is the cached last
/// interval index (C `plbrk`) — pass the same mutable cell across calls so a
/// monotonically-walking input does not re-scan the table from the start.
///
/// Returns the converted value and whether the input was in range. The table
/// is pre-resolved by the caller (the C `findBrkTable`/`init`/`ppbrk` cache is
/// the record's stored `Arc<BrkTable>`), so the C `linr < 2` and
/// table-not-found paths do not arise here.
pub fn cvt_raw_to_eng_bpt(val: f64, table: &BrkTable, lbrk: &mut usize) -> (f64, BptStatus) {
    let pts = &table.points;
    let number = pts.len(); // >= 2 by construction
    let mut status = BptStatus::InRange;

    // Limit the cached index to [0, number-2].
    let mut l = (*lbrk).min(number - 2);

    if pts[l + 1].raw > pts[l].raw {
        // raw values increase down the table
        while val > pts[l + 1].raw {
            l += 1;
            if l > number - 2 {
                status = BptStatus::OutOfRange;
                break;
            }
        }
        while val < pts[l].raw {
            if l == 0 {
                status = BptStatus::OutOfRange;
                break;
            }
            l -= 1;
        }
    } else {
        // raw values decrease down the table
        while val <= pts[l + 1].raw {
            l += 1;
            if l > number - 2 {
                status = BptStatus::OutOfRange;
                break;
            }
        }
        while val > pts[l].raw {
            if l == 0 {
                status = BptStatus::OutOfRange;
                break;
            }
            l -= 1;
        }
    }

    *lbrk = l;
    let p = &pts[l];
    (p.eng + (val - p.raw) * p.slope, status)
}

/// Convert an engineering value to raw through `table`, used by `ao`. Port of
/// C `cvtEngToRawBpt` (`cvtBpt.c:123-201`) — the inverse of
/// [`cvt_raw_to_eng_bpt`], bracketing on `eng` instead of `raw`.
pub fn cvt_eng_to_raw_bpt(val: f64, table: &BrkTable, lbrk: &mut usize) -> (f64, BptStatus) {
    let pts = &table.points;
    let number = pts.len(); // >= 2 by construction
    let mut status = BptStatus::InRange;

    let mut l = (*lbrk).min(number - 2);

    if pts[l + 1].eng > pts[l].eng {
        // eng values increase down the table
        while val > pts[l + 1].eng {
            l += 1;
            if l > number - 2 {
                status = BptStatus::OutOfRange;
                break;
            }
        }
        while val < pts[l].eng {
            if l == 0 {
                status = BptStatus::OutOfRange;
                break;
            }
            l -= 1;
        }
    } else {
        // eng values decrease down the table
        while val <= pts[l + 1].eng {
            l += 1;
            if l > number - 2 {
                status = BptStatus::OutOfRange;
                break;
            }
        }
        while val > pts[l].eng {
            if l == 0 {
                status = BptStatus::OutOfRange;
                break;
            }
            l -= 1;
        }
    }

    *lbrk = l;
    let p = &pts[l];
    (p.raw + (val - p.eng) / p.slope, status)
}

/// The first `LINR` value that selects a breakpoint table (after the three
/// fixed `menuConvert` choices `NO_CONVERSION`=0, `SLOPE`=1, `LINEAR`=2).
pub const LINR_FIRST_BREAKTABLE: i16 = 3;

/// The first `LINR` value available to a NON-standard (user-loaded) table.
/// The 12 standard `menuConvert` breakpoint-table names occupy
/// `LINR_FIRST_BREAKTABLE ..= 14`; a loaded table whose name is not one of
/// them is appended starting here. Mirrors C `menuConvert`, where
/// `typeKdegF`=3 .. `typeSdegC`=14 are reserved and an IOC application adds
/// further choices after the standard ones.
pub const LINR_FIRST_USER_TABLE: i16 = LINR_FIRST_BREAKTABLE + STANDARD_CONVERT_NAMES.len() as i16;

/// The 12 standard `menuConvert` breakpoint-table names, in `menuConvert.dbd`
/// declaration order — `LINR` 3..=14. These are exactly the entries of
/// [`MENU_CONVERT`](crate::server::record::MENU_CONVERT) (the `LINR` enum
/// labels) past the three fixed conversions; the
/// `standard_names_match_menu_convert_tail` test pins the two in sync so the
/// registry index and the caget `LINR` display can never disagree.
const STANDARD_CONVERT_NAMES: &[&str] = &[
    "typeKdegF",
    "typeKdegC",
    "typeJdegF",
    "typeJdegC",
    "typeEdegF(ixe only)",
    "typeEdegC(ixe only)",
    "typeTdegF",
    "typeTdegC",
    "typeRdegF",
    "typeRdegC",
    "typeSdegF",
    "typeSdegC",
];

/// The fixed `LINR` index of a standard `menuConvert` breakpoint-table name
/// (3..=14), or `None` if `name` is not one of the standard 12. Independent of
/// whether the table's DATA is loaded — the menu choice always exists, exactly
/// like C's built-in `menuConvert` menu.
fn standard_index_of(name: &str) -> Option<i16> {
    STANDARD_CONVERT_NAMES
        .iter()
        .position(|n| *n == name)
        .map(|pos| LINR_FIRST_BREAKTABLE + pos as i16)
}

/// Registry of breakpoint tables. Two structures mirror C exactly:
///
/// - `tables` is the load-on-demand DATA map (C `bptList`): name -> table,
///   populated by [`Self::insert`], first-wins on redefinition.
/// - the `LINR` index -> name map is the `menuConvert` menu. Its first 12
///   breakpoint slots (`LINR` 3..=14) are the fixed `STANDARD_CONVERT_NAMES`;
///   a loaded table whose name is NOT standard extends the menu in `extra_menu`
///   at `LINR_FIRST_USER_TABLE +` load position (C: an IOC adds menuConvert
///   choices after the standard ones).
///
/// So `LINR` indexes the STATIC menu (stable: a name never moves once placed),
/// and only the name->DATA lookup is dynamic — matching C
/// `findBrkTable`/`papChoiceValue[linr]` (cvtBpt.c:25-39) +
/// `dbFindBrkTable`/`bptList`. A standard `LINR` (e.g. 9 = `typeTdegF`) that
/// EPICS ships no `bpt*.data` for resolves to no table and the conversion fails,
/// exactly like C `dbFindBrkTable` returning NULL.
///
/// The four tables EPICS DOES ship data for (`typeJdegC`, `typeJdegF`,
/// `typeKdegC`, `typeKdegF`) are compiled in and present in every registry from
/// construction — see [`Self::new`].
#[derive(Debug, Clone)]
pub struct BreakTableRegistry {
    tables: BTreeMap<String, Arc<BrkTable>>,
    extra_menu: Vec<String>,
}

impl Default for BreakTableRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BreakTableRegistry {
    /// A registry holding the vendored breakpoint tables, and nothing else.
    ///
    /// The four thermocouple tables an EPICS install ships in `dbd/bpt*.dbd` are
    /// compiled into
    /// [`bpt_generated::BREAK_TABLES`](crate::server::record::bpt_generated::BREAK_TABLES)
    /// and seeded here, so a registry cannot exist without them and no load path
    /// can forget to install them. A `.db` that says `field(LINR,"typeKdegC")`
    /// converts.
    ///
    /// DEVIATION FROM C, stated: C's `bptList` starts EMPTY. The tables live in
    /// separate `.dbd` files and an IOC opts in with
    /// `dbLoadDatabase("$(EPICS_BASE)/dbd/bptTypeKdegC.dbd")`; without that line
    /// the C IOC leaves `VAL == RVAL` unconverted and raises `STAT=SOFT`
    /// (measured on the built `softIoc`, 7.0.10.1-DEV: `LINR=typeKdegC`,
    /// `RVAL=1702` gives `VAL=1702`/`STAT=SOFT` with the file unloaded and
    /// `VAL=417.918353467`/`STAT=NO_ALARM` with it loaded). That opt-in is a
    /// packaging decision of C's build — one `.dbd` per curve, so an IOC links
    /// only the curves it uses — not a semantic one, and the port cannot mirror
    /// it: it has no `dbLoadDatabase`, so the `SOFT` arm was the ONLY reachable
    /// one. The bytes seeded here are exactly the bytes C would have loaded, so
    /// the only behaviour that changes is the behaviour that was unreachable.
    ///
    /// First-wins still holds ([`Self::insert`]): a `.db` that defines its own
    /// `breaktable(typeKdegC)` is ignored in favour of the vendored one — which
    /// is what C does too, once the `.dbd` is loaded first.
    pub fn new() -> Self {
        let mut registry = Self {
            tables: BTreeMap::new(),
            extra_menu: Vec::new(),
        };
        for (name, points) in crate::server::record::bpt_generated::BREAK_TABLES {
            let table = BrkTable::build(*name, points).unwrap_or_else(|e| {
                unreachable!("vendored breakpoint table {name} is malformed: {e}")
            });
            registry.insert(table);
        }
        registry
    }

    /// Whether no table DATA has been loaded. The standard `menuConvert` names
    /// always reserve `LINR` 3..=14 regardless; this reports whether any
    /// breakpoint data exists to install into a record.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Insert a table. A redefinition of an existing name is silently IGNORED
    /// (the first-loaded table wins); a genuinely new non-standard name extends
    /// the menu at the next free user index. Append-only menu growth keeps
    /// every existing table's `LINR` index fixed across later loads.
    ///
    /// First-wins matches C: `dbBreakHead` (dbLexRoutines.c:982-985) finds the
    /// name already in `bptList`, sets `duplicate=TRUE`, and returns without
    /// allocating; `dbBreakBody` (:1012-1014) then discards the new points and
    /// keeps the original. A second `breaktable(name){…}` with different data
    /// is therefore a no-op, not an override.
    pub fn insert(&mut self, table: BrkTable) {
        if self.tables.contains_key(&table.name) {
            return;
        }
        // A non-standard name extends the menu past the standard block (C: an
        // IOC adds a menuConvert choice). A standard name fills its reserved
        // slot and does not extend the menu.
        if standard_index_of(&table.name).is_none() && !self.extra_menu.contains(&table.name) {
            self.extra_menu.push(table.name.clone());
        }
        self.tables.insert(table.name.clone(), Arc::new(table));
    }

    /// Look up a table's DATA by name.
    pub fn get(&self, name: &str) -> Option<Arc<BrkTable>> {
        self.tables.get(name).cloned()
    }

    /// The `LINR` index that selects the named table: a standard `menuConvert`
    /// name's fixed index (3..=14, even if its data is not loaded), else a
    /// loaded non-standard table's `LINR_FIRST_USER_TABLE +` load position,
    /// else `None`.
    pub fn linr_index_of(&self, name: &str) -> Option<i16> {
        if let Some(idx) = standard_index_of(name) {
            return Some(idx);
        }
        self.extra_menu
            .iter()
            .position(|n| n == name)
            .map(|pos| LINR_FIRST_USER_TABLE + pos as i16)
    }

    /// The table selected by a `LINR` value (`>= 3`): resolve the index to a
    /// menuConvert name (standard block or user extension) and look up its
    /// loaded DATA. `None` if the index names nothing, or the named table's
    /// data was never loaded (C `dbFindBrkTable` -> NULL -> conversion fails).
    pub fn table_for_linr(&self, linr: i16) -> Option<Arc<BrkTable>> {
        let name = self.name_for_linr(linr)?;
        self.tables.get(name).cloned()
    }

    /// The `menuConvert` name a `LINR` value selects, or `None` if out of the
    /// menu. The inverse of [`Self::linr_index_of`].
    fn name_for_linr(&self, linr: i16) -> Option<&str> {
        if linr < LINR_FIRST_BREAKTABLE {
            return None;
        }
        if linr < LINR_FIRST_USER_TABLE {
            return STANDARD_CONVERT_NAMES
                .get((linr - LINR_FIRST_BREAKTABLE) as usize)
                .copied();
        }
        self.extra_menu
            .get((linr - LINR_FIRST_USER_TABLE) as usize)
            .map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small monotonic-up table resembling a thermocouple curve: raw counts
    /// increase, engineering degrees increase. Slopes: (10-0)/(100-0)=0.1,
    /// (30-10)/(300-100)=0.1, last copies 0.1.
    fn ramp_table() -> BrkTable {
        BrkTable::build("ramp", &[(0.0, 0.0), (100.0, 10.0), (300.0, 30.0)]).unwrap()
    }

    #[test]
    fn build_computes_slopes_and_copies_last() {
        let t = ramp_table();
        assert_eq!(t.points.len(), 3);
        assert!((t.points[0].slope - 0.1).abs() < 1e-12);
        assert!((t.points[1].slope - 0.1).abs() < 1e-12);
        // Last point copies the previous slope (C paBrkInt[number-1].slope).
        assert_eq!(t.points[2].slope, t.points[1].slope);
    }

    #[test]
    fn build_rejects_too_few_points() {
        assert!(BrkTable::build("x", &[(0.0, 0.0)]).is_err());
        assert!(BrkTable::build("x", &[]).is_err());
    }

    #[test]
    fn build_rejects_zero_slope() {
        // Equal eng on consecutive points -> slope 0.
        let e = BrkTable::build("flat", &[(0.0, 5.0), (10.0, 5.0)]).unwrap_err();
        assert!(e.contains("slope is zero"), "{e}");
    }

    #[test]
    fn build_rejects_sign_change() {
        // Up then down -> sign change.
        let e = BrkTable::build("v", &[(0.0, 0.0), (10.0, 10.0), (20.0, 0.0)]).unwrap_err();
        assert!(e.contains("slope changes sign"), "{e}");
    }

    #[test]
    fn raw_to_eng_in_range_interpolates() {
        let t = ramp_table();
        let mut lbrk = 0;
        // raw 50 sits in [0,100]: eng = 0 + (50-0)*0.1 = 5.
        let (eng, status) = cvt_raw_to_eng_bpt(50.0, &t, &mut lbrk);
        assert_eq!(status, BptStatus::InRange);
        assert!((eng - 5.0).abs() < 1e-12, "eng={eng}");
        // raw 200 sits in [100,300]: eng = 10 + (200-100)*0.1 = 20.
        let (eng, status) = cvt_raw_to_eng_bpt(200.0, &t, &mut lbrk);
        assert_eq!(status, BptStatus::InRange);
        assert!((eng - 20.0).abs() < 1e-12, "eng={eng}");
    }

    #[test]
    fn raw_to_eng_above_table_extrapolates_out_of_range() {
        let t = ramp_table();
        let mut lbrk = 0;
        // raw 400 is past the high end (300): extrapolate with the last slope
        // from the last point: eng = 30 + (400-300)*0.1 = 40, status OutOfRange.
        let (eng, status) = cvt_raw_to_eng_bpt(400.0, &t, &mut lbrk);
        assert_eq!(status, BptStatus::OutOfRange);
        assert!((eng - 40.0).abs() < 1e-12, "eng={eng}");
    }

    #[test]
    fn raw_to_eng_below_table_extrapolates_out_of_range() {
        let t = ramp_table();
        let mut lbrk = 0;
        // raw -100 is below the low end (0): extrapolate from the first point:
        // eng = 0 + (-100-0)*0.1 = -10, status OutOfRange.
        let (eng, status) = cvt_raw_to_eng_bpt(-100.0, &t, &mut lbrk);
        assert_eq!(status, BptStatus::OutOfRange);
        assert!((eng + 10.0).abs() < 1e-12, "eng={eng}");
    }

    #[test]
    fn eng_to_raw_is_inverse_in_range() {
        let t = ramp_table();
        let mut lbrk = 0;
        // eng 20 -> raw 200 (inverse of the in-range case above).
        let (raw, status) = cvt_eng_to_raw_bpt(20.0, &t, &mut lbrk);
        assert_eq!(status, BptStatus::InRange);
        assert!((raw - 200.0).abs() < 1e-9, "raw={raw}");
    }

    #[test]
    fn decreasing_raw_table_brackets_correctly() {
        // raw decreases down the table while eng increases (slope negative).
        let t = BrkTable::build("dn", &[(300.0, 0.0), (100.0, 20.0), (0.0, 30.0)]).unwrap();
        let mut lbrk = 0;
        // raw 200 sits between 300 and 100: first interval slope
        // (20-0)/(100-300) = -0.1, eng = 0 + (200-300)*(-0.1) = 10.
        let (eng, status) = cvt_raw_to_eng_bpt(200.0, &t, &mut lbrk);
        assert_eq!(status, BptStatus::InRange);
        assert!((eng - 10.0).abs() < 1e-12, "eng={eng}");
    }

    /// The standard breakpoint-table names MUST equal the `LINR` enum labels
    /// past the three fixed conversions — otherwise the registry index and the
    /// caget `LINR` display would disagree (e.g. `LINR`=3 converting through one
    /// table while the enum shows another). Pins the single source.
    #[test]
    fn standard_names_match_menu_convert_tail() {
        assert_eq!(
            STANDARD_CONVERT_NAMES,
            &crate::server::record::MENU_CONVERT[LINR_FIRST_BREAKTABLE as usize..]
        );
        assert_eq!(LINR_FIRST_USER_TABLE, 15);
    }

    /// A loaded NON-standard table extends the menu past the standard block
    /// (`LINR` 15+) in load order, never colliding with the reserved standard
    /// indices 3..=14.
    #[test]
    fn user_tables_index_from_fifteen_in_load_order() {
        let mut reg = BreakTableRegistry::new();
        // "zeta" loaded first -> first user slot (15); "alpha" -> 16.
        reg.insert(BrkTable::build("zeta", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        reg.insert(BrkTable::build("alpha", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(reg.linr_index_of("zeta"), Some(15));
        assert_eq!(reg.linr_index_of("alpha"), Some(16));
        assert_eq!(reg.linr_index_of("missing"), None);
        assert_eq!(reg.table_for_linr(15).unwrap().name, "zeta");
        assert_eq!(reg.table_for_linr(16).unwrap().name, "alpha");
        assert!(reg.table_for_linr(17).is_none());
        assert!(reg.table_for_linr(2).is_none());
    }

    /// A standard `menuConvert` name binds to its FIXED reserved index
    /// (`typeKdegF`=3 .. `typeSdegC`=14) regardless of load order, and never
    /// extends the user block — so `field(LINR,"typeKdegF")` converts through
    /// the typeKdegF data, not whatever loaded first.
    #[test]
    fn standard_named_table_binds_to_reserved_index() {
        let mut reg = BreakTableRegistry::new();
        // A non-standard table loaded FIRST must not steal index 3.
        reg.insert(BrkTable::build("ramp", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        reg.insert(BrkTable::build("typeKdegF", &[(0.0, 0.0), (1.0, 2.0)]).unwrap());
        assert_eq!(reg.linr_index_of("typeKdegF"), Some(3));
        assert_eq!(reg.linr_index_of("typeSdegC"), Some(14));
        assert_eq!(reg.linr_index_of("ramp"), Some(15));
        assert_eq!(reg.table_for_linr(3).unwrap().name, "typeKdegF");
        assert_eq!(reg.table_for_linr(15).unwrap().name, "ramp");
    }

    /// A reserved standard index with no DATA resolves to no table — exactly
    /// like C `dbFindBrkTable` returning NULL, so the conversion fails rather
    /// than silently using a different table.
    ///
    /// The example used to be `typeKdegF` (3), which now HAS data: EPICS ships
    /// `dbd/bptTypeKdegF.dbd` and the port vendors it. Of the twelve
    /// `menuConvert` breakpoint names, EPICS ships `bpt*.data` for only the four
    /// J/K thermocouple curves; `typeTdegC` (10) is one of the eight it does not,
    /// in C and here.
    #[test]
    fn standard_index_without_data_resolves_to_no_table() {
        let mut reg = BreakTableRegistry::new();
        reg.insert(BrkTable::build("ramp", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(reg.linr_index_of("typeTdegC"), Some(10));
        assert!(reg.table_for_linr(10).is_none());
    }

    /// Stability: a later insert must NOT shift an existing table's index —
    /// the regression behind the wrong-table-across-loads bug.
    #[test]
    fn registry_index_is_stable_across_later_inserts() {
        let mut reg = BreakTableRegistry::new();
        reg.insert(BrkTable::build("zeta", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(reg.linr_index_of("zeta"), Some(15));
        // Loading another user table must leave zeta at 15.
        reg.insert(BrkTable::build("alpha", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(
            reg.linr_index_of("zeta"),
            Some(15),
            "zeta index must not shift"
        );
        assert_eq!(reg.table_for_linr(15).unwrap().name, "zeta");
        assert_eq!(reg.linr_index_of("alpha"), Some(16));
        // A same-name redefinition is IGNORED (C first-wins): index unchanged,
        // ORIGINAL data kept (the new eng=20.0 is discarded).
        reg.insert(BrkTable::build("zeta", &[(0.0, 0.0), (2.0, 20.0)]).unwrap());
        assert_eq!(
            reg.linr_index_of("zeta"),
            Some(15),
            "redefinition keeps index"
        );
        assert_eq!(
            reg.get("zeta").unwrap().points[1].eng,
            1.0,
            "first-wins: original zeta data kept, redefinition discarded"
        );
    }
}
