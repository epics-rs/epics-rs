//! `req_file.c`: the `.req` reader `saveData_Init` takes its PV names from.
//!
//! The file is read whole at open; `pos` plays `ftell`. Two C defects are not
//! copied. `cur` is a `signed char`, so a 0xFF byte reads as `EOF` and ends
//! the section; here only the end does. And at the end of the file `fgetc`
//! leaves `ftell` where it is, so `ftell - 1` lands on the last byte and a
//! last section with no trailing newline lost that byte; here a section
//! bound is the position of the current char, the file length at its end.

use std::path::Path;

/// C `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t'..=b'\r')
}

/// C `isidchar`: every legal PV-name char.
fn is_id_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"_.:-[]<>;".contains(&c)
}

/// One `MACRO`.
struct Macro {
    name: Vec<u8>,
    value: Vec<u8>,
}

/// `initMacros`: `name=value` pairs split at commas, names cut to 8 chars and
/// values to 19, the last pair first.
fn init_macros(macro_: &[u8]) -> Vec<Macro> {
    let mut head = Vec::new();
    let mut m = macro_;
    let end = |m: &[u8]| m.is_empty() || m[0] == 0;
    loop {
        while !end(m) && m[0] == b' ' {
            m = &m[1..];
        }
        if end(m) {
            break;
        }
        let mut name = Vec::new();
        while !end(m) && m[0] != b' ' && m[0] != b'=' && name.len() < 8 {
            name.push(m[0]);
            m = &m[1..];
        }
        if end(m) {
            break;
        }
        while !end(m) && m[0] != b'=' {
            m = &m[1..];
        }
        // C steps past the '=' unchecked, past the NUL when there is none.
        if end(m) {
            break;
        }
        m = &m[1..];
        while !end(m) && m[0] == b' ' {
            m = &m[1..];
        }
        if end(m) {
            break;
        }
        let mut value = Vec::new();
        while !end(m) && m[0] != b' ' && m[0] != b',' && value.len() < 19 {
            value.push(m[0]);
            m = &m[1..];
        }
        head.insert(0, Macro { name, value });
        while !end(m) && m[0] != b',' {
            m = &m[1..];
        }
        if end(m) {
            break;
        }
        m = &m[1..];
    }
    head
}

/// One open `REQ_FILE`.
pub struct ReqFile {
    data: Vec<u8>,
    /// `ftell`.
    pos: usize,
    /// Where `cur` came from: `pos - 1`, or the length once at the end.
    cur_pos: usize,
    cur: Option<u8>,
    list_macros: Vec<Macro>,
    file_name: String,
    sect_name: String,
    sect_start: usize,
    sect_end: Option<usize>,
}

impl ReqFile {
    /// `req_open_file`: `None` when the file cannot be read.
    pub fn open(name: &str, macro_: Option<&str>) -> Option<Self> {
        let data = std::fs::read(Path::new(name)).ok()?;
        let mut rf = Self {
            data,
            pos: 0,
            cur_pos: 0,
            cur: None,
            list_macros: macro_
                .map(|m| init_macros(m.as_bytes()))
                .unwrap_or_default(),
            file_name: name.to_string(),
            sect_name: String::new(),
            sect_start: 0,
            sect_end: None,
        };
        rf.rewind_file();
        Some(rf)
    }

    /// `req_infoMacro`'s text.
    pub fn info_macro(&self) -> String {
        let mut s = format!("file name: {}\nmacros:\n", self.file_name);
        for m in &self.list_macros {
            s += &format!(
                " $({})= {}\n",
                String::from_utf8_lossy(&m.name),
                String::from_utf8_lossy(&m.value)
            );
        }
        s
    }

    /// `req_infoSection`'s text. C's `%c` of `EOF` is a raw 0xFF byte; this
    /// prints U+FFFD.
    pub fn info_section(&self) -> String {
        format!(
            "sect_name   = {}\nsect_start  = {}\nsect_end    = {}\ncurrent char= {}\ncurrent pos = {}\n\n",
            self.sect_name,
            self.sect_start,
            self.sect_end.map_or(-1, |e| e as i64),
            self.cur.map_or('\u{FFFD}', char::from),
            self.pos
        )
    }

