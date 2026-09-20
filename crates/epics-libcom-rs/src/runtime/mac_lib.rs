//! libCom `macLib`: the `$(name)` / `${name}` expansion engine (`macCore.c`)
//! and the `name=value,...` definition grammar (`macUtil.c`).
//!
//! It sits in this crate, not under the database loader that is its heaviest
//! user, for the reason C has it in libCom: a consumer with a macro string to honour — an
//! areaDetector `NDAttributesMacros`, a `seq` program's arguments — need not
//! link the database to do it.

use std::collections::HashMap;

/// Resolution options for the macLib expansion engine ([`expand_macros`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct MacroExpandOptions {
    /// Fall back to the process environment when a name is unset,
    /// matching C `macCreateHandle(&h, environ)`. The `.db` parser
    /// leaves this off (its substitutions come only from the
    /// `dbLoadRecords` / `.substitutions` macro map); `dbLoadGroup`
    /// and autosave turn it on.
    pub env_fallback: bool,
    /// Treat `$$` as a literal `$`. An autosave `.req` convenience, NOT
    /// a macLib behavior — C macLib leaves `$$` verbatim (`$` is only
    /// special before `(`/`{`), so the `.db` parser leaves it off.
    pub dollar_escape: bool,
    /// C `macSuppressWarning` / `FLAG_SUPPRESS_WARNINGS`
    /// (`macCore.c:155-168`). It silences the `macLib:` diagnostics AND
    /// changes the text: a suppressed undefined reference is written back
    /// as `$(name)` where a warned one is `$(name,undefined)`
    /// (`refer`, `macCore.c:920-928`). Off is C's default; the `.db`
    /// loader turns it on from `dbQuietMacroWarnings`
    /// (`dbLexRoutines.c:58`, `:273`) and `msi` turns it on outright
    /// (`msi.cpp:154`).
    pub suppress_warnings: bool,
}

/// Outcome of [`expand_macros`]: the expanded text, plus the fault that
/// made it wrong if one did.
///
/// Three faults reach here — a name with no definition, a name that
/// resolves into itself, and a reference whose closing delimiter never
/// matched its opener — and they are C's single `entry->error`
/// (`macCore.c:216-224`) split by cause. All three lists are therefore
/// private, and the only way to read them is [`Self::fault`], or
/// [`Self::errored`] for the same answer as a bool. A consumer able to
/// reach one list directly hard-fails on the fault it named and returns
/// the other one's broken text as success; that is not hypothetical, it
/// is what the autosave `.req` reader did for as long as `undefined`
/// was `pub`.
#[derive(Clone, Debug, Default)]
pub struct MacroExpansion {
    pub text: String,
    faults: MacroFaults,
}

/// C `MAC_ENTRY.error`, kept as the names that set it rather than a
/// bool, and kept as ONE value because C keeps one: `refer` translates a
/// reference name and a used default through the caller's own `entry`
/// so their faults land here, and translates scoped definitions through
/// a separate `MAC_ENTRY subs` whose `error` is never merged back
/// (`macCore.c:820-826`). That second rule is why this is a struct and
/// not three loose fields on [`ExpandCtx`] — [`ExpandCtx::detached`]
/// swaps the whole set out for the scoped region in one move, so no
/// future arm can leak into the caller by being added to only two of
/// three lists.
#[derive(Clone, Debug, Default)]
struct MacroFaults {
    /// Every macro referenced with neither a definition (nor an env
    /// value, when `env_fallback`) nor a default. The text still carries
    /// C's `$(name,undefined)` placeholder for each
    /// (`refer:errval = ",undefined)"`).
    undefined: Vec<String>,
    /// Every macro this expansion refused to resolve because resolving
    /// it re-entered itself — C `refer` finding `refentry->visited`
    /// already set (`macCore.c:895-904`). Separate from
    /// [`Self::undefined`] because the two are different faults with
    /// different placeholders, and a caller that hard-fails on an
    /// undefined name must not be told a recursive one was undefined.
    recursive: Vec<String>,
    /// Every `$(`/`${` whose closing delimiter never arrived — C
    /// `refer`'s first error arm (`macCore.c:862-875`). Each entry is
    /// the raw text C copied through verbatim, from the `$` to the end
    /// of the string, because that arm writes no placeholder and names
    /// no macro: the reference never became a name to look up.
    unterminated: Vec<String>,
}

