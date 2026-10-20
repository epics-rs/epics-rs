//! Layout XML front end: libxml2 2.9.4 well-formedness as `verify_xml`
//! (NDFileHDF5LayoutXML.cpp:161) runs it, parser options 0. Only a libxml
//! fatal error fails a document; warnings and namespace errors pass, and no
//! external subset or entity is ever loaded. Productions carry the names of
//! their parser.c functions.
//!
//! Not copied from libxml: a parameter-entity cycle it loops on forever is
//! refused here (`push_cap`), and PE-hook recursion is capped.

use std::collections::HashMap;
use std::rc::Rc;

use super::{LayoutError, Token};

struct Fatal(&'static str);
type R<T = ()> = Result<T, Fatal>;

const LOOP: &str = "entity reference loop";
const MAX_NAME: usize = 50000;
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Where `content` stopped.
enum Stop {
    StartTag,
    EndTag,
    End,
}

/// An `xmlParseEntityRef` result.
enum Ref {
    Predefined(char),
    Entity(usize, String),
    Undeclared,
}

/// libxml `IS_BYTE_CHAR`.
fn byte_char(c: char) -> bool {
    c >= ' ' || matches!(c, '\t' | '\n' | '\r')
}

fn fail<T>(msg: &'static str) -> R<T> {
    Err(Fatal(msg))
}

fn blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// libxml `IS_CHAR`.
fn is_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Prolog,
    Dtd,
    EntityDecl,
    EntityValue,
    AttrValue,
    Content,
    Comment,
    Pi,
    SystemLiteral,
    PublicLiteral,
    CData,
    Ignore,
    Epilog,
}

struct Input {
    buf: Rc<[char]>,
    pos: usize,
    /// `xmlNewBlanksWrapperInputStream`'s " %name; ".
    wrapper: bool,
    /// Buffer identity for progress checks: every input over one entity's
    /// content shares it, as their `cur` pointers do in C.
    src: u32,
    id: u32,
    /// Byte offsets equal char offsets.
    ascii: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Internal,
    External,
    Unparsed,
    InternalPe,
    ExternalPe,
}

struct Entity {
    kind: Kind,
    content: Option<Rc<[char]>>,
    src: u32,
    checked: u64,
    children: bool,
    ascii: bool,
}

struct Parser {
    inputs: Vec<Input>,
    ids: u32,
    state: State,
    standalone: bool,
    has_ext: bool,
    has_perefs: bool,
    in_subset: u8,
    depth: u32,
    nbentities: u64,
    ents: Vec<Entity>,
    general: HashMap<String, usize>,
    params: HashMap<String, usize>,
    /// attsSpecial: (element, attribute) QNames -> declared CDATA.
    special: HashMap<(String, String), bool>,
    open: Vec<String>,
    emit: bool,
    tokens: Vec<Token>,
    hooks: u32,
    conds: u32,
    pushes: u64,
    /// libxml re-pushes a PE in a 3-cycle forever; no document that ends
    /// pushes PE content more often than this.
    push_cap: u64,
    /// Some(little-endian) when the document was UTF-16.
    utf16: Option<bool>,
    /// The XML declaration named an encoding libxml switches to.
    input_encoding: bool,
}

/// Check `bytes` and return its element tokens (outer document only).
pub(super) fn parse(bytes: &[u8]) -> Result<Vec<Token>, LayoutError> {
    let mut p = Parser::default();
    p.run(bytes).map_err(not_well_formed)?;
    let mut tokens = std::mem::take(&mut p.tokens);
    for t in &mut tokens {
        if let Token::Open { attrs, .. } = t {
            for (_, v) in attrs.iter_mut().filter(|(_, v)| v.contains('&')) {
                let chars: Vec<char> = v.chars().collect();
                let mut out = String::new();
                p.expand(&chars, 0, &mut out)?;
                *v = out;
            }
        }
    }
    Ok(tokens)
}

/// libxml well-formedness only.
pub(super) fn check(bytes: &[u8]) -> Result<(), LayoutError> {
    Parser::default().run(bytes).map_err(not_well_formed)
}

fn not_well_formed(Fatal(msg): Fatal) -> LayoutError {
    LayoutError(format!("XML not well-formed: {msg}"))
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            inputs: Vec::new(),
            ids: 0,
            state: State::Start,
            standalone: false,
            has_ext: false,
            has_perefs: false,
            in_subset: 0,
            depth: 0,
            nbentities: 0,
            ents: Vec::new(),
            general: HashMap::new(),
            params: HashMap::new(),
            special: HashMap::new(),
            open: Vec::new(),
            emit: true,
            tokens: Vec::new(),
            hooks: 0,
            conds: 0,
            pushes: 0,
            push_cap: 0,
            utf16: None,
            input_encoding: false,
        }
    }
}

impl Parser {
    fn run(&mut self, bytes: &[u8]) -> R {
        let doc = self.decode_input(bytes)?;
        self.push_cap = 2 * doc.len() as u64 + 100_000;
        let src = self.fresh();
        let ascii = doc.iter().all(char::is_ascii);
        self.push(doc.into(), false, src, ascii);
        self.document()
    }

    fn fresh(&mut self) -> u32 {
        self.ids += 1;
        self.ids
    }

    /// `xmlPushInput`: push, then GROW (an empty input pops at once).
    fn push(&mut self, buf: Rc<[char]>, wrapper: bool, src: u32, ascii: bool) {
        let id = self.fresh();
        self.inputs.push(Input {
            buf,
            pos: 0,
            wrapper,
            src,
            id,
            ascii,
        });
        self.grow();
    }

    fn push_wrapper(&mut self, name: &str) {
        let buf: Rc<[char]> = format!(" %{name}; ").chars().collect();
        let src = self.fresh();
        self.push(buf, true, src, name.is_ascii());
    }

    fn push_pe(&mut self, e: usize) -> R {
        self.pushes += 1;
        if self.pushes > self.push_cap {
            return fail("parameter entity loop");
        }
        let ent = &self.ents[e];
        let buf = ent.content.clone().unwrap_or_else(|| Rc::from([]));
        let (src, ascii) = (ent.src, ent.ascii);
        self.push(buf, false, src, ascii);
        Ok(())
    }

    fn top(&self) -> &Input {
        self.inputs.last().expect("input stack is never empty")
    }

    fn raw(&self) -> char {
        self.nxt(0)
    }

    fn nxt(&self, n: usize) -> char {
        let i = self.top();
        i.buf.get(i.pos + n).copied().unwrap_or('\0')
    }

    fn at_end(&self) -> bool {
        let i = self.top();
        i.pos >= i.buf.len()
    }

    fn here(&self) -> (u32, usize) {
        let i = self.top();
        (i.src, i.pos)
    }

    fn id(&self) -> u32 {
        self.top().id
    }

    fn looking_at(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(k, c)| self.nxt(k) == c)
    }

    fn advance(&mut self, n: usize) {
        let i = self.inputs.last_mut().expect("input stack is never empty");
        i.pos = (i.pos + n).min(i.buf.len());
    }

    /// `xmlPopInput`: pop, and keep popping inputs that are used up.
    fn pop_input(&mut self) {
        while self.inputs.len() > 1 {
            self.inputs.pop();
            if !self.at_end() {
                break;
            }
        }
    }

    /// `xmlNextChar` (NEXT).
    fn next(&mut self) -> R {
        if self.at_end() && self.state != State::Comment {
            self.pop_input();
        } else {
            self.advance(1);
        }
        self.after_move()
    }

    fn after_move(&mut self) -> R {
        if self.raw() == '%' {
            self.hook()?;
        }
        if self.at_end() {
            self.pop_input();
        }
        Ok(())
    }

    /// SKIP(n).
    fn skip(&mut self, n: usize) -> R {
        self.advance(n);
        self.after_move()
    }

    /// NEXTL.
    fn nextl(&mut self) -> R {
        self.advance(1);
        if self.raw() == '%' {
            self.hook()?;
        }
        Ok(())
    }

    /// GROW.
    fn grow(&mut self) {
        if self.at_end() {
            self.pop_input();
        }
    }

    /// SHRINK: pops only an input used up past its first 500 bytes.
    fn shrink(&mut self) {
        let i = self.top();
        if self.inputs.len() > 1
            && self.at_end()
            && (i.pos > 500 || i.buf[..i.pos].iter().map(|c| c.len_utf8()).sum::<usize>() > 500)
        {
            self.pop_input();
        }
    }

    /// `xmlSkipBlankChars` (SKIP_BLANKS).
    fn skip_blanks(&mut self) -> R<usize> {
        let mut res = 0;
        if self.inputs.len() == 1 && self.state != State::Dtd {
            while blank(self.raw()) {
                self.advance(1);
                res += 1;
            }
            return Ok(res);
        }
        loop {
            let mut cur = self.raw();
            while blank(cur) {
                self.next()?;
                cur = self.raw();
                res += 1;
            }
            while cur == '\0' && self.inputs.len() > 1 && self.state != State::Comment {
                self.pop_input();
                cur = self.raw();
            }
            if self.raw() == '%' {
                self.hook()?;
            }
            // C tests the char read before the hook ran.
            if !blank(cur) {
                return Ok(res);
            }
        }
    }

    /// `xmlParserHandlePEReference`, run by NEXT, SKIP and NEXTL.
    fn hook(&mut self) -> R {
        if self.raw() != '%' {
            return Ok(());
        }
        match self.state {
            State::Start | State::Prolog | State::Epilog => {
                return fail("PEReference in prolog or epilog");
            }
            State::Dtd if self.inputs.len() > 1 && !blank(self.nxt(1)) && self.nxt(1) != '\0' => {}
            _ => return Ok(()),
        }
        if self.hooks >= 256 {
            return fail("PEReference nesting too deep");
        }
        self.hooks += 1;
        let r = self.hook_body();
        self.hooks -= 1;
        r
    }

    fn hook_body(&mut self) -> R {
        self.next()?;
        let Some(name) = self.name()? else {
            return fail("PEReference: no name");
        };
        if self.raw() != ';' {
            return fail("PEReference: expecting ';'");
        }
        self.next()?;
        let Some(&e) = self.params.get(&name) else {
            return self.undeclared();
        };
        if !self.top().wrapper {
            self.push_wrapper(&name);
            return Ok(());
        }
        if self.ents[e].kind == Kind::ExternalPe {
            return Ok(());
        }
        self.push_pe(e)?;
        self.grow();
        // xmlSwitchEncoding skips a UTF-8 BOM when 4 bytes are left.
        let i = self.top();
        if i.buf.len() - i.pos >= 2 && self.raw() == '\u{FEFF}' {
            self.advance(1);
        }
        Ok(())
    }

    /// WFC Entity Declared: fatal unless the DTD may declare it elsewhere.
    fn undeclared(&mut self) -> R {
        if self.standalone || (!self.has_ext && !self.has_perefs) {
            return fail("entity not defined");
        }
        self.entity_check(0, None)
    }
}

// --- document -----------------------------------------------------------------

