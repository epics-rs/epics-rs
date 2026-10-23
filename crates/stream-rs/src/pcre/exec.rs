//! `pcre_exec.c` for this build: no UTF, UCP or JIT, and no `pcre_extra`,
//! because StreamDevice calls `pcre_exec(code, NULL, ...)`. Without an
//! extra the limits are the compile-time MATCH_LIMIT and
//! MATCH_LIMIT_RECURSION (lowered by `(*LIMIT_...)`), and no MARK is ever
//! handed back, so `md->mark`, `md->nomatch_mark` and each `save_mark` are
//! left out: match() writes them and never reads them. `pcre_callout` is
//! NULL in StreamDevice, so OP_CALLOUT is only skipped.
//!
//! C's match() recurses natively through RMATCH. Here each incarnation is a
//! `Frame` on a heap stack and the function body is a state machine:
//! RMATCH pushes a child and records the label (C's RM number) to resume
//! at, RRETURN pops back to that label with the child's result. Pointers
//! into the subject, the code and other frames become indices.

use super::internal::*;
use super::newline;

const OVFLMASK: u32 = 0xffff_0000;
const OVFLBIT: u32 = 0x0001_0000;

const MATCH_CONDASSERT: i32 = 1;
const MATCH_CBEGROUP: i32 = 2;

const MATCH_MATCH: i32 = 1;
const MATCH_NOMATCH: i32 = 0;
const MATCH_ACCEPT: i32 = -999;
const MATCH_KETRPOS: i32 = -998;
const MATCH_ONCE: i32 = -997;
const MATCH_COMMIT: i32 = -996;
const MATCH_PRUNE: i32 = -995;
const MATCH_SKIP: i32 = -994;
const MATCH_SKIP_ARG: i32 = -993;
const MATCH_THEN: i32 = -992;
const MATCH_BACKTRACK_MAX: i32 = MATCH_THEN;
const MATCH_BACKTRACK_MIN: i32 = MATCH_COMMIT;

const REP_MIN: [i32; 11] = [0, 0, 1, 1, 0, 0, 0, 0, 0, 1, 0];
const REP_MAX: [i32; 11] = [0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1];

const INT_MAX: i32 = i32::MAX;

/// `match_data`, less the fields only callouts, MARKs, UTF and study use.
/// Subject positions index `s` (`start_subject` is 0) and code positions
/// index `code` (`start_code`).
struct Md<'a> {
    s: &'a [u8],
    code: &'a [u8],
    name_table: &'a [u8],
    name_entry_size: usize,
    /// `offset_vector`: the caller's vector, or the temporary one when the
    /// back references need more than the caller gave.
    ovec: Vec<i32>,
    offset_end: usize,
    offset_max: usize,
    lcc: &'a [u8],
    fcc: &'a [u8],
    ctypes: &'a [u8],
    nltype: i32,
    nllen: i32,
    nl: [u8; 2],
    partial: i32,
    end_subject: usize,
    start_offset: usize,
    /// Overloaded as in C: a subject position, the THEN opcode for
    /// MATCH_THEN, or the SKIP name for MATCH_SKIP_ARG.
    start_match_ptr: usize,
    end_match_ptr: usize,
    start_used_ptr: usize,
    end_offset_top: usize,
    capture_last: u32,
    match_call_count: u64,
    match_limit: u64,
    match_limit_recursion: u64,
    match_function_type: i32,
    /// The frame whose OP_RECURSE is innermost.
    recursive: Option<usize>,
    once_target: usize,
    hitend: bool,
    notbol: bool,
    noteol: bool,
    notempty: bool,
    notempty_atstart: bool,
    endonly: bool,
    jscript_compat: bool,
    bsr_anycrlf: bool,
    hasthen: bool,
    skip_arg_count: i32,
    ignore_skip_arg: i32,
}

impl Md<'_> {
    /// IS_NEWLINE.
    fn is_newline(&mut self, p: usize) -> bool {
        if self.nltype != NLTYPE_FIXED {
            p < self.end_subject
                && newline::is_newline(self.s, p, self.nltype, self.end_subject, &mut self.nllen)
        } else {
            let nllen = self.nllen as usize;
            p + nllen <= self.end_subject
                && self.s[p] == self.nl[0]
                && (nllen == 1 || self.s[p + 1] == self.nl[1])
        }
    }

    /// WAS_NEWLINE.
    fn was_newline(&mut self, p: usize) -> bool {
        if self.nltype != NLTYPE_FIXED {
            p > 0 && newline::was_newline(self.s, p, self.nltype, 0, &mut self.nllen)
        } else {
            let nllen = self.nllen as usize;
            p >= nllen
                && self.s[p - nllen] == self.nl[0]
                && (nllen == 1 || self.s[p - nllen + 1] == self.nl[1])
        }
    }

    /// The CRLF partial test after OP_ANY: is `c` the first half of a
    /// two-character fixed newline?
    fn crlf_half(&self, c: u8) -> bool {
        self.nltype == NLTYPE_FIXED && self.nllen == 2 && c == self.nl[0]
    }

    fn ctype(&self, c: u8, bit: u8) -> bool {
        self.ctypes[c as usize] & bit != 0
    }
}

fn is_hspace(c: u8) -> bool {
    matches!(c, 0x09 | 0x20 | 0xa0)
}

fn is_vspace(c: u8) -> bool {
    matches!(c, 0x0a..=0x0d | 0x85)
}

/// The one-byte test of a character type; None for OP_ANY, OP_ALLANY,
/// OP_ANYBYTE and OP_ANYNL, which C codes separately, and for an opcode
/// that is not a character type.
fn type_matches(md: &Md, ctype: u8, c: u8) -> Option<bool> {
    Some(match ctype {
        OP_NOT_DIGIT => !md.ctype(c, CTYPE_DIGIT),
        OP_DIGIT => md.ctype(c, CTYPE_DIGIT),
        OP_NOT_WHITESPACE => !md.ctype(c, CTYPE_SPACE),
        OP_WHITESPACE => md.ctype(c, CTYPE_SPACE),
        OP_NOT_WORDCHAR => !md.ctype(c, CTYPE_WORD),
        OP_WORDCHAR => md.ctype(c, CTYPE_WORD),
        OP_NOT_HSPACE => !is_hspace(c),
        OP_HSPACE => is_hspace(c),
        OP_NOT_VSPACE => !is_vspace(c),
        OP_VSPACE => is_vspace(c),
        _ => return None,
    })
}

/// `strcmp` of two NUL-terminated names in the code.
fn names_equal(code: &[u8], a: usize, b: usize) -> bool {
    let end = |p: usize| p + code[p..].iter().position(|&c| c == 0).unwrap();
    code[a..end(a)] == code[b..end(b)]
}

/// `match_ref`: the subject bytes matched by back reference `offset`, -1
/// for no match, -2 for a partial match at the end of the subject.
fn match_ref(md: &Md, offset: usize, eptr: usize, length: i32, caseless: bool) -> i32 {
    if length < 0 {
        return -1;
    }
    let s = md.s;
    let mut e = eptr;
    for p in (md.ovec[offset] as usize..).take(length as usize) {
        if e >= md.end_subject {
            return -2;
        }
        let same = if caseless {
            md.lcc[s[p] as usize] == md.lcc[s[e] as usize]
        } else {
            s[p] == s[e]
        };
        if !same {
            return -1;
        }
        e += 1;
    }
    (e - eptr) as i32
}

/// `recursion_info`, kept in the frame that ran the OP_RECURSE.
struct Recursion {
    group_num: u32,
    saved_capture_last: u32,
    subject_position: usize,
    prevrec: Option<usize>,
    offset_save: Vec<i32>,
}

/// Where a frame resumes: C's TAIL_RECURSE, the opcode loop, a label
/// inside an opcode, or the statement after an RMATCH (named by its RM
/// number).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Pc {
    #[default]
    TailRecurse,
    Op,
    BraLoop,
    PossessiveCapture,
    PossessiveCaptureLoop,
    PossessiveNonCapture,
    PossessiveNonCaptureLoop,
    CondChoose,
    RecurseLoop,
    Rm1,
    Rm2,
    Rm3,
    Rm4,
    Rm5,
    Rm6,
    Rm7,
    Rm8,
    Rm9,
    Rm10,
    Rm11,
    Rm12,
    Rm13,
    Rm14,
    Rm15,
    Rm17,
    Rm19,
    Rm24,
    Rm25,
    Rm26,
    Rm27,
    Rm29,
    Rm31,
    Rm33,
    Rm35,
    Rm43,
    Rm47,
    Rm48,
    Rm49,
    Rm50,
    Rm51,
    Rm52,
    Rm53,
    Rm54,
    Rm55,
    Rm56,
    Rm57,
    Rm58,
    Rm63,
    Rm64,
    Rm65,
    Rm66,
}

/// The locals of one match() incarnation that live across an RMATCH.
/// C's doubled-up names (allow_zero for cur_is_word and so on) get their
/// own fields.
#[derive(Default)]
struct Frame {
    ret: Pc,
    eptr: usize,
    ecode: usize,
    mstart: usize,
    offset_top: usize,
    /// The frame holding the innermost `eptrblock`, as `newptrb`.
    eptrb: Option<usize>,
    rdepth: u32,
    /// `newptrb`: `epb_saved_eptr`, `epb_prev`.
    newptrb: (usize, Option<usize>),
    fi: i32,
    fc: u32,
    foc: u32,
    minimize: bool,
    possessive: bool,
    caseless: bool,
    allow_zero: bool,
    matched_once: bool,
    condition: bool,
    condassert: bool,
    cbegroup: bool,
    condcode: u8,
    callpat: usize,
    data: usize,
    next: usize,
    pp: usize,
    prev: usize,
    /// None is C's NULL.
    saved_eptr: Option<usize>,
    recursion: Option<Box<Recursion>>,
    codelink: usize,
    code_offset: usize,
    ctype: u8,
    length: i32,
    max: i32,
    min: i32,
    number: u32,
    offset: usize,
    op: u8,
    save_capture_last: u32,
    save_offset1: i32,
    save_offset2: i32,
    save_offset3: i32,
}

/// The frames that fit C's default 8 MiB thread stack.
const FRAME_STACK_MAX: usize = (8 << 20) / std::mem::size_of::<Frame>();