    /// `eos`: the end of the section or of the file.
    pub fn eos(&self) -> bool {
        self.cur.is_none()
    }

    /// `current`.
    pub fn current(&self) -> Option<u8> {
        self.cur
    }

    fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// The name between `[` and `]`, at most 20 chars, `cur` left on the
    /// char that stopped it.
    fn read_section_name(&mut self) -> String {
        self.read_char();
        let mut name = Vec::new();
        while let Some(c) = self.cur {
            if c == b']' || name.len() >= 20 {
                break;
            }
            name.push(c);
            self.read_char();
        }
        String::from_utf8_lossy(&name).into_owned()
    }

    /// From the current char to the next `[` line: the section's bounds, then
    /// back to its start.
    fn bound_section(&mut self) {
        self.sect_start = self.cur_pos;
        while !self.eos() && self.cur != Some(b'[') {
            self.skip_line();
        }
        self.sect_end = Some(self.cur_pos);
        self.rewind_sect();
    }

    /// `req_gotoSection`: false when the file has no such section.
    pub fn goto_section(&mut self, section: &str) -> bool {
        self.rewind_file();
        loop {
            while !self.eos() && self.cur != Some(b'[') {
                self.skip_line();
            }
            if self.eos() {
                return false;
            }
            let name = self.read_section_name();
            if self.cur != Some(b']') {
                return false;
            }
            self.skip_line();
            if name == section {
                break;
            }
            if self.eos() {
                return false;
            }
        }
        self.sect_name = section.to_string();
        self.bound_section();
        true
    }

    /// `req_nextSection`: the next section's name, `None` at the last one.
    pub fn next_section(&mut self) -> Option<&str> {
        let end = self.sect_end.take()?;
        self.seek(end);
        self.read_char();
        if self.cur != Some(b'[') {
            return None;
        }
        self.sect_name = self.read_section_name();
        if self.cur != Some(b']') {
            return None;
        }
        self.skip_line();
        self.bound_section();
        Some(&self.sect_name)
    }

    /// `req_getSectName`.
    pub fn sect_name(&self) -> &str {
        &self.sect_name
    }

    /// `req_rewindFile`: section `ALL`, the whole file.
    pub fn rewind_file(&mut self) {
        self.seek(0);
        self.sect_start = 0;
        self.sect_end = None;
        self.sect_name = "ALL".to_string();
        self.read_char();
        self.skip_comment();
    }

    /// `req_rewindSect`.
    pub fn rewind_sect(&mut self) {
        self.seek(self.sect_start);
        self.read_char();
        self.skip_comment();
    }

    /// `req_skipLine`.
    pub fn skip_line(&mut self) -> Option<u8> {
        while !self.eos() && self.cur != Some(b'\n') {
            self.read_char();
        }
        self.skip_comment()
    }

    /// `req_skipSpace`.
    pub fn skip_space(&mut self) -> Option<u8> {
        self.skip_comment()
    }

    /// `req_skipComment`: whitespace, and every line from a `#` on.
    pub fn skip_comment(&mut self) -> Option<u8> {
        while self.cur.is_some_and(is_space) {
            self.read_char();
        }
        while self.cur == Some(b'#') {
            while !self.eos() && self.cur != Some(b'\n') {
                self.read_char();
            }
            while self.cur.is_some_and(is_space) {
                self.read_char();
            }
        }
        self.cur
    }

    /// `req_readChar`.
    pub fn read_char(&mut self) -> Option<u8> {
        if self.sect_end == Some(self.pos) {
            self.cur = None;
            self.cur_pos = self.pos;
        } else {
            self.cur = self.data.get(self.pos).copied();
            self.cur_pos = self.pos;
            if self.cur.is_some() {
                self.pos += 1;
            }
        }
        self.cur
    }

