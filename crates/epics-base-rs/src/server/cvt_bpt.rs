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

/// Registry of loaded breakpoint tables in INSERTION (load) order. A table's
/// `LINR` index is `LINR_FIRST_BREAKTABLE + insertion_position` and is STABLE:
/// loading more tables appends them and never shifts an existing table's
/// index, so a record already resolved to a table keeps pointing at the same
/// table after later loads (the three fixed `menuConvert` choices
/// `NO_CONVERSION`/`SLOPE`/`LINEAR` occupy indices 0..=2).
///
/// This mirrors C's stability: `LINR` indexes the STATIC `menuConvert` menu
/// (menuConvert.dbd declaration order) via `findBrkTable`/`papChoiceValue[linr]`
/// (cvtBpt.c:25-39), and only the name->table lookup (`dbFindBrkTable` over
/// `bptList`) is dynamic — a record's index never moves when more tables load.
/// A name-sorted index (the original model here) violated that: adding a table
/// re-sorted the list and silently re-pointed already-resolved records.
///
/// epics-base-rs does not port the standard menuConvert names
/// (`typeKdegF`=3 .. `typeSdegC`=15), so the absolute LINR *value* of a
/// dynamically-loaded table follows load order rather than C's static
/// declaration order; the table is still resolved correctly by name. Porting
/// the standard menu/tables to align the absolute value is a separate gap.
#[derive(Debug, Clone, Default)]
pub struct BreakTableRegistry {
    tables: Vec<Arc<BrkTable>>,
}

/// The first `LINR` value that selects a breakpoint table (after the three
/// fixed `menuConvert` choices `NO_CONVERSION`, `SLOPE`, `LINEAR`).
pub const LINR_FIRST_BREAKTABLE: i16 = 3;

impl BreakTableRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no tables have been registered.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Insert a table. A redefinition of an existing name is silently IGNORED
    /// (the first-loaded table wins); a new name is APPENDED at the next free
    /// index. Append-only growth keeps every existing table's `LINR` index
    /// fixed across later loads.
    ///
    /// First-wins matches C: `dbBreakHead` (dbLexRoutines.c:982-985) finds the
    /// name already in `bptList`, sets `duplicate=TRUE`, and returns without
    /// allocating; `dbBreakBody` (:1012-1014) then discards the new points and
    /// keeps the original. A second `breaktable(name){…}` with different data
    /// is therefore a no-op, not an override.
    pub fn insert(&mut self, table: BrkTable) {
        if self.tables.iter().any(|t| t.name == table.name) {
            return;
        }
        self.tables.push(Arc::new(table));
    }

    /// Look up a table by name.
    pub fn get(&self, name: &str) -> Option<Arc<BrkTable>> {
        self.tables.iter().find(|t| t.name == name).cloned()
    }

    /// The `LINR` index that selects the named table, or `None` if it is not
    /// registered. The index is `3 + insertion_position` and never shifts.
    pub fn linr_index_of(&self, name: &str) -> Option<i16> {
        self.tables
            .iter()
            .position(|t| t.name == name)
            .map(|pos| LINR_FIRST_BREAKTABLE + pos as i16)
    }

    /// The table selected by a `LINR` value (`>= 3`), or `None` if the index
    /// is out of range. The inverse of [`Self::linr_index_of`].
    pub fn table_for_linr(&self, linr: i16) -> Option<Arc<BrkTable>> {
        if linr < LINR_FIRST_BREAKTABLE {
            return None;
        }
        let pos = (linr - LINR_FIRST_BREAKTABLE) as usize;
        self.tables.get(pos).cloned()
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

    #[test]
    fn registry_indexes_in_insertion_order_from_three() {
        let mut reg = BreakTableRegistry::new();
        // Insertion order (NOT name-sorted): "zeta" loaded first -> index 3.
        reg.insert(BrkTable::build("zeta", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        reg.insert(BrkTable::build("alpha", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(reg.linr_index_of("zeta"), Some(3));
        assert_eq!(reg.linr_index_of("alpha"), Some(4));
        assert_eq!(reg.linr_index_of("missing"), None);
        assert_eq!(reg.table_for_linr(3).unwrap().name, "zeta");
        assert_eq!(reg.table_for_linr(4).unwrap().name, "alpha");
        assert!(reg.table_for_linr(5).is_none());
        assert!(reg.table_for_linr(2).is_none());
    }

    /// Stability: a later insert must NOT shift an existing table's index —
    /// the regression behind the wrong-table-across-loads bug. A name-sorted
    /// registry would move "zeta" from 3 to 4 when "alpha" loads.
    #[test]
    fn registry_index_is_stable_across_later_inserts() {
        let mut reg = BreakTableRegistry::new();
        reg.insert(BrkTable::build("zeta", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(reg.linr_index_of("zeta"), Some(3));
        // Loading an alphabetically-earlier table must leave zeta at 3.
        reg.insert(BrkTable::build("alpha", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());
        assert_eq!(
            reg.linr_index_of("zeta"),
            Some(3),
            "zeta index must not shift"
        );
        assert_eq!(reg.table_for_linr(3).unwrap().name, "zeta");
        assert_eq!(reg.linr_index_of("alpha"), Some(4));
        // A same-name redefinition is IGNORED (C first-wins): index unchanged,
        // ORIGINAL data kept (the new eng=20.0 is discarded).
        reg.insert(BrkTable::build("zeta", &[(0.0, 0.0), (2.0, 20.0)]).unwrap());
        assert_eq!(
            reg.linr_index_of("zeta"),
            Some(3),
            "redefinition keeps index"
        );
        assert_eq!(
            reg.get("zeta").unwrap().points[1].eng,
            1.0,
            "first-wins: original zeta data kept, redefinition discarded"
        );
    }
}
