//! C `do_asVerify` (`verify.c`): a `.sav` file's values compared with the
//! live ones, on stdout as the save task prints them, and optionally the live
//! values written out as a restore file.

use std::path::Path;

use crate::calc::engine::cvt::{fmt_f, fmt_g};
use crate::calc::engine::string::{escaped_from_raw, raw_from_escaped};
use crate::calc::engine::strtod::strtod;
use crate::server::database::PvDatabase;
use crate::server::record::record_instance::value_as_dbr_string;
use crate::types::{DbFieldType, EpicsValue};

/// `save_restore.h` `BUF_SIZE`: what one `fgets` reads.
const BUF_SIZE: usize = 200;
/// `PVNAME_STRINGSZ`, with its terminating NUL.
const PVNAME_STRINGSZ: usize = 61;
const MAX_STRING_SIZE: usize = 40;
const FSMALL: f64 = 1.0e-6;
const DSMALL: f64 = 1.0e-8;
const ASVERSION: &str = "asVerify R5.3";
const ARRAY_MARKER: &[u8] = b"@array@";
const ELEMENT: u8 = b'"';
const ESCAPE: u8 = b'\\';

/// C `fgets(s, BUF_SIZE, fp)` over the file's bytes.
struct Lines<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lines<'a> {
    fn fgets(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        let mut n = rest.len().min(BUF_SIZE - 1);
        if let Some(nl) = rest[..n].iter().position(|&c| c == b'\n') {
            n = nl + 1;
        }
        self.pos += n;
        Some(&rest[..n])
    }
}

/// C `atoi`: leading space, a sign, decimal digits, wrapping as `int`.
fn atoi(s: &[u8]) -> i32 {
    let s = &s[s.iter().take_while(|c| c.is_ascii_whitespace()).count()..];
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let v = digits
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .fold(0i64, |v, &c| {
            v.wrapping_mul(10).wrapping_add((c - b'0') as i64)
        });
    (if neg { v.wrapping_neg() } else { v }) as i32
}

fn atof(s: &[u8]) -> f64 {
    strtod(s).value
}

/// `safeDoubleToFloat` (verify.c:454-470).
fn safe_double_to_float(d: f64) -> f32 {
    let abs = d.abs();
    if d == 0.0 {
        0.0
    } else if abs >= f32::MAX as f64 {
        f32::MAX.copysign(d as f32)
    } else if abs <= f32::MIN_POSITIVE as f64 {
        f32::MIN_POSITIVE.copysign(d as f32)
    } else {
        d as f32
    }
}

/// C `strncpy` into a `MAX_STRING_SIZE` slot after `epicsStrnRawFromEscaped`
/// into the same: the raw text, at most 39 bytes, cut at a NUL.
fn element_text(raw: &[u8]) -> Vec<u8> {
    let mut v = raw_from_escaped(raw);
    v.truncate(MAX_STRING_SIZE - 1);
    if let Some(nul) = v.iter().position(|&c| c == 0) {
        v.truncate(nul);
    }
    v
}

