use super::error::CalcError;

/// C `SCALC_STRING_SIZE` (`sCalcPostfixPvt.h:18`) — the size of the `char`
/// array every sCalc string lives in.
pub const SCALC_STRING_SIZE: usize = 40;

/// The longest string sCalc can hold: the array minus its NUL terminator.
pub const SCALC_STRING_MAX: usize = SCALC_STRING_SIZE - 1;

/// A string as sCalc has them: at most [`SCALC_STRING_MAX`] bytes, with no NUL.
///
/// Every string on C's stack is `stackElement.local_string`, a
/// `char[SCALC_STRING_SIZE]` (`sCalcPostfixPvt.h:22-25`), and every operator
/// that writes one bounds the write to fit — the line numbers in the block
/// below are `sCalcPerform.c`, not the header this paragraph opened with:
///
/// ```c
/// strncat(ps->s, ps1->s, SCALC_STRING_SIZE-strlen(ps->s)-1);   /* ADD, :975 */
/// strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1);                 /* PRINTF :1566,
///                                                                 TR_ESC :1801,
///                                                                 ESC :1811,
///                                                                 XOR8 :1861 */
/// for (i=0; (i<SCALC_STRING_SIZE-1) && *post; ) *s++ = *post++; /* LITERAL_STRING
///                                                                  :1497 */
/// ```
///
/// and `strNcpy` (`sCalcPerform.c:68-74`) stops at the source's NUL as well as
/// at the bound. So the bound is not a property of any one operator — it is a
/// property of the value, and it is spelled here once, as a type, so that no
/// producer can forget it: `ScalcString` cannot be constructed longer than 39
/// bytes and cannot contain a NUL.
///
/// It holds BYTES, as C's `char[]` does. `LEN`, `BYTE`, the comparisons
/// (`strcmp`, i.e. unsigned-byte order) and `SUBRANGE` all count bytes, and the
/// record fields these values come from and go to (`PvString`) are byte-faithful
/// too, so nothing on the path re-encodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScalcString(Vec<u8>);

impl ScalcString {
    pub fn new() -> Self {
        ScalcString(Vec::new())
    }

    /// C `strNcpy(dest, src, SCALC_STRING_SIZE)` — copy until the source's NUL
    /// or the bound, whichever comes first. This is the ONLY way a `ScalcString`
    /// is built, so both halves of the invariant hold everywhere.
    pub fn from_c(bytes: impl AsRef<[u8]>) -> Self {
        let b = bytes.as_ref();
        let end = b
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(b.len())
            .min(SCALC_STRING_MAX);
        ScalcString(b[..end].to_vec())
    }

    /// C `strNcpy(dest, src, SCALC_STRING_SIZE-1)` — the OTHER bound, and it is
    /// one byte shorter: **38**.
    ///
    /// `strNcpy(dest, src, N)` copies while `ii < N-1` (`sCalcPerform.c:68-74`),
    /// so the bound is always `N-1`. The two `N`s C passes are not
    /// interchangeable:
    ///
    /// ```c
    /// strNcpy(ps->s, psarg[i], SCALC_STRING_SIZE);    /* FETCH_AA :1474  -> 39 */
    /// strNcpy(ps->s, tmpstr,   SCALC_STRING_SIZE-1);  /* PRINTF   :1566  -> 38 */
    /// ```
    ///
    /// The 38-byte form is what every opcode that builds its result in the
    /// scratch buffer `tmpstr` uses: PRINTF (`:1566`), BIN_WRITE (`:1632`),
    /// SSCANF's `%c`/`%[`/`%s` (`:1684`), TR_ESC (`:1801`), ESC (`:1814`) and
    /// the three replacing checksums CRC16 (`:1823`), LRC (`:1845`) and XOR8
    /// (`:1861`).
    ///
    /// The 39-byte form is FETCH_AA, LITERAL_STRING, AMODBUS's intermediate
    /// (`:1849`) and every `strncat` (`:975`, `:1825`, `:1850`, `:1863`), whose
    /// `SCALC_STRING_SIZE-strlen(ps->s)-1` bounds the TOTAL to 39.
    pub fn from_strncpy(bytes: impl AsRef<[u8]>) -> Self {
        let mut s = ScalcString::from_c(bytes);
        s.0.truncate(SCALC_STRING_SIZE - 2);
        s
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value as text, for the Rust-side callers that need `&str` (record
    /// fields keep the bytes). Non-UTF-8 bytes come back as U+FFFD; nothing in
    /// the engine round-trips through this.
    pub fn as_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }
}

impl From<&str> for ScalcString {
    fn from(s: &str) -> Self {
        ScalcString::from_c(s.as_bytes())
    }
}

impl From<String> for ScalcString {
    fn from(s: String) -> Self {
        ScalcString::from_c(s.as_bytes())
    }
}

impl From<&[u8]> for ScalcString {
    fn from(b: &[u8]) -> Self {
        ScalcString::from_c(b)
    }
}

impl From<Vec<u8>> for ScalcString {
    fn from(b: Vec<u8>) -> Self {
        ScalcString::from_c(b)
    }
}

impl PartialEq<str> for ScalcString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for ScalcString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl std::fmt::Display for ScalcString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str_lossy())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StackValue {
    Double(f64),
    Str(ScalcString),
}