/// match(): MATCH_MATCH, MATCH_NOMATCH, a negative MATCH_xxx for the
/// backtracking verbs, or a PCRE_ERROR_xxx.
fn run_match(md: &mut Md, stack: &mut Vec<Frame>, eptr: usize, mstart: usize) -> i32 {
    let s = md.s;
    let code = md.code;
    let mut f = Frame {
        eptr,
        mstart,
        offset_top: 2,
        ..Frame::default()
    };
    let mut pc = Pc::TailRecurse;
    let mut rrc: i32 = 0;

    // The macros are defined inside the loop so that `'run` resolves:
    // macro_rules labels are hygienic.
    'run: loop {
        macro_rules! rmatch {
            ($eptr:expr, $ecode:expr, $eptrb:expr, $ret:expr) => {{
                let child = Frame {
                    eptr: $eptr,
                    ecode: $ecode,
                    mstart: f.mstart,
                    offset_top: f.offset_top,
                    eptrb: $eptrb,
                    rdepth: f.rdepth + 1,
                    ..Frame::default()
                };
                f.ret = $ret;
                if stack.len() == stack.capacity() {
                    let more = stack.capacity().max(4).min(FRAME_STACK_MAX - stack.len());
                    stack.reserve_exact(more);
                }
                stack.push(std::mem::replace(&mut f, child));
                pc = Pc::TailRecurse;
                continue 'run;
            }};
        }
        macro_rules! rreturn {
            ($v:expr) => {{
                let v = $v;
                match stack.pop() {
                    None => {
                        #[cfg(test)]
                        tests::HIGH_WATER.set(stack.capacity());
                        return v;
                    }
                    Some(parent) => {
                        f = parent;
                        rrc = v;
                        pc = f.ret;
                        continue 'run;
                    }
                }
            }};
        }
        macro_rules! scheck_partial {
            () => {
                if md.partial != 0 && f.eptr > md.start_used_ptr {
                    md.hitend = true;
                    if md.partial > 1 {
                        rreturn!(PCRE_ERROR_PARTIAL);
                    }
                }
            };
        }
        macro_rules! check_partial {
            () => {
                if md.partial != 0 && f.eptr >= md.end_subject && f.eptr > md.start_used_ptr {
                    md.hitend = true;
                    if md.partial > 1 {
                        rreturn!(PCRE_ERROR_PARTIAL);
                    }
                }
            };
        }
        // A THEN within the current branch of a group with alternatives is
        // caught here and becomes NOMATCH; otherwise it passes outwards.
        macro_rules! then_check {
            () => {
                if rrc == MATCH_THEN {
                    f.next = f.ecode + get(code, f.ecode + 1);
                    if md.start_match_ptr < f.next
                        && (code[f.ecode] == OP_ALT || code[f.next] == OP_ALT)
                    {
                        rrc = MATCH_NOMATCH;
                    }
                }
            };
        }
        macro_rules! len {
            ($at:expr) => {
                OP_LENGTHS[code[$at] as usize] as usize
            };
        }
        // The frame holding a recursion_info or eptrblock: the current one is
        // not on the stack.
        macro_rules! frame_at {
            ($i:expr) => {
                if $i == stack.len() { &f } else { &stack[$i] }
            };
        }
        macro_rules! rec_at {
            ($i:expr) => {
                frame_at!($i).recursion.as_deref().unwrap()
            };
        }
        // The back-off loop of a maximizing repeat: at the first optional
        // position, carry on by tail recursion.
        macro_rules! back_off {
            ($ret:expr) => {{
                if f.eptr == f.pp {
                    pc = Pc::TailRecurse;
                    continue 'run;
                }
                rmatch!(f.eptr, f.ecode, f.eptrb, $ret);
            }};
        }

        match pc {
            Pc::TailRecurse => {
                let count = md.match_call_count;
                md.match_call_count += 1;
                if count >= md.match_limit {
                    rreturn!(PCRE_ERROR_MATCHLIMIT);
                }
                // C recurses on its 8 MiB stack and dies there long before
                // MATCH_LIMIT_RECURSION; the frames stop at as many bytes.
                if u64::from(f.rdepth) >= md.match_limit_recursion || stack.len() >= FRAME_STACK_MAX
                {
                    rreturn!(PCRE_ERROR_RECURSIONLIMIT);
                }
                if md.match_function_type == MATCH_CBEGROUP {
                    f.newptrb = (f.eptr, f.eptrb);
                    f.eptrb = Some(stack.len());
                    md.match_function_type = 0;
                }
                pc = Pc::Op;
            }

            Pc::Op => {
                f.minimize = false;
                f.possessive = false;
                f.op = code[f.ecode];
                let op = f.op;
                match op {
                    OP_MARK => {
                        rmatch!(
                            f.eptr,
                            f.ecode + len!(f.ecode) + code[f.ecode + 1] as usize,
                            f.eptrb,
                            Pc::Rm55
                        );
                    }
                    OP_FAIL => rreturn!(MATCH_NOMATCH),
                    OP_COMMIT => rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm52),
                    OP_PRUNE => rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm51),
                    OP_PRUNE_ARG => {
                        rmatch!(
                            f.eptr,
                            f.ecode + len!(f.ecode) + code[f.ecode + 1] as usize,
                            f.eptrb,
                            Pc::Rm56
                        );
                    }
                    OP_SKIP => rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm53),
                    OP_SKIP_ARG => {
                        md.skip_arg_count += 1;
                        if md.skip_arg_count <= md.ignore_skip_arg {
                            f.ecode += len!(f.ecode) + code[f.ecode + 1] as usize;
                        } else {
                            rmatch!(
                                f.eptr,
                                f.ecode + len!(f.ecode) + code[f.ecode + 1] as usize,
                                f.eptrb,
                                Pc::Rm57
                            );
                        }
                    }
                    OP_THEN => rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm54),
                    OP_THEN_ARG => {
                        rmatch!(
                            f.eptr,
                            f.ecode + len!(f.ecode) + code[f.ecode + 1] as usize,
                            f.eptrb,
                            Pc::Rm58
                        );
                    }

                    // An atomic group without captures, handled like an
                    // assertion.
                    OP_ONCE_NC => {
                        f.prev = f.ecode;
                        f.saved_eptr = Some(f.eptr);
                        rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, f.eptrb, Pc::Rm64);
                    }

                    OP_CBRA | OP_SCBRA => {
                        f.number = get2(code, f.ecode + 1 + LINK_SIZE);
                        f.offset = (f.number as usize) << 1;
                        if f.offset < md.offset_max {
                            let number = f.number as usize;
                            f.save_offset1 = md.ovec[f.offset];
                            f.save_offset2 = md.ovec[f.offset + 1];
                            f.save_offset3 = md.ovec[md.offset_end - number];
                            f.save_capture_last = md.capture_last;
                            md.ovec[md.offset_end - number] = f.eptr as i32;
                            if op >= OP_SBRA {
                                md.match_function_type = MATCH_CBEGROUP;
                            }
                            rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm1);
                        }
                        // No room to save the capture: a non-capturing group.
                        pc = Pc::BraLoop;
                    }
                    OP_ONCE | OP_BRA | OP_SBRA => pc = Pc::BraLoop,

                    OP_CBRAPOS | OP_SCBRAPOS => {
                        f.allow_zero = false;
                        pc = Pc::PossessiveCapture;
                    }
                    OP_BRAPOS | OP_SBRAPOS => {
                        f.allow_zero = false;
                        pc = Pc::PossessiveNonCapture;
                    }

                    OP_COND | OP_SCOND => {
                        f.codelink = get(code, f.ecode + 1);
                        f.ecode += 1 + LINK_SIZE;
                        // An auto-callout sits between OP_COND and an
                        // assertion condition.
                        if code[f.ecode] == OP_CALLOUT {
                            let n = OP_LENGTHS[OP_CALLOUT as usize] as usize;
                            f.ecode += n;
                            f.codelink -= n;
                        }
                        f.condition = false;
                        f.condcode = code[f.ecode];
                        match f.condcode {
                            OP_RREF => {
                                if let Some(r) = md.recursive {
                                    let recno = get2(code, f.ecode + 1);
                                    f.condition =
                                        recno == RREF_ANY || recno == rec_at!(r).group_num;
                                }
                            }
                            OP_DNRREF => {
                                if let Some(r) = md.recursive {
                                    let group_num = rec_at!(r).group_num;
                                    let count = get2(code, f.ecode + 1 + IMM2_SIZE);
                                    let mut slot =
                                        get2(code, f.ecode + 1) as usize * md.name_entry_size;
                                    for _ in 0..count {
                                        f.condition = get2(md.name_table, slot) == group_num;
                                        if f.condition {
                                            break;
                                        }
                                        slot += md.name_entry_size;
                                    }
                                }
                            }
                            OP_CREF => {
                                f.offset = (get2(code, f.ecode + 1) as usize) << 1;
                                f.condition = f.offset < f.offset_top && md.ovec[f.offset] >= 0;
                            }
                            OP_DNCREF => {
                                let count = get2(code, f.ecode + 1 + IMM2_SIZE);
                                let mut slot =
                                    get2(code, f.ecode + 1) as usize * md.name_entry_size;
                                for _ in 0..count {
                                    f.offset = (get2(md.name_table, slot) as usize) << 1;
                                    f.condition = f.offset < f.offset_top && md.ovec[f.offset] >= 0;
                                    if f.condition {
                                        break;
                                    }
                                    slot += md.name_entry_size;
                                }
                            }
                            OP_DEF | OP_FAIL => {}
                            _ => {
                                md.match_function_type = MATCH_CONDASSERT;
                                rmatch!(f.eptr, f.ecode, None, Pc::Rm3);
                            }
                        }
                        pc = Pc::CondChoose;
                    }

                    // Before OP_ACCEPT, OP_CLOSEs close the open groups.
                    OP_CLOSE => {
                        f.number = get2(code, f.ecode + 1);
                        f.offset = (f.number as usize) << 1;
                        md.capture_last = (md.capture_last & OVFLMASK) | f.number;
                        if f.offset >= md.offset_max {
                            md.capture_last |= OVFLBIT;
                        } else {
                            md.ovec[f.offset] = md.ovec[md.offset_end - f.number as usize];
                            md.ovec[f.offset + 1] = f.eptr as i32;
                            if f.offset >= f.offset_top {
                                md.ovec[f.offset_top..f.offset].fill(-1);
                                f.offset_top = f.offset + 2;
                            }
                        }
                        f.ecode += 1 + IMM2_SIZE;
                    }

                    OP_END | OP_ACCEPT | OP_ASSERT_ACCEPT => {
                        if f.eptr == f.mstart
                            && op != OP_ASSERT_ACCEPT
                            && md.recursive.is_none()
                            && (md.notempty || (md.notempty_atstart && f.mstart == md.start_offset))
                        {
                            rreturn!(MATCH_NOMATCH);
                        }
                        md.end_match_ptr = f.eptr;
                        md.end_offset_top = f.offset_top;
                        md.start_match_ptr = f.mstart;
                        rreturn!(if op == OP_END {
                            MATCH_MATCH
                        } else {
                            MATCH_ACCEPT
                        });
                    }

                    OP_ASSERT | OP_ASSERTBACK => {
                        f.condassert = md.match_function_type == MATCH_CONDASSERT;
                        if f.condassert {
                            md.match_function_type = 0;
                        }
                        rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, None, Pc::Rm4);
                    }
                    OP_ASSERT_NOT | OP_ASSERTBACK_NOT => {
                        f.condassert = md.match_function_type == MATCH_CONDASSERT;
                        if f.condassert {
                            md.match_function_type = 0;
                        }
                        rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, None, Pc::Rm5);
                    }

                    // The start of each lookbehind branch: a byte count.
                    OP_REVERSE => {
                        let back = get(code, f.ecode + 1);
                        if back > f.eptr {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.eptr -= back;
                        if f.eptr < md.start_used_ptr {
                            md.start_used_ptr = f.eptr;
                        }
                        f.ecode += 1 + LINK_SIZE;
                    }

                    OP_CALLOUT => f.ecode += 2 + 2 * LINK_SIZE,

                    OP_RECURSE => {
                        f.callpat = get(code, f.ecode + 1);
                        let recno = if f.callpat == 0 {
                            0
                        } else {
                            get2(code, f.callpat + 1 + LINK_SIZE)
                        };
                        // Recursing again without advancing the subject.
                        let mut ri = md.recursive;
                        while let Some(r) = ri {
                            let rec = rec_at!(r);
                            if recno == rec.group_num && f.eptr == rec.subject_position {
                                rreturn!(PCRE_ERROR_RECURSELOOP);
                            }
                            ri = rec.prevrec;
                        }
                        let offset_save = md.ovec[..md.offset_end].to_vec();
                        let rec = Recursion {
                            group_num: recno,
                            saved_capture_last: md.capture_last,
                            subject_position: f.eptr,
                            prevrec: md.recursive,
                            offset_save,
                        };
                        match &mut f.recursion {
                            Some(b) => **b = rec,
                            None => f.recursion = Some(Box::new(rec)),
                        }
                        md.recursive = Some(stack.len());
                        f.ecode += 1 + LINK_SIZE;
                        f.cbegroup = code[f.callpat] >= OP_SBRA;
                        pc = Pc::RecurseLoop;
                    }

                    OP_ALT => {
                        while code[f.ecode] == OP_ALT {
                            f.ecode += get(code, f.ecode + 1);
                        }
                    }

                    // Before a group that may occur zero times.
                    OP_BRAZERO => {
                        f.next = f.ecode + 1;
                        rmatch!(f.eptr, f.next, f.eptrb, Pc::Rm10);
                    }
                    OP_BRAMINZERO => {
                        f.next = f.ecode + 1;
                        loop {
                            f.next += get(code, f.next + 1);
                            if code[f.next] != OP_ALT {
                                break;
                            }
                        }
                        rmatch!(f.eptr, f.next + 1 + LINK_SIZE, f.eptrb, Pc::Rm11);
                    }
                    OP_SKIPZERO => {
                        f.next = f.ecode + 1;
                        loop {
                            f.next += get(code, f.next + 1);
                            if code[f.next] != OP_ALT {
                                break;
                            }
                        }
                        f.ecode = f.next + 1 + LINK_SIZE;
                    }
                    OP_BRAPOSZERO => {
                        f.ecode += 1;
                        f.op = code[f.ecode];
                        f.allow_zero = true;
                        pc = if f.op == OP_CBRAPOS || f.op == OP_SCBRAPOS {
                            Pc::PossessiveCapture
                        } else {
                            Pc::PossessiveNonCapture
                        };
                    }

                    OP_KET | OP_KETRMIN | OP_KETRMAX | OP_KETRPOS => {
                        f.prev = f.ecode - get(code, f.ecode + 1);
                        let prev_op = code[f.prev];
                        if prev_op >= OP_SBRA || prev_op == OP_ONCE {
                            let i = f.eptrb.expect("a could-be-empty group has an eptrblock");
                            let (saved, up) = frame_at!(i).newptrb;
                            f.saved_eptr = Some(saved);
                            f.eptrb = up;
                        } else {
                            f.saved_eptr = None;
                        }

                        // The end of an assertion or of a capture-free
                        // atomic group.
                        if (OP_ASSERT..=OP_ASSERTBACK_NOT).contains(&prev_op)
                            || prev_op == OP_ONCE_NC
                        {
                            md.end_match_ptr = f.eptr;
                            md.end_offset_top = f.offset_top;
                            md.start_match_ptr = f.mstart;
                            rreturn!(MATCH_MATCH);
                        }

                        if matches!(prev_op, OP_CBRA | OP_SCBRA | OP_CBRAPOS | OP_SCBRAPOS) {
                            f.number = get2(code, f.prev + 1 + LINK_SIZE);
                            f.offset = (f.number as usize) << 1;
                            if let Some(r) = md.recursive
                                && rec_at!(r).group_num == f.number
                            {
                                md.end_match_ptr = f.eptr;
                                md.start_match_ptr = f.mstart;
                                rreturn!(MATCH_MATCH);
                            }
                            md.capture_last = (md.capture_last & OVFLMASK) | f.number;
                            if f.offset >= md.offset_max {
                                md.capture_last |= OVFLBIT;
                            } else {
                                // A skipped group's offsets are unset.
                                if f.offset > f.offset_top {
                                    md.ovec[f.offset_top..f.offset].fill(-1);
                                }
                                md.ovec[f.offset] = md.ovec[md.offset_end - f.number as usize];
                                md.ovec[f.offset + 1] = f.eptr as i32;
                                if f.offset_top <= f.offset {
                                    f.offset_top = f.offset + 2;
                                }
                            }
                        }

                        if op == OP_KETRPOS {
                            md.start_match_ptr = f.mstart;
                            md.end_match_ptr = f.eptr;
                            md.end_offset_top = f.offset_top;
                            rreturn!(MATCH_KETRPOS);
                        }

                        // A non-repeating ket, or a repeat that matched no
                        // characters.
                        if op == OP_KET || Some(f.eptr) == f.saved_eptr {
                            if prev_op == OP_ONCE {
                                rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, f.eptrb, Pc::Rm12);
                            }
                            f.ecode += 1 + LINK_SIZE;
                        } else if op == OP_KETRMIN {
                            rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, f.eptrb, Pc::Rm7);
                        } else {
                            rmatch!(f.eptr, f.prev, f.eptrb, Pc::Rm13);
                        }
                    }

                    OP_CIRC | OP_SOD => {
                        if op == OP_CIRC && md.notbol && f.eptr == 0 {
                            rreturn!(MATCH_NOMATCH);
                        }
                        if f.eptr != 0 {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.ecode += 1;
                    }
                    OP_CIRCM => {
                        if md.notbol && f.eptr == 0 {
                            rreturn!(MATCH_NOMATCH);
                        }
                        if f.eptr != 0 && (f.eptr == md.end_subject || !md.was_newline(f.eptr)) {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.ecode += 1;
                    }
                    OP_SOM => {
                        if f.eptr != md.start_offset {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.ecode += 1;
                    }
                    OP_SET_SOM => {
                        f.mstart = f.eptr;
                        f.ecode += 1;
                    }
                    OP_DOLLM => {
                        if f.eptr < md.end_subject {
                            if !md.is_newline(f.eptr) {
                                if md.partial != 0
                                    && f.eptr + 1 >= md.end_subject
                                    && md.crlf_half(s[f.eptr])
                                {
                                    md.hitend = true;
                                    if md.partial > 1 {
                                        rreturn!(PCRE_ERROR_PARTIAL);
                                    }
                                }
                                rreturn!(MATCH_NOMATCH);
                            }
                        } else {
                            if md.noteol {
                                rreturn!(MATCH_NOMATCH);
                            }
                            scheck_partial!();
                        }
                        f.ecode += 1;
                    }
                    OP_DOLL | OP_EOD | OP_EODN => {
                        if op == OP_DOLL && md.noteol {
                            rreturn!(MATCH_NOMATCH);
                        }
                        if op == OP_EOD || (op == OP_DOLL && md.endonly) {
                            // \z
                            if f.eptr < md.end_subject {
                                rreturn!(MATCH_NOMATCH);
                            }
                        } else if f.eptr < md.end_subject
                            && (!md.is_newline(f.eptr)
                                || f.eptr + md.nllen as usize != md.end_subject)
                        {
                            // \Z: the end, or a newline that ends the subject
                            if md.partial != 0
                                && f.eptr + 1 >= md.end_subject
                                && md.crlf_half(s[f.eptr])
                            {
                                md.hitend = true;
                                if md.partial > 1 {
                                    rreturn!(PCRE_ERROR_PARTIAL);
                                }
                            }
                            rreturn!(MATCH_NOMATCH);
                        }
                        scheck_partial!();
                        f.ecode += 1;
                    }

                    OP_NOT_WORD_BOUNDARY | OP_WORD_BOUNDARY => {
                        let prev_is_word = if f.eptr == 0 {
                            false
                        } else {
                            if f.eptr <= md.start_used_ptr {
                                md.start_used_ptr = f.eptr - 1;
                            }
                            md.ctype(s[f.eptr - 1], CTYPE_WORD)
                        };
                        let cur_is_word = if f.eptr >= md.end_subject {
                            scheck_partial!();
                            false
                        } else {
                            md.ctype(s[f.eptr], CTYPE_WORD)
                        };
                        f.ecode += 1;
                        if (op == OP_WORD_BOUNDARY) == (cur_is_word == prev_is_word) {
                            rreturn!(MATCH_NOMATCH);
                        }
                    }

                    OP_ANY | OP_ALLANY => {
                        if op == OP_ANY {
                            if md.is_newline(f.eptr) {
                                rreturn!(MATCH_NOMATCH);
                            }
                            if md.partial != 0
                                && f.eptr + 1 == md.end_subject
                                && md.crlf_half(s[f.eptr])
                            {
                                md.hitend = true;
                                if md.partial > 1 {
                                    rreturn!(PCRE_ERROR_PARTIAL);
                                }
                            }
                        }
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.eptr += 1;
                        f.ecode += 1;
                    }
                    OP_ANYBYTE => {
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.eptr += 1;
                        f.ecode += 1;
                    }
                    OP_NOT_DIGIT | OP_DIGIT | OP_NOT_WHITESPACE | OP_WHITESPACE
                    | OP_NOT_WORDCHAR | OP_WORDCHAR | OP_NOT_HSPACE | OP_HSPACE | OP_NOT_VSPACE
                    | OP_VSPACE => {
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        let c = s[f.eptr];
                        f.eptr += 1;
                        if type_matches(md, op, c) != Some(true) {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.ecode += 1;
                    }
                    OP_ANYNL => {
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        let c = s[f.eptr];
                        f.eptr += 1;
                        match c {
                            b'\r' => {
                                if f.eptr >= md.end_subject {
                                    scheck_partial!();
                                } else if s[f.eptr] == b'\n' {
                                    f.eptr += 1;
                                }
                            }
                            b'\n' => {}
                            0x0b | 0x0c | 0x85 => {
                                if md.bsr_anycrlf {
                                    rreturn!(MATCH_NOMATCH);
                                }
                            }
                            _ => rreturn!(MATCH_NOMATCH),
                        }
                        f.ecode += 1;
                    }

                    // A back reference, possibly repeated. An unset one has
                    // length -1 (0 in JavaScript mode), so it fails unless
                    // the repeat allows zero.
                    OP_DNREF | OP_DNREFI | OP_REF | OP_REFI => {
                        if op == OP_DNREF || op == OP_DNREFI {
                            f.caseless = op == OP_DNREFI;
                            let count = get2(code, f.ecode + 1 + IMM2_SIZE);
                            let mut slot = get2(code, f.ecode + 1) as usize * md.name_entry_size;
                            f.ecode += 1 + 2 * IMM2_SIZE;
                            f.length = if md.jscript_compat { 0 } else { -1 };
                            f.offset = 0;
                            for _ in 0..count {
                                f.offset = (get2(md.name_table, slot) as usize) << 1;
                                if f.offset < f.offset_top && md.ovec[f.offset] >= 0 {
                                    f.length = md.ovec[f.offset + 1] - md.ovec[f.offset];
                                    break;
                                }
                                slot += md.name_entry_size;
                            }
                        } else {
                            f.caseless = op == OP_REFI;
                            f.offset = (get2(code, f.ecode + 1) as usize) << 1;
                            f.ecode += 1 + IMM2_SIZE;
                            f.length = if f.offset >= f.offset_top || md.ovec[f.offset] < 0 {
                                if md.jscript_compat { 0 } else { -1 }
                            } else {
                                md.ovec[f.offset + 1] - md.ovec[f.offset]
                            };
                        }

                        match code[f.ecode] {
                            OP_CRSTAR..=OP_CRMINQUERY => {
                                let c = (code[f.ecode] - OP_CRSTAR) as usize;
                                f.ecode += 1;
                                f.minimize = c & 1 != 0;
                                f.min = REP_MIN[c];
                                f.max = REP_MAX[c];
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                            }
                            OP_CRRANGE | OP_CRMINRANGE => {
                                f.minimize = code[f.ecode] == OP_CRMINRANGE;
                                f.min = get2(code, f.ecode + 1) as i32;
                                f.max = get2(code, f.ecode + 1 + IMM2_SIZE) as i32;
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                                f.ecode += 1 + 2 * IMM2_SIZE;
                            }
                            _ => {
                                f.length = match_ref(md, f.offset, f.eptr, f.length, f.caseless);
                                if f.length < 0 {
                                    if f.length == -2 {
                                        f.eptr = md.end_subject;
                                    }
                                    check_partial!();
                                    rreturn!(MATCH_NOMATCH);
                                }
                                f.eptr += f.length as usize;
                                continue 'run;
                            }
                        }

                        if f.length == 0 || (f.length < 0 && f.min == 0) {
                            continue 'run;
                        }
                        for _ in 1..=f.min {
                            let slength = match_ref(md, f.offset, f.eptr, f.length, f.caseless);
                            if slength < 0 {
                                if slength == -2 {
                                    f.eptr = md.end_subject;
                                }
                                check_partial!();
                                rreturn!(MATCH_NOMATCH);
                            }
                            f.eptr += slength as usize;
                        }
                        if f.min == f.max {
                            continue 'run;
                        }
                        if f.minimize {
                            f.fi = f.min;
                            rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm14);
                        }
                        f.pp = f.eptr;
                        for _ in f.min..f.max {
                            let slength = match_ref(md, f.offset, f.eptr, f.length, f.caseless);
                            if slength < 0 {
                                // Not CHECK_PARTIAL: eptr stays put for soft
                                // partial matching.
                                if slength == -2
                                    && md.partial != 0
                                    && md.end_subject > md.start_used_ptr
                                {
                                    md.hitend = true;
                                    if md.partial > 1 {
                                        rreturn!(PCRE_ERROR_PARTIAL);
                                    }
                                }
                                break;
                            }
                            f.eptr += slength as usize;
                        }
                        rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm15);
                    }

                    OP_NCLASS | OP_CLASS => {
                        f.data = f.ecode + 1;
                        f.ecode += 1 + 32;
                        match code[f.ecode] {
                            OP_CRSTAR..=OP_CRMINQUERY | OP_CRPOSSTAR..=OP_CRPOSQUERY => {
                                let c = code[f.ecode] - OP_CRSTAR;
                                f.ecode += 1;
                                if c < OP_CRPOSSTAR - OP_CRSTAR {
                                    f.minimize = c & 1 != 0;
                                } else {
                                    f.possessive = true;
                                }
                                f.min = REP_MIN[c as usize];
                                f.max = REP_MAX[c as usize];
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                            }
                            OP_CRRANGE | OP_CRMINRANGE | OP_CRPOSRANGE => {
                                f.minimize = code[f.ecode] == OP_CRMINRANGE;
                                f.possessive = code[f.ecode] == OP_CRPOSRANGE;
                                f.min = get2(code, f.ecode + 1) as i32;
                                f.max = get2(code, f.ecode + 1 + IMM2_SIZE) as i32;
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                                f.ecode += 1 + 2 * IMM2_SIZE;
                            }
                            _ => {
                                f.min = 1;
                                f.max = 1;
                            }
                        }
                        let in_class = |c: u8| code[f.data + c as usize / 8] & (1 << (c & 7)) != 0;
                        for _ in 1..=f.min {
                            if f.eptr >= md.end_subject {
                                scheck_partial!();
                                rreturn!(MATCH_NOMATCH);
                            }
                            let c = s[f.eptr];
                            f.eptr += 1;
                            if !in_class(c) {
                                rreturn!(MATCH_NOMATCH);
                            }
                        }
                        if f.min == f.max {
                            continue 'run;
                        }
                        if f.minimize {
                            f.fi = f.min;
                            rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm17);
                        }
                        f.pp = f.eptr;
                        for _ in f.min..f.max {
                            if f.eptr >= md.end_subject {
                                scheck_partial!();
                                break;
                            }
                            if !in_class(s[f.eptr]) {
                                break;
                            }
                            f.eptr += 1;
                        }
                        if f.possessive {
                            continue 'run;
                        }
                        rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm19);
                    }

                    OP_CHAR => {
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        let c = s[f.eptr];
                        f.eptr += 1;
                        if code[f.ecode + 1] != c {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.ecode += 2;
                    }
                    OP_CHARI => {
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        if md.lcc[code[f.ecode + 1] as usize] != md.lcc[s[f.eptr] as usize] {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.eptr += 1;
                        f.ecode += 2;
                    }

                    // A single character repeated.
                    OP_STAR..=OP_POSUPTOI => {
                        match op {
                            OP_EXACT | OP_EXACTI => {
                                f.min = get2(code, f.ecode + 1) as i32;
                                f.max = f.min;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            OP_POSUPTO | OP_POSUPTOI | OP_UPTO | OP_UPTOI | OP_MINUPTO
                            | OP_MINUPTOI => {
                                f.possessive = op == OP_POSUPTO || op == OP_POSUPTOI;
                                f.min = 0;
                                f.max = get2(code, f.ecode + 1) as i32;
                                f.minimize = op == OP_MINUPTO || op == OP_MINUPTOI;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            OP_POSSTAR | OP_POSSTARI => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = INT_MAX;
                                f.ecode += 1;
                            }
                            OP_POSPLUS | OP_POSPLUSI => {
                                f.possessive = true;
                                f.min = 1;
                                f.max = INT_MAX;
                                f.ecode += 1;
                            }
                            OP_POSQUERY | OP_POSQUERYI => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = 1;
                                f.ecode += 1;
                            }
                            _ => {
                                // STAR, PLUS, QUERY and their MIN and I forms
                                let c =
                                    (op - if op < OP_STARI { OP_STAR } else { OP_STARI }) as usize;
                                f.ecode += 1;
                                f.minimize = c & 1 != 0;
                                f.min = REP_MIN[c];
                                f.max = REP_MAX[c];
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                            }
                        }
                        f.fc = code[f.ecode] as u32;
                        f.ecode += 1;
                        // Caseless when foc differs; caseful compares fc twice.
                        f.foc = if op >= OP_STARI {
                            md.fcc[f.fc as usize] as u32
                        } else {
                            f.fc
                        };
                        let (fc, foc) = (f.fc, f.foc);
                        let is = |c: u8| fc == c as u32 || foc == c as u32;
                        for _ in 1..=f.min {
                            if f.eptr >= md.end_subject {
                                scheck_partial!();
                                rreturn!(MATCH_NOMATCH);
                            }
                            if !is(s[f.eptr]) {
                                rreturn!(MATCH_NOMATCH);
                            }
                            f.eptr += 1;
                        }
                        if f.min == f.max {
                            continue 'run;
                        }
                        let caseless = op >= OP_STARI;
                        if f.minimize {
                            f.fi = f.min;
                            rmatch!(
                                f.eptr,
                                f.ecode,
                                f.eptrb,
                                if caseless { Pc::Rm24 } else { Pc::Rm26 }
                            );
                        }
                        f.pp = f.eptr;
                        for _ in f.min..f.max {
                            if f.eptr >= md.end_subject {
                                scheck_partial!();
                                break;
                            }
                            if !is(s[f.eptr]) {
                                break;
                            }
                            f.eptr += 1;
                        }
                        if f.possessive {
                            continue 'run;
                        }
                        back_off!(if caseless { Pc::Rm25 } else { Pc::Rm27 });
                    }

                    OP_NOT | OP_NOTI => {
                        if f.eptr >= md.end_subject {
                            scheck_partial!();
                            rreturn!(MATCH_NOMATCH);
                        }
                        let ch = code[f.ecode + 1];
                        let c = s[f.eptr];
                        f.eptr += 1;
                        if ch == c || (op == OP_NOTI && md.fcc[ch as usize] == c) {
                            rreturn!(MATCH_NOMATCH);
                        }
                        f.ecode += 2;
                    }

                    // A negated single character repeated.
                    OP_NOTSTAR..=OP_NOTPOSUPTOI => {
                        match op {
                            OP_NOTEXACT | OP_NOTEXACTI => {
                                f.min = get2(code, f.ecode + 1) as i32;
                                f.max = f.min;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            OP_NOTUPTO | OP_NOTUPTOI | OP_NOTMINUPTO | OP_NOTMINUPTOI => {
                                f.min = 0;
                                f.max = get2(code, f.ecode + 1) as i32;
                                f.minimize = op == OP_NOTMINUPTO || op == OP_NOTMINUPTOI;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            OP_NOTPOSSTAR | OP_NOTPOSSTARI => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = INT_MAX;
                                f.ecode += 1;
                            }
                            OP_NOTPOSPLUS | OP_NOTPOSPLUSI => {
                                f.possessive = true;
                                f.min = 1;
                                f.max = INT_MAX;
                                f.ecode += 1;
                            }
                            OP_NOTPOSQUERY | OP_NOTPOSQUERYI => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = 1;
                                f.ecode += 1;
                            }
                            OP_NOTPOSUPTO | OP_NOTPOSUPTOI => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = get2(code, f.ecode + 1) as i32;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            _ => {
                                let c = (op
                                    - if op >= OP_NOTSTARI {
                                        OP_NOTSTARI
                                    } else {
                                        OP_NOTSTAR
                                    }) as usize;
                                f.ecode += 1;
                                f.minimize = c & 1 != 0;
                                f.min = REP_MIN[c];
                                f.max = REP_MAX[c];
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                            }
                        }
                        f.fc = code[f.ecode] as u32;
                        f.ecode += 1;
                        f.foc = if op >= OP_NOTSTARI {
                            md.fcc[f.fc as usize] as u32
                        } else {
                            f.fc
                        };
                        let (fc, foc) = (f.fc, f.foc);
                        let is = |c: u8| fc == c as u32 || foc == c as u32;
                        for _ in 1..=f.min {
                            if f.eptr >= md.end_subject {
                                scheck_partial!();
                                rreturn!(MATCH_NOMATCH);
                            }
                            let c = s[f.eptr];
                            f.eptr += 1;
                            if is(c) {
                                rreturn!(MATCH_NOMATCH);
                            }
                        }
                        if f.min == f.max {
                            continue 'run;
                        }
                        let caseless = op >= OP_NOTSTARI;
                        if f.minimize {
                            f.fi = f.min;
                            rmatch!(
                                f.eptr,
                                f.ecode,
                                f.eptrb,
                                if caseless { Pc::Rm29 } else { Pc::Rm33 }
                            );
                        }
                        f.pp = f.eptr;
                        for _ in f.min..f.max {
                            if f.eptr >= md.end_subject {
                                scheck_partial!();
                                break;
                            }
                            if is(s[f.eptr]) {
                                break;
                            }
                            f.eptr += 1;
                        }
                        if f.possessive {
                            continue 'run;
                        }
                        back_off!(if caseless { Pc::Rm31 } else { Pc::Rm35 });
                    }

                    // A character type repeated.
                    OP_TYPESTAR..=OP_TYPEPOSUPTO => {
                        match op {
                            OP_TYPEEXACT => {
                                f.min = get2(code, f.ecode + 1) as i32;
                                f.max = f.min;
                                f.minimize = true;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            OP_TYPEUPTO | OP_TYPEMINUPTO => {
                                f.min = 0;
                                f.max = get2(code, f.ecode + 1) as i32;
                                f.minimize = op == OP_TYPEMINUPTO;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            OP_TYPEPOSSTAR => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = INT_MAX;
                                f.ecode += 1;
                            }
                            OP_TYPEPOSPLUS => {
                                f.possessive = true;
                                f.min = 1;
                                f.max = INT_MAX;
                                f.ecode += 1;
                            }
                            OP_TYPEPOSQUERY => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = 1;
                                f.ecode += 1;
                            }
                            OP_TYPEPOSUPTO => {
                                f.possessive = true;
                                f.min = 0;
                                f.max = get2(code, f.ecode + 1) as i32;
                                f.ecode += 1 + IMM2_SIZE;
                            }
                            _ => {
                                let c = (op - OP_TYPESTAR) as usize;
                                f.ecode += 1;
                                f.minimize = c & 1 != 0;
                                f.min = REP_MIN[c];
                                f.max = REP_MAX[c];
                                if f.max == 0 {
                                    f.max = INT_MAX;
                                }
                            }
                        }
                        f.ctype = code[f.ecode];
                        f.ecode += 1;
                        let ctype = f.ctype;

                        if f.min > 0 {
                            match ctype {
                                OP_ANY => {
                                    for _ in 1..=f.min {
                                        if f.eptr >= md.end_subject {
                                            scheck_partial!();
                                            rreturn!(MATCH_NOMATCH);
                                        }
                                        if md.is_newline(f.eptr) {
                                            rreturn!(MATCH_NOMATCH);
                                        }
                                        if md.partial != 0
                                            && f.eptr + 1 >= md.end_subject
                                            && md.crlf_half(s[f.eptr])
                                        {
                                            md.hitend = true;
                                            if md.partial > 1 {
                                                rreturn!(PCRE_ERROR_PARTIAL);
                                            }
                                        }
                                        f.eptr += 1;
                                    }
                                }
                                OP_ALLANY | OP_ANYBYTE => {
                                    if f.eptr + f.min as usize > md.end_subject {
                                        scheck_partial!();
                                        rreturn!(MATCH_NOMATCH);
                                    }
                                    f.eptr += f.min as usize;
                                }
                                OP_ANYNL => {
                                    for _ in 1..=f.min {
                                        if f.eptr >= md.end_subject {
                                            scheck_partial!();
                                            rreturn!(MATCH_NOMATCH);
                                        }
                                        let c = s[f.eptr];
                                        f.eptr += 1;
                                        match c {
                                            b'\r' => {
                                                if f.eptr < md.end_subject && s[f.eptr] == b'\n' {
                                                    f.eptr += 1;
                                                }
                                            }
                                            b'\n' => {}
                                            0x0b | 0x0c | 0x85 => {
                                                if md.bsr_anycrlf {
                                                    rreturn!(MATCH_NOMATCH);
                                                }
                                            }
                                            _ => rreturn!(MATCH_NOMATCH),
                                        }
                                    }
                                }
                                _ => {
                                    if type_matches(md, ctype, 0).is_none() {
                                        rreturn!(PCRE_ERROR_INTERNAL);
                                    }
                                    for _ in 1..=f.min {
                                        if f.eptr >= md.end_subject {
                                            scheck_partial!();
                                            rreturn!(MATCH_NOMATCH);
                                        }
                                        if type_matches(md, ctype, s[f.eptr]) != Some(true) {
                                            rreturn!(MATCH_NOMATCH);
                                        }
                                        f.eptr += 1;
                                    }
                                }
                            }
                        }

                        if f.min == f.max {
                            continue 'run;
                        }
                        if f.minimize {
                            f.fi = f.min;
                            rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm43);
                        }

                        f.pp = f.eptr;
                        match ctype {
                            OP_ANY => {
                                for _ in f.min..f.max {
                                    if f.eptr >= md.end_subject {
                                        scheck_partial!();
                                        break;
                                    }
                                    if md.is_newline(f.eptr) {
                                        break;
                                    }
                                    if md.partial != 0
                                        && f.eptr + 1 >= md.end_subject
                                        && md.crlf_half(s[f.eptr])
                                    {
                                        md.hitend = true;
                                        if md.partial > 1 {
                                            rreturn!(PCRE_ERROR_PARTIAL);
                                        }
                                    }
                                    f.eptr += 1;
                                }
                            }
                            OP_ALLANY | OP_ANYBYTE => {
                                let c = (f.max - f.min) as usize;
                                if c > md.end_subject - f.eptr {
                                    f.eptr = md.end_subject;
                                    scheck_partial!();
                                } else {
                                    f.eptr += c;
                                }
                            }
                            OP_ANYNL => {
                                for _ in f.min..f.max {
                                    if f.eptr >= md.end_subject {
                                        scheck_partial!();
                                        break;
                                    }
                                    let c = s[f.eptr];
                                    if c == b'\r' {
                                        f.eptr += 1;
                                        if f.eptr >= md.end_subject {
                                            break;
                                        }
                                        if s[f.eptr] == b'\n' {
                                            f.eptr += 1;
                                        }
                                    } else {
                                        if c != b'\n'
                                            && (md.bsr_anycrlf
                                                || (c != 0x0b && c != 0x0c && c != 0x85))
                                        {
                                            break;
                                        }
                                        f.eptr += 1;
                                    }
                                }
                            }
                            _ => {
                                if type_matches(md, ctype, 0).is_none() {
                                    rreturn!(PCRE_ERROR_INTERNAL);
                                }
                                for _ in f.min..f.max {
                                    if f.eptr >= md.end_subject {
                                        scheck_partial!();
                                        break;
                                    }
                                    if type_matches(md, ctype, s[f.eptr]) != Some(true) {
                                        break;
                                    }
                                    f.eptr += 1;
                                }
                            }
                        }
                        if f.possessive {
                            continue 'run;
                        }
                        back_off!(Pc::Rm47);
                    }

                    _ => rreturn!(PCRE_ERROR_UNKNOWN_OPCODE),
                }
            }

            // The alternatives of a non-capturing or atomic group, and of a
            // capturing group with no room to save its capture.
            Pc::BraLoop => {
                if f.op >= OP_SBRA || f.op == OP_ONCE {
                    md.match_function_type = MATCH_CBEGROUP;
                } else if !md.hasthen && code[f.ecode + get(code, f.ecode + 1)] != OP_ALT {
                    // The final alternative, with no THEN to catch.
                    f.ecode += len!(f.ecode);
                    pc = Pc::TailRecurse;
                    continue 'run;
                }
                f.save_capture_last = md.capture_last;
                rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm2);
            }

            // A possessive capturing group with an unlimited repeat:
            // OP_KETRPOS hands each iteration back here.
            Pc::PossessiveCapture => {
                f.number = get2(code, f.ecode + 1 + LINK_SIZE);
                f.offset = (f.number as usize) << 1;
                if f.offset >= md.offset_max {
                    pc = Pc::PossessiveNonCapture;
                    continue 'run;
                }
                f.matched_once = false;
                f.code_offset = f.ecode;
                f.save_offset1 = md.ovec[f.offset];
                f.save_offset2 = md.ovec[f.offset + 1];
                f.save_offset3 = md.ovec[md.offset_end - f.number as usize];
                f.save_capture_last = md.capture_last;
                pc = Pc::PossessiveCaptureLoop;
            }
            Pc::PossessiveCaptureLoop => {
                md.ovec[md.offset_end - f.number as usize] = f.eptr as i32;
                if f.op >= OP_SBRA {
                    md.match_function_type = MATCH_CBEGROUP;
                }
                rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm63);
            }
            Pc::PossessiveNonCapture => {
                f.matched_once = false;
                f.code_offset = f.ecode;
                f.save_capture_last = md.capture_last;
                pc = Pc::PossessiveNonCaptureLoop;
            }
            Pc::PossessiveNonCaptureLoop => {
                if f.op >= OP_SBRA {
                    md.match_function_type = MATCH_CBEGROUP;
                }
                rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm48);
            }

            // Obey the branch the condition chose.
            Pc::CondChoose => {
                f.ecode += if f.condition {
                    OP_LENGTHS[f.condcode as usize] as usize
                } else {
                    f.codelink
                };
                if f.condition || code[f.ecode - (1 + LINK_SIZE)] == OP_ALT {
                    if f.op != OP_SCOND {
                        pc = Pc::TailRecurse;
                        continue 'run;
                    }
                    md.match_function_type = MATCH_CBEGROUP;
                    rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm49);
                }
                // A false condition with no second branch.
                pc = Pc::Op;
            }

            Pc::RecurseLoop => {
                if f.cbegroup {
                    md.match_function_type = MATCH_CBEGROUP;
                }
                rmatch!(f.eptr, f.callpat + len!(f.callpat), f.eptrb, Pc::Rm6);
            }

            Pc::Rm1 => {
                'next: {
                    // MATCH_ONCE is backing up through an atomic group.
                    if rrc == MATCH_ONCE {
                        break 'next;
                    }
                    then_check!();
                    if rrc != MATCH_NOMATCH {
                        rreturn!(rrc);
                    }
                    md.capture_last = f.save_capture_last;
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] != OP_ALT {
                        break 'next;
                    }
                    if f.op >= OP_SBRA {
                        md.match_function_type = MATCH_CBEGROUP;
                    }
                    rmatch!(f.eptr, f.ecode + len!(f.ecode), f.eptrb, Pc::Rm1);
                }
                md.ovec[f.offset] = f.save_offset1;
                md.ovec[f.offset + 1] = f.save_offset2;
                md.ovec[md.offset_end - f.number as usize] = f.save_offset3;
                rreturn!(rrc);
            }
            Pc::Rm2 => {
                then_check!();
                if rrc != MATCH_NOMATCH {
                    if rrc == MATCH_ONCE {
                        let mut scode = f.ecode;
                        if code[scode] != OP_ONCE {
                            while code[scode] == OP_ALT {
                                scode += get(code, scode + 1);
                            }
                            scode -= get(code, scode + 1);
                        }
                        if md.once_target == scode {
                            rrc = MATCH_NOMATCH;
                        }
                    }
                    rreturn!(rrc);
                }
                f.ecode += get(code, f.ecode + 1);
                if code[f.ecode] != OP_ALT {
                    rreturn!(MATCH_NOMATCH);
                }
                md.capture_last = f.save_capture_last;
                pc = Pc::BraLoop;
            }
            Pc::Rm3 => {
                if rrc == MATCH_MATCH {
                    if md.end_offset_top > f.offset_top {
                        f.offset_top = md.end_offset_top;
                    }
                    f.condition = true;
                    // Past the assertion, and any BRAZERO before it.
                    if code[f.ecode] == OP_BRAZERO {
                        f.ecode += 1;
                    }
                    f.ecode += get(code, f.ecode + 1);
                    while code[f.ecode] == OP_ALT {
                        f.ecode += get(code, f.ecode + 1);
                    }
                    f.ecode = f.ecode + 1 + LINK_SIZE - OP_LENGTHS[f.condcode as usize] as usize;
                } else if rrc != MATCH_NOMATCH && rrc != MATCH_THEN {
                    rreturn!(rrc);
                }
                pc = Pc::CondChoose;
            }
            Pc::Rm4 => {
                let matched = rrc == MATCH_MATCH || rrc == MATCH_ACCEPT;
                if matched {
                    f.mstart = md.start_match_ptr;
                } else {
                    then_check!();
                    if rrc != MATCH_NOMATCH {
                        rreturn!(rrc);
                    }
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] == OP_ALT {
                        rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, None, Pc::Rm4);
                    }
                }
                if code[f.ecode] == OP_KET {
                    rreturn!(MATCH_NOMATCH);
                }
                if f.condassert {
                    rreturn!(MATCH_MATCH);
                }
                loop {
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] != OP_ALT {
                        break;
                    }
                }
                f.ecode += 1 + LINK_SIZE;
                f.offset_top = md.end_offset_top;
                pc = Pc::Op;
            }
            Pc::Rm5 => {
                let mut fail_rest = false;
                match rrc {
                    MATCH_MATCH | MATCH_ACCEPT => rreturn!(MATCH_NOMATCH),
                    MATCH_NOMATCH => {}
                    MATCH_THEN | MATCH_COMMIT | MATCH_SKIP | MATCH_SKIP_ARG | MATCH_PRUNE => {
                        then_check!();
                        // COMMIT, SKIP, PRUNE and an uncaught THEN fail the
                        // whole assertion, which makes it true.
                        fail_rest = rrc != MATCH_NOMATCH;
                    }
                    _ => rreturn!(rrc),
                }
                if fail_rest {
                    loop {
                        f.ecode += get(code, f.ecode + 1);
                        if code[f.ecode] != OP_ALT {
                            break;
                        }
                    }
                } else {
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] == OP_ALT {
                        rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, None, Pc::Rm5);
                    }
                }
                if f.condassert {
                    rreturn!(MATCH_MATCH);
                }
                f.ecode += 1 + LINK_SIZE;
                pc = Pc::Op;
            }
            Pc::Rm6 => {
                let rec = f.recursion.as_deref().unwrap();
                md.ovec[..rec.offset_save.len()].copy_from_slice(&rec.offset_save);
                md.capture_last = rec.saved_capture_last;
                md.recursive = rec.prevrec;
                if rrc == MATCH_MATCH || rrc == MATCH_ACCEPT {
                    // \K in the recursion moves the start, as in Perl.
                    f.eptr = md.end_match_ptr;
                    f.mstart = md.start_match_ptr;
                    pc = Pc::Op;
                    continue 'run;
                }
                // THEN, SKIP, PRUNE and COMMIT do not escape a recursion.
                if (MATCH_BACKTRACK_MIN..=MATCH_BACKTRACK_MAX).contains(&rrc) {
                    rreturn!(MATCH_NOMATCH);
                }
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                md.recursive = Some(stack.len());
                f.callpat += get(code, f.callpat + 1);
                if code[f.callpat] == OP_ALT {
                    pc = Pc::RecurseLoop;
                    continue 'run;
                }
                md.recursive = f.recursion.as_deref().unwrap().prevrec;
                rreturn!(MATCH_NOMATCH);
            }
            Pc::Rm7 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if code[f.prev] == OP_ONCE {
                    rmatch!(f.eptr, f.prev, f.eptrb, Pc::Rm8);
                }
                if code[f.prev] >= OP_SBRA {
                    rmatch!(f.eptr, f.prev, f.eptrb, Pc::Rm50);
                }
                f.ecode = f.prev;
                pc = Pc::TailRecurse;
            }
            Pc::Rm8 | Pc::Rm9 | Pc::Rm12 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                md.once_target = f.prev;
                rreturn!(MATCH_ONCE);
            }
            Pc::Rm13 => {
                if rrc == MATCH_ONCE && md.once_target == f.prev {
                    rrc = MATCH_NOMATCH;
                }
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if code[f.prev] == OP_ONCE {
                    rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, f.eptrb, Pc::Rm9);
                }
                f.ecode += 1 + LINK_SIZE;
                pc = Pc::TailRecurse;
            }
            Pc::Rm10 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                loop {
                    f.next += get(code, f.next + 1);
                    if code[f.next] != OP_ALT {
                        break;
                    }
                }
                f.ecode = f.next + 1 + LINK_SIZE;
                pc = Pc::Op;
            }
            Pc::Rm11 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                f.ecode += 1;
                pc = Pc::Op;
            }
            Pc::Rm14 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if f.fi >= f.max {
                    rreturn!(MATCH_NOMATCH);
                }
                let slength = match_ref(md, f.offset, f.eptr, f.length, f.caseless);
                if slength < 0 {
                    if slength == -2 {
                        f.eptr = md.end_subject;
                    }
                    check_partial!();
                    rreturn!(MATCH_NOMATCH);
                }
                f.eptr += slength as usize;
                f.fi += 1;
                rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm14);
            }
            Pc::Rm15 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                // C steps eptr back by length while it stays >= pp.
                if f.eptr < f.pp + f.length as usize {
                    rreturn!(MATCH_NOMATCH);
                }
                f.eptr -= f.length as usize;
                rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm15);
            }
            Pc::Rm17 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if f.fi >= f.max {
                    rreturn!(MATCH_NOMATCH);
                }
                if f.eptr >= md.end_subject {
                    scheck_partial!();
                    rreturn!(MATCH_NOMATCH);
                }
                let c = s[f.eptr];
                f.eptr += 1;
                if code[f.data + c as usize / 8] & (1 << (c & 7)) == 0 {
                    rreturn!(MATCH_NOMATCH);
                }
                f.fi += 1;
                rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm17);
            }
            Pc::Rm19 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if f.eptr == f.pp {
                    rreturn!(MATCH_NOMATCH);
                }
                f.eptr -= 1;
                rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm19);
            }
            // Minimizing single-character repeats: try the rest, then one more.
            Pc::Rm24 | Pc::Rm26 | Pc::Rm29 | Pc::Rm33 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if f.fi >= f.max {
                    rreturn!(MATCH_NOMATCH);
                }
                if f.eptr >= md.end_subject {
                    scheck_partial!();
                    rreturn!(MATCH_NOMATCH);
                }
                let c = s[f.eptr] as u32;
                f.eptr += 1;
                let is = f.fc == c || f.foc == c;
                let negated = matches!(pc, Pc::Rm29 | Pc::Rm33);
                if is == negated {
                    rreturn!(MATCH_NOMATCH);
                }
                f.fi += 1;
                rmatch!(f.eptr, f.ecode, f.eptrb, pc);
            }
            Pc::Rm25 | Pc::Rm27 | Pc::Rm31 | Pc::Rm35 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                f.eptr -= 1;
                back_off!(pc);
            }
            Pc::Rm43 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                if f.fi >= f.max {
                    rreturn!(MATCH_NOMATCH);
                }
                if f.eptr >= md.end_subject {
                    scheck_partial!();
                    rreturn!(MATCH_NOMATCH);
                }
                if f.ctype == OP_ANY && md.is_newline(f.eptr) {
                    rreturn!(MATCH_NOMATCH);
                }
                let c = s[f.eptr];
                f.eptr += 1;
                match f.ctype {
                    OP_ANY => {
                        if md.partial != 0 && f.eptr >= md.end_subject && md.crlf_half(c) {
                            md.hitend = true;
                            if md.partial > 1 {
                                rreturn!(PCRE_ERROR_PARTIAL);
                            }
                        }
                    }
                    OP_ALLANY | OP_ANYBYTE => {}
                    OP_ANYNL => match c {
                        b'\r' => {
                            if f.eptr < md.end_subject && s[f.eptr] == b'\n' {
                                f.eptr += 1;
                            }
                        }
                        b'\n' => {}
                        0x0b | 0x0c | 0x85 => {
                            if md.bsr_anycrlf {
                                rreturn!(MATCH_NOMATCH);
                            }
                        }
                        _ => rreturn!(MATCH_NOMATCH),
                    },
                    ctype => match type_matches(md, ctype, c) {
                        Some(true) => {}
                        Some(false) => rreturn!(MATCH_NOMATCH),
                        None => rreturn!(PCRE_ERROR_INTERNAL),
                    },
                }
                f.fi += 1;
                rmatch!(f.eptr, f.ecode, f.eptrb, Pc::Rm43);
            }
            Pc::Rm47 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                f.eptr -= 1;
                if f.ctype == OP_ANYNL
                    && f.eptr > f.pp
                    && s[f.eptr] == b'\n'
                    && s[f.eptr - 1] == b'\r'
                {
                    f.eptr -= 1;
                }
                back_off!(Pc::Rm47);
            }
            Pc::Rm48 => {
                'done: {
                    if rrc == MATCH_KETRPOS {
                        f.offset_top = md.end_offset_top;
                        f.ecode = f.code_offset;
                        f.matched_once = true;
                        f.mstart = md.start_match_ptr;
                        if f.eptr == md.end_match_ptr {
                            // An empty iteration ends the repeat.
                            loop {
                                f.ecode += get(code, f.ecode + 1);
                                if code[f.ecode] != OP_ALT {
                                    break;
                                }
                            }
                            break 'done;
                        }
                        f.eptr = md.end_match_ptr;
                        pc = Pc::PossessiveNonCaptureLoop;
                        continue 'run;
                    }
                    then_check!();
                    if rrc != MATCH_NOMATCH {
                        rreturn!(rrc);
                    }
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] != OP_ALT {
                        break 'done;
                    }
                    md.capture_last = f.save_capture_last;
                    pc = Pc::PossessiveNonCaptureLoop;
                    continue 'run;
                }
                if f.matched_once || f.allow_zero {
                    f.ecode += 1 + LINK_SIZE;
                    pc = Pc::Op;
                    continue 'run;
                }
                rreturn!(MATCH_NOMATCH);
            }
            Pc::Rm49 | Pc::Rm50 => rreturn!(rrc),
            Pc::Rm51 | Pc::Rm52 | Pc::Rm56 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                rreturn!(if pc == Pc::Rm52 {
                    MATCH_COMMIT
                } else {
                    MATCH_PRUNE
                });
            }
            Pc::Rm53 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                md.start_match_ptr = f.eptr;
                rreturn!(MATCH_SKIP);
            }
            Pc::Rm54 | Pc::Rm58 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                md.start_match_ptr = f.ecode;
                rreturn!(MATCH_THEN);
            }
            Pc::Rm55 => {
                // A SKIP whose name is this MARK's skips to here.
                if rrc == MATCH_SKIP_ARG && names_equal(code, f.ecode + 2, md.start_match_ptr) {
                    md.start_match_ptr = f.eptr;
                    rreturn!(MATCH_SKIP);
                }
                rreturn!(rrc);
            }
            Pc::Rm57 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                md.start_match_ptr = f.ecode + 2;
                rreturn!(MATCH_SKIP_ARG);
            }
            Pc::Rm63 => {
                'done: {
                    if rrc == MATCH_KETRPOS {
                        f.offset_top = md.end_offset_top;
                        f.ecode = f.code_offset;
                        f.save_capture_last = md.capture_last;
                        f.matched_once = true;
                        f.mstart = md.start_match_ptr;
                        if f.eptr == md.end_match_ptr {
                            loop {
                                f.ecode += get(code, f.ecode + 1);
                                if code[f.ecode] != OP_ALT {
                                    break;
                                }
                            }
                            break 'done;
                        }
                        f.eptr = md.end_match_ptr;
                        pc = Pc::PossessiveCaptureLoop;
                        continue 'run;
                    }
                    then_check!();
                    if rrc != MATCH_NOMATCH {
                        rreturn!(rrc);
                    }
                    md.capture_last = f.save_capture_last;
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] != OP_ALT {
                        break 'done;
                    }
                    pc = Pc::PossessiveCaptureLoop;
                    continue 'run;
                }
                if !f.matched_once {
                    md.ovec[f.offset] = f.save_offset1;
                    md.ovec[f.offset + 1] = f.save_offset2;
                    md.ovec[md.offset_end - f.number as usize] = f.save_offset3;
                }
                if f.allow_zero || f.matched_once {
                    f.ecode += 1 + LINK_SIZE;
                    pc = Pc::Op;
                    continue 'run;
                }
                rreturn!(MATCH_NOMATCH);
            }
            Pc::Rm64 => {
                // Note: MATCH_ACCEPT does not end an atomic group.
                if rrc == MATCH_MATCH {
                    f.mstart = md.start_match_ptr;
                } else {
                    then_check!();
                    if rrc != MATCH_NOMATCH {
                        rreturn!(rrc);
                    }
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] == OP_ALT {
                        rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, f.eptrb, Pc::Rm64);
                    }
                }
                if code[f.ecode] != OP_ONCE_NC && code[f.ecode] != OP_ALT {
                    rreturn!(MATCH_NOMATCH);
                }
                loop {
                    f.ecode += get(code, f.ecode + 1);
                    if code[f.ecode] != OP_ALT {
                        break;
                    }
                }
                f.offset_top = md.end_offset_top;
                f.eptr = md.end_match_ptr;
                if code[f.ecode] == OP_KET || Some(f.eptr) == f.saved_eptr {
                    f.ecode += 1 + LINK_SIZE;
                    pc = Pc::Op;
                    continue 'run;
                }
                if code[f.ecode] == OP_KETRMIN {
                    rmatch!(f.eptr, f.ecode + 1 + LINK_SIZE, f.eptrb, Pc::Rm65);
                }
                rmatch!(f.eptr, f.prev, f.eptrb, Pc::Rm66);
            }
            Pc::Rm65 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                f.ecode = f.prev;
                pc = Pc::TailRecurse;
            }
            Pc::Rm66 => {
                if rrc != MATCH_NOMATCH {
                    rreturn!(rrc);
                }
                f.ecode += 1 + LINK_SIZE;
                pc = Pc::TailRecurse;
            }
        }
    }
}