/// `read_array` (verify.c:472-629): the element texts of the `@array@` value
/// that starts in `value`, reading on into the file as an element or the
/// array runs past the line.
fn read_array(lines: &mut Lines<'_>, value: &[u8]) -> Vec<Vec<u8>> {
    let mut elements = Vec::new();
    let Some(begin) = value.iter().position(|&c| c == b'{') else {
        // C reads the next line here (:619-623), discarding the PV it holds.
        return elements;
    };
    let mut cur: &[u8] = value;
    let mut i = begin;
    loop {
        // Find the element's beginning, or the array's end.
        loop {
            match cur.get(i) {
                None => match lines.fgets() {
                    Some(line) => (cur, i) = (line, 0),
                    // C dereferences the NULL `fgets` gave it here (:505, :500).
                    None => {
                        println!("read_array: *** EOF during array-parse");
                        return elements;
                    }
                },
                Some(&ELEMENT) => break,
                Some(b'}') => return elements,
                Some(_) => i += 1,
            }
        }
        // Accumulate the element's printable characters.
        i += 1;
        let mut text = Vec::new();
        let mut end_of_file = false;
        while text.len() < MAX_STRING_SIZE - 1 && cur.get(i) != Some(&ELEMENT) {
            if i >= cur.len() {
                match lines.fgets() {
                    Some(line) => (cur, i) = (line, 0),
                    None => {
                        println!("read_array:array_restore: *** premature EOF.");
                        end_of_file = true;
                        break;
                    }
                }
                if cur[0] == ELEMENT {
                    break;
                }
            } else if cur[i] == ESCAPE && matches!(cur.get(i + 1), Some(&ELEMENT | &ESCAPE)) {
                i += 1;
            }
            if (0x20..=0x7e).contains(&cur[i]) {
                text.push(cur[i]);
            }
            i += 1;
        }
        // Pass the element's end, past what did not fit.
        while !end_of_file {
            while i < cur.len() && cur[i] != ELEMENT && cur[i] != ESCAPE {
                i += 1;
            }
            match cur.get(i) {
                Some(&ELEMENT) => {
                    i += 1;
                    break;
                }
                Some(_) => {
                    i += 1;
                    if matches!(cur.get(i), Some(&ELEMENT | &ESCAPE)) {
                        i += 1;
                    }
                }
                None => match lines.fgets() {
                    Some(line) => (cur, i) = (line, 0),
                    None => end_of_file = true,
                },
            }
        }
        elements.push(element_text(&text));
        if end_of_file {
            return elements;
        }
    }
}

/// C `printf("%-*s", width, s)`.
fn left(s: &str, width: usize) -> String {
    format!("{s:<width$}")
}

/// What `ca_array_get` of `element_count` elements returns: the value's
/// elements, the rest of the count zero (`read_array` clears the file's
/// side the same way, :583-590).
fn padded<T: Clone + Default>(mut v: Vec<T>, count: usize) -> Vec<T> {
    v.resize(count, T::default());
    v
}

fn f64s(value: &EpicsValue, count: usize) -> Vec<f64> {
    let v = match value.convert_to(DbFieldType::Double) {
        EpicsValue::Double(d) => vec![d],
        EpicsValue::DoubleArray(a) => a.iter().copied().collect(),
        _ => Vec::new(),
    };
    padded(v, count)
}

fn shorts(value: &EpicsValue, count: usize) -> Vec<i16> {
    let v = match value {
        EpicsValue::Enum(i) | EpicsValue::EnumWithChoices { index: i, .. } => vec![*i as i16],
        EpicsValue::EnumArray(a) => a.iter().map(|&i| i as i16).collect(),
        _ => Vec::new(),
    };
    padded(v, count)
}

/// `DBR_STRING` elements, each cut to `MAX_STRING_SIZE - 1`.
fn strings(value: &EpicsValue, count: usize) -> Vec<Vec<u8>> {
    let cut = |s: &[u8]| s[..s.len().min(MAX_STRING_SIZE - 1)].to_vec();
    let v = if value.is_array() {
        match value.get_convert(DbFieldType::String) {
            Ok(EpicsValue::StringArray(a)) => a.iter().map(|s| cut(s.as_bytes())).collect(),
            _ => Vec::new(),
        }
    } else {
        value_as_dbr_string(value)
            .map(|s| vec![cut(s.as_bytes())])
            .unwrap_or_default()
    };
    padded(v, count)
}

/// `DBR_CHAR` elements as the C string they hold.
fn char_string(value: &EpicsValue) -> Vec<u8> {
    let bytes: Vec<u8> = match value {
        EpicsValue::CharArray(a) | EpicsValue::UCharArray(a) => a.iter().copied().collect(),
        EpicsValue::Char(c) | EpicsValue::UChar(c) => vec![*c],
        _ => Vec::new(),
    };
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    bytes[..end].to_vec()
}

fn array_line(pv: &str, elements: impl Iterator<Item = String>) -> String {
    let mut line = format!("{pv} @array@ {{ ");
    for e in elements {
        line.push_str(&format!("\"{e}\" "));
    }
    line.push_str("}\n");
    line
}