    /// `req_readId`: at most `max - 1` id chars (`max` is C's buffer size).
    pub fn read_id(&mut self, max: usize) -> String {
        let max = max.saturating_sub(1);
        let mut dest = Vec::new();
        self.skip_comment();
        while let Some(c) = self.cur {
            if !is_id_char(c) || dest.len() >= max {
                break;
            }
            dest.push(c);
            self.read_char();
        }
        self.skip_comment();
        String::from_utf8_lossy(&dest).into_owned()
    }

    /// `readMacro`: `$(name)`, `None` when malformed or not defined.
    fn read_macro(&mut self) -> Option<&Macro> {
        self.skip_space();
        if self.cur != Some(b'$') {
            return None;
        }
        self.read_char();
        if self.cur != Some(b'(') {
            return None;
        }
        self.read_char();
        let mut name = Vec::new();
        while let Some(c) = self.cur {
            if c == b')' || name.len() >= 8 {
                break;
            }
            name.push(c);
            self.read_char();
        }
        if self.cur != Some(b')') {
            return None;
        }
        self.read_char();
        self.list_macros.iter().find(|m| m.name == name)
    }

    /// `req_readMacId`: `req_readId` with `$(name)` substituted. `None` is C's
    /// -1, a macro that is not defined; C callers that test `== 0` take it
    /// for a name.
    pub fn read_mac_id(&mut self, max: usize) -> Option<String> {
        let max = max.saturating_sub(1);
        let mut dest = Vec::new();
        self.skip_comment();
        while let Some(c) = self.cur {
            if !(c == b'$' || is_id_char(c)) || dest.len() >= max {
                break;
            }
            if c == b'$' {
                let value = self.read_macro()?.value.clone();
                let room = max - dest.len();
                dest.extend(value.iter().take(room));
            } else {
                dest.push(c);
                self.read_char();
            }
        }
        self.skip_comment();
        Some(String::from_utf8_lossy(&dest).into_owned())
    }