/// Which fault an expansion hit, and the first text that hit it — the
/// macro name for the two arms that parsed one, the copied-through
/// source for the arm that did not. One variant per way C sets
/// `entry->error`, so a caller cannot report a recursion as an undefined
/// name, nor either of them as a reference that was never closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroFault<'a> {
    /// No definition, no env value, no default — C's
    /// `$(name,undefined)` placeholder (`macCore.c:913-928`).
    Undefined(&'a str),
    /// Resolving the name re-entered itself — C's `refentry->visited`
    /// guard (`macCore.c:895-904`).
    Recursive(&'a str),
    /// The closing delimiter never matched the opener, so C copied the
    /// reference and everything after it through verbatim
    /// (`macCore.c:862-875`). The payload is that copied text — this arm
    /// carries no macro NAME because the reference never produced one.
    Unterminated(&'a str),
}

impl MacroExpansion {
    /// The single owner of "did this expansion come out wrong, and
    /// why". Every arm is read here so that no caller can read only
    /// some of them.
    ///
    /// A macro is never in more than one list: `recursive` is recorded
    /// only where the name resolved to a table entry, `undefined` only
    /// where it resolved to nothing, `unterminated` only where no name
    /// was ever parsed. So the order below decides nothing but which of
    /// several independent faults a string carrying more than one
    /// reports first.
    #[must_use]
    pub fn fault(&self) -> Option<MacroFault<'_>> {
        if let Some(name) = self.faults.undefined.first() {
            return Some(MacroFault::Undefined(name.as_str()));
        }
        if let Some(name) = self.faults.recursive.first() {
            return Some(MacroFault::Recursive(name.as_str()));
        }
        self.faults
            .unterminated
            .first()
            .map(|raw| MacroFault::Unterminated(raw.as_str()))
    }

    /// C `MAC_ENTRY.error`: whether the expansion came out wrong, by any
    /// of the arms that set it. `macExpandString` returns a negative
    /// length for all of them alike (`macCore.c:216-224`), and that
    /// negative length is the whole of what the `.db` reader's per-line
    /// warning and `macDefExpand`'s `NULL` are looking at. Delegates to
    /// [`Self::fault`] so the bool and the name can never disagree.
    #[must_use]
    pub fn errored(&self) -> bool {
        self.fault().is_some()
    }
}

/// C `entry->type`: the word every `macLib:` notice prints in front of
/// the entry name, and the reason those notices are not all about
/// `string`. C carries six (`macCore.c:208`, `:283`, `:418`, `:595`,
/// `:811`, `:826`); the `"default value"` seat is the one this port has
/// no use for, because it slices a default out instead of running C's
/// discarding first pass over it.
const KIND_STRING: &str = "string";
const KIND_MACRO: &str = "macro";
const KIND_ENVIRONMENT: &str = "environment variable";
const KIND_SCOPE_MARKER: &str = "scope marker";
const KIND_SCOPED_MACRO: &str = "scoped macro";

/// One `MAC_ENTRY.error`, carrying the text that raised it.
///
/// C keeps a bare `int error` and merges it with `entry->error =
/// entry->error || refentry->error` (`macCore.c:885`), which is all its
/// callers need: every one of them only compares `macExpandString`'s
/// returned length against zero. This port's callers ask WHICH fault
/// ([`MacroExpansion::fault`]), so the cause travels with the flag and a
/// fault merged out of a cached value still names the macro that broke.
#[derive(Clone, Debug)]
enum TableFault {
    Undefined(String),
    Recursive(String),
    Unterminated(String),
}

impl MacroFaults {
    /// The only way a fault gets in, so an arm added to [`MacroFault`]
    /// cannot be routed to the wrong list or to no list at all.
    fn raise(&mut self, fault: TableFault) {
        match fault {
            TableFault::Undefined(name) => self.undefined.push(name),
            TableFault::Recursive(name) => self.recursive.push(name),
            TableFault::Unterminated(text) => self.unterminated.push(text),
        }
    }

    /// C `entry->error = entry->error || refentry->error`
    /// (`macCore.c:885`): a reference that resolves to a cached value
    /// inherits that value's fault.
    ///
    /// C inherits one bit and so cannot say what the macro's fault was;
    /// this inherits the causes, which costs nothing — only
    /// [`MacroExpansion::fault`] reads them, and it reads the first.
    fn merge(&mut self, other: &MacroFaults) {
        self.undefined.extend_from_slice(&other.undefined);
        self.recursive.extend_from_slice(&other.recursive);
        self.unterminated.extend_from_slice(&other.unterminated);
    }

    /// Re-seat every recursion in this set onto `name`.
    ///
    /// A recursion is a property of the macro that could not be
    /// resolved, not of the inner reference the resolution had to refuse
    /// to find that out — C's own notice says so in as many words,
    /// `macro A is recursive (expanding macro B)` (`macCore.c:895-901`).
    /// So a fault merged out of `A`'s cached value reports `A`, which is
    /// the name the caller wrote. The other two arms are properties of
    /// something INSIDE the value — a name that resolves to nothing, a
    /// bracket that never closes — and keep their own text.
    fn rename_recursive(&mut self, name: &str) {
        for entry in &mut self.recursive {
            name.clone_into(entry);
        }
    }
}

/// C `MAC_ENTRY` (`macLib.h:34-45`): one macro, holding both the
/// definition as given and the expansion cached from it.
///
/// The cache is the point of the type. C expands every raw value into
/// `entry->value` in one pass over the whole table and then resolves a
/// reference by COPYING that value (`refer`, `macCore.c:882-886`), so a
/// macro whose own value is faulty raises its notice once, under its own
/// name, before any caller's string is looked at — and every later
/// reference to it reports the cached fault instead of deriving a fresh
/// one from a different seat.
#[derive(Clone, Debug)]
struct MacEntry {
    /// C `entry->name`. The scope markers carry the literal `<scope>`.
    name: String,
    /// C `entry->type` — one of the `KIND_*` constants above.
    kind: &'static str,
    /// C `entry->rawval`: the definition exactly as given.
    rawval: String,
    /// C `entry->value`: the definition with its own references
    /// resolved. `None` until [`expand_table`] fills it, and set back to
    /// `None` by any redefinition of this entry.
    value: Option<String>,
    /// C `entry->error`, as the causes that raised it — see
    /// [`TableFault`]. Reset at the top of every [`expand_table`] pass,
    /// exactly where C resets the bool (`macCore.c:670`).
    faults: MacroFaults,
    /// C `entry->visited`: raised around a translation of THIS entry's
    /// raw value, so a reference that comes back round to it is refused
    /// rather than followed (`macCore.c:888-893`).
    visited: bool,
    /// C `entry->special`: a `<scope>` marker rather than a macro
    /// (`macPushScope`, `macCore.c:416-419`).
    special: bool,
    /// C `entry->level`: the scope depth this entry was defined at,
    /// which decides whether a redefinition overwrites it or shadows it.
    level: usize,
}

/// The `macPutValue` calls a caller wants made, in the order it wants
/// them made in.
///
/// C builds its table one `macPutValue` at a time — `macInstallMacros`
/// walks the `pairs` array `macParseDefns` produced, in file order
/// (`macUtil.c:250-275`) — and `expand` then walks the table in that
/// same order (`macCore.c:655`), so the sequence a `.db` load's macLib
/// notices come out in is the sequence the definitions were written in.
/// A [`HashMap`] cannot carry that, and sorting its keys by name only
/// looked right because the shapes measured so far happened to be in
/// alphabetical order already.
///
/// So the order is carried here instead, and the conversion from a
/// [`HashMap`] is the one place the loss is named: those definitions
/// arrive in no order at all, and sorting them by name at least makes
/// the notices reproducible from run to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MacroDefs {
    /// Name/value pairs in definition order, at most one entry per name.
    defs: Vec<(String, String)>,
}

impl MacroDefs {
    /// An empty set of definitions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// C `macPutValue` (`macCore.c:262-289`): a redefinition replaces the
    /// value and keeps the entry where it is, because `rawval` writes
    /// through the entry `lookup` found rather than appending a second
    /// one.
    pub fn put(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        match self.defs.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value.into(),
            None => self.defs.push((name, value.into())),
        }
    }

    /// The definitions in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.defs.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// How many names are defined.
    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether nothing is defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

impl FromIterator<(String, String)> for MacroDefs {
    /// Definition order, last definition of a name winning — C's, since
    /// every one of these is a `macPutValue`.
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        let mut defs = Self::new();
        for (name, value) in iter {
            defs.put(name, value);
        }
        defs
    }
}

impl From<Vec<(String, String)>> for MacroDefs {
    fn from(pairs: Vec<(String, String)>) -> Self {
        pairs.into_iter().collect()
    }
}

impl From<&MacroDefs> for MacroDefs {
    fn from(defs: &MacroDefs) -> Self {
        defs.clone()
    }
}

impl From<&HashMap<String, String>> for MacroDefs {
    /// The lossy direction, and the only one: a hash map has no
    /// definition order to carry, so the names are sorted to make the
    /// notice order at least the same on every run. A caller that knows
    /// the order the operator wrote should build a [`MacroDefs`]
    /// directly.
    fn from(macros: &HashMap<String, String>) -> Self {
        let mut names: Vec<&String> = macros.keys().collect();
        names.sort_unstable();
        names
            .into_iter()
            .map(|name| (name.clone(), macros[name].clone()))
            .collect()
    }
}