impl Parser {
    /// `xmlDetectCharEncoding`, the declared encoding and end-of-line
    /// handling, up front: libxml fails on every byte it cannot decode and
    /// on every non-`IS_CHAR` it reads.
    fn decode_input(&mut self, bytes: &[u8]) -> R<Vec<char>> {
        let mut b = bytes;
        if b.len() >= 4 {
            match b[..4] {
                [0, 0, 0, 0x3C]
                | [0x3C, 0, 0, 0]
                | [0, 0, 0x3C, 0]
                | [0, 0x3C, 0, 0]
                | [0x4C, 0x6F, 0xA7, 0x94] => return fail("unsupported encoding"),
                [0x3C, 0, 0x3F, 0] => self.utf16 = Some(true),
                [0, 0x3C, 0, 0x3F] => self.utf16 = Some(false),
                [0xEF, 0xBB, 0xBF, _] => b = &b[3..],
                [0xFE, 0xFF, _, _] => (self.utf16, b) = (Some(false), &b[2..]),
                [0xFF, 0xFE, _, _] => (self.utf16, b) = (Some(true), &b[2..]),
                _ => {}
            }
        }
        let raw: Vec<char> = match self.utf16 {
            Some(le) => {
                let units = b.as_chunks::<2>().0.iter().map(|&p| {
                    if le {
                        u16::from_le_bytes(p)
                    } else {
                        u16::from_be_bytes(p)
                    }
                });
                char::decode_utf16(units)
                    .collect::<Result<_, _>>()
                    .map_err(|_| Fatal("invalid UTF-16"))?
            }
            None => decode_8bit(b)?,
        };
        let mut out = Vec::with_capacity(raw.len());
        let mut it = raw.into_iter().peekable();
        while let Some(c) = it.next() {
            if c == '\r' {
                it.next_if_eq(&'\n');
                out.push('\n');
            } else if is_char(c) {
                out.push(c);
            } else {
                return fail("invalid character");
            }
        }
        Ok(out)
    }

    /// `xmlParseDocument`.
    fn document(&mut self) -> R {
        if self.at_end() {
            return fail("Document is empty");
        }
        if self.looking_at("<?xml") && blank(self.nxt(5)) {
            self.xml_decl()?;
            self.skip_blanks()?;
        }
        self.misc()?;
        if self.looking_at("<!DOCTYPE") {
            self.in_subset = 1;
            self.doctype_decl()?;
            if self.raw() == '[' {
                self.state = State::Dtd;
                self.internal_subset()?;
            }
            self.in_subset = 0;
            // xmlCleanSpecialAttr
            self.special.retain(|_, cdata| !*cdata);
            self.state = State::Prolog;
            self.misc()?;
        }
        self.grow();
        if self.raw() != '<' {
            return fail("Start tag expected, '<' not found");
        }
        self.state = State::Content;
        self.element()?;
        self.state = State::Epilog;
        self.misc()?;
        if !self.at_end() {
            return fail("Extra content at the end of the document");
        }
        Ok(())
    }

    /// `xmlParseXMLDecl`.
    fn xml_decl(&mut self) -> R {
        self.skip(5)?;
        if !blank(self.raw()) {
            return fail("Blank needed after '<?xml'");
        }
        self.skip_blanks()?;
        let Some(version) = self.version_info()? else {
            return fail("Malformed declaration expecting version");
        };
        // XML 1.0 5th edition: other 1.x versions only warn.
        if !version.starts_with("1.") {
            return fail("Unsupported version");
        }
        if !blank(self.raw()) {
            if self.looking_at("?>") {
                return self.skip(2);
            }
            return fail("Blank needed here");
        }
        self.encoding_decl()?;
        if self.input_encoding && !blank(self.raw()) {
            if self.looking_at("?>") {
                return self.skip(2);
            }
            return fail("Blank needed here");
        }
        self.grow();
        self.skip_blanks()?;
        self.standalone = self.sd_decl()?;
        self.skip_blanks()?;
        if self.looking_at("?>") {
            return self.skip(2);
        }
        fail("parsing XML declaration: '?>' expected")
    }

    /// `Eq` and the opening quote of a declaration value: returns the quote.
    fn eq_quote(&mut self) -> R<char> {
        self.skip_blanks()?;
        if self.raw() != '=' {
            return fail("'=' required");
        }
        self.next()?;
        self.skip_blanks()?;
        let q = self.raw();
        if q != '"' && q != '\'' {
            return fail("String not started expecting ' or \"");
        }
        self.next()?;
        Ok(q)
    }

    fn close_quote(&mut self, q: char) -> R {
        if self.raw() != q {
            return fail("String not closed expecting \" or '");
        }
        self.next()
    }

    /// `xmlParseVersionInfo` + `xmlParseVersionNum`.
    fn version_info(&mut self) -> R<Option<String>> {
        if !self.looking_at("version") {
            return Ok(None);
        }
        self.skip(7)?;
        let q = self.eq_quote()?;
        let mut v = String::new();
        if self.raw().is_ascii_digit() {
            v.push(self.raw());
            self.next()?;
            if self.raw() == '.' {
                v.push('.');
                self.next()?;
                while self.raw().is_ascii_digit() {
                    v.push(self.raw());
                    self.next()?;
                }
            }
        }
        self.close_quote(q)?;
        Ok(v.contains('.').then_some(v))
    }

    /// `xmlParseEncodingDecl`. `decode` already applied an 8-bit encoding.
    fn encoding_decl(&mut self) -> R {
        self.skip_blanks()?;
        if !self.looking_at("encoding") {
            return Ok(());
        }
        self.skip(8)?;
        let q = self.eq_quote()?;
        let mut name = String::new();
        if !self.raw().is_ascii_alphabetic() {
            return fail("Invalid XML encoding name");
        }
        while self.raw().is_ascii_alphanumeric() || matches!(self.raw(), '.' | '_' | '-') {
            name.push(self.raw());
            self.next()?;
        }
        self.close_quote(q)?;
        let name = name.to_ascii_uppercase();
        if let Some(le) = self.utf16 {
            let ok = match name.as_str() {
                "UTF-16" | "UTF16" | "UTF-8" | "UTF8" => true,
                "UTF-16LE" => le,
                "UTF-16BE" => !le,
                _ => false,
            };
            if !ok {
                return fail("encoding switch on UTF-16 input");
            }
        }
        self.input_encoding = !matches!(name.as_str(), "UTF-8" | "UTF8" | "UTF-16" | "UTF16");
        Ok(())
    }

    /// `xmlParseSDDecl`: standalone="yes".
    fn sd_decl(&mut self) -> R<bool> {
        self.skip_blanks()?;
        if !self.looking_at("standalone") {
            return Ok(false);
        }
        self.skip(10)?;
        let q = self.eq_quote()?;
        let yes = if self.looking_at("no") {
            self.skip(2)?;
            false
        } else if self.looking_at("yes") {
            self.skip(3)?;
            true
        } else {
            return fail("standalone accepts only 'yes' or 'no'");
        };
        self.close_quote(q)?;
        Ok(yes)
    }

    /// `xmlParseMisc`.
    fn misc(&mut self) -> R {
        loop {
            if self.looking_at("<?") {
                self.pi()?;
            } else if self.looking_at("<!--") {
                self.comment()?;
            } else if blank(self.raw()) {
                self.next()?;
            } else {
                return Ok(());
            }
        }
    }

    /// `xmlParsePI`.
    fn pi(&mut self) -> R {
        let saved = self.state;
        self.state = State::Pi;
        let id = self.id();
        self.skip(2)?;
        self.shrink();
        let Some(target) = self.name()? else {
            return fail("xmlParsePI : no target name");
        };
        if target.len() == 3 && target.eq_ignore_ascii_case("xml") {
            return fail("XML declaration allowed only at the start of the document");
        }
        if !self.looking_at("?>") {
            if !blank(self.raw()) {
                return fail("ParsePI: PI space expected");
            }
            self.skip_blanks()?;
            let mut c = self.raw();
            while is_char(c) && !(c == '?' && self.nxt(1) == '>') {
                self.nextl()?;
                c = self.raw();
                if c == '\0' {
                    self.shrink();
                    self.grow();
                    c = self.raw();
                }
            }
            if c != '?' {
                return fail("ParsePI: PI never end");
            }
        }
        if self.id() != id {
            return fail("PI declaration doesn't start and stop in the same entity");
        }
        self.skip(2)?;
        self.state = saved;
        Ok(())
    }

    /// `xmlParseComment`: it can only end in the input it started in.
    fn comment(&mut self) -> R {
        if !self.looking_at("<!--") {
            return Ok(());
        }
        let saved = self.state;
        self.state = State::Comment;
        let id = self.id();
        self.skip(4)?;
        let i = self.top();
        let rest = &i.buf[i.pos..];
        let Some(k) = rest.windows(2).position(|w| w == ['-', '-']) else {
            return fail("Comment not terminated");
        };
        if self.id() != id {
            return fail("comment doesn't start and stop in the same entity");
        }
        if rest.get(k + 2) != Some(&'>') {
            return fail("Double hyphen within comment");
        }
        self.skip(k + 3)?;
        self.state = saved;
        Ok(())
    }

    /// `xmlParseElement` + `xmlParseContent`, iteratively.
    fn element(&mut self) -> R {
        // A balanced chunk's pseudo root counts toward the depth limit.
        let max = if self.emit { 256 } else { 255 };
        let floor = self.open.len();
        loop {
            if self.open.len() > max {
                return fail("Excessive depth in document");
            }
            let (name, attrs) = self.start_tag()?;
            let empty = self.looking_at("/>");
            if !empty && self.raw() != '>' {
                return fail("Couldn't find end of Start Tag");
            }
            if self.emit {
                self.tokens.push(Token::Open {
                    name: name.clone(),
                    attrs,
                    self_closing: empty,
                });
            }
            if empty {
                self.skip(2)?;
            } else {
                self.advance(1);
                self.open.push(name);
            }
            loop {
                if self.open.len() == floor {
                    return Ok(());
                }
                match self.content()? {
                    Stop::StartTag => break,
                    Stop::EndTag => self.end_tag()?,
                    Stop::End => return fail("Premature end of data in tag"),
                }
            }
        }
    }

    /// `xmlParseContent` up to a start tag, an end tag or the end.
    fn content(&mut self) -> R<Stop> {
        self.grow();
        loop {
            if self.at_end() {
                return Ok(Stop::End);
            }
            if self.looking_at("</") {
                return Ok(Stop::EndTag);
            }
            let test = self.here();
            if self.looking_at("<?") {
                self.pi()?;
            } else if self.looking_at("<![CDATA[") {
                self.cdsect()?;
            } else if self.looking_at("<!--") {
                self.comment()?;
                self.state = State::Content;
            } else if self.raw() == '<' {
                return Ok(Stop::StartTag);
            } else if self.raw() == '&' {
                self.reference()?;
            } else {
                self.char_data()?;
            }
            self.grow();
            self.shrink();
            if self.here() == test {
                return fail("detected an error in element content");
            }
        }
    }

    /// `xmlParseStartTag2`: the element QName as written and its attributes.
    fn start_tag(&mut self) -> R<(String, Vec<(String, String)>)> {
        self.advance(1);
        let Some(q) = self.qname()? else {
            return fail("StartTag: invalid element name");
        };
        let elem = qname_text(&q);
        self.skip_blanks()?;
        self.grow();
        let mut attrs = Vec::new();
        let mut plain: Vec<String> = Vec::new();
        let mut ns: Vec<Option<String>> = Vec::new();
        while self.raw() != '>' && !self.looking_at("/>") && byte_char(self.raw()) {
            let (aq, value) = self.attribute(&elem)?;
            let raw = qname_text(&aq);
            let special = |v: &str| v == XML_NS || v == XMLNS_NS;
            match (aq.0.as_deref(), aq.1.as_str()) {
                (None, "xmlns") => {
                    if value.is_empty() || !special(&value) {
                        if ns.contains(&None) {
                            return fail("Attribute xmlns redefined");
                        }
                        ns.push(None);
                    }
                }
                (Some("xmlns"), p) => {
                    if !(p == "xml" || p == "xmlns" || value.is_empty() || special(&value)) {
                        let p = Some(p.to_string());
                        if ns.contains(&p) {
                            return fail("Attribute xmlns:prefix redefined");
                        }
                        ns.push(p);
                    }
                }
                _ => {
                    if plain.contains(&raw) {
                        return fail("Attribute redefined");
                    }
                    plain.push(raw.clone());
                }
            }
            attrs.push((raw, value));
            self.grow();
            if self.raw() == '>' || self.looking_at("/>") {
                break;
            }
            if !blank(self.raw()) {
                return fail("attributes construct error");
            }
            self.skip_blanks()?;
        }
        Ok((elem, attrs))
    }