impl StackValue {
    /// The one way to put a string on the stack. Every producer goes through it,
    /// so C's `char[40]` bound and NUL termination are structural rather than
    /// something each operator has to remember (see [`ScalcString`]).
    pub fn str(bytes: impl AsRef<[u8]>) -> StackValue {
        StackValue::Str(ScalcString::from_c(bytes))
    }

    /// The same, for the opcodes C bounds with `strNcpy(dest, src,
    /// SCALC_STRING_SIZE-1)` — a **38**-byte result, not 39. Which of the two
    /// bounds applies is a property of the opcode that produced the value, so
    /// it is spelled at the one place the value is built, never re-checked
    /// afterwards. See [`ScalcString::from_strncpy`] for the site list.
    pub fn str_ncpy(bytes: impl AsRef<[u8]>) -> StackValue {
        StackValue::Str(ScalcString::from_strncpy(bytes))
    }

    pub fn is_double(&self) -> bool {
        matches!(self, StackValue::Double(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, StackValue::Str(_))
    }

    /// C `toDouble` (sCalcPerform.c:80-83): a numeric operand position COERCES a
    /// string, it never rejects one —
    ///
    /// ```c
    /// #define toDouble(ps)  {if (isString(ps)) to_double(ps);}
    /// #define to_double(ps) {(ps)->d = atof((ps)->s); (ps)->s = NULL;}
    /// ```
    ///
    /// and `atof` is `strtod`, so it takes the leading numeric prefix and
    /// answers 0 when there is none. Compiled sCalc: `AA+1` is 13 for AA="12",
    /// 13 for AA="12abc", 1 for AA="abc", and 1001 for AA="1e3".
    ///
    /// This is the ONLY way to read a numeric operand off the sCalc stack.
    /// There is deliberately no fallible accessor: C has no type error in a
    /// numeric position, so the port must not be able to raise one. (The
    /// reverse — a STRING position handed a double — is a real C error for
    /// BIN_READ/BIN_WRITE/SSCANF, and that is what `as_bytes` is for.)
    pub fn to_double(&self) -> f64 {
        match self {
            StackValue::Double(v) => *v,
            StackValue::Str(s) => super::strtod::strtod(s.as_bytes()).value,
        }
    }

    /// A STRING operand: C's `if (isDouble(ps)) return(-1)`.
    pub fn as_bytes(&self) -> Result<&[u8], CalcError> {
        match self {
            StackValue::Str(s) => Ok(s.as_bytes()),
            StackValue::Double(_) => Err(CalcError::TypeMismatch),
        }
    }

    /// C `toString` (sCalcPerform.c:86-96) — the stack element's STRING form.
    ///
    /// ```c
    /// #define toString(ps) {if (isDouble(ps)) to_string(ps);}
    /// ```
    ///
    /// so a double becomes text by [`cvt::to_string`](super::cvt::to_string) —
    /// `cvtDoubleToString(d, s, 8)` — and by nothing else. This is the ONLY way
    /// to read a string operand that C coerces rather than rejects.
    pub fn into_string_value(self) -> ScalcString {
        match self {
            StackValue::Str(s) => s,
            StackValue::Double(v) => ScalcString::from_c(super::cvt::to_string(v)),
        }
    }
}