/// C `do_asVerify(fileName, verbose, debug, write_restore_file,
/// restoreFileName)`: the count of PVs that differed plus those not
/// connected, or -1 when the file cannot be read. verbose -1 prints only the
/// problems.
pub async fn do_as_verify(
    db: &PvDatabase,
    file: &Path,
    verbose: i32,
    restore_file: Option<&Path>,
) -> i32 {
    let read = {
        let file = file.to_path_buf();
        crate::runtime::fs::blocking(move || std::fs::read(file)).await
    };
    let Ok(data) = read else {
        println!("asVerify: Can't open '{}'.", file.display());
        return -1;
    };

    let mut trial = restore_file.map(|r| {
        let mut name = r.as_os_str().to_owned();
        name.push("B");
        std::path::PathBuf::from(name)
    });
    let mut fr = String::new();
    if let Some(path) = &trial {
        if crate::runtime::fs::write(path, Vec::new()).await.is_err() {
            println!(
                "asVerify: Can't open restore_file '{}' for writing.",
                path.display()
            );
            trial = None;
        } else {
            fr.push_str(&format!(
                "# {ASVERSION}\tAutomatically generated - DO NOT MODIFY - datetime\n"
            ));
        }
    }

    // :82-94: `<END>` 6 or 7 bytes from the end.
    let len = data.len();
    let end_at = |back: usize| len >= back && &data[len - back..len - back + 5] == b"<END>";
    if !end_at(6) && !end_at(7) {
        if verbose > 0 {
            println!("asVerify: Can't find <END> marker.  File may be bad.");
        }
        if trial.is_some() {
            fr.push_str(
                "# # # Could not find end marker in original .sav file.  File may be bad.\n",
            );
        }
    }

    let (mut num_pvs, mut num_differences, mut num_not_connected) = (0, 0, 0);
    let mut wrote_head = false;
    let mut header = || {
        if !std::mem::replace(&mut wrote_head, true) {
            println!("    PVname saved_value live_value");
            println!("    =============================");
        }
    };
    let mut lines = Lines {
        data: &data,
        pos: 0,
    };
    while let Some(bp) = lines.fgets() {
        if bp[0] == b'#' {
            if bp.windows(13).any(|w| w == b"Search Issued") {
                num_not_connected += 1;
            }
            if trial.is_some() {
                fr.push_str(&String::from_utf8_lossy(bp));
            }
            continue;
        }
        // sscanf(bp, "%80s%c%[^\n\r]", PVname, &c, value_string)
        let start = bp
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(bp.len());
        let name_len = bp[start..]
            .iter()
            .take(80)
            .take_while(|c| !c.is_ascii_whitespace())
            .count();
        if name_len == 0 {
            // C's sscanf assigns nothing, so it checks the previous line's PV
            // again, with an empty saved value.
            continue;
        }
        let name_bytes = &bp[start..start + name_len];
        let rest = bp.get(start + name_len + 1..).unwrap_or_default();
        let value_end = rest
            .iter()
            .position(|&c| c == b'\n' || c == b'\r')
            .unwrap_or(rest.len());
        let mut value: Vec<u8> = rest[..value_end].to_vec();
        if name_len >= PVNAME_STRINGSZ {
            if trial.is_some() {
                fr.push_str("#? ");
                fr.push_str(&String::from_utf8_lossy(bp));
            }
            continue;
        }
        if !name_bytes[0].is_ascii_alphanumeric() {
            continue;
        }
        let pv = String::from_utf8_lossy(name_bytes).into_owned();

        let Ok(live) = db.get_pv(&pv) else {
            println!(
                "asVerify: *** '{pv}' not connected.  Saved value='{}'",
                String::from_utf8_lossy(&value)
            );
            num_not_connected += 1;
            if trial.is_some() {
                fr.push_str(&format!("#'{pv}' not connected\n"));
            }
            continue;
        };
        num_pvs += 1;
        let native = live.dbr_type();
        let is_long_string = native == DbFieldType::Char && pv.ends_with('$');
        let element_count = db
            .restore_target(&pv)
            .map(|t| t.elements as usize)
            .unwrap_or(live.count() as usize)
            .max(1);
        let is_scalar_in_file = !value.starts_with(ARRAY_MARKER);
        let is_scalar = is_scalar_in_file && element_count <= 1;
        let is_string_waveform =
            native == DbFieldType::Char && element_count > 1 && !is_long_string;
        let read = if is_scalar_in_file {
            Vec::new()
        } else {
            padded(read_array(&mut lines, &value), element_count)
        };
        let prefix = |different: bool| if different { "*** " } else { "    " };
        let mismatch = |asverify: &str| {
            println!(
                "{asverify}*** {pv:<25} is {} in file, but {} in ioc.",
                if is_scalar_in_file { "scalar" } else { "array" },
                if is_scalar { "scalar" } else { "array" }
            );
        };
        let array_diffs = |different: usize| {
            format!(
                "{}{} (array) {different} diff{}",
                prefix(different > 0),
                left(&pv, 25),
                if different == 1 { ' ' } else { 's' }
            )
        };

        match native {
            DbFieldType::Float | DbFieldType::Double => {
                let float = native == DbFieldType::Float;
                let live = f64s(&live, element_count);
                // The float branch reads and subtracts in `float`.
                let narrow = |v: f64| if float { v as f32 as f64 } else { v };
                let small = if float { FSMALL } else { DSMALL };
                if is_scalar == is_scalar_in_file {
                    let mut max_diff = 0.0f64;
                    let different = if is_scalar {
                        usize::from(narrow(narrow(atof(&value)) - live[0]).abs() > small)
                    } else {
                        let mut n = 0;
                        for (r, l) in read.iter().zip(&live) {
                            let r = if float {
                                safe_double_to_float(atof(r)) as f64
                            } else {
                                atof(r)
                            };
                            let diff = narrow(r - l).abs();
                            max_diff = max_diff.max(diff);
                            n += usize::from(diff > small);
                        }
                        n
                    };
                    if different > 0 {
                        num_differences += 1;
                    }
                    if different > 0 || verbose > 0 {
                        header();
                        if is_scalar {
                            println!(
                                "{}{} {} {}",
                                prefix(different > 0),
                                left(&pv, 25),
                                left(&fmt_f(narrow(atof(&value)), 6), 25),
                                fmt_f(live[0], 6)
                            );
                        } else if different > 0 {
                            println!("{}, maxDiff={}", array_diffs(different), fmt_f(max_diff, 6));
                        } else {
                            println!("{}", array_diffs(different));
                        }
                    }
                } else {
                    mismatch(if float { "asVerify: " } else { "" });
                }
                if trial.is_some() {
                    let g = |v: f64| fmt_g(v, if float { 7 } else { 14 }, false, false);
                    if is_scalar {
                        fr.push_str(&format!("{pv} {}\n", g(live[0])));
                    } else {
                        fr.push_str(&array_line(&pv, live.iter().map(|&v| g(v))));
                    }
                }
            }
            DbFieldType::Enum => {
                let live = shorts(&live, element_count);
                if is_scalar == is_scalar_in_file {
                    let different = if is_scalar {
                        usize::from(atoi(&value) != live[0] as i32)
                    } else {
                        // C reads these elements as strings (`read_array`'s
                        // switch has no DBF_SHORT arm, :488-493), overrunning
                        // the short buffer, and compares their bytes as shorts;
                        // they are read as the numbers they are here.
                        read.iter()
                            .zip(&live)
                            .filter(|(r, l)| atoi(r) as i16 != **l)
                            .count()
                    };
                    if different > 0 {
                        num_differences += 1;
                    }
                    if different > 0 || verbose > 0 {
                        header();
                        if is_scalar {
                            println!(
                                "{}{} {} {}",
                                prefix(different > 0),
                                left(&pv, 25),
                                left(&atoi(&value).to_string(), 25),
                                live[0]
                            );
                        } else {
                            println!("{}", array_diffs(different));
                        }
                    }
                } else {
                    mismatch("");
                }
                if trial.is_some() {
                    if is_scalar {
                        fr.push_str(&format!("{pv} {}\n", live[0]));
                    } else {
                        fr.push_str(&array_line(&pv, live.iter().map(|v| v.to_string())));
                    }
                }
            }
            _ => {
                let whole = is_long_string || is_string_waveform;
                let live_whole = char_string(&live);
                let live = strings(&live, element_count);
                if whole && !bp.ends_with(b"\n") {
                    // :336-346: one more read fills the value to BUF_SIZE, and
                    // the rest of the line is discarded.
                    if let Some(more) = lines.fgets() {
                        let room = (BUF_SIZE - 1).saturating_sub(value.len());
                        value.extend_from_slice(&more[..more.len().min(room)]);
                        if value.last() == Some(&b'\n') {
                            value.pop();
                        }
                        let mut last = more;
                        while !last.ends_with(b"\n") {
                            match lines.fgets() {
                                Some(line) => last = line,
                                None => break,
                            }
                        }
                    }
                }
                if is_scalar != is_scalar_in_file && !whole {
                    mismatch("");
                } else {
                    let (different, saved) = if whole {
                        let saved = raw_from_escaped(&value);
                        (usize::from(saved != live_whole), saved)
                    } else if is_scalar {
                        let saved = raw_from_escaped(&value);
                        (usize::from(saved != live[0]), saved)
                    } else {
                        let n = read.iter().zip(&live).filter(|(r, l)| r != l).count();
                        (n, Vec::new())
                    };
                    if different > 0 {
                        num_differences += 1;
                    }
                    if different > 0 || verbose > 0 {
                        header();
                        if is_scalar || whole {
                            let shown = if whole { &live_whole } else { &live[0] };
                            let nspace = 24usize.saturating_sub(saved.len()).max(1);
                            println!(
                                "{}{} '{}'{}'{}'",
                                prefix(different > 0),
                                left(&pv, 24),
                                String::from_utf8_lossy(&saved),
                                " ".repeat(nspace),
                                String::from_utf8_lossy(shown)
                            );
                        } else {
                            println!("{}", array_diffs(different));
                        }
                    }
                }
                if trial.is_some() {
                    if whole {
                        let cut = &live_whole[..live_whole.len().min(BUF_SIZE - 1)];
                        fr.push_str(&format!("{pv} {}\n", escaped_from_raw(cut)));
                    } else if is_scalar {
                        fr.push_str(&format!("{pv} {}\n", escaped_from_raw(&live[0])));
                    } else {
                        // :403-422: `"` and `\` escaped, newline and return as
                        // `\n` and `\r`, an escape counting against the 39.
                        fr.push_str(&array_line(
                            &pv,
                            live.iter().map(|s| {
                                let mut e = String::new();
                                let mut j = 0;
                                for &c in s {
                                    if j >= MAX_STRING_SIZE - 1 {
                                        break;
                                    }
                                    if c == ELEMENT || c == ESCAPE {
                                        e.push(ESCAPE as char);
                                        j += 1;
                                    }
                                    match c {
                                        b'\n' => e.push_str("\\n"),
                                        b'\r' => e.push_str("\\r"),
                                        c => e.push(c as char),
                                    }
                                    j += 1;
                                }
                                e
                            }),
                        ));
                    }
                }
            }
        }
    }

    if let (Some(path), Some(restore)) = (&trial, restore_file) {
        fr.push_str("<END>\n");
        let wrote = crate::runtime::fs::write(path, fr.into_bytes())
            .await
            .is_ok();
        if wrote && num_not_connected < num_pvs / 2 {
            let _ = crate::runtime::fs::copy(path, restore).await;
        }
    }

    if verbose >= 0 {
        let s = |n: i32| if n == 1 { " " } else { "s " };
        println!(
            "{num_differences} PV{}differed.  ({num_pvs} PV{}checked; {num_not_connected} PV{}not connected)",
            s(num_differences),
            s(num_pvs),
            s(num_not_connected)
        );
    }
    num_differences + num_not_connected
}