    /// `xmlParseAttribute2`: the QName and the value as libxml keeps it.
    fn attribute(&mut self, elem: &str) -> R<((Option<String>, String), String)> {
        self.grow();
        let Some(q) = self.qname()? else {
            return fail("error parsing attribute name");
        };
        let normalize = self
            .special
            .contains_key(&(elem.to_string(), qname_text(&q)));
        self.skip_blanks()?;
        if self.raw() != '=' {
            return fail("Specification mandate value for attribute");
        }
        self.next()?;
        self.skip_blanks()?;
        let v = self.att_value(normalize)?;
        self.state = State::Content;
        Ok((q, v))
    }

    /// `xmlParseAttValueComplex`, plus `xmlAttrNormalizeSpace2` when the
    /// attribute is declared non-CDATA. Entity references stay `&name;`;
    /// a `&` from a reference becomes `&#38;`.
    fn att_value(&mut self, normalize: bool) -> R<String> {
        let limit = self.raw();
        if limit != '"' && limit != '\'' {
            return fail("AttValue: \" or ' expected");
        }
        self.state = State::AttrValue;
        self.next()?;
        let mut buf = String::new();
        let mut in_space = false;
        let mut c = self.raw();
        while c != limit && is_char(c) && c != '<' {
            if c == '&' {
                in_space = false;
                if self.nxt(1) == '#' {
                    match self.char_ref()? {
                        '&' => buf.push_str("&#38;"),
                        ch => buf.push(ch),
                    }
                } else {
                    let r = self.entity_ref()?;
                    self.nbentities += 1;
                    match r {
                        Ref::Predefined('&') => buf.push_str("&#38;"),
                        Ref::Predefined(ch) => buf.push(ch),
                        Ref::Entity(e, name) => {
                            if self.ents[e].children {
                                self.nbentities += 1;
                            }
                            if self.ents[e].content.is_some() && self.ents[e].checked == 0 {
                                self.check_content(e, false)?;
                            }
                            buf.push('&');
                            buf.push_str(&name);
                            buf.push(';');
                        }
                        Ref::Undeclared => {}
                    }
                }
            } else {
                if blank(c) {
                    if !buf.is_empty() || !normalize {
                        if !normalize || !in_space {
                            buf.push(' ');
                        }
                        in_space = true;
                    }
                } else {
                    in_space = false;
                    buf.push(c);
                }
                self.nextl()?;
            }
            self.grow();
            c = self.raw();
        }
        if self.raw() == '<' {
            return fail("Unescaped '<' not allowed in attributes values");
        }
        if self.raw() != limit {
            return fail("AttValue: ' expected");
        }
        self.next()?;
        if normalize {
            buf = buf
                .split(' ')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
        }
        Ok(buf)
    }

    /// `xmlParseCharRef`, including its digit counter: a hex letter is
    /// refused where the counter reaches 20.
    fn char_ref(&mut self) -> R<char> {
        let hex = self.looking_at("&#x");
        self.skip(if hex { 3 } else { 2 })?;
        self.grow();
        let (mut val, mut count, mut out) = (0u32, 0u32, false);
        while self.raw() != ';' {
            count += 1;
            if count > 21 {
                count = 0;
                self.grow();
            }
            let d = match self.raw() {
                c @ '0'..='9' => c as u32 - '0' as u32,
                c @ ('a'..='f' | 'A'..='F') if hex && count < 20 => {
                    c.to_ascii_lowercase() as u32 - 'a' as u32 + 10
                }
                _ => return fail("xmlParseCharRef: invalid value"),
            };
            val = val.wrapping_mul(if hex { 16 } else { 10 }).wrapping_add(d);
            out |= val > 0x10FFFF;
            self.next()?;
            count += 1;
        }
        self.advance(1);
        match char::from_u32(val).filter(|&c| is_char(c) && !out) {
            Some(c) => Ok(c),
            None => fail("xmlParseCharRef: invalid xmlChar value"),
        }
    }

    /// `xmlParseCDSect`.
    fn cdsect(&mut self) -> R {
        self.skip(9)?;
        self.state = State::CData;
        let r = self.raw();
        if !is_char(r) {
            return fail("CData section not finished");
        }
        self.nextl()?;
        let s = self.raw();
        if !is_char(s) {
            return fail("CData section not finished");
        }
        self.nextl()?;
        let (mut r, mut s, mut cur) = (r, s, self.raw());
        while is_char(cur) && !(r == ']' && s == ']' && cur == '>') {
            (r, s) = (s, cur);
            self.nextl()?;
            cur = self.raw();
        }
        self.state = State::Content;
        if cur != '>' {
            return fail("CData section not finished");
        }
        self.nextl()
    }

    /// `xmlParseCharData`.
    fn char_data(&mut self) -> R {
        let i = self.top();
        let rest = &i.buf[i.pos..];
        let n = rest
            .iter()
            .position(|&c| c == '<' || c == '&')
            .unwrap_or(rest.len());
        if rest[..n].windows(3).any(|w| w == [']', ']', '>']) {
            return fail("Sequence ']]>' not allowed in content");
        }
        self.advance(n);
        Ok(())
    }

    /// `xmlParseEndTag2`: `</` + the open QName as written, S?, `>`.
    fn end_tag(&mut self) -> R {
        self.skip(2)?;
        let name = self.open.pop().expect("an element is open");
        if !self.looking_at(&name) {
            return fail("Opening and ending tag mismatch");
        }
        self.advance(name.chars().count());
        self.skip_blanks()?;
        if self.raw() != '>' {
            return fail("expected '>'");
        }
        self.advance(1);
        if self.emit {
            self.tokens.push(Token::Close(name));
        }
        Ok(())
    }
}

// --- names ----------------------------------------------------------------

