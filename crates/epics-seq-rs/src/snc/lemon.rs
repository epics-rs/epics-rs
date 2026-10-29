//! The parts of the lemon parser generator (sequencer `src/lemon/lemon.c`)
//! that decide the automaton for a grammar: symbol numbering, LALR(1)
//! states, conflict resolution, table compression and state order.
//!
//! snc's parse depends on more than the language: lemon's default
//! reductions and shift-reduce actions decide when actions run and where
//! error recovery starts, and so the order of snc's messages. Building the
//! tables with lemon's own algorithm keeps that behaviour; the
//! `reference_snl_tables` test compares [`Lalr::report`] with the `snl.out`
//! report the C build writes.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Assoc {
    Left,
    Right,
    Unk,
}

/// One rule in lemon syntax, aliases and a trailing `[PREC]` allowed, and
/// whether a code block follows it.
pub(crate) struct RuleSpec {
    pub text: &'static str,
    pub code: bool,
}

pub(crate) struct Sym {
    pub name: &'static str,
    /// lemon's `ISUPPER(name[0])`; `$` is not one
    terminal: bool,
    prec: i32,
    assoc: Assoc,
    /// rules with this lhs, newest first (lemon's `nextlhs` chain)
    rules: Vec<usize>,
    firstset: Vec<bool>,
    lambda: bool,
}

pub(crate) struct Rule {
    pub lhs: usize,
    pub rhs: Vec<usize>,
    precsym: Option<usize>,
    pub code: bool,
    /// rule number in the tables: rules with code first
    pub irule: usize,
    lhs_start: bool,
}

/// Action kinds, in lemon's `enum e_action` order (it sorts by it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Shift,
    Accept,
    Reduce,
    Error,
    SsConflict,
    SrConflict,
    RrConflict,
    ShResolved,
    RdResolved,
    NotUsed,
    ShiftReduce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    None,
    State(usize),
    Rule(usize),
}

#[derive(Clone, Debug)]
struct Action {
    sp: usize,
    kind: Kind,
    x: Target,
    sp_opt: Option<usize>,
    /// allocation order, lemon's last sort key
    seq: usize,
}

struct Config {
    rule: usize,
    dot: usize,
    fws: Vec<bool>,
    fplp: Vec<usize>,
    bplp: Vec<usize>,
    complete: bool,
}

struct State {
    basis: Vec<usize>,
    cfp: Vec<usize>,
    statenum: usize,
    actions: Vec<Action>,
    auto_reduce: bool,
    dflt_reduce: Option<usize>,
    n_tkn_act: usize,
    n_nt_act: usize,
}

#[derive(Default)]
struct ConfigList {
    current: Vec<usize>,
    basis: Vec<usize>,
    table: HashMap<(usize, usize), usize>,
}

/// A grammar and its automaton.
pub(crate) struct Lalr {
    /// symbols by index, `{default}` last
    pub syms: Vec<Sym>,
    pub nsymbol: usize,
    pub nterminal: usize,
    /// rules in grammar order
    pub rules: Vec<Rule>,
    cfgs: Vec<Config>,
    /// states in construction order
    states: Vec<State>,
    /// construction indices in final state order (lemon's `sorted`)
    state_order: Vec<usize>,
    by_basis: HashMap<Vec<(usize, usize)>, usize>,
    pub nxstate: usize,
    pub nconflict: usize,
    seq: usize,
}