/// `pcre_exec(re, NULL, subject, length, start_offset, options, offsets,
/// offsetcount)`. `None` is C's NULL; `offsets` must hold `offsetcount`
/// ints. Returns as C: the number of captured pairs, 0 if they did not
/// fit, or a PCRE_ERROR_xxx.
pub fn pcre_exec(
    re: Option<&Pcre>,
    subject: Option<&[u8]>,
    length: i32,
    start_offset: i32,
    options: i32,
    mut offsets: Option<&mut [i32]>,
    offsetcount: i32,
) -> i32 {
    let options = options as u32;
    if options & !PUBLIC_EXEC_OPTIONS != 0 {
        return PCRE_ERROR_BADOPTION;
    }
    let (Some(re), Some(subject)) = (re, subject) else {
        return PCRE_ERROR_NULL;
    };
    if offsets.is_none() && offsetcount > 0 {
        return PCRE_ERROR_NULL;
    }
    if offsetcount < 0 {
        return PCRE_ERROR_BADCOUNT;
    }
    if length < 0 {
        return PCRE_ERROR_BADLENGTH;
    }
    if start_offset < 0 || start_offset > length {
        return PCRE_ERROR_BADOFFSET;
    }
    if re.magic_number != MAGIC_NUMBER {
        return if re.magic_number == REVERSED_MAGIC_NUMBER {
            PCRE_ERROR_BADENDIANNESS
        } else {
            PCRE_ERROR_BADMAGIC
        };
    }
    if re.flags & PCRE_MODE == 0 {
        return PCRE_ERROR_BADMODE;
    }
    let partial = if options & PCRE_PARTIAL_HARD != 0 {
        2
    } else if options & PCRE_PARTIAL_SOFT != 0 {
        1
    } else {
        0
    };

    // Limits in the pattern override only when smaller.
    let mut match_limit = MATCH_LIMIT;
    let mut match_limit_recursion = MATCH_LIMIT_RECURSION;
    if re.flags & PCRE_MLSET != 0 && u64::from(re.limit_match) < match_limit {
        match_limit = u64::from(re.limit_match);
    }
    if re.flags & PCRE_RLSET != 0 && u64::from(re.limit_recursion) < match_limit_recursion {
        match_limit_recursion = u64::from(re.limit_recursion);
    }

    let tables = re.tables();
    let anchored = (re.options | options) & PCRE_ANCHORED != 0;
    let startline = re.flags & PCRE_STARTLINE != 0;
    let firstline = re.options & PCRE_FIRSTLINE != 0;

    let bsr_anycrlf = match options & (PCRE_BSR_ANYCRLF | PCRE_BSR_UNICODE) {
        0 => re.options & PCRE_BSR_ANYCRLF != 0,
        PCRE_BSR_ANYCRLF => true,
        PCRE_BSR_UNICODE => false,
        _ => return PCRE_ERROR_BADNEWLINE,
    };
    let nl_bits = if options & PCRE_NEWLINE_BITS == 0 {
        re.options
    } else {
        options
    } & PCRE_NEWLINE_BITS;
    let newline: i32 = match nl_bits {
        0 => NEWLINE,
        PCRE_NEWLINE_CR => 0x0d,
        PCRE_NEWLINE_LF => 0x0a,
        PCRE_NEWLINE_CRLF => 0x0d0a,
        PCRE_NEWLINE_ANY => -1,
        PCRE_NEWLINE_ANYCRLF => -2,
        _ => return PCRE_ERROR_BADNEWLINE,
    };
    let (nltype, nllen, nl) = match newline {
        -2 => (NLTYPE_ANYCRLF, 0, [0; 2]),
        n if n < 0 => (NLTYPE_ANY, 0, [0; 2]),
        n if n > 255 => (NLTYPE_FIXED, 2, [(n >> 8) as u8, n as u8]),
        n => (NLTYPE_FIXED, 1, [n as u8, 0]),
    };

    if partial != 0 && re.flags & PCRE_NOPARTIAL != 0 {
        return PCRE_ERROR_BADPARTIAL;
    }

    // More back references than the caller's vector holds: match into a
    // temporary vector. Otherwise match in (a copy of) the caller's, which
    // match() reads before it writes, as C does.
    let offsetcount = offsetcount as usize;
    let mut ocount = offsetcount - offsetcount % 3;
    let arg_offset_max = (2 * ocount) / 3;
    let top_backref = re.top_backref as usize;
    let using_temporary_offsets = top_backref > 0 && top_backref >= ocount / 3;
    let mut ovec = if using_temporary_offsets {
        ocount = top_backref * 3 + 3;
        vec![-1; ocount]
    } else {
        offsets
            .as_deref()
            .map(|o| o[..offsetcount].to_vec())
            .unwrap_or_default()
    };
    // The working slots, and the whole-match pair.
    let top_bracket = re.top_bracket as usize;
    let low = ocount.saturating_sub(top_bracket).max(2);
    if low < ocount {
        ovec[low..ocount].fill(-1);
    }
    if offsetcount > 0 {
        ovec[0] = -1;
    }
    if offsetcount > 1 {
        ovec[1] = -1;
    }

    let s = &subject[..length as usize];
    let start_offset = start_offset as usize;
    let mut md = Md {
        s,
        code: re.code(),
        name_table: re.name_table(),
        name_entry_size: re.name_entry_size as usize,
        ovec,
        offset_end: ocount,
        offset_max: (2 * ocount) / 3,
        lcc: &tables[LCC_OFFSET..],
        fcc: &tables[FCC_OFFSET..],
        ctypes: &tables[CTYPES_OFFSET..],
        nltype,
        nllen,
        nl,
        partial,
        end_subject: s.len(),
        start_offset,
        start_match_ptr: 0,
        end_match_ptr: 0,
        start_used_ptr: 0,
        end_offset_top: 0,
        capture_last: 0,
        match_call_count: 0,
        match_limit,
        match_limit_recursion,
        match_function_type: 0,
        recursive: None,
        once_target: usize::MAX,
        hitend: false,
        notbol: options & PCRE_NOTBOL != 0,
        noteol: options & PCRE_NOTEOL != 0,
        notempty: options & PCRE_NOTEMPTY != 0,
        notempty_atstart: options & PCRE_NOTEMPTY_ATSTART != 0,
        endonly: re.options & PCRE_DOLLAR_ENDONLY != 0,
        jscript_compat: re.options & PCRE_JAVASCRIPT_COMPAT != 0,
        bsr_anycrlf,
        hasthen: re.flags & PCRE_HASTHEN != 0,
        skip_arg_count: 0,
        ignore_skip_arg: 0,
    };

    // A first character for an unanchored match, and a required one.
    let mut first_char = None;
    if !anchored && re.flags & PCRE_FIRSTSET != 0 {
        let fc = re.first_char as u8;
        let fc2 = if re.flags & PCRE_FCH_CASELESS != 0 {
            md.fcc[fc as usize]
        } else {
            fc
        };
        first_char = Some((fc, fc2));
    }
    let mut req_char = None;
    if re.flags & PCRE_REQCHSET != 0 {
        let rc = re.req_char as u8;
        let rc2 = if re.flags & PCRE_RCH_CASELESS != 0 {
            md.fcc[rc as usize]
        } else {
            rc
        };
        req_char = Some((rc, rc2));
    }
    let start_optimize = (options | re.options) & PCRE_NO_START_OPTIMIZE == 0;

    let end_subject = md.end_subject;
    let mut start_match = start_offset;
    let mut req_char_ptr = start_match as isize - 1;
    let mut start_partial = 0;
    let mut match_partial = None;
    let mut stack = Vec::new();
    let mut rc;

    // The bumpalong loop; once for an anchored pattern.
    loop {
        if start_optimize {
            // FIRSTLINE: start no later than the first newline.
            let mut scan_end = end_subject;
            if firstline {
                let mut t = start_match;
                while t < end_subject && !md.is_newline(t) {
                    t += 1;
                }
                scan_end = t;
            }
            if let Some((fc, fc2)) = first_char {
                while start_match < scan_end && s[start_match] != fc && s[start_match] != fc2 {
                    start_match += 1;
                }
            } else if startline && start_match > start_offset {
                while start_match < scan_end && !md.was_newline(start_match) {
                    start_match += 1;
                }
                if s[start_match - 1] == b'\r'
                    && (md.nltype == NLTYPE_ANY || md.nltype == NLTYPE_ANYCRLF)
                    && start_match < scan_end
                    && s[start_match] == b'\n'
                {
                    start_match += 1;
                }
            }
        }

        // The required character must appear; not searched for on very
        // long subjects, or for partial matching.
        if start_optimize
            && partial == 0
            && let Some((rc1, rc2)) = req_char
            && ((end_subject - start_match) as i32) < REQ_BYTE_MAX
        {
            let mut p = start_match + usize::from(first_char.is_some());
            if p as isize > req_char_ptr {
                while p < end_subject {
                    let c = s[p];
                    p += 1;
                    if c == rc1 || c == rc2 {
                        p -= 1;
                        break;
                    }
                }
                if p >= end_subject {
                    rc = MATCH_NOMATCH;
                    break;
                }
                req_char_ptr = p as isize;
            }
        }

        md.start_match_ptr = start_match;
        md.start_used_ptr = start_match;
        md.match_call_count = 0;
        md.match_function_type = 0;
        md.end_offset_top = 0;
        md.skip_arg_count = 0;
        rc = run_match(&mut md, &mut stack, start_match, start_match);
        if md.hitend && match_partial.is_none() {
            start_partial = md.start_used_ptr;
            match_partial = Some(start_match);
        }

        let new_start_match = match rc {
            // No MARK matched the SKIP's name: Perl ignores the SKIP, so
            // rematch here ignoring it.
            MATCH_SKIP_ARG => {
                md.ignore_skip_arg = md.skip_arg_count;
                start_match
            }
            MATCH_SKIP if md.start_match_ptr > start_match => md.start_match_ptr,
            MATCH_SKIP | MATCH_NOMATCH | MATCH_PRUNE | MATCH_THEN => {
                md.ignore_skip_arg = 0;
                start_match + 1
            }
            MATCH_COMMIT => {
                rc = MATCH_NOMATCH;
                break;
            }
            _ => break,
        };
        rc = MATCH_NOMATCH;

        if firstline && md.is_newline(start_match) {
            break;
        }
        start_match = new_start_match;
        if anchored || start_match > end_subject {
            break;
        }
        // Do not start between the CR and LF of a newline, unless the
        // pattern matches CR or LF explicitly.
        if start_match > start_offset
            && s[start_match - 1] == b'\r'
            && start_match < end_subject
            && s[start_match] == b'\n'
            && re.flags & PCRE_HASCRORLF == 0
            && (md.nltype == NLTYPE_ANY || md.nltype == NLTYPE_ANYCRLF || md.nllen == 2)
        {
            start_match += 1;
        }
    }

    let end_offset_top = md.end_offset_top;
    if !using_temporary_offsets && let Some(o) = offsets.as_deref_mut() {
        o[..offsetcount].copy_from_slice(&md.ovec);
    }

    if rc == MATCH_MATCH || rc == MATCH_ACCEPT {
        if using_temporary_offsets {
            if arg_offset_max >= 4 {
                offsets.as_deref_mut().unwrap()[2..arg_offset_max]
                    .copy_from_slice(&md.ovec[2..arg_offset_max]);
            }
            if end_offset_top > arg_offset_max {
                md.capture_last |= OVFLBIT;
            }
        }
        rc = if md.capture_last & OVFLBIT != 0 && end_offset_top >= arg_offset_max {
            0
        } else {
            (end_offset_top / 2) as i32
        };
        // Unused pairs at the end are unset.
        if end_offset_top / 2 <= top_bracket
            && let Some(o) = offsets.as_deref_mut()
        {
            let resetcount = (2 + top_bracket * 2).min(offsetcount);
            if end_offset_top < resetcount {
                o[end_offset_top..resetcount].fill(-1);
            }
        }
        if offsetcount < 2 {
            rc = 0;
        } else {
            let o = offsets.as_deref_mut().unwrap();
            o[0] = md.start_match_ptr as i32;
            o[1] = md.end_match_ptr as i32;
        }
        return rc;
    }

    if rc != MATCH_NOMATCH && rc != PCRE_ERROR_PARTIAL {
        return rc;
    }
    if let Some(match_partial) = match_partial {
        if offsetcount > 1 {
            let o = offsets.unwrap();
            o[0] = start_partial as i32;
            o[1] = end_subject as i32;
            if offsetcount > 2 {
                o[2] = match_partial as i32;
            }
        }
        PCRE_ERROR_PARTIAL
    } else {
        PCRE_ERROR_NOMATCH
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::pcre::compile::pcre_compile2;

    thread_local! {
        /// The frame stack's capacity when the last match() returned.
        pub(super) static HIGH_WATER: Cell<usize> = const { Cell::new(0) };
    }

    /// ^(a)*b nests a frame per 'a': past 8 MiB of frames the match stops
    /// with RECURSIONLIMIT and never holds more, where MATCH_LIMIT_RECURSION
    /// alone let it run to gigabytes.
    #[test]
    fn deep_match_stops_at_eight_mib_of_frames() {
        let re = pcre_compile2(b"^(a)*b", 0, None).unwrap();
        let subject = vec![b'a'; 1 << 20];
        let rc = pcre_exec(
            Some(&re),
            Some(&subject),
            subject.len() as i32,
            0,
            0,
            None,
            0,
        );
        assert_eq!(rc, PCRE_ERROR_RECURSIONLIMIT);
        let bytes = HIGH_WATER.get() * std::mem::size_of::<Frame>();
        assert!(bytes <= 8 << 20, "{bytes}");
        assert!(bytes > 4 << 20, "{bytes}");
    }
}