    /// `req_readString`: a quoted string, at most `max - 1` chars; empty when
    /// the current char is not `"`.
    pub fn read_string(&mut self, max: usize) -> String {
        let max = max.saturating_sub(1);
        let mut dest = Vec::new();
        self.skip_comment();
        if self.cur != Some(b'"') {
            return String::new();
        }
        self.read_char();
        while let Some(c) = self.cur {
            if c == b'"' || dest.len() >= max {
                break;
            }
            dest.push(c);
            self.read_char();
        }
        if self.cur == Some(b'"') {
            self.read_char();
        }
        self.skip_comment();
        String::from_utf8_lossy(&dest).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/save_data");

    fn cur(rf: &ReqFile) -> i32 {
        rf.cur.map_or(-1, i32::from)
    }

    /// C's count: -1 for a missing macro, else the length.
    fn count(id: &Option<String>) -> i64 {
        id.as_ref().map_or(-1, |s| s.len() as i64)
    }

    fn stuck(rf: &mut ReqFile, out: &mut String, before: usize) -> usize {
        if rf.pos == before && !rf.eos() {
            writeln!(out, "  stuck at {}", cur(rf)).unwrap();
            rf.read_char();
            rf.skip_comment();
        }
        rf.pos
    }

    /// The walk `tests/fixtures/save_data/req_file.c` prints.
    fn walk(file: &str, macros: &str) -> String {
        let mut out = String::new();
        let mut rf = ReqFile::open(file, Some(macros)).unwrap();
        out += &rf.info_macro().replace(&format!("{FIXTURES}/"), "");
        for s in [
            "prefix",
            "status",
            "message",
            "filename",
            "fullPathName",
            "counter",
            "fileSystem",
            "subdir",
            "basename",
            "realTime1D",
            "noSuch",
            "averyveryveryverylon",
            "noclose",
        ] {
            let found = rf.goto_section(s);
            writeln!(out, "goto {s} = {}", if found { 0 } else { -1 }).unwrap();
            if found {
                let id = rf.read_mac_id(61);
                let eos = i32::from(rf.eos());
                writeln!(
                    out,
                    "  id {} '{}' eos={eos} cur={}",
                    count(&id),
                    id.unwrap_or_default(),
                    cur(&rf)
                )
                .unwrap();
            }
        }
        if rf.goto_section("scanRecord") {
            let mut pos = rf.pos;
            while !rf.eos() {
                let id = rf.read_mac_id(61);
                writeln!(out, "  scan {} '{}'", count(&id), id.unwrap_or_default()).unwrap();
                if rf.current() == Some(b',') {
                    rf.read_char();
                    let id = rf.read_mac_id(61);
                    writeln!(out, "  , {} '{}'", count(&id), id.unwrap_or_default()).unwrap();
                }
                pos = stuck(&mut rf, &mut out, pos);
            }
        }
        if rf.goto_section("extraPV") {
            let mut pos = rf.pos;
            while !rf.eos() {
                let id = rf.read_mac_id(61);
                writeln!(out, "  extra {} '{}'", count(&id), id.unwrap_or_default()).unwrap();
                if rf.current() == Some(b'"') {
                    let desc = rf.read_string(61);
                    writeln!(out, "  desc {} '{desc}'", desc.len()).unwrap();
                }
                pos = stuck(&mut rf, &mut out, pos);
            }
        }
        rf.goto_section("prefix");
        while let Some(name) = rf.next_section() {
            writeln!(out, "next {name}").unwrap();
            let mut pos = rf.pos;
            while !rf.eos() {
                let id = rf.read_id(40);
                writeln!(out, "  {} '{id}'", id.len()).unwrap();
                pos = stuck(&mut rf, &mut out, pos);
            }
        }
        out
    }

    fn fixture(name: &str) -> String {
        format!("{FIXTURES}/{name}")
    }

    #[test]
    fn walk_matches_c_on_save_data_req() {
        assert_eq!(
            walk(&fixture("saveData.req"), "P=xxx:"),
            std::fs::read_to_string(fixture("req_file_saveData.txt")).unwrap()
        );
    }

    #[test]
    fn walk_matches_c_on_edge_cases() {
        assert_eq!(
            walk(
                &fixture("edge.req"),
                "P = ioc:, Q=q,LONGNAMESX=toolongname,BIG=0123456789012345678901234,P=dup:"
            ),
            std::fs::read_to_string(fixture("req_file_edge.txt")).unwrap()
        );
    }

    fn scratch(name: &str, body: &[u8]) -> String {
        let dir = std::env::temp_dir().join(format!("sscan-req-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// C's `ftell - 1` at the end of the file cut the last byte of a last
    /// section with no trailing newline.
    #[test]
    fn last_section_keeps_its_last_byte() {
        let mut rf = ReqFile::open(&scratch("nonl.req", b"[a]\nscan1"), None).unwrap();
        assert!(rf.goto_section("a"));
        assert_eq!(rf.read_mac_id(61).as_deref(), Some("scan1"));
        assert!(rf.eos());
    }

    /// A 0xFF byte is a char, not C's `EOF`.
    #[test]
    fn byte_ff_does_not_end_the_section() {
        let mut rf = ReqFile::open(&scratch("ff.req", b"[a]\n\xffx\nscan1\n"), None).unwrap();
        assert!(rf.goto_section("a"));
        assert_eq!(rf.read_mac_id(61).as_deref(), Some(""));
        assert_eq!(rf.current(), Some(0xff));
    }

    /// A macro with no `=` ends the list (C read past the string's NUL).
    #[test]
    fn macro_without_equals_ends_the_list() {
        let rf = ReqFile::open(&scratch("m.req", b"x\n"), Some("A=1,B")).unwrap();
        assert_eq!(
            rf.info_macro().lines().skip(2).collect::<Vec<_>>(),
            [" $(A)= 1"]
        );
    }
}