/// C `MAC_HANDLE` (`macLib.h:47-57`): the macro table, the scope depth,
/// and the one bit that says whether the cached values can be trusted.
///
/// This is the unit C creates once per FILE and expands once per line
/// (`dbReadCOM` holds `macHandle` across the whole `.db`,
/// `dbLexRoutines.c:256-300`), which is what makes a macro's own fault a
/// once-per-file notice rather than a once-per-line one. Callers that
/// have a single string and no file keep using [`expand_macros`], which
/// is this type for the length of one call.
///
/// Entries are in the order they were defined in, which is C's
/// `macPutValue` call order and the order the expansion pass walks them
/// in (C `expand`, `macCore.c:655`) — so it is the order the notices a
/// faulty definition raises come out in.
/// That order is a property of [`MacroDefs`], not of this type: a table
/// built from one is in the caller's order by construction, and a table
/// built from a [`HashMap`] is in the only order a hash map can offer.
pub struct MacroTable {
    /// C `handle->list`. Ordered and searched from the tail.
    ///
    /// While an expansion is running it is only appended to or truncated
    /// at the tail — see [`MacroTable::pop_scope`] — which is what lets
    /// `refer` hold an index across a nested translation.
    /// [`MacroTable::undefine`] is the one mutation that removes from the
    /// middle, and it is reachable only from the file reader, between
    /// lines, where no such index exists.
    entries: Vec<MacEntry>,
    /// C `handle->level`: how many scopes are open.
    level: usize,
    /// C `handle->dirty`: some raw value has changed since the last
    /// [`expand_table`], so no cached value may be used. Raised by every
    /// definition and every scope pop, lowered only by a completed
    /// expansion pass.
    dirty: bool,
    /// C's handle flags, plus this port's `$$` convenience.
    opts: MacroExpandOptions,
}

impl MacroTable {
    /// C `macCreateHandle` + one `macPutValue` per pair
    /// (`macCore.c:64-118`). The table starts dirty: nothing is expanded
    /// until something asks for an expansion.
    ///
    /// The definitions arrive in [`MacroDefs`] order and are installed in
    /// it, so the notice order of the first expansion pass is the
    /// caller's own definition order and does not have to be arranged
    /// for afterwards.
    #[must_use]
    pub fn new(defs: impl Into<MacroDefs>, opts: MacroExpandOptions) -> Self {
        let entries = defs
            .into()
            .defs
            .into_iter()
            .map(|(name, rawval)| MacEntry {
                name,
                kind: KIND_MACRO,
                rawval,
                value: None,
                faults: MacroFaults::default(),
                visited: false,
                special: false,
                level: 0,
            })
            .collect();
        Self {
            entries,
            level: 0,
            dirty: true,
            opts,
        }
    }

    /// C `macExpandString` (`macCore.c:175-227`): bring the cached
    /// values up to date, then translate `src` under a stack entry typed
    /// `"string"` whose name is `src` itself.
    ///
    /// Both halves matter to what comes out. The table pass is what
    /// decides the text of a reference into a cycle and the seat of
    /// every notice a macro's own value raises; the string pass is the
    /// only place the caller's own text is ever looked at.
    #[must_use]
    pub fn expand(&mut self, src: &str) -> MacroExpansion {
        let suppressed = self.opts.suppress_warnings;
        let mut ctx = ExpandCtx {
            table: self,
            seat: Seat {
                kind: KIND_STRING,
                name: src.to_string(),
                faults: MacroFaults::default(),
            },
            suppressed,
        };
        expand_table(&mut ctx);
        let chars: Vec<char> = src.chars().collect();
        let mut out = String::with_capacity(src.len());
        trans(&chars, 0, &mut ctx, &mut out);
        MacroExpansion {
            text: out,
            faults: ctx.seat.faults,
        }
    }

    /// C `macPutValue( handle, name, value )` (`macCore.c:262-289`) with
    /// a non-NULL value: install `rawval` for `name` at the current scope
    /// level.
    ///
    /// The value is stored RAW. C never expands a definition as it is
    /// installed — `macInstallMacros` hands `macPutValue` exactly the
    /// bytes `macParseDefns` cut out (`macUtil.c:250-275`) — so a
    /// definition that mentions another macro stays live and follows
    /// whatever that macro is when it is finally read.
    pub fn define(&mut self, name: &str, rawval: String) {
        self.put(name, KIND_MACRO, rawval);
    }

    /// C `macPutValue( handle, name, NULL )` (`macCore.c:274-280`): the
    /// name is deleted rather than defined, which is what a definition
    /// with no `=` in it means (`macParseDefns`'s `del[i]`,
    /// `macUtil.c:105-110`).
    ///
    /// Deleting is not the same as defining nothing: an OUTER definition
    /// of the same name is uncovered by it, and a reference that finds
    /// nothing at all is undefined rather than empty.
    pub fn undefine(&mut self, name: &str) {
        let Some(idx) = self.lookup(name) else {
            return;
        };
        self.entries.remove(idx);
        self.dirty = true;
    }