/// The symbol names in `s`, aliases dropped. As in lemon, an alias ends a
/// symbol, so `AMPERSAND(t)expr(x)` is two.
fn symbols(s: &'static str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut rest = s;
    loop {
        rest = rest.trim_start();
        if let Some(alias) = rest.strip_prefix('(') {
            rest = &alias[alias.find(')').expect("unterminated alias") + 1..];
            continue;
        }
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end == 0 {
            assert!(rest.is_empty(), "bad rule text at {rest:?}");
            return out;
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
}

/// `lhs ::= rhs... . [PREC]` with aliases dropped.
fn parse_rule(text: &'static str) -> (&'static str, Vec<&'static str>, Option<&'static str>) {
    let (lhs, rest) = text.split_once("::=").expect("rule without ::=");
    let (rhs, tail) = rest.split_once('.').expect("rule without '.'");
    let prec = tail
        .trim()
        .strip_prefix('[')
        .map(|p| p.trim_end_matches(']').trim());
    let lhs = symbols(lhs);
    assert_eq!(lhs.len(), 1, "rule lhs {lhs:?}");
    (lhs[0], symbols(rhs), prec)
}

fn set_union(s1: &mut [bool], s2: &[bool]) -> bool {
    let mut progress = false;
    for (a, &b) in s1.iter_mut().zip(s2) {
        if b && !*a {
            *a = true;
            progress = true;
        }
    }
    progress
}

impl Lalr {
    /// Run lemon on a grammar: `precedence` is the `%left`/`%right` lines
    /// in order, `rules` the rules in order; `%type` lines create no
    /// symbol earlier than its first rule here and are left out.
    pub fn new(precedence: &[(Assoc, &'static str)], rules: &[RuleSpec]) -> Self {
        // Symbol_new order: "$", "error", the grammar, "{default}".
        let mut names: Vec<&'static str> = Vec::new();
        let mut ids: HashMap<&'static str, usize> = HashMap::new();
        let mut intern = |name: &'static str| -> usize {
            *ids.entry(name).or_insert_with(|| {
                names.push(name);
                names.len() - 1
            })
        };
        intern("$");
        intern("error");
        let mut prec_of: HashMap<usize, (i32, Assoc)> = HashMap::new();
        for (level, (assoc, list)) in precedence.iter().enumerate() {
            for name in list.split_whitespace() {
                let id = intern(name);
                prec_of.entry(id).or_insert((level as i32 + 1, *assoc));
            }
        }
        let mut raw_rules = Vec::new();
        for spec in rules {
            let (lhs, rhs, prec) = parse_rule(spec.text);
            let lhs = intern(lhs);
            let rhs: Vec<usize> = rhs.into_iter().map(&mut intern).collect();
            let prec = prec.map(&mut intern);
            raw_rules.push((lhs, rhs, prec, spec.code));
        }
        intern("{default}");

        // Symbolcmpp: terminals (and "$") first, each class in creation order.
        let class = |name: &str| if name.as_bytes()[0] > b'Z' { 2 } else { 1 };
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by_key(|&i| (class(names[i]), i));
        let mut index_of = vec![0; names.len()];
        for (new, &old) in order.iter().enumerate() {
            index_of[old] = new;
        }
        let mut syms: Vec<Sym> = order
            .iter()
            .map(|&old| {
                let (prec, assoc) = prec_of.get(&old).copied().unwrap_or((-1, Assoc::Unk));
                Sym {
                    name: names[old],
                    terminal: names[old].as_bytes()[0].is_ascii_uppercase(),
                    prec,
                    assoc,
                    rules: Vec::new(),
                    firstset: Vec::new(),
                    lambda: false,
                }
            })
            .collect();
        let nsymbol = syms.len() - 1;
        let nterminal = (1..)
            .find(|&i| !syms[i].name.as_bytes()[0].is_ascii_uppercase())
            .expect("no nonterminal");

        let mut next_irule = 0;
        let mut rules: Vec<Rule> = raw_rules
            .into_iter()
            .map(|(lhs, rhs, prec, code)| {
                let irule = if code {
                    next_irule += 1;
                    next_irule - 1
                } else {
                    usize::MAX
                };
                Rule {
                    lhs: index_of[lhs],
                    rhs: rhs.into_iter().map(|s| index_of[s]).collect(),
                    precsym: prec.map(|s| index_of[s]),
                    code,
                    irule,
                    lhs_start: false,
                }
            })
            .collect();
        for r in rules.iter_mut().filter(|r| r.irule == usize::MAX) {
            r.irule = next_irule;
            next_irule += 1;
        }
        for (i, r) in rules.iter().enumerate() {
            syms[r.lhs].rules.insert(0, i);
        }

        let mut lalr = Lalr {
            syms,
            nsymbol,
            nterminal,
            rules,
            cfgs: Vec::new(),
            states: Vec::new(),
            state_order: Vec::new(),
            by_basis: HashMap::new(),
            nxstate: 0,
            nconflict: 0,
            seq: 0,
        };
        lalr.find_rule_precedences();
        lalr.find_first_sets();
        lalr.find_states();
        lalr.find_links();
        lalr.find_follow_sets();
        lalr.find_actions();
        lalr.compress_tables();
        lalr.resort_states();
        lalr
    }

    fn find_rule_precedences(&mut self) {
        for r in &mut self.rules {
            if r.precsym.is_none() {
                r.precsym = r.rhs.iter().copied().find(|&s| self.syms[s].prec >= 0);
            }
        }
    }

    fn find_first_sets(&mut self) {
        let nterminal = self.nterminal;
        for s in &mut self.syms[nterminal..] {
            s.firstset = vec![false; nterminal];
        }
        loop {
            let mut progress = false;
            for r in &self.rules {
                if self.syms[r.lhs].lambda {
                    continue;
                }
                if r.rhs.iter().all(|&s| self.syms[s].lambda) {
                    self.syms[r.lhs].lambda = true;
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        loop {
            let mut progress = false;
            for r in &self.rules {
                let s1 = r.lhs;
                for &s2 in &r.rhs {
                    if self.syms[s2].terminal {
                        if !self.syms[s1].firstset[s2] {
                            self.syms[s1].firstset[s2] = true;
                            progress = true;
                        }
                        break;
                    } else if s1 == s2 {
                        if !self.syms[s1].lambda {
                            break;
                        }
                    } else {
                        let f2 = self.syms[s2].firstset.clone();
                        progress |= set_union(&mut self.syms[s1].firstset, &f2);
                        if !self.syms[s2].lambda {
                            break;
                        }
                    }
                }
            }
            if !progress {
                break;
            }
        }
    }

    fn add_config(&mut self, list: &mut ConfigList, rule: usize, dot: usize, basis: bool) -> usize {
        if let Some(&c) = list.table.get(&(rule, dot)) {
            return c;
        }
        let c = self.cfgs.len();
        self.cfgs.push(Config {
            rule,
            dot,
            fws: vec![false; self.nterminal],
            fplp: Vec::new(),
            bplp: Vec::new(),
            complete: false,
        });
        list.current.push(c);
        if basis {
            list.basis.push(c);
        }
        list.table.insert((rule, dot), c);
        c
    }

    fn find_states(&mut self) {
        let start = self.rules[0].lhs;
        let mut list = ConfigList::default();
        for rp in self.syms[start].rules.clone() {
            self.rules[rp].lhs_start = true;
            let c = self.add_config(&mut list, rp, 0, true);
            self.cfgs[c].fws[0] = true;
        }
        self.getstate(list);
    }

    fn config_key(&self, c: usize) -> (usize, usize) {
        (self.cfgs[c].rule, self.cfgs[c].dot)
    }

    fn getstate(&mut self, mut list: ConfigList) -> usize {
        list.basis.sort_by_key(|&c| self.config_key(c));
        let key: Vec<_> = list.basis.iter().map(|&c| self.config_key(c)).collect();
        if let Some(&st) = self.by_basis.get(&key) {
            // Hand the new basis' backward links to the existing state.
            let existing = self.states[st].basis.clone();
            for (&x, &y) in list.basis.iter().zip(&existing) {
                let links = std::mem::take(&mut self.cfgs[x].bplp);
                self.cfgs[y].bplp.extend(links);
            }
            return st;
        }
        self.closure(&mut list);
        list.current.sort_by_key(|&c| self.config_key(c));
        let st = self.states.len();
        self.states.push(State {
            basis: list.basis,
            cfp: list.current,
            statenum: st,
            actions: Vec::new(),
            auto_reduce: false,
            dflt_reduce: None,
            n_tkn_act: 0,
            n_nt_act: 0,
        });
        self.by_basis.insert(key, st);
        self.buildshifts(st);
        st
    }

    fn closure(&mut self, list: &mut ConfigList) {
        let mut i = 0;
        while i < list.current.len() {
            let cfp = list.current[i];
            i += 1;
            let (rule, dot) = self.config_key(cfp);
            let rhs = self.rules[rule].rhs.clone();
            if dot >= rhs.len() || self.syms[rhs[dot]].terminal {
                continue;
            }
            for newrp in self.syms[rhs[dot]].rules.clone() {
                let newcfp = self.add_config(list, newrp, 0, false);
                let mut all_lambda = true;
                for &xsp in &rhs[dot + 1..] {
                    if self.syms[xsp].terminal {
                        self.cfgs[newcfp].fws[xsp] = true;
                        all_lambda = false;
                        break;
                    }
                    let first = self.syms[xsp].firstset.clone();
                    set_union(&mut self.cfgs[newcfp].fws, &first);
                    if !self.syms[xsp].lambda {
                        all_lambda = false;
                        break;
                    }
                }
                if all_lambda {
                    self.cfgs[cfp].fplp.push(newcfp);
                }
            }
        }
    }

    fn symbol_after_dot(&self, c: usize) -> Option<usize> {
        let (rule, dot) = self.config_key(c);
        self.rules[rule].rhs.get(dot).copied()
    }

    fn add_action(&mut self, st: usize, sp: usize, kind: Kind, x: Target) {
        self.seq += 1;
        self.states[st].actions.push(Action {
            sp,
            kind,
            x,
            sp_opt: None,
            seq: self.seq,
        });
    }

    fn buildshifts(&mut self, st: usize) {
        let cfp = self.states[st].cfp.clone();
        for &c in &cfp {
            self.cfgs[c].complete = false;
        }
        for (pos, &c) in cfp.iter().enumerate() {
            if self.cfgs[c].complete {
                continue;
            }
            let Some(sp) = self.symbol_after_dot(c) else {
                continue;
            };
            let mut list = ConfigList::default();
            for &b in &cfp[pos..] {
                if self.cfgs[b].complete || self.symbol_after_dot(b) != Some(sp) {
                    continue;
                }
                self.cfgs[b].complete = true;
                let (rule, dot) = self.config_key(b);
                let n = self.add_config(&mut list, rule, dot + 1, true);
                self.cfgs[n].bplp.push(b);
            }
            let newstp = self.getstate(list);
            self.add_action(st, sp, Kind::Shift, Target::State(newstp));
        }
    }

    fn find_links(&mut self) {
        for st in 0..self.states.len() {
            for c in self.states[st].cfp.clone() {
                for other in self.cfgs[c].bplp.clone() {
                    self.cfgs[other].fplp.push(c);
                }
            }
        }
    }

    fn find_follow_sets(&mut self) {
        let all: Vec<usize> = self.states.iter().flat_map(|s| s.cfp.clone()).collect();
        for &c in &all {
            self.cfgs[c].complete = false;
        }
        loop {
            let mut progress = false;
            for &c in &all {
                if self.cfgs[c].complete {
                    continue;
                }
                let fws = self.cfgs[c].fws.clone();
                for p in self.cfgs[c].fplp.clone() {
                    if set_union(&mut self.cfgs[p].fws, &fws) {
                        self.cfgs[p].complete = false;
                        progress = true;
                    }
                }
                self.cfgs[c].complete = true;
            }
            if !progress {
                break;
            }
        }
    }

    /// lemon's `actioncmp` order.
    fn sort_actions(&mut self, st: usize) {
        self.states[st].actions.sort_by_key(|a| {
            let rule = match (a.kind, a.x) {
                (Kind::Reduce | Kind::ShiftReduce, Target::Rule(r)) => r,
                _ => 0,
            };
            (a.sp, a.kind, rule, Reverse(a.seq))
        });
    }

    fn find_actions(&mut self) {
        for st in 0..self.states.len() {
            for c in self.states[st].cfp.clone() {
                let (rule, dot) = self.config_key(c);
                if dot != self.rules[rule].rhs.len() {
                    continue;
                }
                for j in 0..self.nterminal {
                    if self.cfgs[c].fws[j] {
                        self.add_action(st, j, Kind::Reduce, Target::Rule(rule));
                    }
                }
            }
        }
        let start = self.rules[0].lhs;
        self.add_action(0, start, Kind::Accept, Target::None);

        for st in 0..self.states.len() {
            self.sort_actions(st);
            let n = self.states[st].actions.len();
            for i in 0..n.saturating_sub(1) {
                let mut j = i + 1;
                while j < n && self.states[st].actions[j].sp == self.states[st].actions[i].sp {
                    self.nconflict += self.resolve_conflict(st, i, j);
                    j += 1;
                }
            }
        }
    }

    fn resolve_conflict(&mut self, st: usize, x: usize, y: usize) -> usize {
        let prec = |s: Option<usize>| s.map(|s| (self.syms[s].prec, self.syms[s].assoc));
        let rule_prec = |a: &Action| match a.x {
            Target::Rule(r) => self.rules[r].precsym,
            _ => None,
        };
        let (apx, apy) = (&self.states[st].actions[x], &self.states[st].actions[y]);
        let mut errcnt = 0;
        let (mut kx, mut ky) = (apx.kind, apy.kind);
        if kx == Kind::Shift && ky == Kind::Shift {
            ky = Kind::SsConflict;
            errcnt += 1;
        }
        if kx == Kind::Shift && ky == Kind::Reduce {
            let (px, ax) = prec(Some(apx.sp)).expect("symbol");
            match prec(rule_prec(apy)) {
                None => {
                    ky = Kind::SrConflict;
                    errcnt += 1;
                }
                Some(_) if px < 0 => {
                    ky = Kind::SrConflict;
                    errcnt += 1;
                }
                Some((py, _)) if py < 0 => {
                    ky = Kind::SrConflict;
                    errcnt += 1;
                }
                Some((py, _)) if px > py => ky = Kind::RdResolved,
                Some((py, _)) if px < py => kx = Kind::ShResolved,
                Some(_) if ax == Assoc::Right => ky = Kind::RdResolved,
                Some(_) if ax == Assoc::Left => kx = Kind::ShResolved,
                Some(_) => kx = Kind::Error,
            }
        } else if kx == Kind::Reduce && ky == Kind::Reduce {
            match (prec(rule_prec(apx)), prec(rule_prec(apy))) {
                (Some((px, _)), Some((py, _))) if px >= 0 && py >= 0 && px != py => {
                    if px > py {
                        ky = Kind::RdResolved;
                    } else {
                        kx = Kind::RdResolved;
                    }
                }
                _ => {
                    ky = Kind::RrConflict;
                    errcnt += 1;
                }
            }
        }
        self.states[st].actions[x].kind = kx;
        self.states[st].actions[y].kind = ky;
        errcnt
    }

    fn compress_tables(&mut self) {
        let default_sym = self.syms.len() - 1;
        for st in 0..self.states.len() {
            let acts = &self.states[st].actions;
            let reduce_rule = |a: &Action| match (a.kind, a.x) {
                (Kind::Reduce, Target::Rule(r)) => Some(r),
                _ => None,
            };
            let mut nbest = 0;
            let mut rbest: Option<usize> = None;
            for (i, ap) in acts.iter().enumerate() {
                let Some(rp) = reduce_rule(ap) else { continue };
                if self.rules[rp].lhs_start || Some(rp) == rbest {
                    continue;
                }
                let n = 1 + acts[i + 1..]
                    .iter()
                    .filter(|a2| {
                        let r2 = reduce_rule(a2);
                        r2.is_some() && r2 != rbest && r2 == Some(rp)
                    })
                    .count();
                if n > nbest {
                    nbest = n;
                    rbest = Some(rp);
                }
            }
            let Some(rbest) = rbest else { continue };

            // Combine matching REDUCE actions into a single default.
            let acts = &mut self.states[st].actions;
            let first = acts
                .iter()
                .position(|a| reduce_rule(a) == Some(rbest))
                .expect("best rule has an action");
            acts[first].sp = default_sym;
            for a in &mut acts[first + 1..] {
                if reduce_rule(a) == Some(rbest) {
                    a.kind = Kind::NotUsed;
                }
            }
            self.sort_actions(st);

            let acts = &self.states[st].actions;
            let other = acts.iter().any(|a| {
                a.kind == Kind::Shift || (a.kind == Kind::Reduce && a.x != Target::Rule(rbest))
            });
            if !other {
                self.states[st].auto_reduce = true;
                self.states[st].dflt_reduce = Some(rbest);
            }
        }

        // SHIFT to an auto-reduce state becomes SHIFTREDUCE.
        for st in 0..self.states.len() {
            for i in 0..self.states[st].actions.len() {
                let a = &self.states[st].actions[i];
                if a.kind != Kind::Shift {
                    continue;
                }
                let Target::State(next) = a.x else { continue };
                if let (true, Some(rp)) =
                    (self.states[next].auto_reduce, self.states[next].dflt_reduce)
                {
                    let a = &mut self.states[st].actions[i];
                    a.kind = Kind::ShiftReduce;
                    a.x = Target::Rule(rp);
                }
            }
        }

        // A SHIFTREDUCE by a codeless single-symbol rule on a nonterminal
        // takes the action of the rule's lhs.
        for st in 0..self.states.len() {
            let mut i = 0;
            while i < self.states[st].actions.len() {
                let a = &self.states[st].actions[i];
                let applies = match (a.kind, a.x) {
                    (Kind::ShiftReduce, Target::Rule(rp)) => {
                        let r = &self.rules[rp];
                        !r.code && r.rhs.len() == 1 && a.sp >= self.nterminal
                    }
                    _ => false,
                };
                if !applies {
                    i += 1;
                    continue;
                }
                let Target::Rule(rp) = a.x else {
                    unreachable!()
                };
                let lhs = self.rules[rp].lhs;
                let acts = &self.states[st].actions;
                let j = (0..acts.len())
                    .find(|&j| j != i && acts[j].sp == lhs)
                    .expect("lhs action");
                let (sp, kind, x) = (acts[j].sp, acts[j].kind, acts[j].x);
                let a = &mut self.states[st].actions[i];
                a.sp_opt = Some(sp);
                a.kind = kind;
                a.x = x;
            }
        }
    }

    /// lemon's `compute_action`: the table value of an action, if any.
    fn compute_action(&self, a: &Action) -> Option<usize> {
        let nstate = self.states.len();
        let nrule = self.rules.len();
        match (a.kind, a.x) {
            (Kind::Shift, Target::State(s)) => Some(self.states[s].statenum),
            (Kind::ShiftReduce, Target::Rule(r)) => Some(self.rules[r].irule + nstate),
            (Kind::Reduce, Target::Rule(r)) => Some(self.rules[r].irule + nstate + nrule),
            (Kind::Error, _) => Some(nstate + nrule * 2),
            (Kind::Accept, _) => Some(nstate + nrule * 2 + 1),
            _ => None,
        }
    }

    fn resort_states(&mut self) {
        for st in 0..self.states.len() {
            let (mut tkn, mut nt) = (0, 0);
            for a in &self.states[st].actions {
                if self.compute_action(a).is_some() {
                    if a.sp < self.nterminal {
                        tkn += 1;
                    } else if a.sp < self.nsymbol {
                        nt += 1;
                    }
                }
            }
            self.states[st].n_tkn_act = tkn;
            self.states[st].n_nt_act = nt;
        }
        // State 0 stays first; sorted[] keeps the old indices as targets.
        let mut order: Vec<usize> = (1..self.states.len()).collect();
        order.sort_by_key(|&s| {
            let st = &self.states[s];
            (
                Reverse(st.n_nt_act),
                Reverse(st.n_tkn_act),
                Reverse(st.statenum),
            )
        });
        order.insert(0, 0);
        for (new, &old) in order.iter().enumerate() {
            self.states[old].statenum = new;
        }
        let mut nxstate = order.len();
        while nxstate > 1 && self.states[order[nxstate - 1]].auto_reduce {
            nxstate -= 1;
        }
        self.nxstate = nxstate;
        self.state_order = order;
    }

    fn rule_print(&self, out: &mut String, rule: usize, cursor: Option<usize>) {
        let r = &self.rules[rule];
        let _ = write!(out, "{} ::=", self.syms[r.lhs].name);
        for i in 0..=r.rhs.len() {
            if cursor == Some(i) {
                out.push_str(" *");
            }
            if i == r.rhs.len() {
                break;
            }
            let _ = write!(out, " {}", self.syms[r.rhs[i]].name);
        }
    }

    /// lemon's `PrintAction` at indent 30, without `-p`; false if it
    /// prints nothing.
    fn print_action(&self, out: &mut String, a: &Action) -> bool {
        let name = self.syms[a.sp].name;
        let statenum = |x| match x {
            Target::State(s) => self.states[s].statenum,
            _ => unreachable!("shift without state"),
        };
        let rule = |x| match x {
            Target::Rule(r) => r,
            _ => unreachable!("reduce without rule"),
        };
        match a.kind {
            Kind::Shift => {
                let _ = write!(out, "{name:>30} shift        {:<7}", statenum(a.x));
            }
            Kind::Reduce => {
                let _ = write!(
                    out,
                    "{name:>30} reduce       {:<7}",
                    self.rules[rule(a.x)].irule
                );
                self.rule_print(out, rule(a.x), None);
            }
            Kind::ShiftReduce => {
                let _ = write!(
                    out,
                    "{name:>30} shift-reduce {:<7}",
                    self.rules[rule(a.x)].irule
                );
                self.rule_print(out, rule(a.x), None);
            }
            Kind::Accept => {
                let _ = write!(out, "{name:>30} accept");
            }
            Kind::Error => {
                let _ = write!(out, "{name:>30} error");
            }
            Kind::SrConflict | Kind::RrConflict => {
                let _ = write!(
                    out,
                    "{name:>30} reduce       {:<7} ** Parsing conflict **",
                    self.rules[rule(a.x)].irule
                );
            }
            Kind::SsConflict => {
                let _ = write!(
                    out,
                    "{name:>30} shift        {:<7} ** Parsing conflict **",
                    statenum(a.x)
                );
            }
            Kind::ShResolved | Kind::RdResolved | Kind::NotUsed => return false,
        }
        if let Some(opt) = a.sp_opt {
            let _ = write!(out, "  /* because {name}=={} */", self.syms[opt].name);
        }
        true
    }

    /// lemon's `ReportOutput`: the `.out` file text.
    pub fn report(&self) -> String {
        let mut out = String::new();
        for &st in &self.state_order[..self.nxstate] {
            let stp = &self.states[st];
            let _ = writeln!(out, "State {}:", stp.statenum);
            for &c in &stp.cfp {
                let (rule, dot) = self.config_key(c);
                if dot == self.rules[rule].rhs.len() {
                    let _ = write!(out, "    {:>5} ", format!("({})", self.rules[rule].irule));
                } else {
                    out.push_str("          ");
                }
                self.rule_print(&mut out, rule, Some(dot));
                out.push('\n');
            }
            out.push('\n');
            for a in &stp.actions {
                if self.print_action(&mut out, a) {
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        out.push_str("----------------------------------------------------\n");
        out.push_str("Symbols:\n");
        for (i, sp) in self.syms[..self.nsymbol].iter().enumerate() {
            let _ = write!(out, "  {i:>3}: {}", sp.name);
            if !sp.terminal {
                out.push(':');
                if sp.lambda {
                    out.push_str(" <lambda>");
                }
                for (j, _) in sp
                    .firstset
                    .iter()
                    .enumerate()
                    .filter(|(_, in_set)| **in_set)
                {
                    let _ = write!(out, " {}", self.syms[j].name);
                }
            }
            out.push('\n');
        }
        out
    }

    /// The tables the parser driver reads.
    pub fn tables(&self) -> Tables {
        let nstate = self.states.len();
        let nrule = self.rules.len();
        let nsymbol = self.nsymbol;
        let mut action = vec![NO_ACTION; nstate * nsymbol];
        let mut default = vec![(nstate + 2 * nrule) as u32; nstate];
        for stp in &self.states {
            for a in &stp.actions {
                let Some(act) = self.compute_action(a) else {
                    continue;
                };
                if a.sp < nsymbol {
                    action[stp.statenum * nsymbol + a.sp] = act as u32;
                } else {
                    default[stp.statenum] = act as u32;
                }
            }
        }
        let mut by_irule: Vec<usize> = (0..nrule).collect();
        by_irule.sort_by_key(|&r| self.rules[r].irule);
        Tables {
            nstate,
            nxstate: self.nxstate,
            nrule,
            nsymbol,
            errsym: self
                .syms
                .iter()
                .position(|s| s.name == "error")
                .expect("error symbol"),
            action,
            default,
            rule_lhs: by_irule.iter().map(|&r| self.rules[r].lhs).collect(),
            rule_nrhs: by_irule.iter().map(|&r| self.rules[r].rhs.len()).collect(),
            rule_index: by_irule,
        }
    }
}

const NO_ACTION: u32 = u32::MAX;

/// Action tables in lemon's encoding (see `snl.lt`): shifts are state
/// numbers, then shift-reduces, reduces, error and accept by rule number.
pub(crate) struct Tables {
    pub nstate: usize,
    pub nxstate: usize,
    pub nrule: usize,
    pub nsymbol: usize,
    pub errsym: usize,
    action: Vec<u32>,
    default: Vec<u32>,
    /// by table rule number
    pub rule_lhs: Vec<usize>,
    pub rule_nrhs: Vec<usize>,
    /// grammar-order index of each table rule number
    pub rule_index: Vec<usize>,
}

impl Tables {
    pub fn yy_max_shift(&self) -> u32 {
        self.nxstate as u32 - 1
    }
    pub fn yy_min_shiftreduce(&self) -> u32 {
        self.nstate as u32
    }
    pub fn yy_max_shiftreduce(&self) -> u32 {
        (self.nstate + self.nrule) as u32 - 1
    }
    pub fn yy_min_reduce(&self) -> u32 {
        (self.nstate + self.nrule) as u32
    }
    pub fn yy_max_reduce(&self) -> u32 {
        (self.nstate + 2 * self.nrule) as u32 - 1
    }
    pub fn yy_error_action(&self) -> u32 {
        (self.nstate + 2 * self.nrule) as u32
    }
    pub fn yy_accept_action(&self) -> u32 {
        (self.nstate + 2 * self.nrule) as u32 + 1
    }

    /// The action of state `stateno` on symbol `look_ahead`, else the
    /// state's default (lemon's `yy_find_reduce_action`; the shift variant
    /// differs only for pending reductions, which the driver checks).
    pub fn find_action(&self, stateno: u32, look_ahead: usize) -> u32 {
        let stateno = stateno as usize;
        match self.action[stateno * self.nsymbol + look_ahead] {
            NO_ACTION => self.default[stateno],
            act => act,
        }
    }
}