fn name_start(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '_' | ':'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

fn name_char(c: char) -> bool {
    name_start(c)
        || matches!(c, '0'..='9' | '-' | '.' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

fn too_long(s: &str) -> R {
    if s.len() > MAX_NAME {
        return fail("name too long");
    }
    Ok(())
}

/// `xmlParseStringName` over `s[*at..]`.
fn string_name(s: &[char], at: &mut usize) -> R<Option<String>> {
    if !s.get(*at).is_some_and(|&c| name_start(c)) {
        return Ok(None);
    }
    let n = s[*at..].iter().take_while(|&&c| name_char(c)).count();
    let name: String = s[*at..*at + n].iter().collect();
    too_long(&name)?;
    *at += n;
    Ok(Some(name))
}

/// A QName as written: `xmlParseQName`'s prefix and local part rejoined.
fn qname_text(q: &(Option<String>, String)) -> String {
    match &q.0 {
        Some(p) => format!("{p}:{}", q.1),
        None => q.1.clone(),
    }
}

impl Parser {
    /// `xmlParseName`. The ASCII fast path runs no PE hook; the complex
    /// path reads with NEXTL, which may.
    fn name(&mut self) -> R<Option<String>> {
        self.grow();
        let i = self.top();
        let rest = &i.buf[i.pos..];
        if rest
            .first()
            .is_some_and(|&c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        {
            let n = rest
                .iter()
                .take_while(|&&c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
                .count();
            if rest.get(n).is_some_and(char::is_ascii) {
                let s: String = rest[..n].iter().collect();
                too_long(&s)?;
                self.advance(n);
                return Ok(Some(s));
            }
        }
        let mut c = self.raw();
        if !name_start(c) {
            return Ok(None);
        }
        let id = self.id();
        let mut s = String::new();
        while name_char(c) {
            s.push(c);
            self.nextl()?;
            c = self.raw();
        }
        too_long(&s)?;
        // C reads the name back from the current buffer.
        let i = self.top();
        if self.id() != id && i.buf[..i.pos].iter().map(|c| c.len_utf8()).sum::<usize>() < s.len() {
            return fail("unexpected change of input buffer");
        }
        Ok(Some(s))
    }

    /// `xmlParseNCName`; only content parses QNames, where no hook runs.
    fn ncname(&mut self) -> R<Option<String>> {
        let i = self.top();
        let rest = &i.buf[i.pos..];
        let mut n = rest
            .iter()
            .take_while(|&&c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .count();
        let fast = rest
            .first()
            .is_some_and(|&c| c.is_ascii_alphabetic() || c == '_');
        if !(fast && rest.get(n).is_some_and(char::is_ascii)) {
            self.grow();
            let i = self.top();
            let rest = &i.buf[i.pos..];
            if !rest.first().is_some_and(|&c| name_start(c) && c != ':') {
                return Ok(None);
            }
            n = rest
                .iter()
                .take_while(|&&c| name_char(c) && c != ':')
                .count();
        }
        let i = self.top();
        let s: String = i.buf[i.pos..i.pos + n].iter().collect();
        too_long(&s)?;
        self.advance(n);
        Ok(Some(s))
    }

    /// `xmlParseQName`: (prefix, local). Malformed QNames only draw a
    /// namespace error.
    fn qname(&mut self) -> R<Option<(Option<String>, String)>> {
        self.grow();
        let Some(l) = self.ncname()? else {
            if self.raw() == ':' {
                return Ok(self.name()?.map(|n| (None, n)));
            }
            return Ok(None);
        };
        if self.raw() != ':' {
            return Ok(Some((None, l)));
        }
        self.next()?;
        let Some(l2) = self.ncname()? else {
            let t = self.nmtoken()?.unwrap_or_default();
            return Ok(Some((None, format!("{l}:{t}"))));
        };
        if self.raw() == ':' {
            self.next()?;
            let tmp = self.name()?.unwrap_or_default();
            return Ok(Some((Some(l), format!("{l2}:{tmp}"))));
        }
        Ok(Some((Some(l), l2)))
    }

    /// `xmlParseNmtoken`: a token may run on past the end of an input.
    fn nmtoken(&mut self) -> R<Option<String>> {
        self.grow();
        let mut s = String::new();
        let mut c = self.raw();
        while name_char(c) {
            s.push(c);
            self.nextl()?;
            c = self.raw();
            if c == '\0' {
                self.grow();
                c = self.raw();
            }
        }
        if s.is_empty() {
            return Ok(None);
        }
        too_long(&s)?;
        Ok(Some(s))
    }
}

// --- DTD ----------------------------------------------------------------------

/// libxml `IS_PUBIDCHAR_CH`.
fn pubid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " \r\n-'()+,./:=?;!*#@$_%".contains(c)
}

impl Parser {
    /// `xmlParseDocTypeDecl`. The external subset is never loaded.
    fn doctype_decl(&mut self) -> R {
        self.skip(9)?;
        self.skip_blanks()?;
        if self.name()?.is_none() {
            return fail("xmlParseDocTypeDecl : no DOCTYPE name !");
        }
        self.skip_blanks()?;
        let (public, system) = self.external_id(true)?;
        self.has_ext |= public || system.is_some();
        self.skip_blanks()?;
        if self.raw() == '[' {
            return Ok(());
        }
        if self.raw() != '>' {
            return fail("DOCTYPE improperly terminated");
        }
        self.next()
    }

    /// `xmlParseExternalID`: whether a public literal was read, and the
    /// system literal.
    fn external_id(&mut self, strict: bool) -> R<(bool, Option<String>)> {
        self.shrink();
        if self.looking_at("SYSTEM") {
            self.skip(6)?;
            if !blank(self.raw()) {
                return fail("Space required after 'SYSTEM'");
            }
            self.skip_blanks()?;
            return Ok((false, Some(self.system_literal()?)));
        }
        if !self.looking_at("PUBLIC") {
            return Ok((false, None));
        }
        self.skip(6)?;
        if !blank(self.raw()) {
            return fail("Space required after 'PUBLIC'");
        }
        self.skip_blanks()?;
        self.pubid_literal()?;
        if strict {
            if !blank(self.raw()) {
                return fail("Space required after the Public Identifier");
            }
        } else {
            // [83] PublicID: the system literal is optional; C peeks
            // within the current buffer only.
            self.grow();
            let i = self.top();
            let rest = &i.buf[i.pos..];
            let k = rest.iter().take_while(|&&c| blank(c)).count();
            if k == 0 || !matches!(rest.get(k), Some('"' | '\'')) {
                return Ok((true, None));
            }
        }
        self.skip_blanks()?;
        Ok((true, Some(self.system_literal()?)))
    }

    /// `xmlParseSystemLiteral`.
    fn system_literal(&mut self) -> R<String> {
        self.shrink();
        let stop = self.raw();
        if stop != '"' && stop != '\'' {
            return fail("SystemLiteral \" or ' expected");
        }
        self.next()?;
        let saved = self.state;
        self.state = State::SystemLiteral;
        let (mut lit, mut size) = (String::new(), 100usize);
        let mut c = self.raw();
        while is_char(c) && c != stop {
            if lit.len() + 5 >= size {
                if size > MAX_NAME {
                    return fail("SystemLiteral too long");
                }
                size *= 2;
            }
            lit.push(c);
            self.nextl()?;
            c = self.raw();
            if c == '\0' {
                self.grow();
                self.shrink();
                c = self.raw();
            }
        }
        self.state = saved;
        if !is_char(c) {
            return fail("Unfinished System or Public ID \" or ' expected");
        }
        self.next()?;
        Ok(lit)
    }

    /// `xmlParsePubidLiteral`.
    fn pubid_literal(&mut self) -> R {
        self.shrink();
        let stop = self.raw();
        if stop != '"' && stop != '\'' {
            return fail("PubidLiteral \" or ' expected");
        }
        self.next()?;
        let saved = self.state;
        self.state = State::PublicLiteral;
        let (mut len, mut size) = (0usize, 100usize);
        let mut c = self.raw();
        while pubid_char(c) && c != stop {
            if len + 1 >= size {
                if size > MAX_NAME {
                    return fail("Public ID too long");
                }
                size *= 2;
            }
            len += 1;
            self.next()?;
            c = self.raw();
            if c == '\0' {
                self.grow();
                self.shrink();
                c = self.raw();
            }
        }
        if c != stop {
            return fail("Unfinished System or Public ID \" or ' expected");
        }
        self.next()?;
        self.state = saved;
        Ok(())
    }

    /// `xmlParseInternalSubset`.
    fn internal_subset(&mut self) -> R {
        self.state = State::Dtd;
        self.next()?;
        while self.raw() != ']' {
            let check = self.here();
            self.skip_blanks()?;
            self.markup_decl()?;
            self.pe_reference()?;
            while self.at_end() && self.inputs.len() > 1 {
                self.pop_input();
            }
            if self.here() == check {
                return fail("xmlParseInternalSubset: error detected in Markup declaration");
            }
        }
        self.next()?;
        self.skip_blanks()?;
        if self.raw() != '>' {
            return fail("DOCTYPE improperly terminated");
        }
        self.next()
    }

    /// `xmlParseMarkupDecl`.
    fn markup_decl(&mut self) -> R {
        self.grow();
        if self.looking_at("<!EL") {
            self.element_decl()?;
        } else if self.looking_at("<!EN") {
            self.entity_decl()?;
        } else if self.looking_at("<!A") {
            self.attlist_decl()?;
        } else if self.looking_at("<!N") {
            self.notation_decl()?;
        } else if self.looking_at("<!-") {
            self.comment()?;
        } else if self.looking_at("<?") {
            self.pi()?;
        }
        if self.inputs.len() == 1 {
            self.pe_reference()?;
        } else if self.looking_at("<![") {
            self.conditional_sections()?;
        }
        self.state = State::Dtd;
        Ok(())
    }

    /// `xmlParsePEReference`. An external PE is never loaded.
    fn pe_reference(&mut self) -> R {
        if self.raw() != '%' {
            return Ok(());
        }
        self.next()?;
        let Some(name) = self.name()? else {
            return fail("xmlParsePEReference: no name");
        };
        if self.raw() != ';' {
            return fail("PEReference: expecting ';'");
        }
        self.next()?;
        self.nbentities += 1;
        match self.params.get(&name).copied() {
            None => self.undeclared()?,
            Some(_) if !self.top().wrapper => self.push_wrapper(&name),
            Some(e) if self.ents[e].kind == Kind::InternalPe => self.push_pe(e)?,
            Some(_) => return Ok(()),
        }
        self.has_perefs = true;
        Ok(())
    }

    /// `xmlParseElementDecl`.
    fn element_decl(&mut self) -> R {
        if !self.looking_at("<!ELEMENT") {
            return Ok(());
        }
        let id = self.id();
        self.skip(9)?;
        if !blank(self.raw()) {
            return fail("Space required after 'ELEMENT'");
        }
        self.skip_blanks()?;
        if self.name()?.is_none() {
            return fail("xmlParseElementDecl: no name for Element");
        }
        while self.at_end() && self.inputs.len() > 1 {
            self.pop_input();
        }
        if !blank(self.raw()) {
            return fail("Space required after the element name");
        }
        self.skip_blanks()?;
        if self.looking_at("EMPTY") {
            self.skip(5)?;
        } else if self.looking_at("ANY") {
            self.skip(3)?;
        } else if self.raw() == '(' {
            self.content_decl()?;
        } else {
            return fail("xmlParseElementDecl: 'EMPTY', 'ANY' or '(' expected");
        }
        self.skip_blanks()?;
        while self.at_end() && self.inputs.len() > 1 {
            self.pop_input();
        }
        self.skip_blanks()?;
        if self.raw() != '>' {
            return fail("xmlParseElementDecl: expected '>'");
        }
        if self.id() != id {
            return fail("Element declaration doesn't start and stop in the same entity");
        }
        self.next()
    }

    /// `xmlParseElementContentDecl`.
    fn content_decl(&mut self) -> R {
        self.next()?;
        self.grow();
        self.skip_blanks()?;
        if self.looking_at("#PCDATA") {
            self.mixed_content_decl()?;
        } else {
            self.children_content_decl(1)?;
        }
        self.skip_blanks().map(drop)
    }

    /// `xmlParseElementMixedContentDecl`.
    fn mixed_content_decl(&mut self) -> R {
        self.grow();
        self.skip(7)?;
        self.skip_blanks()?;
        self.shrink();
        if self.raw() == ')' {
            self.next()?;
            if self.raw() == '*' {
                self.next()?;
            }
            return Ok(());
        }
        while self.raw() == '|' {
            self.next()?;
            self.skip_blanks()?;
            if self.name()?.is_none() {
                return fail("xmlParseElementMixedContentDecl : Name expected");
            }
            self.skip_blanks()?;
            self.grow();
        }
        if !self.looking_at(")*") {
            return fail("xmlParseElementMixedContentDecl : ')*' expected");
        }
        self.skip(2)
    }

    /// `xmlParseElementChildrenContentDeclPriv`.
    fn children_content_decl(&mut self, depth: u32) -> R {
        if depth > 128 {
            return fail("xmlParseElementChildrenContentDecl : depth too deep");
        }
        self.skip_blanks()?;
        self.grow();
        self.cp(depth)?;
        self.skip_blanks()?;
        self.shrink();
        let mut sep = None;
        while self.raw() != ')' {
            let c = self.raw();
            if c != ',' && c != '|' {
                return fail("xmlParseElementChildrenContentDecl : ',' '|' or ')' expected");
            }
            if *sep.get_or_insert(c) != c {
                return fail("xmlParseElementChildrenContentDecl : mixed separators");
            }
            self.next()?;
            self.grow();
            self.skip_blanks()?;
            self.grow();
            self.cp(depth)?;
            self.skip_blanks()?;
            self.grow();
        }
        self.next()?;
        if matches!(self.raw(), '?' | '*' | '+') {
            self.next()?;
        }
        Ok(())
    }

    /// One content particle: a nested group or a Name with its occurrence.
    fn cp(&mut self, depth: u32) -> R {
        if self.raw() == '(' {
            self.next()?;
            self.skip_blanks()?;
            self.children_content_decl(depth + 1)?;
            self.skip_blanks()?;
            self.grow();
            return Ok(());
        }
        if self.name()?.is_none() {
            return fail("xmlParseElementChildrenContentDecl : Name expected");
        }
        self.grow();
        if matches!(self.raw(), '?' | '*' | '+') {
            self.next()?;
        }
        self.grow();
        Ok(())
    }

    /// `xmlParseEntityDecl`. External entities are declared, never loaded.
    fn entity_decl(&mut self) -> R {
        if !self.looking_at("<!ENTITY") {
            return Ok(());
        }
        let id = self.id();
        self.shrink();
        self.skip(8)?;
        if self.skip_blanks()? == 0 {
            return fail("Space required after '<!ENTITY'");
        }
        let pe = self.raw() == '%';
        if pe {
            self.next()?;
            if self.skip_blanks()? == 0 {
                return fail("Space required after '%'");
            }
        }
        let Some(name) = self.name()? else {
            return fail("xmlParseEntityDecl: no name");
        };
        if self.skip_blanks()? == 0 {
            return fail("Space required after the entity name");
        }
        self.state = State::EntityDecl;
        if matches!(self.raw(), '"' | '\'') {
            let value = self.entity_value()?;
            let kind = if pe { Kind::InternalPe } else { Kind::Internal };
            self.declare(name, kind, Some(value));
        } else {
            let (public, system) = self.external_id(true)?;
            if !public && system.is_none() {
                return fail("Entity value required");
            }
            // An invalid system URI is only an error message.
            let uri = system.as_deref().map(parse_uri);
            if uri == Some(Some(true)) {
                return fail("Fragment not allowed");
            }
            if pe {
                if uri == Some(Some(false)) {
                    self.declare(name, Kind::ExternalPe, None);
                }
            } else {
                if self.raw() != '>' && !blank(self.raw()) {
                    return fail("Space required before 'NDATA'");
                }
                self.skip_blanks()?;
                if self.looking_at("NDATA") {
                    self.skip(5)?;
                    if !blank(self.raw()) {
                        return fail("Space required after 'NDATA'");
                    }
                    self.skip_blanks()?;
                    self.name()?;
                    self.declare(name, Kind::Unparsed, None);
                } else {
                    self.declare(name, Kind::External, None);
                }
            }
        }
        self.skip_blanks()?;
        if self.raw() != '>' {
            return fail("xmlParseEntityDecl: entity not terminated");
        }
        if self.id() != id {
            return fail("Entity declaration doesn't start and stop in the same entity");
        }
        self.next()
    }

    /// `xmlAddEntity`: a redeclaration is ignored.
    fn declare(&mut self, name: String, kind: Kind, content: Option<Vec<char>>) {
        let pe = matches!(kind, Kind::InternalPe | Kind::ExternalPe);
        if (if pe { &self.params } else { &self.general }).contains_key(&name) {
            return;
        }
        let src = self.fresh();
        let ascii = content
            .as_ref()
            .is_none_or(|c| c.iter().all(char::is_ascii));
        self.ents.push(Entity {
            kind,
            content: content.map(Rc::from),
            src,
            checked: 0,
            children: false,
            ascii,
        });
        let e = self.ents.len() - 1;
        if pe {
            self.params.insert(name, e);
        } else {
            self.general.insert(name, e);
        }
    }

    /// `xmlParseEntityValue`: the literal with parameter entities replaced.
    fn entity_value(&mut self) -> R<Vec<char>> {
        let stop = self.raw();
        self.state = State::EntityValue;
        let id = self.id();
        self.grow();
        self.next()?;
        let mut buf = Vec::new();
        let mut c = self.raw();
        // A quote inside PE text does not close the literal.
        while is_char(c) && (c != stop || self.id() != id) {
            buf.push(c);
            self.nextl()?;
            while self.at_end() && self.inputs.len() > 1 {
                self.pop_input();
            }
            self.grow();
            c = self.raw();
        }
        let mut k = 0;
        while k < buf.len() {
            let ch = buf[k];
            if ch == '%' || (ch == '&' && buf.get(k + 1) != Some(&'#')) {
                k += 1;
                if string_name(&buf, &mut k)?.is_none() || buf.get(k) != Some(&';') {
                    return fail(
                        "EntityValue: '%' or '&' forbidden except for entities references",
                    );
                }
                if ch == '%' && self.in_subset == 1 && self.inputs.len() == 1 {
                    return fail("PEReferences forbidden in internal subset");
                }
            }
            k += 1;
        }
        if c != stop {
            return fail("EntityValue: \" or ' expected");
        }
        self.next()?;
        self.depth += 1;
        let r = self.decode(&buf, true);
        self.depth -= 1;
        r
    }

    /// `xmlParseAttributeListDecl`.
    fn attlist_decl(&mut self) -> R {
        if !self.looking_at("<!ATTLIST") {
            return Ok(());
        }
        self.skip(9)?;
        if !blank(self.raw()) {
            return fail("Space required after '<!ATTLIST'");
        }
        self.skip_blanks()?;
        let Some(elem) = self.name()? else {
            return fail("ATTLIST: no name for Element");
        };
        self.skip_blanks()?;
        self.grow();
        while self.raw() != '>' {
            let check = self.here();
            self.grow();
            let Some(attr) = self.name()? else {
                return fail("ATTLIST: no name for Attribute");
            };
            self.grow();
            if !blank(self.raw()) {
                return fail("Space required after the attribute name");
            }
            self.skip_blanks()?;
            let cdata = self.attribute_type()?;
            self.grow();
            if !blank(self.raw()) {
                return fail("Space required after the attribute type");
            }
            self.skip_blanks()?;
            self.default_decl()?;
            self.grow();
            if self.raw() != '>' {
                if !blank(self.raw()) {
                    return fail("Space required after the attribute default value");
                }
                self.skip_blanks()?;
            }
            if self.here() == check {
                return fail("in xmlParseAttributeListDecl");
            }
            // xmlAddSpecialAttrs: the first declaration wins.
            self.special.entry((elem.clone(), attr)).or_insert(cdata);
            self.grow();
        }
        self.next()
    }

    /// `xmlParseAttributeType`: whether the type is CDATA.
    fn attribute_type(&mut self) -> R<bool> {
        self.shrink();
        for kw in [
            "CDATA", "IDREFS", "IDREF", "ID", "ENTITY", "ENTITIES", "NMTOKENS", "NMTOKEN",
        ] {
            if self.looking_at(kw) {
                self.skip(kw.len())?;
                return Ok(kw == "CDATA");
            }
        }
        let notation = self.looking_at("NOTATION");
        if notation {
            self.skip(8)?;
            if !blank(self.raw()) {
                return fail("Space required after 'NOTATION'");
            }
            self.skip_blanks()?;
        }
        if self.raw() != '(' {
            return fail("'(' required to start an enumerated type");
        }
        self.shrink();
        loop {
            self.next()?;
            self.skip_blanks()?;
            let token = if notation {
                self.name()?
            } else {
                self.nmtoken()?
            };
            if token.is_none() {
                return fail("Name or Nmtoken expected in an enumerated type");
            }
            self.skip_blanks()?;
            if self.raw() != '|' {
                break;
            }
        }
        if self.raw() != ')' {
            return fail("')' required to finish an enumerated type");
        }
        self.next()?;
        Ok(false)
    }

    /// `xmlParseDefaultDecl`.
    fn default_decl(&mut self) -> R {
        if self.looking_at("#REQUIRED") {
            return self.skip(9);
        }
        if self.looking_at("#IMPLIED") {
            return self.skip(8);
        }
        if self.looking_at("#FIXED") {
            self.skip(6)?;
            if !blank(self.raw()) {
                return fail("Space required after '#FIXED'");
            }
            self.skip_blanks()?;
        }
        self.att_value(false)?;
        self.state = State::Dtd;
        Ok(())
    }

    /// `xmlParseNotationDecl`, with SAX2's refusal of a notation that has
    /// neither identifier.
    fn notation_decl(&mut self) -> R {
        if !self.looking_at("<!NOTATION") {
            return Ok(());
        }
        let id = self.id();
        self.shrink();
        self.skip(10)?;
        if !blank(self.raw()) {
            return fail("Space required after '<!NOTATION'");
        }
        self.skip_blanks()?;
        if self.name()?.is_none() {
            return fail("NOTATION: Name expected here");
        }
        if !blank(self.raw()) {
            return fail("Space required after the NOTATION name'");
        }
        self.skip_blanks()?;
        let (public, system) = self.external_id(false)?;
        self.skip_blanks()?;
        if self.raw() != '>' {
            return fail("'>' required to close NOTATION declaration");
        }
        if self.id() != id {
            return fail("Notation declaration doesn't start and stop in the same entity");
        }
        self.next()?;
        if !public && system.is_none() {
            return fail("externalID or PublicID missing");
        }
        Ok(())
    }

    /// `xmlParseConditionalSections`; C recurses without bound.
    fn conditional_sections(&mut self) -> R {
        if self.conds >= 256 {
            return fail("conditional sections nested too deep");
        }
        self.conds += 1;
        let r = self.conditional_body();
        self.conds -= 1;
        r
    }

    fn conditional_body(&mut self) -> R {
        self.skip(3)?;
        self.skip_blanks()?;
        if self.looking_at("INCLUDE") {
            self.skip(7)?;
            self.skip_blanks()?;
            if self.raw() != '[' {
                return fail("XML conditional section '[' expected");
            }
            self.next()?;
            while self.raw() != '\0' && !self.looking_at("]]>") {
                let check = self.here();
                if self.looking_at("<![") {
                    self.conditional_sections()?;
                } else if blank(self.raw()) {
                    self.next()?;
                } else if self.raw() == '%' {
                    self.pe_reference()?;
                } else {
                    self.markup_decl()?;
                }
                while self.at_end() && self.inputs.len() > 1 {
                    self.pop_input();
                }
                if self.here() == check {
                    return fail("Content error in the external subset");
                }
            }
        } else if self.looking_at("IGNORE") {
            self.skip(6)?;
            self.skip_blanks()?;
            if self.raw() != '[' {
                return fail("XML conditional section '[' expected");
            }
            self.next()?;
            let saved = self.state;
            self.state = State::Ignore;
            let mut depth = 0i64;
            while depth >= 0 && self.raw() != '\0' {
                if self.looking_at("<![") {
                    depth += 1;
                    self.skip(3)?;
                } else if self.looking_at("]]>") {
                    depth -= 1;
                    if depth >= 0 {
                        self.skip(3)?;
                    }
                } else {
                    self.next()?;
                }
            }
            self.state = saved;
        } else {
            return fail("XML conditional section INCLUDE or IGNORE keyword expected");
        }
        self.shrink();
        if self.raw() == '\0' {
            return fail("XML conditional section not closed");
        }
        self.skip(3)
    }
}

/// `xmlParseURI` (an RFC 3986 URI-reference): None when invalid, else
/// whether it has a fragment. libxml's IPv4 branch never matches (it does
/// not step over the second dot), so a host is an IP literal or a reg-name.
fn parse_uri(s: &str) -> Option<bool> {
    let b = s.as_bytes();
    absolute_uri(b).or_else(|| relative_ref(b))
}

fn unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~".contains(&c)
}

fn sub_delim(c: u8) -> bool {
    b"!$&'()*+,;=".contains(&c)
}

fn pct(b: &[u8], i: usize) -> bool {
    b.get(i) == Some(&b'%')
        && b.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
}

fn pchar(b: &[u8], i: usize) -> bool {
    b.get(i)
        .is_some_and(|&c| unreserved(c) || sub_delim(c) || c == b':' || c == b'@')
        || pct(b, i)
}

fn step(b: &[u8], i: usize) -> usize {
    if b[i] == b'%' { i + 3 } else { i + 1 }
}

fn segment(b: &[u8], mut i: usize, forbid: Option<u8>) -> usize {
    while pchar(b, i) && Some(b[i]) != forbid {
        i = step(b, i);
    }
    i
}

/// path-abempty: ( "/" segment )*
fn path_tail(b: &[u8], mut i: usize) -> usize {
    while b.get(i) == Some(&b'/') {
        i = segment(b, i + 1, None);
    }
    i
}

fn authority(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while b
        .get(j)
        .is_some_and(|&c| unreserved(c) || sub_delim(c) || c == b':')
        || pct(b, j)
    {
        j = step(b, j);
    }
    let mut k = if b.get(j) == Some(&b'@') { j + 1 } else { i };
    if b.get(k) == Some(&b'[') {
        k += b[k..].iter().position(|&c| c == b']')? + 1;
    } else {
        while b.get(k).is_some_and(|&c| unreserved(c) || sub_delim(c)) || pct(b, k) {
            k = step(b, k);
        }
    }
    if b.get(k) == Some(&b':') {
        k += 1;
        if !b.get(k).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        while b.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
    }
    Some(k)
}

/// [ "?" query ] [ "#" fragment ] and the end of the string.
fn query_fragment(b: &[u8], mut i: usize) -> Option<bool> {
    if b.get(i) == Some(&b'?') {
        i += 1;
        while pchar(b, i) || matches!(b.get(i), Some(b'/' | b'?')) {
            i = step(b, i);
        }
    }
    let fragment = b.get(i) == Some(&b'#');
    if fragment {
        i += 1;
        while pchar(b, i) || matches!(b.get(i), Some(b'/' | b'?' | b'[' | b']')) {
            i = step(b, i);
        }
    }
    (i == b.len()).then_some(fragment)
}

fn absolute_uri(b: &[u8]) -> Option<bool> {
    if !b.first()?.is_ascii_alphabetic() {
        return None;
    }
    let mut i = 1 + b[1..]
        .iter()
        .take_while(|&&c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
        .count();
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    i = if b[i..].starts_with(b"//") {
        path_tail(b, authority(b, i + 2)?)
    } else if b.get(i) == Some(&b'/') || pchar(b, i) {
        path_tail(b, segment(b, i + usize::from(b[i] == b'/'), None))
    } else {
        i
    };
    query_fragment(b, i)
}

fn relative_ref(b: &[u8]) -> Option<bool> {
    let i = if b.starts_with(b"//") {
        path_tail(b, authority(b, 2)?)
    } else if b.first() == Some(&b'/') {
        path_tail(b, segment(b, 1, None))
    } else {
        // path-noscheme: no ':' in the first segment
        path_tail(b, segment(b, 0, Some(b':')))
    };
    query_fragment(b, i)
}

// --- entities ---------------------------------------------------------------

fn predefined(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        "quot" => Some('"'),
        _ => None,
    }
}

/// `xmlParseStringCharRef` over `s[*i..]`.
fn string_char_ref(s: &[char], i: &mut usize) -> R<char> {
    let hex = s.get(*i + 2) == Some(&'x');
    *i += if hex { 3 } else { 2 };
    let (mut val, mut out) = (0u32, false);
    while let Some(&c) = s.get(*i).filter(|&&c| c != ';') {
        let d = match c.to_digit(if hex { 16 } else { 10 }) {
            Some(d) => d,
            None => return fail("xmlParseStringCharRef: invalid value"),
        };
        val = val.wrapping_mul(if hex { 16 } else { 10 }).wrapping_add(d);
        out |= val > 0x10FFFF;
        *i += 1;
    }
    if s.get(*i).is_none() {
        return fail("xmlParseStringCharRef: invalid value");
    }
    *i += 1;
    match char::from_u32(val).filter(|&c| is_char(c) && !out) {
        Some(c) => Ok(c),
        None => fail("xmlParseStringCharRef: invalid xmlChar value"),
    }
}

/// `xmlStringDecodeEntities`' output buffer: its byte count and allocated
/// size decide when the amplification check runs.
struct DecodeBuf {
    out: Vec<char>,
    nb: usize,
    size: usize,
}

impl DecodeBuf {
    fn push(&mut self, c: char) {
        self.out.push(c);
        self.nb += c.len_utf8();
        if self.nb + 100 > self.size {
            self.size = self.size * 2 + 100;
        }
    }
}

/// `xmlParseBalancedChunkMemoryInternal` result.
#[derive(PartialEq, Eq)]
enum Chunk {
    Ok,
    Loop,
    Failed,
}

impl Parser {
    /// Byte offset in the current input (`consumed` in libxml checks).
    fn consumed(&self) -> u64 {
        let i = self.top();
        let n = if i.ascii {
            i.pos
        } else {
            i.buf[..i.pos].iter().map(|c| c.len_utf8()).sum()
        };
        n as u64
    }

    /// `xmlParseEntityRef`.
    fn entity_ref(&mut self) -> R<Ref> {
        self.grow();
        self.next()?;
        let Some(name) = self.name()? else {
            return fail("xmlParseEntityRef: no name");
        };
        if self.raw() != ';' {
            return fail("EntityRef: expecting ';'");
        }
        self.next()?;
        self.lookup(name)
    }

    /// The shared tail of `xmlParseEntityRef` and `xmlParseStringEntityRef`.
    fn lookup(&mut self, name: String) -> R<Ref> {
        if let Some(c) = predefined(&name) {
            return Ok(Ref::Predefined(c));
        }
        self.nbentities += 1;
        let Some(&e) = self.general.get(&name) else {
            self.undeclared()?;
            return Ok(Ref::Undeclared);
        };
        let ent = &self.ents[e];
        if ent.kind == Kind::Unparsed {
            return fail("Entity reference to unparsed entity");
        }
        if self.state == State::AttrValue {
            if ent.kind == Kind::External {
                return fail("Attribute references external entity");
            }
            if (ent.checked & 1 == 1 || ent.checked == 0)
                && ent.content.as_ref().is_some_and(|c| c.contains(&'<'))
            {
                return fail("'<' in entity is not allowed in attributes values");
            }
        }
        Ok(Ref::Entity(e, name))
    }

    /// `xmlParseReference`. External entities are never loaded.
    fn reference(&mut self) -> R {
        if self.nxt(1) == '#' {
            return self.char_ref().map(drop);
        }
        let Ref::Entity(e, _) = self.entity_ref()? else {
            return Ok(());
        };
        let was_checked = self.ents[e].checked;
        let internal = self.ents[e].kind == Kind::Internal;
        let content = self.ents[e].content.clone().unwrap_or_else(|| Rc::from([]));
        if was_checked == 0 && internal {
            let old = self.nbentities;
            self.depth += 1;
            let ret = self.balanced_chunk(&content);
            self.depth -= 1;
            self.ents[e].checked =
                ((self.nbentities - old + 1) * 2) | u64::from(content.contains(&'<'));
            if ret == Chunk::Loop {
                return fail(LOOP);
            }
            self.entity_check(0, Some(e))?;
            if ret == Chunk::Failed {
                return fail("Entity failed to parse");
            }
            self.ents[e].children = !content.is_empty();
        } else if was_checked != 1 {
            self.nbentities += was_checked / 2;
        }
        // libxml parses content it kept no nodes for again; only a loop
        // counts there.
        if !self.ents[e].children && was_checked != 0 && internal {
            self.depth += 1;
            let ret = self.balanced_chunk(&content);
            self.depth -= 1;
            if ret == Chunk::Loop {
                return fail(LOOP);
            }
        }
        Ok(())
    }

    /// `xmlParseBalancedChunkMemoryInternal`: entity content parsed as
    /// content in a parser context of its own.
    fn balanced_chunk(&mut self, content: &Rc<[char]>) -> Chunk {
        if self.depth > 40 {
            return Chunk::Loop;
        }
        if content.is_empty() {
            return Chunk::Ok;
        }
        let inputs = std::mem::take(&mut self.inputs);
        let open = std::mem::take(&mut self.open);
        let saved = (
            self.state,
            self.standalone,
            self.has_ext,
            self.has_perefs,
            self.in_subset,
            self.nbentities,
            self.depth,
            self.emit,
        );
        (self.state, self.standalone, self.has_ext, self.has_perefs) =
            (State::Content, false, false, false);
        (self.in_subset, self.nbentities, self.depth, self.emit) = (0, 0, saved.6 + 1, false);
        let src = self.fresh();
        self.push(
            content.clone(),
            false,
            src,
            content.iter().all(char::is_ascii),
        );
        let r = self.chunk_body();
        let sub_nb = self.nbentities;
        self.inputs = inputs;
        self.open = open;
        (self.state, self.standalone, self.has_ext, self.has_perefs) =
            (saved.0, saved.1, saved.2, saved.3);
        (self.in_subset, self.nbentities, self.depth, self.emit) =
            (saved.4, saved.5, saved.6, saved.7);
        self.nbentities += sub_nb;
        match r {
            Ok(()) => Chunk::Ok,
            Err(Fatal(m)) if m == LOOP => Chunk::Loop,
            Err(_) => Chunk::Failed,
        }
    }

    fn chunk_body(&mut self) -> R {
        loop {
            match self.content()? {
                Stop::StartTag => self.element()?,
                Stop::EndTag => return fail("chunk is not well balanced"),
                Stop::End => return Ok(()),
            }
        }
    }

    /// Decode an entity's content once to size it into `checked`.
    fn check_content(&mut self, e: usize, mark: bool) -> R {
        let content = self.ents[e].content.clone().unwrap_or_else(|| Rc::from([]));
        if mark {
            self.ents[e].checked = 1;
        }
        let old = self.nbentities;
        self.depth += 1;
        let rep = self.decode(&content, false);
        self.depth -= 1;
        let rep = rep?;
        self.ents[e].checked = ((self.nbentities - old + 1) * 2) | u64::from(rep.contains(&'<'));
        Ok(())
    }

    /// `xmlParserEntityCheck`: entity amplification is refused as a loop.
    fn entity_check(&mut self, size: usize, ent: Option<usize>) -> R {
        if let Some(e) = ent {
            let x = &self.ents[e];
            if x.content.is_some() && x.checked == 0 && x.kind != Kind::ExternalPe {
                self.check_content(e, true)?;
            }
        }
        let consumed = self.consumed();
        let ok = match (size, ent) {
            (1.., _) => {
                size < 1000 || (size as u64) < 10 * consumed && self.nbentities * 3 < 10 * consumed
            }
            (0, Some(e)) => return self.density(self.ents[e].checked),
            (0, None) => self.nbentities <= 10000,
        };
        if !ok {
            return fail(LOOP);
        }
        Ok(())
    }

    fn density(&self, checked: u64) -> R {
        if (checked / 2) * 3 < self.consumed() * 10 {
            return Ok(());
        }
        fail(LOOP)
    }

    /// `xmlStringDecodeEntities`: REF mode expands general entities, PE
    /// mode (entity values) parameter entities.
    fn decode(&mut self, s: &[char], pe: bool) -> R<Vec<char>> {
        if self.depth > 40 {
            return fail(LOOP);
        }
        let mut b = DecodeBuf {
            out: Vec::new(),
            nb: 0,
            size: 300,
        };
        let mut i = 0;
        while let Some(&c) = s.get(i) {
            let e = if c == '&' && s.get(i + 1) == Some(&'#') {
                b.push(string_char_ref(s, &mut i)?);
                continue;
            } else if c == '&' && !pe {
                i += 1;
                let Some(name) = string_name(s, &mut i)? else {
                    return fail("xmlParseStringEntityRef: no name");
                };
                if s.get(i) != Some(&';') {
                    return fail("EntityRef: expecting ';'");
                }
                i += 1;
                match self.lookup(name)? {
                    Ref::Predefined(ch) => {
                        self.density(0)?;
                        b.push(ch);
                        continue;
                    }
                    Ref::Undeclared => continue,
                    Ref::Entity(e, name) => {
                        self.entity_check(0, Some(e))?;
                        self.nbentities += self.ents[e].checked / 2;
                        if self.ents[e].content.is_none() {
                            b.out.push('&');
                            b.out.extend(name.chars());
                            b.out.push(';');
                            b.nb += 1;
                            if b.nb + name.len() + 100 > b.size {
                                b.size = b.size * 2 + name.len() + 100;
                            }
                            b.nb += name.len() + 1;
                            continue;
                        }
                        e
                    }
                }
            } else if c == '%' && pe {
                i += 1;
                let Some(name) = string_name(s, &mut i)? else {
                    return fail("xmlParseStringPEReference: no name");
                };
                if s.get(i) != Some(&';') {
                    return fail("PEReference: expecting ';'");
                }
                i += 1;
                self.nbentities += 1;
                let found = self.params.get(&name).copied();
                if found.is_none() {
                    self.undeclared()?;
                }
                self.has_perefs = true;
                let Some(e) = found else { continue };
                self.entity_check(0, Some(e))?;
                self.nbentities += self.ents[e].checked / 2;
                if self.ents[e].content.is_none() {
                    continue;
                }
                e
            } else {
                b.push(c);
                i += 1;
                continue;
            };
            let content = self.ents[e].content.clone().unwrap_or_else(|| Rc::from([]));
            self.depth += 1;
            let rep = self.decode(&content, pe);
            self.depth -= 1;
            for c in rep? {
                for _ in 0..c.len_utf8() {
                    b.nb += 1;
                    if b.nb + 100 > b.size {
                        self.entity_check(b.nb, Some(e))?;
                        b.size = b.size * 2 + 100;
                    }
                }
                b.out.push(c);
            }
        }
        Ok(b.out)
    }

    /// Attribute text for the layout reader: `&#38;` and `&name;` that
    /// libxml left in a value, replaced.
    fn expand(&self, s: &[char], depth: u32, out: &mut String) -> Result<(), LayoutError> {
        let mut i = 0;
        while let Some(&c) = s.get(i) {
            if out.len() > 10_000_000 {
                return Err(LayoutError("attribute value expands past 10M bytes".into()));
            }
            if c == '&' && s.get(i + 1) == Some(&'#') {
                let mut j = i;
                if let Ok(ch) = string_char_ref(s, &mut j) {
                    out.push(ch);
                    i = j;
                    continue;
                }
            } else if c == '&' {
                let mut j = i + 1;
                if let Ok(Some(name)) = string_name(s, &mut j) {
                    if s.get(j) == Some(&';') {
                        i = j + 1;
                        if let Some(ch) = predefined(&name) {
                            out.push(ch);
                        } else if let Some(content) = self
                            .general
                            .get(&name)
                            .and_then(|&e| self.ents[e].content.as_ref())
                        {
                            if depth < 40 {
                                self.expand(content, depth + 1, out)?;
                            }
                        }
                        continue;
                    }
                }
            }
            out.push(c);
            i += 1;
        }
        Ok(())
    }
}

/// Bytes of an 8-bit document in its declared encoding.
fn decode_8bit(b: &[u8]) -> R<Vec<char>> {
    let name = decl_encoding(b).map(|e| e.to_ascii_uppercase());
    match name.as_deref() {
        None | Some("UTF-8" | "UTF8") => std::str::from_utf8(b)
            .map(|s| s.chars().collect())
            .map_err(|_| Fatal("Input is not proper UTF-8")),
        Some("ISO-8859-1" | "ISO-LATIN-1") => Ok(b.iter().map(|&x| char::from(x)).collect()),
        Some("ASCII" | "US-ASCII") => b
            .iter()
            .map(|&x| {
                if x < 0x80 {
                    Ok(char::from(x))
                } else {
                    fail("byte outside ASCII")
                }
            })
            .collect(),
        Some(name) => {
            let Some((_, high)) = ISO8859_HIGH.iter().find(|(k, _)| {
                name == format!("ISO-8859-{k}") || (*k == 2 && name == "ISO-LATIN-2")
            }) else {
                return fail("Unsupported encoding");
            };
            b.iter()
                .map(|&x| match x {
                    ..0xA0 => Ok(char::from(x)),
                    _ => char::from_u32(u32::from(high[usize::from(x - 0xA0)]))
                        .filter(|&c| c != '\0')
                        .ok_or(Fatal("byte undefined in encoding")),
                })
                .collect()
        }
    }
}

/// EncName of a leading `<?xml ... encoding="..."` declaration.
fn decl_encoding(b: &[u8]) -> Option<&str> {
    if !b.starts_with(b"<?xml") || !b.get(5).is_some_and(|&c| blank(char::from(c))) {
        return None;
    }
    let end = b.windows(2).position(|w| w == b"?>")?;
    let decl = std::str::from_utf8(&b[..end]).ok()?;
    let rest = decl[decl.find("encoding")? + 8..].trim_start_matches(blank);
    let rest = rest.strip_prefix('=')?.trim_start_matches(blank);
    let q = rest.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let body = &rest[1..];
    let name = &body[..body.find(q)?];
    let mut cs = name.chars();
    (cs.next()?.is_ascii_alphabetic()
        && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    .then_some(name)
}

/// libxml's ISO-8859-x tables (encoding.c), 0xA0..=0xFF; 0 is undefined.
const ISO8859_HIGH: [(u8, [u16; 96]); 14] = [
    (
        2,
        [
            0x00a0, 0x0104, 0x02d8, 0x0141, 0x00a4, 0x013d, 0x015a, 0x00a7, 0x00a8, 0x0160, 0x015e,
            0x0164, 0x0179, 0x00ad, 0x017d, 0x017b, 0x00b0, 0x0105, 0x02db, 0x0142, 0x00b4, 0x013e,
            0x015b, 0x02c7, 0x00b8, 0x0161, 0x015f, 0x0165, 0x017a, 0x02dd, 0x017e, 0x017c, 0x0154,
            0x00c1, 0x00c2, 0x0102, 0x00c4, 0x0139, 0x0106, 0x00c7, 0x010c, 0x00c9, 0x0118, 0x00cb,
            0x011a, 0x00cd, 0x00ce, 0x010e, 0x0110, 0x0143, 0x0147, 0x00d3, 0x00d4, 0x0150, 0x00d6,
            0x00d7, 0x0158, 0x016e, 0x00da, 0x0170, 0x00dc, 0x00dd, 0x0162, 0x00df, 0x0155, 0x00e1,
            0x00e2, 0x0103, 0x00e4, 0x013a, 0x0107, 0x00e7, 0x010d, 0x00e9, 0x0119, 0x00eb, 0x011b,
            0x00ed, 0x00ee, 0x010f, 0x0111, 0x0144, 0x0148, 0x00f3, 0x00f4, 0x0151, 0x00f6, 0x00f7,
            0x0159, 0x016f, 0x00fa, 0x0171, 0x00fc, 0x00fd, 0x0163, 0x02d9,
        ],
    ),
    (
        3,
        [
            0x00a0, 0x0126, 0x02d8, 0x00a3, 0x00a4, 0x0000, 0x0124, 0x00a7, 0x00a8, 0x0130, 0x015e,
            0x011e, 0x0134, 0x00ad, 0x0000, 0x017b, 0x00b0, 0x0127, 0x00b2, 0x00b3, 0x00b4, 0x00b5,
            0x0125, 0x00b7, 0x00b8, 0x0131, 0x015f, 0x011f, 0x0135, 0x00bd, 0x0000, 0x017c, 0x00c0,
            0x00c1, 0x00c2, 0x0000, 0x00c4, 0x010a, 0x0108, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb,
            0x00cc, 0x00cd, 0x00ce, 0x00cf, 0x0000, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x0120, 0x00d6,
            0x00d7, 0x011c, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x016c, 0x015c, 0x00df, 0x00e0, 0x00e1,
            0x00e2, 0x0000, 0x00e4, 0x010b, 0x0109, 0x00e7, 0x00e8, 0x00e9, 0x00ea, 0x00eb, 0x00ec,
            0x00ed, 0x00ee, 0x00ef, 0x0000, 0x00f1, 0x00f2, 0x00f3, 0x00f4, 0x0121, 0x00f6, 0x00f7,
            0x011d, 0x00f9, 0x00fa, 0x00fb, 0x00fc, 0x016d, 0x015d, 0x02d9,
        ],
    ),
    (
        4,
        [
            0x00a0, 0x0104, 0x0138, 0x0156, 0x00a4, 0x0128, 0x013b, 0x00a7, 0x00a8, 0x0160, 0x0112,
            0x0122, 0x0166, 0x00ad, 0x017d, 0x00af, 0x00b0, 0x0105, 0x02db, 0x0157, 0x00b4, 0x0129,
            0x013c, 0x02c7, 0x00b8, 0x0161, 0x0113, 0x0123, 0x0167, 0x014a, 0x017e, 0x014b, 0x0100,
            0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c6, 0x012e, 0x010c, 0x00c9, 0x0118, 0x00cb,
            0x0116, 0x00cd, 0x00ce, 0x012a, 0x0110, 0x0145, 0x014c, 0x0136, 0x00d4, 0x00d5, 0x00d6,
            0x00d7, 0x00d8, 0x0172, 0x00da, 0x00db, 0x00dc, 0x0168, 0x016a, 0x00df, 0x0101, 0x00e1,
            0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x012f, 0x010d, 0x00e9, 0x0119, 0x00eb, 0x0117,
            0x00ed, 0x00ee, 0x012b, 0x0111, 0x0146, 0x014d, 0x0137, 0x00f4, 0x00f5, 0x00f6, 0x00f7,
            0x00f8, 0x0173, 0x00fa, 0x00fb, 0x00fc, 0x0169, 0x016b, 0x02d9,
        ],
    ),
    (
        5,
        [
            0x00a0, 0x0401, 0x0402, 0x0403, 0x0404, 0x0405, 0x0406, 0x0407, 0x0408, 0x0409, 0x040a,
            0x040b, 0x040c, 0x00ad, 0x040e, 0x040f, 0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415,
            0x0416, 0x0417, 0x0418, 0x0419, 0x041a, 0x041b, 0x041c, 0x041d, 0x041e, 0x041f, 0x0420,
            0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042a, 0x042b,
            0x042c, 0x042d, 0x042e, 0x042f, 0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436,
            0x0437, 0x0438, 0x0439, 0x043a, 0x043b, 0x043c, 0x043d, 0x043e, 0x043f, 0x0440, 0x0441,
            0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447, 0x0448, 0x0449, 0x044a, 0x044b, 0x044c,
            0x044d, 0x044e, 0x044f, 0x2116, 0x0451, 0x0452, 0x0453, 0x0454, 0x0455, 0x0456, 0x0457,
            0x0458, 0x0459, 0x045a, 0x045b, 0x045c, 0x00a7, 0x045e, 0x045f,
        ],
    ),
    (
        6,
        [
            0x00a0, 0x0000, 0x0000, 0x0000, 0x00a4, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
            0x0000, 0x060c, 0x00ad, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
            0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x061b, 0x0000, 0x0000, 0x0000, 0x061f, 0x0000,
            0x0621, 0x0622, 0x0623, 0x0624, 0x0625, 0x0626, 0x0627, 0x0628, 0x0629, 0x062a, 0x062b,
            0x062c, 0x062d, 0x062e, 0x062f, 0x0630, 0x0631, 0x0632, 0x0633, 0x0634, 0x0635, 0x0636,
            0x0637, 0x0638, 0x0639, 0x063a, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0640, 0x0641,
            0x0642, 0x0643, 0x0644, 0x0645, 0x0646, 0x0647, 0x0648, 0x0649, 0x064a, 0x064b, 0x064c,
            0x064d, 0x064e, 0x064f, 0x0650, 0x0651, 0x0652, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
            0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
        ],
    ),
    (
        7,
        [
            0x00a0, 0x2018, 0x2019, 0x00a3, 0x0000, 0x0000, 0x00a6, 0x00a7, 0x00a8, 0x00a9, 0x0000,
            0x00ab, 0x00ac, 0x00ad, 0x0000, 0x2015, 0x00b0, 0x00b1, 0x00b2, 0x00b3, 0x0384, 0x0385,
            0x0386, 0x00b7, 0x0388, 0x0389, 0x038a, 0x00bb, 0x038c, 0x00bd, 0x038e, 0x038f, 0x0390,
            0x0391, 0x0392, 0x0393, 0x0394, 0x0395, 0x0396, 0x0397, 0x0398, 0x0399, 0x039a, 0x039b,
            0x039c, 0x039d, 0x039e, 0x039f, 0x03a0, 0x03a1, 0x0000, 0x03a3, 0x03a4, 0x03a5, 0x03a6,
            0x03a7, 0x03a8, 0x03a9, 0x03aa, 0x03ab, 0x03ac, 0x03ad, 0x03ae, 0x03af, 0x03b0, 0x03b1,
            0x03b2, 0x03b3, 0x03b4, 0x03b5, 0x03b6, 0x03b7, 0x03b8, 0x03b9, 0x03ba, 0x03bb, 0x03bc,
            0x03bd, 0x03be, 0x03bf, 0x03c0, 0x03c1, 0x03c2, 0x03c3, 0x03c4, 0x03c5, 0x03c6, 0x03c7,
            0x03c8, 0x03c9, 0x03ca, 0x03cb, 0x03cc, 0x03cd, 0x03ce, 0x0000,
        ],
    ),
    (
        8,
        [
            0x00a0, 0x0000, 0x00a2, 0x00a3, 0x00a4, 0x00a5, 0x00a6, 0x00a7, 0x00a8, 0x00a9, 0x00d7,
            0x00ab, 0x00ac, 0x00ad, 0x00ae, 0x00af, 0x00b0, 0x00b1, 0x00b2, 0x00b3, 0x00b4, 0x00b5,
            0x00b6, 0x00b7, 0x00b8, 0x00b9, 0x00f7, 0x00bb, 0x00bc, 0x00bd, 0x00be, 0x0000, 0x0000,
            0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
            0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
            0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x2017, 0x05d0, 0x05d1,
            0x05d2, 0x05d3, 0x05d4, 0x05d5, 0x05d6, 0x05d7, 0x05d8, 0x05d9, 0x05da, 0x05db, 0x05dc,
            0x05dd, 0x05de, 0x05df, 0x05e0, 0x05e1, 0x05e2, 0x05e3, 0x05e4, 0x05e5, 0x05e6, 0x05e7,
            0x05e8, 0x05e9, 0x05ea, 0x0000, 0x0000, 0x200e, 0x200f, 0x0000,
        ],
    ),
    (
        9,
        [
            0x00a0, 0x00a1, 0x00a2, 0x00a3, 0x00a4, 0x00a5, 0x00a6, 0x00a7, 0x00a8, 0x00a9, 0x00aa,
            0x00ab, 0x00ac, 0x00ad, 0x00ae, 0x00af, 0x00b0, 0x00b1, 0x00b2, 0x00b3, 0x00b4, 0x00b5,
            0x00b6, 0x00b7, 0x00b8, 0x00b9, 0x00ba, 0x00bb, 0x00bc, 0x00bd, 0x00be, 0x00bf, 0x00c0,
            0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c6, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb,
            0x00cc, 0x00cd, 0x00ce, 0x00cf, 0x011e, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x00d5, 0x00d6,
            0x00d7, 0x00d8, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x0130, 0x015e, 0x00df, 0x00e0, 0x00e1,
            0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x00e7, 0x00e8, 0x00e9, 0x00ea, 0x00eb, 0x00ec,
            0x00ed, 0x00ee, 0x00ef, 0x011f, 0x00f1, 0x00f2, 0x00f3, 0x00f4, 0x00f5, 0x00f6, 0x00f7,
            0x00f8, 0x00f9, 0x00fa, 0x00fb, 0x00fc, 0x0131, 0x015f, 0x00ff,
        ],
    ),
    (
        10,
        [
            0x00a0, 0x0104, 0x0112, 0x0122, 0x012a, 0x0128, 0x0136, 0x00a7, 0x013b, 0x0110, 0x0160,
            0x0166, 0x017d, 0x00ad, 0x016a, 0x014a, 0x00b0, 0x0105, 0x0113, 0x0123, 0x012b, 0x0129,
            0x0137, 0x00b7, 0x013c, 0x0111, 0x0161, 0x0167, 0x017e, 0x2015, 0x016b, 0x014b, 0x0100,
            0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c6, 0x012e, 0x010c, 0x00c9, 0x0118, 0x00cb,
            0x0116, 0x00cd, 0x00ce, 0x00cf, 0x00d0, 0x0145, 0x014c, 0x00d3, 0x00d4, 0x00d5, 0x00d6,
            0x0168, 0x00d8, 0x0172, 0x00da, 0x00db, 0x00dc, 0x00dd, 0x00de, 0x00df, 0x0101, 0x00e1,
            0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x012f, 0x010d, 0x00e9, 0x0119, 0x00eb, 0x0117,
            0x00ed, 0x00ee, 0x00ef, 0x00f0, 0x0146, 0x014d, 0x00f3, 0x00f4, 0x00f5, 0x00f6, 0x0169,
            0x00f8, 0x0173, 0x00fa, 0x00fb, 0x00fc, 0x00fd, 0x00fe, 0x0138,
        ],
    ),
    (
        11,
        [
            0x00a0, 0x0e01, 0x0e02, 0x0e03, 0x0e04, 0x0e05, 0x0e06, 0x0e07, 0x0e08, 0x0e09, 0x0e0a,
            0x0e0b, 0x0e0c, 0x0e0d, 0x0e0e, 0x0e0f, 0x0e10, 0x0e11, 0x0e12, 0x0e13, 0x0e14, 0x0e15,
            0x0e16, 0x0e17, 0x0e18, 0x0e19, 0x0e1a, 0x0e1b, 0x0e1c, 0x0e1d, 0x0e1e, 0x0e1f, 0x0e20,
            0x0e21, 0x0e22, 0x0e23, 0x0e24, 0x0e25, 0x0e26, 0x0e27, 0x0e28, 0x0e29, 0x0e2a, 0x0e2b,
            0x0e2c, 0x0e2d, 0x0e2e, 0x0e2f, 0x0e30, 0x0e31, 0x0e32, 0x0e33, 0x0e34, 0x0e35, 0x0e36,
            0x0e37, 0x0e38, 0x0e39, 0x0e3a, 0x0000, 0x0000, 0x0000, 0x0000, 0x0e3f, 0x0e40, 0x0e41,
            0x0e42, 0x0e43, 0x0e44, 0x0e45, 0x0e46, 0x0e47, 0x0e48, 0x0e49, 0x0e4a, 0x0e4b, 0x0e4c,
            0x0e4d, 0x0e4e, 0x0e4f, 0x0e50, 0x0e51, 0x0e52, 0x0e53, 0x0e54, 0x0e55, 0x0e56, 0x0e57,
            0x0e58, 0x0e59, 0x0e5a, 0x0e5b, 0x0000, 0x0000, 0x0000, 0x0000,
        ],
    ),
    (
        13,
        [
            0x00a0, 0x201d, 0x00a2, 0x00a3, 0x00a4, 0x201e, 0x00a6, 0x00a7, 0x00d8, 0x00a9, 0x0156,
            0x00ab, 0x00ac, 0x00ad, 0x00ae, 0x00c6, 0x00b0, 0x00b1, 0x00b2, 0x00b3, 0x201c, 0x00b5,
            0x00b6, 0x00b7, 0x00f8, 0x00b9, 0x0157, 0x00bb, 0x00bc, 0x00bd, 0x00be, 0x00e6, 0x0104,
            0x012e, 0x0100, 0x0106, 0x00c4, 0x00c5, 0x0118, 0x0112, 0x010c, 0x00c9, 0x0179, 0x0116,
            0x0122, 0x0136, 0x012a, 0x013b, 0x0160, 0x0143, 0x0145, 0x00d3, 0x014c, 0x00d5, 0x00d6,
            0x00d7, 0x0172, 0x0141, 0x015a, 0x016a, 0x00dc, 0x017b, 0x017d, 0x00df, 0x0105, 0x012f,
            0x0101, 0x0107, 0x00e4, 0x00e5, 0x0119, 0x0113, 0x010d, 0x00e9, 0x017a, 0x0117, 0x0123,
            0x0137, 0x012b, 0x013c, 0x0161, 0x0144, 0x0146, 0x00f3, 0x014d, 0x00f5, 0x00f6, 0x00f7,
            0x0173, 0x0142, 0x015b, 0x016b, 0x00fc, 0x017c, 0x017e, 0x2019,
        ],
    ),
    (
        14,
        [
            0x00a0, 0x1e02, 0x1e03, 0x00a3, 0x010a, 0x010b, 0x1e0a, 0x00a7, 0x1e80, 0x00a9, 0x1e82,
            0x1e0b, 0x1ef2, 0x00ad, 0x00ae, 0x0178, 0x1e1e, 0x1e1f, 0x0120, 0x0121, 0x1e40, 0x1e41,
            0x00b6, 0x1e56, 0x1e81, 0x1e57, 0x1e83, 0x1e60, 0x1ef3, 0x1e84, 0x1e85, 0x1e61, 0x00c0,
            0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c6, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb,
            0x00cc, 0x00cd, 0x00ce, 0x00cf, 0x0174, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x00d5, 0x00d6,
            0x1e6a, 0x00d8, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x00dd, 0x0176, 0x00df, 0x00e0, 0x00e1,
            0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x00e7, 0x00e8, 0x00e9, 0x00ea, 0x00eb, 0x00ec,
            0x00ed, 0x00ee, 0x00ef, 0x0175, 0x00f1, 0x00f2, 0x00f3, 0x00f4, 0x00f5, 0x00f6, 0x1e6b,
            0x00f8, 0x00f9, 0x00fa, 0x00fb, 0x00fc, 0x00fd, 0x0177, 0x00ff,
        ],
    ),
    (
        15,
        [
            0x00a0, 0x00a1, 0x00a2, 0x00a3, 0x20ac, 0x00a5, 0x0160, 0x00a7, 0x0161, 0x00a9, 0x00aa,
            0x00ab, 0x00ac, 0x00ad, 0x00ae, 0x00af, 0x00b0, 0x00b1, 0x00b2, 0x00b3, 0x017d, 0x00b5,
            0x00b6, 0x00b7, 0x017e, 0x00b9, 0x00ba, 0x00bb, 0x0152, 0x0153, 0x0178, 0x00bf, 0x00c0,
            0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c6, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb,
            0x00cc, 0x00cd, 0x00ce, 0x00cf, 0x00d0, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x00d5, 0x00d6,
            0x00d7, 0x00d8, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x00dd, 0x00de, 0x00df, 0x00e0, 0x00e1,
            0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x00e7, 0x00e8, 0x00e9, 0x00ea, 0x00eb, 0x00ec,
            0x00ed, 0x00ee, 0x00ef, 0x00f0, 0x00f1, 0x00f2, 0x00f3, 0x00f4, 0x00f5, 0x00f6, 0x00f7,
            0x00f8, 0x00f9, 0x00fa, 0x00fb, 0x00fc, 0x00fd, 0x00fe, 0x00ff,
        ],
    ),
    (
        16,
        [
            0x00a0, 0x0104, 0x0105, 0x0141, 0x20ac, 0x201e, 0x0160, 0x00a7, 0x0161, 0x00a9, 0x0218,
            0x00ab, 0x0179, 0x00ad, 0x017a, 0x017b, 0x00b0, 0x00b1, 0x010c, 0x0142, 0x017d, 0x201d,
            0x00b6, 0x00b7, 0x017e, 0x010d, 0x0219, 0x00bb, 0x0152, 0x0153, 0x0178, 0x017c, 0x00c0,
            0x00c1, 0x00c2, 0x0102, 0x00c4, 0x0106, 0x00c6, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb,
            0x00cc, 0x00cd, 0x00ce, 0x00cf, 0x0110, 0x0143, 0x00d2, 0x00d3, 0x00d4, 0x0150, 0x00d6,
            0x015a, 0x0170, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x0118, 0x021a, 0x00df, 0x00e0, 0x00e1,
            0x00e2, 0x0103, 0x00e4, 0x0107, 0x00e6, 0x00e7, 0x00e8, 0x00e9, 0x00ea, 0x00eb, 0x00ec,
            0x00ed, 0x00ee, 0x00ef, 0x0111, 0x0144, 0x00f2, 0x00f3, 0x00f4, 0x0151, 0x00f6, 0x015b,
            0x0171, 0x00f9, 0x00fa, 0x00fb, 0x00fc, 0x0119, 0x021b, 0x00ff,
        ],
    ),
];