    /// C `lookup( handle, name, FALSE )` (`macCore.c:571-585`): search
    /// backwards "so scoping works" — the newest definition of a name
    /// wins — and never match a scope marker.
    fn lookup(&self, name: &str) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|e| !e.special && e.name == name)
    }

    /// [`Self::lookup`] with the rest of C's `lookup`: on a miss under
    /// `FLAG_USE_ENVIRONMENT`, the environment is read and the value is
    /// INSTALLED as an entry typed `"environment variable"`
    /// (`macCore.c:586-598`).
    ///
    /// Installing it is not an optimisation, it is why an environment
    /// hit dirties the table: the entry arrives with no cached value, so
    /// every reference after it in the same string resolves from raw
    /// values until the next expansion pass.
    fn lookup_or_env(&mut self, name: &str) -> Option<usize> {
        if let Some(i) = self.lookup(name) {
            return Some(i);
        }
        if !self.opts.env_fallback || name.is_empty() {
            return None;
        }
        let value = crate::runtime::env::get(name)?;
        Some(self.put(name, KIND_ENVIRONMENT, value))
    }

    /// C `macPutValue` (`macCore.c:262-289`) and the `rawval` it ends in
    /// (`:610-619`).
    ///
    /// A definition at or below the current scope level overwrites in
    /// place; one that came from an OUTER scope is shadowed by a new
    /// entry instead, so popping the scope brings the outer value back.
    /// Either way the whole table goes dirty, because any other entry
    /// may reference this one and no cached value can be trusted until
    /// they are all rebuilt.
    fn put(&mut self, name: &str, kind: &'static str, rawval: String) -> usize {
        let idx = match self.lookup(name) {
            Some(i) if self.entries[i].level >= self.level => i,
            _ => {
                self.entries.push(MacEntry {
                    name: name.to_string(),
                    kind,
                    rawval: String::new(),
                    value: None,
                    faults: MacroFaults::default(),
                    visited: false,
                    special: false,
                    level: self.level,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[idx];
        entry.kind = kind;
        entry.rawval = rawval;
        entry.value = None;
        entry.faults = MacroFaults::default();
        self.dirty = true;
        idx
    }

    /// C `macPushScope` (`macCore.c:400-424`): a marker entry at the
    /// tail, which everything defined from here on sits after.
    fn push_scope(&mut self) {
        self.level += 1;
        self.entries.push(MacEntry {
            name: String::from("<scope>"),
            kind: KIND_SCOPE_MARKER,
            rawval: String::new(),
            value: None,
            faults: MacroFaults::default(),
            visited: false,
            special: true,
            level: self.level,
        });
    }

    /// C `macPopScope` (`macCore.c:434-475`): delete the most recent
    /// `<scope>` marker and every entry defined since it.
    ///
    /// Those entries are exactly the tail — nothing is ever inserted
    /// before an existing entry — so the deletion is a truncation, and
    /// an index held by an enclosing [`refer`] frame into anything below
    /// the marker survives it. C's `delete` dirties the table for the
    /// same reason a redefinition does: a surviving entry may have
    /// referenced what just went away.
    fn pop_scope(&mut self) {
        let at = self
            .entries
            .iter()
            .rposition(|e| e.special)
            .expect("refer pushes the scope marker before it can pop one");
        self.entries.truncate(at);
        self.level -= 1;
        self.dirty = true;
    }
}

/// The `MAC_ENTRY` a translation runs under: the two words every
/// `macLib:` notice prints, and the `error` field the faults land in.
///
/// C swaps it four ways and the swaps are the whole of why one notice
/// says `string` and the next says `macro`. `macExpandString` seats a
/// stack entry typed `"string"` whose name is the caller's whole source
/// string (`macCore.c:206-209`); [`expand_table`] seats the table entry
/// being expanded (`:668`); and `refer` seats a throwaway `dflt` around
/// the default's discarding pass (`:805-816`) and a throwaway `subs`
/// around the scoped definitions (`:820-826`), neither of which merges
/// its error back. Everything else — a resolved value translated raw, a
/// used default — keeps the caller's seat, which is why a fault found
/// three macros deep still names the entry the chain started from.
struct Seat {
    kind: &'static str,
    name: String,
    faults: MacroFaults,
}

/// Engine state threaded through [`trans`] / [`refer`] / [`parse_scoped`]:
/// the table being resolved against, the [`Seat`] the current
/// translation runs under, and the suppression bit the guards below
/// raise and lower.
///
/// The scope stack and the recursion stack that used to be here are both
/// gone into the table, where C keeps them: a scoped definition is an
/// entry between a `<scope>` marker and the tail, and "currently being
/// expanded" is [`MacEntry::visited`].
struct ExpandCtx<'a> {
    table: &'a mut MacroTable,
    seat: Seat,
    /// C `handle->flags & FLAG_SUPPRESS_WARNINGS`, which `refer` raises
    /// and lowers around regions rather than setting once
    /// (`macCore.c:795-800`, `:805-816`, `:822-859`). Seeded from
    /// [`MacroExpandOptions::suppress_warnings`] and read wherever a
    /// `macLib:` notice is about to be written, so a caller's knob and a
    /// region's own quiet are the same bit.
    suppressed: bool,
}

impl ExpandCtx<'_> {
    /// C's `flags = handle->flags; handle->flags |=
    /// FLAG_SUPPRESS_WARNINGS; …; handle->flags = flags` around the
    /// translation of a reference NAME (`macCore.c:795-800`). The
    /// notices go quiet, but the seat is unchanged, so a fault inside the
    /// name still fails the surrounding expansion. Measured on `softIoc
    /// R7.0.10`: `$($(NAMEREF))` writes ONE line and it names the
    /// suppressed placeholder, `macro $(NAMEREF) is undefined`.
    fn suppressing<R>(&mut self, body: impl FnOnce(&mut Self) -> R) -> R {
        let saved = std::mem::replace(&mut self.suppressed, true);
        let out = body(self);
        self.suppressed = saved;
        out
    }

    /// Run `body` under a different [`Seat`] and hand back the faults it
    /// raised, for the caller to merge or to drop. Every seat change in
    /// C is this shape, so this is the only way the seat moves.
    fn seated<R>(&mut self, seat: Seat, body: impl FnOnce(&mut Self) -> R) -> (R, MacroFaults) {
        let saved = std::mem::replace(&mut self.seat, seat);
        let out = body(self);
        let raised = std::mem::replace(&mut self.seat, saved).faults;
        (out, raised)
    }

    /// C's `MAC_ENTRY subs` for the scoped-definition region
    /// (`macCore.c:820-826`): a fresh seat whose `error` is never merged
    /// back, under a raised suppression flag. So a fault inside
    /// `,K=$(UNDEF)` neither warns nor fails the expansion around it —
    /// measured on `softIoc R7.0.10`, `$(P,K=$(UNDEF))` with `P` defined
    /// is silent and loads as `pval`.
    ///
    /// `name` is C's `subs.name`, which it re-seats between the two
    /// halves of a definition: the reference's own name while the
    /// definition's NAME is translated, the definition's name while its
    /// VALUE is (`:840`, `:847`).
    fn detached<R>(&mut self, name: String, body: impl FnOnce(&mut Self) -> R) -> R {
        let seat = Seat {
            kind: KIND_SCOPED_MACRO,
            name,
            faults: MacroFaults::default(),
        };
        let (out, _discarded) = self.suppressing(|ctx| ctx.seated(seat, body));
        out
    }
}

/// C `expand` (`macCore.c:645-679`): translate every raw value into its
/// cached value, each under its OWN entry, then mark the table clean.
///
/// This is not a pass over "the macros the string mentions" — C expands
/// the whole table on the first use after any change, and that is what
/// makes a resolved reference a COPY (`refer`, `:882-886`) rather than a
/// re-translation. Both halves of the difference are observable.
/// Measured on `softIoc R7.0.10` with `A=$(B)`, `B=$(A)` delivered
/// through `dbLoadTemplate` and a `.db` line reading `$(A)`: C writes
/// `macro A is recursive (expanding macro B)` and then `macro B is
/// recursive (expanding macro A)` — both seated on the table entry, not
/// on the string — and loads `$(B,recursive)`, which is `A`'s cached
/// value and not anything the string pass could have produced.
///
/// The table stays dirty for the duration, so a reference met while
/// expanding takes `refer`'s raw branch and [`MacEntry::visited`] is the
/// only thing between a cycle and an unbounded recursion — exactly as in
/// C, where `handle->dirty` is cleared only after the loop.
fn expand_table(ctx: &mut ExpandCtx) {
    if !ctx.table.dirty {
        return;
    }
    let mut i = 0;
    // Not a `for` over a snapshot: resolving one entry can APPEND
    // another (an environment value materialises as an entry), and C's
    // list walk reaches those too.
    while i < ctx.table.entries.len() {
        // A `<scope>` marker has no raw value at all — `create` leaves it
        // NULL (`macCore.c:556`) and C would fault here. It never does,
        // because every scope `refer` opens is closed inside the same
        // reference, and the public `macPushScope` callers expand between
        // scopes rather than inside one.
        if ctx.table.entries[i].special {
            i += 1;
            continue;
        }
        let raw: Vec<char> = ctx.table.entries[i].rawval.chars().collect();
        let seat = Seat {
            kind: ctx.table.entries[i].kind,
            name: ctx.table.entries[i].name.clone(),
            faults: MacroFaults::default(),
        };
        let mut value = String::new();
        // C starts at level 1 "so quotes and escapes will be removed
        // from expanded value" (`macCore.c:672-673`) — a cached value is
        // never the user's own text.
        let (_, raised) = ctx.seated(seat, |ctx| trans(&raw, 1, ctx, &mut value));
        let entry = &mut ctx.table.entries[i];
        entry.value = Some(value);
        entry.faults = raised;
        i += 1;
    }
    ctx.table.dirty = false;
}

/// Expand `$(...)` / `${...}` macro references, mirroring the C `macLib`
/// engine (`modules/libcom/src/macLib/macCore.c` `expand` / `trans` /
/// `refer`). This is the single macLib implementation for the crate; the
/// `.db` parser, `dbLoadGroup`, and autosave all route through it (with
/// per-caller [`MacroExpandOptions`]) rather than re-implementing it.
///
/// One call is one [`MacroTable`], which is C's handle for the length of
/// one string. Callers that expand many strings against one set of
/// macros — a file's worth of lines — should build the table once and
/// call [`MacroTable::expand`] per line instead, because a macro whose
/// own value is faulty raises its notice once per TABLE, not once per
/// string.
///
/// Implemented behaviors:
///
///   - every raw value is expanded into a cached value before the
///     caller's string is looked at, and a reference to a macro COPIES
///     that cached value with its error merged rather than translating
///     the raw value again (C `expand`, `macCore.c:645-679`; `refer`,
///     `:882-886`). A definition, a scope pop or an environment hit
///     invalidates the cache, and until the next pass references
///     resolve from raw values.
///   - `\<char>` blocks macro detection; both bytes reach the output in
///     the caller's own string, and the backslash is dropped from
///     anything that arrived through a macro (`trans:701-703,739-744`;
///     `macLib.plt:52`).
///   - macros are NOT expanded inside single quotes (`trans:733-736`);
///     the quote characters themselves survive only at level 0.
///   - a reference name is itself macro-expanded before lookup
///     (`refer` runs `trans` on the name — `$($(WHICH))`).
///   - the name terminates at `=`, `,` or the closing bracket
///     (`macEnd = "=,)"`); `,name=val` introduces scoped macro
///     definitions visible only inside that reference's expansion, and
///     visible to the definitions AFTER them in the same list
///     (`macPushScope` precedes the loop, `macCore.c:827-850`).
///   - a self- or mutually-referential macro is refused at C's
///     per-entry `visited` guard (`macCore.c:888-904`), leaving
///     `$(name,recursive)` and [`MacroFault::Recursive`].
///   - an undefined macro with no default emits the placeholder
///     `$(name,undefined)` (`refer:errval = ",undefined)"`) and comes
///     back as [`MacroFault::Undefined`].
///   - a reference whose closing delimiter never matched its opener is
///     copied through verbatim together with everything after it, and
///     nothing in that tail is expanded a second time
///     (`refer`, `macCore.c:862-875`) — [`MacroFault::Unterminated`].
///   - with [`MacroExpandOptions::env_fallback`], an otherwise-unset
///     name resolves from the process environment before the default
///     (C `macCreateHandle(&h, environ)`).
#[must_use]
pub fn expand_macros(
    input: &str,
    macros: impl Into<MacroDefs>,
    opts: MacroExpandOptions,
) -> MacroExpansion {
    MacroTable::new(macros, opts).expand(input)
}

/// Expand `$(...)` / `${...}` macro references with the default
/// macLib options (no env fallback, no `$` escape, undefined →
/// placeholder). Thin wrapper over [`expand_macros`], for the callers
/// those defaults are right for: the `include` / `path` / `substitute`
/// directives of the `.db` reader, and `.acf` text through
/// [`substitute_macros_per_line`]. `dbLoadGroup` and autosave reuse the
/// same engine but not this wrapper — both need options of their own,
/// and both call [`expand_macros`] directly.
pub fn substitute_macros(input: &str, macros: impl Into<MacroDefs>) -> String {
    expand_macros(input, macros, MacroExpandOptions::default()).text
}

/// [`substitute_macros`], one line at a time against ONE table — how C's
/// file readers feed macLib.
///
/// `dbLoadRecords` hands `macExpandString` a single `fgets` line
/// (`dbLexRoutines.c:375-391`), and `asInitFile` does the same
/// (`asLibRoutines.c:202-219`), so the expander's quote tracking (`trans`'s
/// `quote`, C `macCore.c`) resets at every newline. Expanding a whole file
/// in one [`substitute_macros`] call let one line's quote state leak into
/// the next: an apostrophe in a `#` comment opened single-quote suppression
/// and silently disabled every `$(...)` on the lines after it, until the
/// parser failed on an unexpanded record name. Within a line the quote
/// rules are unchanged — `'$(X)'` still suppresses.
///
/// The table is the file's, not the line's, because C's is: `asInitFile`
/// creates one handle and reads every line through it, so a macro whose
/// own value is faulty is expanded — and complained about — once.
pub fn substitute_macros_per_line(input: &str, macros: impl Into<MacroDefs>) -> String {
    let mut table = MacroTable::new(macros, MacroExpandOptions::default());
    input
        .split_inclusive('\n')
        .map(|line| table.expand(line).text)
        .collect()
}

/// Translate `chars` into `out`, expanding macro references.
///
/// `level` is C's: 0 is the string the caller handed
/// [`MacroTable::expand`], and every recursion — a macro's value, a
/// reference name, a default, a scoped definition, a cached value being
/// built — runs one deeper, which is what decides whether quotes and
/// backslashes are syntax or text. Scopes and the recursion guard live
/// in [`ExpandCtx::table`].
fn trans(chars: &[char], level: usize, ctx: &mut ExpandCtx, out: &mut String) {
    // C `macCore.c:701-703`: "discard quotes and escapes if level is > 0
    // (i.e. if these aren't the user's quotes and escapes)".
    let discard = level > 0;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Track single/double quote state (C `trans` `quote` var).
        if let Some(q) = quote {
            if c == q {
                quote = None;
                if discard {
                    i += 1;
                    continue;
                }
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            if discard {
                i += 1;
                continue;
            }
        }

        // `$$` → literal `$` (opt-in; autosave `.req` convenience).
        if ctx.table.opts.dollar_escape && c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }

        // `\<char>`: skip macro detection; the backslash itself is
        // emitted only at level 0 (C `if (v < valend && !discard)`).
        if c == '\\' && i + 1 < chars.len() {
            if !discard {
                out.push('\\');
            }
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Macro reference: `$` followed by `(` or `{`, and NOT inside
        // single quotes (C `macRef && quote != '\''`).
        let mac_ref =
            c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{');
        if mac_ref && quote != Some('\'') {
            i = refer(chars, i, level, ctx, out);
            continue;
        }

        out.push(c);
        i += 1;
    }
}

/// Expand one macro reference starting at `chars[start]` (`$`). Returns
/// the index just past the closing bracket, or — when the closing
/// delimiter never matched the opener — the index past the whole
/// remaining scan, which this then copies out verbatim (C
/// `macCore.c:862-875`). `None` is never returned: every `$(`/`${` the
/// caller hands over is consumed here, terminated or not.
fn refer(
    chars: &[char],
    start: usize,
    level: usize,
    ctx: &mut ExpandCtx,
    out: &mut String,
) -> usize {
    let close = if chars[start + 1] == '(' { ')' } else { '}' };
    // Find the matching close bracket, honoring nested `$(`/`${`.
    let body_start = start + 2;
    let mut depth = 1usize;
    let mut j = body_start;
    while j < chars.len() && depth > 0 {
        if j + 1 < chars.len() && chars[j] == '$' && (chars[j + 1] == '(' || chars[j + 1] == '{') {
            depth += 1;
            j += 2;
            continue;
        }
        if depth == 1 && chars[j] == close || depth > 1 && (chars[j] == ')' || chars[j] == '}') {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        j += 1;
    }
    if depth != 0 {
        // C `refer`'s first error arm (`macCore.c:862-875`): the closing
        // delimiter never matched the opener, so this is not a
        // reference. C rewinds the output pointer to where the reference
        // began (`v = *value`) and copies the raw text from the `$`
        // through the last character `trans` scanned — the whole rest of
        // the string, because the failed name translation ran to the end
        // of it — then sets `entry->error` and says so.
        //
        // Copying the tail HERE, rather than handing the caller a `$` to
        // re-scan, is what makes the text match: a re-scan re-enters
        // `trans` just past the `$` and expands whatever `$(…)` follows,
        // where C never looks at the tail again. Measured on `softIoc
        // R7.0.10` with `Q=ZZZ`, C writes `x$(A $(Q) y` for the line
        // `"x$(A $(Q) y"` — the inner `$(Q)` was consumed as part of the
        // unterminated reference's NAME and is not expanded a second
        // time. This port used to write `x$(A ZZZ y`.
        //
        // The arm is reached from a pre-scan for the matching bracket,
        // before any scoped definition in the body has been parsed,
        // where C discovers the mismatch only after parsing them and
        // pushing a scope it then pops. Neither the text nor this notice
        // can see the difference; the one thing that can is that C
        // leaves the table dirty afterwards and this does not, so C may
        // re-expand — and re-announce — a faulty macro value that this
        // announces once.
        let verbatim: String = chars[start..].iter().collect();
        ctx.seat
            .faults
            .raise(TableFault::Unterminated(verbatim.clone()));
        if !ctx.suppressed {
            // C's third `macLib:` notice, alongside `undefined` and
            // `recursive` below and painted by the same `ANSI_MAGENTA`
            // (`errlog.h:301`) that `errlog` strips off a non-terminal
            // console. It quotes `entry->type entry->name`, which is
            // whatever seat this translation runs under: the caller's
            // whole string when the `$(` is in the string, the macro's
            // own name when it is in a macro's value.
            let unterminated = if crate::runtime::log::errlog_console_paints() {
                "\x1b[35;1munterminated\x1b[0m"
            } else {
                "unterminated"
            };
            crate::runtime::log::errlog_printf(&format!(
                "macLib: {unterminated} macro reference in {} {}\n",
                ctx.seat.kind, ctx.seat.name
            ));
        }
        out.push_str(&verbatim);
        return chars.len();
    }
    let body = &chars[body_start..j];
    let after = j + 1;

    // Split the body at the first top-level `=` or `,` (the C
    // `macEnd` terminator set). Nested `$(...)` brackets are skipped
    // so a `=`/`,` inside an inner reference does not terminate.
    let split = top_level_terminator(body);
    let (name_chars, rest) = match split {
        Some(k) => (&body[..k], &body[k..]),
        None => (body, &body[body.len()..]),
    };

    // The name itself may contain macro references — expand it, quietly.
    // C raises `FLAG_SUPPRESS_WARNINGS` for exactly this translation and
    // lowers it again (`macCore.c:795-800`), so the placeholder an
    // unresolved inner reference leaves in the name is the SHORT `$(X)`
    // form and no notice is written for it. The fault still lands,
    // because C hands the name translation the caller's own seat.
    let mut name = String::new();
    ctx.suppressing(|ctx| trans(name_chars, level + 1, ctx, &mut name));

    // Default value (`=...`) and scoped definitions (`,k=v`).
    let mut default: Option<&[char]> = None;
    let mut scoped: Option<&[char]> = None;
    if let Some(first) = rest.first() {
        if *first == '=' {
            // Default runs until the first top-level `,` or end.
            let dflt = &rest[1..];
            match top_level_comma(dflt) {
                Some(k) => {
                    default = Some(&dflt[..k]);
                    scoped = Some(&dflt[k..]);
                }
                None => default = Some(dflt),
            }
        } else if *first == ',' {
            scoped = Some(rest);
        }
    }

    // C pushes the scope only when a `,` list follows, and pops it at
    // the single exit (`macCore.c:822-830`, `:932-935`). The condition
    // is not a saving: a push and its pop each dirty the table, so an
    // unconditional pair would make every reference in a string throw
    // away the cached values the reference before it was resolved from.
    //
    // The frame goes on BEFORE the definitions are read, because C's
    // `macPushScope` does and each `macPutValue` lands in it as the loop
    // reaches it (`:850`). So definition N's value is translated with
    // definitions 1..N-1 visible, and only those: measured on `softIoc
    // R7.0.10` with an outer `A=outer`, `$(B,A=1,B=$(A))` is `1` while
    // the reverse `$(B,B=$(A),A=1)` is `outer`.
    let pop = scoped.is_some();
    if let Some(defs) = scoped {
        ctx.table.push_scope();
        parse_scoped(defs, level, ctx, &name);
    }

    match ctx.table.lookup_or_env(&name) {
        Some(idx) => {
            if ctx.table.entries[idx].visited {
                // C `refer` finding `refentry->visited` already set
                // (`macCore.c:895-904`): the reference is refused, NOT
                // resolved. The port used to emit the value verbatim,
                // which broke the cycle but left the operator with a
                // silently half-expanded `.db`.
                //
                // The seat is the entry whose raw value was being
                // translated when the cycle closed, and `name` is the
                // reference that closed it — C's `entry` and
                // `refentry`, in that order. Reached from
                // [`expand_table`] the seat is a table entry, so the
                // notice reads `macro A is recursive (expanding macro
                // B)`, which is what `softIoc R7.0.10` writes.
                let refkind = ctx.table.entries[idx].kind;
                ctx.seat.faults.raise(TableFault::Recursive(name.clone()));
                if !ctx.suppressed {
                    let recursive = if crate::runtime::log::errlog_console_paints() {
                        "\x1b[35;1mrecursive\x1b[0m"
                    } else {
                        "recursive"
                    };
                    crate::runtime::log::errlog_printf(&format!(
                        "macLib: {} {} is {recursive} (expanding {refkind} {name})\n",
                        ctx.seat.kind, ctx.seat.name
                    ));
                }
                out.push('$');
                out.push('(');
                out.push_str(&name);
                // Same knob, same two texts as the undefined arm
                // (`macCore.c:920-928`).
                if ctx.suppressed {
                    out.push(')');
                } else {
                    out.push_str(",recursive)");
                }
            } else if ctx.table.dirty {
                // No cached value can be trusted, so translate the raw
                // one under the CALLER's seat — C passes `entry`, not
                // `refentry` (`macCore.c:890`) — with the entry's
                // `visited` guard raised around it.
                let raw: Vec<char> = ctx.table.entries[idx].rawval.chars().collect();
                ctx.table.entries[idx].visited = true;
                trans(&raw, level + 1, ctx, out);
                ctx.table.entries[idx].visited = false;
            } else {
                // C `cpy2val( refentry->value, … )` plus `entry->error =
                // entry->error || refentry->error` (`macCore.c:882-886`).
                // The value is COPIED, not re-scanned: whatever the
                // table pass made of it — including the placeholder a
                // cycle left in it — is what the string gets, and the
                // fault that produced it comes across without being
                // raised a second time.
                let entry = &ctx.table.entries[idx];
                let value = entry.value.clone().unwrap_or_default();
                let mut faults = entry.faults.clone();
                faults.rename_recursive(&name);
                out.push_str(&value);
                ctx.seat.faults.merge(&faults);
            }
        }
        None => match default {
            Some(def_chars) => {
                // C `refer` translates the default at `level + 1`
                // (`macCore.c:909`), so every quote in it is discarded —
                // not just a surrounding pair.
                trans(def_chars, level + 1, ctx, out);
            }
            None => {
                // C `refer` (`macCore.c:913-917`), through `errlogPrintf`
                // and not `fprintf` — which is why the magenta on
                // `undefined` follows the console while the
                // `ERROR`/`WARNING` words of the `.db` loader do not:
                // errlog strips escapes when its console is not a
                // terminal (`errlog.c:672-681`) and a direct `fprintf`
                // never enters that pump.
                ctx.seat.faults.raise(TableFault::Undefined(name.clone()));
                if !ctx.suppressed {
                    let undefined = if crate::runtime::log::errlog_console_paints() {
                        "\x1b[35;1mundefined\x1b[0m"
                    } else {
                        "undefined"
                    };
                    crate::runtime::log::errlog_printf(&format!(
                        "macLib: macro {name} is {undefined} (expanding {} {})\n",
                        ctx.seat.kind, ctx.seat.name
                    ));
                }
                out.push('$');
                out.push('(');
                out.push_str(&name);
                // C writes the bare `)` under suppression and the
                // `,undefined)` tail otherwise (`macCore.c:920-928`), so
                // the knob changes the loader's own view of the value and
                // not just what the operator reads.
                if ctx.suppressed {
                    out.push(')');
                } else {
                    out.push_str(",undefined)");
                }
            }
        },
    }

    if pop {
        ctx.table.pop_scope();
    }
    after
}

/// Parse a `,key=val,key2=val2,...` scoped-definition tail into the
/// scope [`refer`] has already pushed. A bare `,key` with no `=`
/// defines nothing (C silently skips it).
///
/// Each definition lands in the table as the loop reaches it, so a later
/// one can reference an earlier one and not the other way round — C
/// `macPutValue` inside the `while (*r == ',')` loop (`macCore.c:850`).
/// Every definition also dirties the table, which is C's explicit
/// `handle->dirty = TRUE` on the next line and the reason the rest of
/// the enclosing string resolves from raw values.
///
/// Both halves of a definition are translated through
/// [`ExpandCtx::detached`], C's `MAC_ENTRY subs` (`macCore.c:820-826`):
/// a fault in a scoped name or value is neither warned about nor merged
/// into the enclosing expansion. `refname` is the seat name C uses for
/// the first half and the definition's own name the seat for the second.
fn parse_scoped(rest: &[char], level: usize, ctx: &mut ExpandCtx, refname: &str) {
    let mut k = 0;
    while k < rest.len() {
        if rest[k] != ',' {
            break;
        }
        k += 1; // step over ','
        // Scoped name: up to next top-level `=` or `,`.
        let seg = &rest[k..];
        let (name_part, tail) = match top_level_terminator(seg) {
            Some(t) => (&seg[..t], &seg[t..]),
            None => (seg, &seg[seg.len()..]),
        };
        let mut sname = String::new();
        ctx.detached(refname.to_string(), |ctx| {
            trans(name_part, level + 1, ctx, &mut sname);
        });
        k += name_part.len();
        if let Some('=') = tail.first() {
            let valseg = &tail[1..];
            let (val_part, _) = match top_level_comma(valseg) {
                Some(t) => (&valseg[..t], &valseg[t..]),
                None => (valseg, &valseg[valseg.len()..]),
            };
            let mut sval = String::new();
            ctx.detached(sname.clone(), |ctx| {
                trans(val_part, level + 1, ctx, &mut sval);
            });
            // C `macPutValue`, which types the new entry `"macro"` even
            // here (`macCore.c:283`) — `"scoped macro"` is the seat the
            // definition was translated under, not the entry's own type.
            ctx.table.put(&sname, KIND_MACRO, sval);
            k += 1 + val_part.len();
        }
        // else: bare `,name` — no value, defines nothing.
    }
}

/// Index of the first top-level `=` or `,` in `body`, skipping any
/// `$(...)` / `${...}` nested reference.
fn top_level_terminator(body: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '$' && i + 1 < body.len() && (body[i + 1] == '(' || body[i + 1] == '{') {
            depth += 1;
            i += 2;
            continue;
        }
        if (c == ')' || c == '}') && depth > 0 {
            depth -= 1;
        } else if depth == 0 && (c == '=' || c == ',') {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of the first top-level `,` in `body` (used to split a
/// default value from trailing scoped definitions).
fn top_level_comma(body: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '$' && i + 1 < body.len() && (body[i + 1] == '(' || body[i + 1] == '{') {
            depth += 1;
            i += 2;
            continue;
        }
        if (c == ')' || c == '}') && depth > 0 {
            depth -= 1;
        } else if depth == 0 && c == ',' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Split an IOC macro definition string into `(name, value)` pairs the
/// way libCom `macParseDefns` does (`macUtil.c:74-196`): commas separate
/// pairs and `=` separates a name from its value, but a separator inside
/// single/double quotes or escaped with a backslash is a literal, and
/// unquoted whitespace around names and values is trimmed. A name with
/// no `=` (e.g. `,FOO,`) is a deletion and yields `None`.
///
/// Quotes and escapes are stripped from both the name and the value. C
/// strips them from names in `macParseDefns` and from values later in
/// `macExpandString`; this port substitutes the value directly with no
/// second `macExpandString` pass, so both are stripped here to reach the
/// same observable substitution.
///
/// Exposed (re-exported as `iocsh::macro_defn_pairs`) so that other macLib
/// consumers — e.g. QSRV's `dbLoadGroup` macro parser — split definition
/// strings through this one owner of the `macParseDefns` grammar instead
/// of a second raw `split(',')` that would tear a quoted value on an
/// embedded comma. Callers that defer `$(...)` expansion to their own
/// `macExpandString` equivalent use these raw split pairs directly and do
/// NOT run `parse_macro_string`, which additionally substitutes the
/// environment eagerly.
pub fn macro_defn_pairs(s: &str) -> Vec<(String, Option<String>)> {
    #[derive(PartialEq, Clone, Copy)]
    enum St {
        PreName,
        InName,
        PreValue,
        InValue,
    }
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut state = St::PreName;
    let mut name = String::new();
    let mut value = String::new();
    // Unquoted whitespace seen mid-token: buffered so trailing whitespace
    // before a delimiter is dropped while interior whitespace is kept.
    let mut pending_ws = String::new();
    let mut quote: Option<char> = None;

    // Enter the token from a "pre" state if needed and report whether it
    // is a VALUE: C removes quotes and escapes from names in place
    // "(unlike values, they will not be re-parsed)" (`macUtil.c:198-200`),
    // so a value keeps them for the expander's `discard` to strip.
    macro_rules! enter_token {
        () => {{
            match state {
                St::PreName => state = St::InName,
                St::PreValue => state = St::InValue,
                _ => {}
            }
            matches!(state, St::InValue)
        }};
    }

    // Append a literal char to the token for the current state, entering
    // the token from a "pre" state if needed and flushing buffered ws.
    macro_rules! push_lit {
        ($c:expr) => {{
            match state {
                St::PreName => state = St::InName,
                St::PreValue => state = St::InValue,
                _ => {}
            }
            let target = if matches!(state, St::InName) {
                &mut name
            } else {
                &mut value
            };
            target.push_str(&pending_ws);
            pending_ws.clear();
            target.push($c);
        }};
    }

    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Escape: `\X` makes `X` a literal (and not a delimiter).
        // Quotes do not suppress escapes.
        if c == '\\' && i + 1 < chars.len() {
            if enter_token!() {
                push_lit!('\\');
            }
            push_lit!(chars[i + 1]);
            i += 2;
            continue;
        }

        // Inside a quote: every char is literal until the matching quote.
        if let Some(q) = quote {
            if c == q {
                quote = None;
                if enter_token!() {
                    push_lit!(c);
                }
            } else {
                push_lit!(c);
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            // An opening quote also begins the token (e.g. `=""`).
            if enter_token!() {
                push_lit!(c);
            }
            i += 1;
            continue;
        }

        match state {
            St::PreName => {
                if c == '=' {
                    state = St::PreValue;
                } else if !(crate::runtime::stdlib::c_isspace(c) || c == ',') {
                    state = St::InName;
                    name.push(c);
                }
                // leading whitespace and bare commas: skip
            }
            St::InName => {
                if c == '=' {
                    pending_ws.clear();
                    state = St::PreValue;
                } else if c == ',' {
                    // name with no '=' → deletion
                    pending_ws.clear();
                    out.push((std::mem::take(&mut name), None));
                    state = St::PreName;
                } else if crate::runtime::stdlib::c_isspace(c) {
                    pending_ws.push(c);
                } else {
                    name.push_str(&pending_ws);
                    pending_ws.clear();
                    name.push(c);
                }
            }
            St::PreValue => {
                if c == ',' {
                    out.push((std::mem::take(&mut name), Some(String::new())));
                    state = St::PreName;
                } else if !crate::runtime::stdlib::c_isspace(c) {
                    state = St::InValue;
                    value.push(c);
                }
                // leading value whitespace: skip
            }
            St::InValue => {
                if c == ',' {
                    pending_ws.clear();
                    out.push((std::mem::take(&mut name), Some(std::mem::take(&mut value))));
                    state = St::PreName;
                } else if crate::runtime::stdlib::c_isspace(c) {
                    pending_ws.push(c);
                } else {
                    value.push_str(&pending_ws);
                    pending_ws.clear();
                    value.push(c);
                }
            }
        }
        i += 1;
    }

    // Flush the token open at end of string.
    match state {
        St::PreName => {}
        St::InName => out.push((std::mem::take(&mut name), None)),
        St::PreValue => out.push((std::mem::take(&mut name), Some(String::new()))),
        St::InValue => out.push((std::mem::take(&mut name), Some(std::mem::take(&mut value)))),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // `expand_macros` reports every undefined macro so a hard-fail
    // caller (autosave) can surface it; the text still carries the
    // C `$(name,undefined)` placeholder for the no-fail callers.
    #[test]
    fn expand_macros_reports_undefined_names() {
        let macros = HashMap::new();
        let r = expand_macros("$(A)$(B=def)$(C)", &macros, MacroExpandOptions::default());
        assert_eq!(r.text, "$(A,undefined)def$(C,undefined)");
        // B had a default → not undefined; A and C are, in scan order.
        assert_eq!(r.faults.undefined, vec!["A".to_string(), "C".to_string()]);
    }

    // The default options match `.db` parse semantics: no env fallback,
    // and `$$` is NOT an escape (macLib leaves it verbatim).
    #[test]
    fn expand_macros_default_opts_leave_dollar_dollar_verbatim() {
        let macros = HashMap::new();
        assert_eq!(substitute_macros("$$100", &macros), "$$100");
        let r = expand_macros("$$100", &macros, MacroExpandOptions::default());
        assert_eq!(r.text, "$$100");
        assert!(r.faults.undefined.is_empty());
    }

    // `env_fallback` resolves an otherwise-unset name from the process
    // environment (C `macCreateHandle(&h, environ)`), at the same level
    // as a defined macro — before any default.
    #[test]
    fn expand_macros_env_fallback_opt_in() {
        let var = "_EPICS_LIBCOM_RS_MACRO_ENV_TEST";
        // Off by default: unset macro stays undefined even with the env set.
        unsafe { std::env::set_var(var, "FROM_ENV") };
        let macros = HashMap::new();
        let off = expand_macros(&format!("$({var})"), &macros, MacroExpandOptions::default());
        assert_eq!(off.text, format!("$({var},undefined)"));
        // On: resolves from the environment.
        let on = expand_macros(
            &format!("$({var})"),
            &macros,
            MacroExpandOptions {
                env_fallback: true,
                ..MacroExpandOptions::default()
            },
        );
        assert_eq!(on.text, "FROM_ENV");
        assert!(on.faults.undefined.is_empty());
        unsafe { std::env::remove_var(var) };
    }
}
