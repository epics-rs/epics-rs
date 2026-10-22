//! Analysis of the parse tree (C `analysis.c`): scopes and declarations,
//! options, channel assign/monitor/sync/syncq, state sets and states,
//! variable references and event flag numbering.

use std::collections::HashSet;
use std::sync::Arc;

use crate::builtin::{
    lookup_builtin_const, lookup_builtin_func, register_builtin_consts, register_builtin_funcs,
};
use crate::driver::Snc;
use crate::node::strtoui;
use crate::sym_table::SymTable;
use crate::types::{
    Ast, Chan, ChanId, ChanList, EXPR_MASK, EvFlag, Extra, HAS_SUB_SCOPE_MASK, Multiplicity,
    NodeId, NodeTag, Options, Program, SCOPE_MASK, Sym, SymScope, SyncQ, SyncQId, SyncQList,
    TypeMask, VarChan, VarId, VarList, VarListId, child, is_scope,
};
use crate::var_types::TypeKind;

/// Access to the syntax tree for [`traverse_syntax_tree`] (C passes the
/// tree and the iteratee's `parg` separately; here the iteratee's context
/// owns the tree).
pub trait AstCx {
    fn ast(&self) -> &Ast;
}

impl AstCx for Ast {
    fn ast(&self) -> &Ast {
        self
    }
}

/// Pre-order traversal of the syntax tree. Call `iteratee` whenever
/// `call_mask` has the node's tag bit set, with the current node and the
/// current scope. Afterwards, if the iteratee returned `true`, recurse
/// into all child nodes except those whose tag bit is set in `stop_mask`.
/// `scope` may be `None`; it is set as soon as the traversal enters a
/// scope. The `next` link of the start node is ignored: this does NOT
/// descend into sibling list elements.
pub fn traverse_syntax_tree<C, F>(
    cx: &mut C,
    ep: Option<NodeId>,
    call_mask: TypeMask,
    stop_mask: TypeMask,
    mut scope: Option<NodeId>,
    iteratee: &mut F,
) where
    C: AstCx,
    F: FnMut(&mut C, NodeId, Option<NodeId>) -> bool,
{
    let Some(ep) = ep else { return };

    if call_mask & cx.ast()[ep].tag.bit() != 0 && !iteratee(cx, ep, scope) {
        return;
    }

    // the iteratee may have changed the tag (E_VAR to E_CONST/E_BUILTIN)
    let tag = cx.ast()[ep].tag;
    if is_scope(tag) {
        scope = Some(ep);
    }

    for i in 0..tag.num_children() {
        let mut cep = cx.ast()[ep].children[i];
        while let Some(c) = cep {
            if cx.ast()[c].tag.bit() & stop_mask == 0 {
                traverse_syntax_tree(cx, Some(c), call_mask, stop_mask, scope, iteratee);
            }
            cep = cx.ast()[c].next;
        }
    }
}

impl Ast {
    /// The variable list slot of a scope node (C `*pvar_list_from_scope`);
    /// `None` until analysis allocates it.
    fn scope_var_list(&self, scope: NodeId) -> Option<VarListId> {
        let node = &self[scope];
        assert!(is_scope(node.tag), "{} is not a scope", node.tag.name());
        match &node.extra {
            Extra::Prog(l) | Extra::Cmpnd(l) | Extra::Funcdef(l) => Some(*l),
            Extra::Ss(ss) => ss.var_list,
            Extra::State(state) => state.var_list,
            Extra::None => None,
            other => unreachable!("{} with extra {other:?}", node.tag.name()),
        }
    }

    fn set_scope_var_list(&mut self, scope: NodeId, list: VarListId) {
        let node = &mut self[scope];
        match (&node.tag, &mut node.extra) {
            (NodeTag::DProg, e) => *e = Extra::Prog(list),
            (NodeTag::SCmpnd, e) => *e = Extra::Cmpnd(list),
            (NodeTag::DFuncdef, e) => *e = Extra::Funcdef(list),
            (NodeTag::DSs, Extra::Ss(ss)) => ss.var_list = Some(list),
            (NodeTag::DState, Extra::State(state)) => state.var_list = Some(list),
            (tag, _) => unreachable!("{} is not a scope", tag.name()),
        }
    }

    /// C `var_list_from_scope`: the variable list analysis gave a scope.
    pub fn var_list_from_scope(&self, scope: NodeId) -> VarListId {
        self.scope_var_list(scope)
            .expect("analysis allocates a variable list for every scope")
    }

    /// C `defn_list_from_scope`: the definitions a scope holds.
    pub fn defn_list_from_scope(&self, scope: NodeId) -> Option<NodeId> {
        let node = &self[scope];
        match node.tag {
            NodeTag::DProg => node.child(child::PROG_DEFNS),
            NodeTag::DSs => node.child(child::SS_DEFNS),
            NodeTag::DState => node.child(child::STATE_DEFNS),
            NodeTag::SCmpnd => node.child(child::CMPND_DEFNS),
            NodeTag::DFuncdef => {
                let decl = node.child(child::FUNCDEF_DECL)?;
                let var = self[decl].e_decl()?;
                // C reads the parameters out of the type's union whatever
                // the type is; analyse_funcdef has reported a non-function.
                match self[self[var].type_].kind {
                    TypeKind::Function { param_decls, .. } => param_decls,
                    _ => None,
                }
            }
            tag => unreachable!("{} is not a scope", tag.name()),
        }
    }
}

/// C `analyse_program`: analyse the tree `prog` under the command-line
/// `options`, reporting through `snc`.
pub fn analyse_program(snc: &mut Snc, ast: &mut Ast, prog: NodeId, options: Options) -> Program {
    let name = ast[prog].token.str.clone();
    let param = Some(match ast[prog].child(child::PROG_PARAM) {
        Some(pp) => ast[pp].token.str.clone().unwrap_or_else(|| Arc::from("")),
        None => Arc::from(""),
    });

    let mut sym_table = SymTable::new();
    register_builtin_consts(&mut sym_table);
    register_builtin_funcs(&mut sym_table);

    let mut cx = Cx {
        snc,
        ast,
        p: Program {
            prog,
            name,
            param,
            options,
            sym_table,
            chan_list: ChanList::default(),
            syncq_list: SyncQList::default(),
            num_ss: 0,
            num_event_flags: 0,
        },
        seen_foreign: false,
    };

    cx.analyse_definitions();
    cx.p.num_ss = cx.connect_states();
    cx.connect_variables();
    cx.connect_state_change_stmts();
    let statesets: Vec<_> = cx
        .ast
        .list(cx.ast[prog].child(child::PROG_STATESETS))
        .collect();
    for ss in statesets {
        cx.check_states_reachable_from_first(ss);
    }
    cx.p.num_event_flags = cx.assign_ef_bits();
    cx.p
}

struct Cx<'a> {
    snc: &'a mut Snc,
    ast: &'a mut Ast,
    p: Program,
    /// C's function-static `seen_foreign` in `analyse_declaration`.
    seen_foreign: bool,
}

impl AstCx for Cx<'_> {
    fn ast(&self) -> &Ast {
        self.ast
    }
}

/// Text of a node's token, `""` where C has NULL.
fn text(ast: &Ast, n: NodeId) -> Arc<str> {
    ast[n].token.str.clone().unwrap_or_else(|| Arc::from(""))
}

/// A variable's name; every variable analysis looks up has one.
fn var_name(ast: &Ast, vp: VarId) -> Arc<str> {
    ast[vp].name.clone().expect("named variable")
}

impl Cx<'_> {
    fn type_kind(&self, vp: VarId) -> &TypeKind {
        &self.ast[self.ast[vp].type_].kind
    }

    fn analyse_structdef(&mut self, defn: NodeId) {
        let name = text(self.ast, defn);
        let members = self.ast[defn].child(child::STRUCTDEF_MEMBERS);
        let t = self.ast.mk_structure_type(name.clone(), members);
        if !self
            .p
            .sym_table
            .insert(&name, SymScope::StructDefs, Sym::Type(t))
        {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("ignoring duplicate struct declaration\n"),
            );
        }
    }

    fn analyse_funcdef(&mut self, defn: NodeId) {
        let decl = self.ast[defn]
            .child(child::FUNCDEF_DECL)
            .expect("funcdef decl");
        let var = self.ast[decl].e_decl().expect("funcdef decl var");
        let TypeKind::Function { param_decls, .. } = *self.type_kind(var) else {
            self.snc
                .error_at_node(&self.ast[decl], format_args!("not a function type\n"));
            return;
        };
        let params: Vec<_> = self.ast.list(param_decls).collect();
        for p in params {
            // check parameter has a name
            let pv = self.ast[p].e_decl().expect("parameter decl var");
            if self.ast[pv].name.is_none() {
                self.snc.error_at_node(
                    &self.ast[p],
                    format_args!("function parameter must have a name\n"),
                );
            }
        }
    }

    fn analyse_defns(&mut self, defn_list: Option<NodeId>, scope: NodeId) {
        let defns: Vec<_> = self.ast.list(defn_list).collect();
        for defn in defns {
            match self.ast[defn].tag {
                NodeTag::DOption => match self.ast[scope].tag {
                    NodeTag::DProg => self.analyse_option(defn),
                    NodeTag::DState => self.analyse_state_option(scope, defn),
                    _ => {}
                },
                NodeTag::DDecl => self.analyse_declaration(scope, defn),
                NodeTag::DFuncdef => {
                    self.analyse_funcdef(defn);
                    let decl = self.ast[defn].child(child::FUNCDEF_DECL).expect("decl");
                    self.analyse_declaration(scope, decl);
                }
                NodeTag::DStructdef => self.analyse_structdef(defn),
                NodeTag::DAssign => self.analyse_assign(scope, defn),
                NodeTag::DMonitor => self.analyse_monitor(scope, defn),
                NodeTag::DSync => self.analyse_sync(scope, defn),
                NodeTag::DSyncq => self.analyse_syncq(scope, defn),
                NodeTag::TText => {}
                tag => unreachable!("{} among definitions", tag.name()),
            }
        }
    }

    fn analyse_scope(&mut self, scope: NodeId, parent_scope: Option<NodeId>) -> bool {
        // NOTE: We always need to allocate a var_list, even if there are no
        // definitions on this level, so later on (see connect_variables)
        // we can traverse in the other direction to find the nearest
        // enclosing scope.
        if self.ast.scope_var_list(scope).is_none() {
            let list = self.ast.add_var_list(VarList {
                parent_scope,
                ..VarList::default()
            });
            self.ast.set_scope_var_list(scope, list);
        }

        let defn_list = self.ast.defn_list_from_scope(scope);
        self.analyse_defns(defn_list, scope);
        if self.ast[scope].tag == NodeTag::DProg {
            let xdefns = self.ast[scope].child(child::PROG_XDEFNS);
            self.analyse_defns(xdefns, scope);
        }
        true // always descend into children
    }

    fn analyse_definitions(&mut self) {
        let prog = self.p.prog;
        traverse_syntax_tree(
            self,
            Some(prog),
            SCOPE_MASK,
            !HAS_SUB_SCOPE_MASK,
            None,
            &mut |cx: &mut Self, scope, parent| cx.analyse_scope(scope, parent),
        );
    }

    /// Options at the top-level. Note: latest given value for option wins.
    fn analyse_option(&mut self, defn: NodeId) {
        let optname = text(self.ast, defn);
        let Extra::Option(optval) = self.ast[defn].extra else {
            unreachable!("option node without value")
        };
        let options = &mut self.p.options;
        for (i, c) in optname.char_indices() {
            match c {
                'a' => options.async_ = optval,
                'c' => options.conn = optval,
                'd' => options.debug = optval,
                'e' => options.newef = optval,
                'l' => options.line = optval,
                'm' => options.main = optval,
                'r' => options.reent = optval,
                's' => options.safe = optval,
                'w' => options.warn = optval,
                'W' => options.xwarn = optval,
                _ => self.snc.report_at_node(
                    &self.ast[defn],
                    format_args!("warning: unknown option '{}'\n", &optname[i..]),
                ),
            }
        }
        if self.p.options.safe {
            self.p.options.reent = true;
        }
    }

    /// Options in state declarations. Note: latest given value for option
    /// wins.
    fn analyse_state_option(&mut self, state: NodeId, defn: NodeId) {
        let optname = text(self.ast, defn);
        let Extra::Option(optval) = self.ast[defn].extra else {
            unreachable!("option node without value")
        };
        for (i, c) in optname.char_indices() {
            let Extra::State(st) = &mut self.ast[state].extra else {
                unreachable!("state node without state data")
            };
            let options = &mut st.options;
            match c {
                't' => options.do_reset_timers = optval,
                'e' => options.no_entry_from_self = optval,
                'x' => options.no_exit_to_self = optval,
                _ => self.snc.report_at_node(
                    &self.ast[defn],
                    format_args!("warning: unknown state option '{}'\n", &optname[i..]),
                ),
            }
        }
    }

    /// Traverse a type and replace foreign struct types with the SNL struct
    /// type of the same name, if one exists.
    ///
    /// C has no `visited` set and recurses forever (segfault) through a
    /// self-referencing struct whose type is shared by three declarators
    /// (`struct S {struct S *next;}; struct S a, b, c;`). Each type is
    /// converted at most once, so visiting it once gives C's result.
    fn fixup_struct_refs(&mut self, t: crate::types::TypeId, visited: &mut HashSet<u32>) {
        if !visited.insert(t.0) {
            return;
        }
        match self.ast[t].kind.clone() {
            TypeKind::None | TypeKind::EvFlag | TypeKind::Void | TypeKind::Prim(_) => {}
            TypeKind::Foreign { tag, name } => {
                if tag == crate::var_types::ForeignTag::Struct
                    && let Some(Sym::Type(r)) = self.p.sym_table.lookup(&name, SymScope::StructDefs)
                {
                    let kind = self.ast[r].kind.clone();
                    assert!(matches!(kind, TypeKind::Struct { .. }));
                    // NOTE: do *not* change is_const
                    self.ast[t].kind = kind;
                }
            }
            TypeKind::Pointer { value_type } => self.fixup_struct_refs(value_type, visited),
            TypeKind::Array { elem_type, .. } => self.fixup_struct_refs(elem_type, visited),
            TypeKind::Function {
                return_type,
                param_decls,
            } => {
                self.fixup_struct_refs(return_type, visited);
                self.fixup_decl_types(param_decls, visited);
            }
            TypeKind::Struct { member_decls, .. } => self.fixup_decl_types(member_decls, visited),
        }
    }

    fn fixup_decl_types(&mut self, decls: Option<NodeId>, visited: &mut HashSet<u32>) {
        let decls: Vec<_> = self.ast.list(decls).collect();
        for d in decls {
            if self.ast[d].tag == NodeTag::DDecl {
                let var = self.ast[d].e_decl().expect("decl var");
                self.fixup_struct_refs(self.ast[var].type_, visited);
            }
        }
    }

    fn analyse_declaration(&mut self, scope: NodeId, defn: NodeId) {
        let vp = self.ast[defn].e_decl().expect("declaration var");

        if *self.type_kind(vp) == TypeKind::None && !self.seen_foreign {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("foreign declarations are deprecated\n"),
            );
            self.seen_foreign = true;
        }
        if self.ast[scope].tag != NodeTag::DProg {
            let things = match self.type_kind(vp) {
                TypeKind::None => Some("foreign objects"),
                TypeKind::EvFlag => Some("event flags"),
                TypeKind::Function { .. } => Some("functions"),
                _ => None,
            };
            if let Some(things) = things {
                self.snc.error_at_node(
                    &self.ast[defn],
                    format_args!("{things} can only be declared at the top-level\n"),
                );
            }
        }
        if *self.type_kind(vp) == TypeKind::EvFlag {
            self.ast[vp].chan = VarChan::EvFlag(EvFlag::default());
        }
        self.fixup_struct_refs(self.ast[vp].type_, &mut HashSet::new());

        let var_list = self.ast.var_list_from_scope(scope);

        // C asserts here and aborts on an unnamed function parameter, after
        // analyse_funcdef reported it; skip the parameter instead.
        let Some(name) = self.ast[vp].name.clone() else {
            return;
        };

        if !self
            .p
            .sym_table
            .insert(&name, SymScope::VarList(var_list), Sym::Var(vp))
        {
            let Some(Sym::Var(vp2)) = self.p.sym_table.lookup(&name, SymScope::VarList(var_list))
            else {
                unreachable!("variable list holds variables only")
            };
            // Note: functions can be declared more than once.
            // We let the C compiler check that they are consistent.
            if !matches!(self.type_kind(vp2), TypeKind::Function { .. }) {
                match self.ast[vp2].decl {
                    Some(d2) => {
                        let tok = &self.ast[d2].token;
                        let (file, line) = (tok.file.clone(), tok.line);
                        self.snc.error_at_node(
                            &self.ast[defn],
                            format_args!(
                                "variable '{name}' already declared at {}:{line}\n",
                                file.as_deref().unwrap_or("(null)")
                            ),
                        );
                    }
                    None => self.snc.error_at_node(
                        &self.ast[defn],
                        format_args!("variable '{name}' already (implicitly) declared\n"),
                    ),
                }
            }
        } else {
            add_var(self.ast, vp, scope);
        }
    }

    fn analyse_assign(&mut self, scope: NodeId, defn: NodeId) {
        let name = text(self.ast, defn);
        let Some(vp) = self.find_var(&name, scope) else {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("cannot assign variable '{name}': variable was not declared\n"),
            );
            return;
        };
        if !self.ast.type_assignable(self.ast[vp].type_) {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("cannot assign variable '{name}': wrong type\n"),
            );
            return;
        }
        if self.ast[vp].scope != Some(scope) {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "cannot assign variable '{name}': \
                     assign must be in the same scope as declaration\n"
                ),
            );
            return;
        }
        if self.ast[scope].tag == NodeTag::DState {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("state local assign is deprecated\n"),
            );
        }
        let subscr = self.ast[defn].child(child::ASSIGN_SUBSCR);
        let pvs = self.ast[defn].child(child::ASSIGN_PVS);
        match (subscr, pvs) {
            (Some(subscr), pvs) => {
                let pv_name = pvs.expect("assign subscript without pv name");
                self.assign_subscript(defn, vp, subscr, pv_name);
            }
            (None, None) => self.assign_single(defn, vp, None),
            (None, Some(pvs)) if self.ast[pvs].tag == NodeTag::EInit => {
                let elems = self.ast[pvs].child(child::INIT_ELEMS);
                self.assign_multi(defn, vp, elems);
            }
            (None, Some(pvs)) => self.assign_single(defn, vp, Some(pvs)),
        }
    }

    /// Assign a (whole) variable to a channel.
    /// Format: `assign <variable> to <string>;`
    fn assign_single(&mut self, defn: NodeId, vp: VarId, pv_name: Option<NodeId>) {
        let name = pv_name.map_or_else(|| Arc::from(""), |n| text(self.ast, n));

        if self.ast[vp].assign != Multiplicity::None {
            let vname = var_name(self.ast, vp);
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{vname}' already assigned\n"),
            );
            return;
        }
        self.ast[vp].assign = Multiplicity::Single;
        let t = self.ast[vp].type_;
        let count = self.ast.type_array_length1(t) * self.ast.type_array_length2(t);
        let cp = self.new_channel(vp, count, 0);
        self.ast[vp].chan = VarChan::Single(cp);
        self.ast[cp].name = Some(name);
    }

    /// The channel of element `n` of a multi-assigned array.
    fn multi_chan(&self, vp: VarId, n: u32) -> ChanId {
        match &self.ast[vp].chan {
            VarChan::Multi(chans) => chans[n as usize].expect("channel per element"),
            other => unreachable!("multi-assigned variable with chan {other:?}"),
        }
    }

    fn assign_elem(&mut self, defn: NodeId, vp: VarId, n_subscr: u32, pv_name: Arc<str>) {
        let t = self.ast[vp].type_;
        assert!(n_subscr < self.ast.type_array_length1(t));
        assert!(self.ast[vp].assign != Multiplicity::Single);

        if self.ast[vp].assign == Multiplicity::None {
            self.ast[vp].assign = Multiplicity::Multi;
            let len1 = self.ast.type_array_length1(t);
            let len2 = self.ast.type_array_length2(t);
            let chans = (0..len1)
                .map(|n| Some(self.new_channel(vp, len2, n)))
                .collect();
            self.ast[vp].chan = VarChan::Multi(chans);
        }
        let cp = self.multi_chan(vp, n_subscr);
        if let Some(old) = self.ast[cp].name.clone() {
            let vname = var_name(self.ast, vp);
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("array element '{vname}[{n_subscr}]' already assigned to '{old}'\n"),
            );
            return;
        }
        self.ast[cp].name = Some(pv_name);
    }

    /// C `strtoui` of a subscript against the array's first dimension,
    /// reporting it out of range.
    fn subscript(&mut self, vp: VarId, subscr: NodeId) -> Option<u32> {
        let s = text(self.ast, subscr);
        let n = strtoui(&s, self.ast.type_array_length1(self.ast[vp].type_));
        if n.is_none() {
            let vname = var_name(self.ast, vp);
            self.snc.error_at_node(
                &self.ast[subscr],
                format_args!("subscript in '{vname}[{s}]' out of range\n"),
            );
        }
        n
    }

    /// Report `variable '<name>' is not an array` unless it is one
    /// (establishes L3).
    fn require_array(&mut self, defn: NodeId, vp: VarId) -> bool {
        if matches!(self.type_kind(vp), TypeKind::Array { .. }) {
            return true;
        }
        let vname = var_name(self.ast, vp);
        self.snc.error_at_node(
            &self.ast[defn],
            format_args!("variable '{vname}' is not an array\n"),
        );
        false
    }

    /// Assign an array element to a channel.
    /// Format: `assign <variable>[<subscr>] to <string>;`
    fn assign_subscript(&mut self, defn: NodeId, vp: VarId, subscr: NodeId, pv_name: NodeId) {
        if !self.require_array(defn, vp) {
            return;
        }
        if self.ast[vp].assign == Multiplicity::Single {
            let vname = var_name(self.ast, vp);
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{vname}' already assigned\n"),
            );
            return;
        }
        let Some(n_subscr) = self.subscript(vp, subscr) else {
            return;
        };
        let pv = text(self.ast, pv_name);
        self.assign_elem(defn, vp, n_subscr, pv);
    }

    /// Assign an array variable to multiple channels.
    /// Format: `assign <variable> to { <string>, <string>, ... };`
    fn assign_multi(&mut self, defn: NodeId, vp: VarId, pv_name_list: Option<NodeId>) {
        if !self.require_array(defn, vp) {
            return;
        }
        if self.ast[vp].assign == Multiplicity::Single {
            let vname = var_name(self.ast, vp);
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("assign: variable '{vname}' already assigned\n"),
            );
            return;
        }
        let len1 = self.ast.type_array_length1(self.ast[vp].type_);
        let mut n_subscr = 0;
        let pv_names: Vec<_> = self.ast.list(pv_name_list).collect();
        for pv_name in pv_names {
            if n_subscr >= len1 {
                let vname = var_name(self.ast, vp);
                self.snc.warning_at_node(
                    &self.ast[pv_name],
                    format_args!(
                        "discarding excess PV names in multiple assign to variable '{vname}'\n"
                    ),
                );
                break;
            }
            let pv = text(self.ast, pv_name);
            self.assign_elem(defn, vp, n_subscr, pv);
            n_subscr += 1;
        }
        // for the remaining array elements, assign to ""
        while n_subscr < len1 {
            self.assign_elem(defn, vp, n_subscr, Arc::from(""));
            n_subscr += 1;
        }
    }

    fn monitor_var(&mut self, defn: NodeId, vp: VarId) {
        let vname = var_name(self.ast, vp);
        if self.ast[vp].assign == Multiplicity::None {
            // establish L2a
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("cannot monitor variable '{vname}': not assigned\n"),
            );
            return;
        }
        if self.ast[vp].monitor == Multiplicity::Single {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("variable '{vname}' already monitored\n"),
            );
            return; // nothing to do
        }
        self.ast[vp].monitor = Multiplicity::Single; // strengthen if M_MULTI
        for cp in self.var_chans(vp) {
            self.ast[cp].monitor = true;
        }
    }

    /// Every channel of an assigned variable (C's `M_SINGLE` / `M_MULTI`
    /// branches).
    fn var_chans(&self, vp: VarId) -> Vec<ChanId> {
        match &self.ast[vp].chan {
            VarChan::Single(cp) => vec![*cp],
            VarChan::Multi(chans) => chans.iter().map(|c| c.expect("channel")).collect(),
            other => unreachable!("assigned variable with chan {other:?}"),
        }
    }

    fn monitor_elem(&mut self, defn: NodeId, vp: VarId, subscr: NodeId) {
        if !self.require_array(defn, vp) {
            return;
        }
        let Some(n_subscr) = self.subscript(vp, subscr) else {
            return;
        };
        let vname = var_name(self.ast, vp);
        if self.ast[vp].assign == Multiplicity::None {
            // establish L2a
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("array element '{vname}[{n_subscr}]' not assigned\n"),
            );
            return;
        }
        if self.ast[vp].monitor == Multiplicity::Single {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("array element '{vname}[{n_subscr}]' already monitored\n"),
            );
            return; // nothing to do
        }
        if self.ast[vp].assign == Multiplicity::Single {
            // establish L1a
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "variable '{vname}' is assigned to a single channel \
                     and can only be monitored wholesale\n"
                ),
            );
            return;
        }
        let cp = self.multi_chan(vp, n_subscr);
        if self.ast[cp].name.is_none() {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("array element '{vname}[{n_subscr}]' not assigned\n"),
            );
            return;
        }
        if self.ast[cp].monitor {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("'{vname}[{n_subscr}]' already monitored\n"),
            );
            return; // nothing to do
        }
        self.ast[cp].monitor = true; // do it
    }

    fn analyse_monitor(&mut self, scope: NodeId, defn: NodeId) {
        let var_name = text(self.ast, defn);
        let Some(vp) = self.find_var(&var_name, scope) else {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("cannot monitor variable '{var_name}': not declared\n"),
            );
            return;
        };
        if self.ast[vp].scope != Some(scope) {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "cannot monitor variable '{var_name}': \
                     monitor must be in the same scope as declaration\n"
                ),
            );
            return;
        }
        if self.ast[scope].tag == NodeTag::DState {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("state local monitor is deprecated\n"),
            );
        }
        match self.ast[defn].child(child::MONITOR_SUBSCR) {
            Some(subscr) => self.monitor_elem(defn, vp, subscr),
            None => self.monitor_var(defn, vp),
        }
    }

    /// C asserts `sync != M_SINGLE` on entry to `sync_var` and `sync_elem`;
    /// analyse_sync establishes it with this error, analyse_syncq does not
    /// and C aborts. Both callers get the error.
    fn require_not_synced(&mut self, defn: NodeId, vp: VarId) -> bool {
        if self.ast[vp].sync != Multiplicity::Single {
            return true;
        }
        let vname = var_name(self.ast, vp);
        self.snc.error_at_node(
            &self.ast[defn],
            format_args!("variable '{vname}' already sync'd\n"),
        );
        false
    }

    fn sync_var(&mut self, defn: NodeId, vp: VarId, evp: VarId) {
        if !self.require_not_synced(defn, vp) {
            return;
        }
        let vname = var_name(self.ast, vp);
        if self.ast[vp].sync == Multiplicity::Multi {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("some array elements of '{vname}' already sync'd\n"),
            );
            return;
        }
        if self.ast[vp].assign == Multiplicity::None {
            // establish L2b
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{vname}' not assigned\n"),
            );
            return;
        }
        self.ast[vp].sync = Multiplicity::Single;
        for cp in self.var_chans(vp) {
            assert!(self.ast[cp].sync.is_none());
            self.ast[cp].sync = Some(evp);
        }
    }

    fn sync_elem(&mut self, defn: NodeId, vp: VarId, subscr: NodeId, evp: VarId) {
        if !self.require_not_synced(defn, vp) || !self.require_array(defn, vp) {
            return;
        }
        let Some(n_subscr) = self.subscript(vp, subscr) else {
            return;
        };
        let vname = var_name(self.ast, vp);
        // establish L1b
        if self.ast[vp].assign == Multiplicity::Single {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "variable '{vname}' is assigned to a single channel \
                     and can only be sync'd wholesale\n"
                ),
            );
            return;
        }
        if self.ast[vp].assign == Multiplicity::None
            || self.ast[self.multi_chan(vp, n_subscr)].name.is_none()
        {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("array element '{vname}[{n_subscr}]' not assigned\n"),
            );
            return;
        }
        let cp = self.multi_chan(vp, n_subscr);
        if self.ast[cp].sync.is_some() {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("'{vname}[{n_subscr}]' already sync'd\n"),
            );
            return; // nothing to do
        }
        self.ast[vp].sync = Multiplicity::Multi;
        self.ast[cp].sync = Some(evp); // do it
    }

    /// Find the event flag `ef_name` for a sync or syncq statement,
    /// reporting it missing or not an event flag.
    fn find_evflag(&mut self, scope: NodeId, defn: NodeId, ef_name: &str) -> Option<VarId> {
        let Some(evp) = self.find_var(ef_name, scope) else {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("event flag '{ef_name}' not declared\n"),
            );
            return None;
        };
        if *self.type_kind(evp) != TypeKind::EvFlag {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{ef_name}' is not an event flag\n"),
            );
            return None;
        }
        Some(evp)
    }

    fn analyse_sync(&mut self, scope: NodeId, defn: NodeId) {
        let var_name = text(self.ast, defn);
        let ef = self.ast[defn]
            .child(child::SYNC_EVFLAG)
            .expect("sync evflag");
        let ef_name = text(self.ast, ef);

        let Some(vp) = self.find_var(&var_name, scope) else {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{var_name}' not declared\n"),
            );
            return;
        };
        if self.ast[vp].scope != Some(scope) {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "cannot sync variable '{var_name}' to event flag '{ef_name}': \
                     sync must be in the same scope as (variable) declaration\n"
                ),
            );
            return;
        }
        if self.ast[scope].tag == NodeTag::DState {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("state local sync is deprecated\n"),
            );
        }
        if !self.require_not_synced(defn, vp) {
            return;
        }
        let Some(evp) = self.find_evflag(scope, defn, &ef_name) else {
            return;
        };
        match self.ast[defn].child(child::SYNC_SUBSCR) {
            Some(subscr) => self.sync_elem(defn, vp, subscr, evp),
            None => self.sync_var(defn, vp, evp),
        }
    }

    fn syncq_var(&mut self, defn: NodeId, vp: VarId, qp: SyncQId) {
        assert!(self.ast[vp].syncq != Multiplicity::Single);
        let vname = var_name(self.ast, vp);
        if self.ast[vp].syncq == Multiplicity::Multi {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("some array elements of '{vname}' already syncq'd\n"),
            );
            return;
        }
        if self.ast[vp].assign == Multiplicity::None {
            // establish L2c
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{vname}' not assigned\n"),
            );
            return;
        }
        self.ast[vp].syncq = Multiplicity::Single;
        for cp in self.var_chans(vp) {
            assert!(self.ast[cp].syncq.is_none());
            self.ast[cp].syncq = Some(qp);
        }
    }

    fn syncq_elem(&mut self, defn: NodeId, vp: VarId, subscr: NodeId, qp: SyncQId) {
        assert!(self.ast[vp].syncq != Multiplicity::Single);
        if !self.require_array(defn, vp) {
            return;
        }
        let Some(n_subscr) = self.subscript(vp, subscr) else {
            return;
        };
        let vname = var_name(self.ast, vp);
        // establish L1c
        if self.ast[vp].assign == Multiplicity::Single {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "variable '{vname}' is assigned to a single channel \
                     and can only be syncq'd wholesale\n"
                ),
            );
            return;
        }
        if self.ast[vp].assign == Multiplicity::None
            || self.ast[self.multi_chan(vp, n_subscr)].name.is_none()
        {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("array element '{vname}[{n_subscr}]' not assigned\n"),
            );
            return;
        }
        let cp = self.multi_chan(vp, n_subscr);
        if self.ast[cp].syncq.is_some() {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("'{vname}[{n_subscr}]' already syncq'd\n"),
            );
            return; // nothing to do
        }
        self.ast[vp].syncq = Multiplicity::Multi;
        self.ast[cp].syncq = Some(qp); // do it
    }

    fn analyse_syncq(&mut self, scope: NodeId, defn: NodeId) {
        let var_name = text(self.ast, defn);
        let Some(vp) = self.find_var(&var_name, scope) else {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{var_name}' not declared\n"),
            );
            return;
        };
        if self.ast[vp].scope != Some(scope) {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!(
                    "cannot syncq variable '{var_name}': \
                     syncq must be in the same scope as declaration\n"
                ),
            );
            return;
        }
        if self.ast[scope].tag == NodeTag::DState {
            self.snc.warning_at_node(
                &self.ast[defn],
                format_args!("state local syncq is deprecated\n"),
            );
        }
        if self.ast[vp].syncq == Multiplicity::Single {
            self.snc.error_at_node(
                &self.ast[defn],
                format_args!("variable '{var_name}' already syncq'd\n"),
            );
            return;
        }
        let mut n_size = 0;
        match self.ast[defn].child(child::SYNCQ_SIZE) {
            None => self.snc.warning_at_node(
                &self.ast[defn],
                format_args!(
                    "leaving out the queue size is deprecated, \
                     queue size defaults to 100 elements\n"
                ),
            ),
            Some(size) => {
                let s = text(self.ast, size);
                match strtoui(&s, u32::MAX) {
                    Some(n) if n >= 1 => n_size = n,
                    _ => {
                        self.snc.error_at_node(
                            &self.ast[size],
                            format_args!("queue size '{s}' out of range\n"),
                        );
                        return;
                    }
                }
            }
        }
        let mut evp = None;
        if let Some(ef) = self.ast[defn].child(child::SYNCQ_EVFLAG) {
            let ef_name = text(self.ast, ef);
            let Some(e) = self.find_evflag(scope, defn, &ef_name) else {
                return;
            };
            let VarChan::EvFlag(flag) = &mut self.ast[e].chan else {
                unreachable!("event flag without flag data")
            };
            if flag.queued {
                self.snc.error_at_node(
                    &self.ast[defn],
                    format_args!("event flag '{ef_name}' is already used for another syncq\n"),
                );
                return;
            }
            flag.queued = true;
            evp = Some(e);
        }
        let qp = self.new_sync_queue(n_size);
        match self.ast[defn].child(child::SYNCQ_SUBSCR) {
            Some(subscr) => {
                if let Some(evp) = evp {
                    self.sync_elem(defn, vp, subscr, evp);
                }
                self.syncq_elem(defn, vp, subscr, qp);
            }
            None => {
                if let Some(evp) = evp {
                    self.sync_var(defn, vp, evp);
                }
                self.syncq_var(defn, vp, qp);
            }
        }
    }

    /// Allocate a channel for this variable, append it to the channel
    /// list, and count it there.
    fn new_channel(&mut self, vp: VarId, count: u32, index: u32) -> ChanId {
        let list = &mut self.p.chan_list;
        if index == 0 {
            self.ast[vp].index = list.num_elems;
        }
        list.num_elems += 1;
        let cp = self.ast.add_chan(Chan {
            next: None,
            name: None,
            index,
            var: vp,
            count,
            monitor: false,
            sync: None,
            syncq: None,
        });
        match list.last {
            None => list.first = Some(cp),
            Some(last) => self.ast[last].next = Some(cp),
        }
        list.last = Some(cp);
        cp
    }

    /// Allocate a sync queue, append it to the sync queue list, and count
    /// it there.
    fn new_sync_queue(&mut self, size: u32) -> SyncQId {
        let list = &mut self.p.syncq_list;
        let qp = self.ast.add_syncq(SyncQ {
            next: None,
            index: list.num_elems,
            size,
        });
        list.num_elems += 1;
        match list.last {
            None => list.first = Some(qp),
            Some(last) => self.ast[last].next = Some(qp),
        }
        list.last = Some(qp);
        qp
    }

    fn find_var(&self, name: &str, scope: NodeId) -> Option<VarId> {
        find_var(self.ast, &self.p.sym_table, name, scope)
    }

    /// Connect a variable reference (E_VAR) to its variable. A name that is
    /// no variable but a builtin constant or function turns the node into
    /// E_CONST or E_BUILTIN. Any other undeclared name gets a variable of
    /// type T_NONE in the top-level scope.
    fn connect_variable(&mut self, ep: NodeId, scope: Option<NodeId>) -> bool {
        let scope = scope.expect("variable reference outside any scope");
        let name = text(self.ast, ep);

        let vp = match self.find_var(&name, scope) {
            Some(vp) => vp,
            None => {
                if let Some(c) = lookup_builtin_const(&self.p.sym_table, &name) {
                    self.ast[ep].tag = NodeTag::EConst;
                    self.ast[ep].extra = Extra::Const(c);
                    return false;
                }
                if let Some(f) = lookup_builtin_func(&self.p.sym_table, &name) {
                    self.ast[ep].tag = NodeTag::EBuiltin;
                    self.ast[ep].extra = Extra::Builtin(f);
                    return false;
                }

                self.snc.extra_warning_at_node(
                    &self.ast[ep],
                    format_args!("treating undeclared object '{name}' as foreign\n"),
                );
                // create a pseudo declaration so we can finish the analysis
                let t = self.ast.mk_no_type();
                let vp = self.ast.add_var(Some(name.clone()), t);
                // add this variable to the top-level scope, NOT the current
                let mut scope = scope;
                let mut var_list = self.ast.var_list_from_scope(scope);
                while let Some(parent) = self.ast[var_list].parent_scope {
                    scope = parent;
                    var_list = self.ast.var_list_from_scope(scope);
                }
                self.p
                    .sym_table
                    .insert(&name, SymScope::VarList(var_list), Sym::Var(vp));
                add_var(self.ast, vp, scope);
                vp
            }
        };
        self.ast[ep].extra = Extra::Var(vp); // make connection
        false // there are no children anyway
    }

    fn connect_variables(&mut self) {
        let prog = self.p.prog;
        traverse_syntax_tree(
            self,
            Some(prog),
            NodeTag::EVar.bit(),
            0,
            None,
            &mut |cx: &mut Self, ep, scope| cx.connect_variable(ep, scope),
        );
    }

    /// Check for duplicate state set and state names and resolve
    /// transitions between states.
    fn connect_states(&mut self) -> u32 {
        let prog = self.p.prog;
        let mut num_ss = 0;
        let ssps: Vec<_> = self
            .ast
            .list(self.ast[prog].child(child::PROG_STATESETS))
            .collect();
        for ssp in ssps {
            let ss_name = text(self.ast, ssp);
            if !self
                .p
                .sym_table
                .insert(&ss_name, SymScope::Node(prog), Sym::Node(ssp))
            {
                let Some(Sym::Node(ssp2)) = self.p.sym_table.lookup(&ss_name, SymScope::Node(prog))
                else {
                    unreachable!("program scope holds state sets only")
                };
                let line2 = self.ast[ssp2].token.line;
                self.snc.error_at_node(
                    &self.ast[ssp],
                    format_args!(
                        "a state set with name '{ss_name}' was already declared at line {line2}\n"
                    ),
                );
            }
            let sps: Vec<_> = self
                .ast
                .list(self.ast[ssp].child(child::SS_STATES))
                .collect();
            for (num_states, &sp) in sps.iter().enumerate() {
                let sp_name = text(self.ast, sp);
                if !self
                    .p
                    .sym_table
                    .insert(&sp_name, SymScope::Node(ssp), Sym::Node(sp))
                {
                    let Some(Sym::Node(sp2)) =
                        self.p.sym_table.lookup(&sp_name, SymScope::Node(ssp))
                    else {
                        unreachable!("state set scope holds states only")
                    };
                    let line2 = self.ast[sp2].token.line;
                    self.snc.error_at_node(
                        &self.ast[sp],
                        format_args!(
                            "a state with name '{sp_name}' in state set '{ss_name}' \
                             was already declared at line {line2}\n"
                        ),
                    );
                }
                state_mut(self.ast, sp).index = num_states as u32;
            }
            let Extra::Ss(ss) = &mut self.ast[ssp].extra else {
                unreachable!("state set node without state set data")
            };
            ss.num_states = sps.len() as u32;
            for &sp in &sps {
                let tps: Vec<_> = self
                    .ast
                    .list(self.ast[sp].child(child::STATE_WHENS))
                    .collect();
                for tp in tps {
                    let mut next_sp = None;
                    if let Some(target) = self.ast[tp].token.str.clone() {
                        next_sp = match self.p.sym_table.lookup(&target, SymScope::Node(ssp)) {
                            Some(Sym::Node(n)) => Some(n),
                            Some(_) => unreachable!("state set scope holds states only"),
                            None => {
                                self.snc.error_at_node(
                                    &self.ast[tp],
                                    format_args!(
                                        "a state with name '{target}' does not \
                                         exist in state set '{ss_name}'\n"
                                    ),
                                );
                                None
                            }
                        };
                    }
                    let Extra::When(when) = &mut self.ast[tp].extra else {
                        unreachable!("when node without transition data")
                    };
                    when.next_state = next_sp;
                }
            }
            num_ss += 1;
        }
        num_ss
    }

    fn connect_state_change_stmts(&mut self) {
        let prog = self.p.prog;
        let mut ssp: Option<NodeId> = None;
        let mut in_when = false;
        traverse_syntax_tree(
            self,
            Some(prog),
            NodeTag::SChange.bit()
                | NodeTag::DSs.bit()
                | NodeTag::DEntex.bit()
                | NodeTag::DWhen.bit(),
            EXPR_MASK,
            None,
            &mut |cx: &mut Self, ep, _scope| match cx.ast[ep].tag {
                NodeTag::DSs => {
                    ssp = Some(ep);
                    true
                }
                NodeTag::DEntex => {
                    // to flag erroneous state change statements, see below
                    in_when = false;
                    true
                }
                NodeTag::DWhen => {
                    in_when = true;
                    true
                }
                NodeTag::SChange => {
                    // C never resets in_when when it leaves a when clause:
                    // a function after the state sets is in the last one.
                    match ssp {
                        Some(ssp) if in_when => {
                            let name = text(cx.ast, ep);
                            match cx.p.sym_table.lookup(&name, SymScope::Node(ssp)) {
                                Some(Sym::Node(sp)) => cx.ast[ep].extra = Extra::Change(sp),
                                Some(_) => unreachable!("state set scope holds states only"),
                                None => {
                                    let ss_name = text(cx.ast, ssp);
                                    cx.snc.error_at_node(
                                        &cx.ast[ep],
                                        format_args!(
                                            "a state with name '{name}' does not \
                                             exist in state set '{ss_name}'\n"
                                        ),
                                    );
                                }
                            }
                        }
                        _ => cx.snc.error_at_node(
                            &cx.ast[ep],
                            format_args!("state change statement not allowed here\n"),
                        ),
                    }
                    false
                }
                tag => unreachable!("{} in connect_state_change_stmts", tag.name()),
            },
        );
    }

    fn check_states_reachable_from_first(&mut self, ssp: NodeId) {
        let sps: Vec<_> = self
            .ast
            .list(self.ast[ssp].child(child::SS_STATES))
            .collect();
        let first = sps[0];
        assert_eq!(state_mut(self.ast, first).index, 0);

        state_mut(self.ast, first).is_target = true;
        mark_states_reachable_from(self.ast, first);

        for sp in sps {
            if !state_mut(self.ast, sp).is_target {
                let (sp_name, ss_name) = (text(self.ast, sp), text(self.ast, ssp));
                self.snc.warning_at_node(
                    &self.ast[sp],
                    format_args!(
                        "state '{sp_name}' in state set '{ss_name}' cannot \
                         be reached from start state\n"
                    ),
                );
            }
        }
    }

    /// Assign event bits to event flags, numbered from 1 in declaration
    /// order. Returns the number of event flags.
    fn assign_ef_bits(&mut self) -> u32 {
        let var_list = self.ast.var_list_from_scope(self.p.prog);
        let mut num_event_flags = 0;
        let mut vp = self.ast[var_list].first;
        while let Some(v) = vp {
            if let VarChan::EvFlag(flag) = &mut self.ast[v].chan {
                num_event_flags += 1;
                flag.index = num_event_flags;
            }
            vp = self.ast[v].next;
        }
        num_event_flags
    }
}

fn state_mut(ast: &mut Ast, sp: NodeId) -> &mut crate::types::State {
    match &mut ast[sp].extra {
        Extra::State(state) => state,
        _ => unreachable!("state node without state data"),
    }
}

/// Mark every state reachable from `sp` through `when` targets and state
/// change statements as a target.
fn mark_states_reachable_from(ast: &mut Ast, sp: NodeId) {
    assert_eq!(ast[sp].tag, NodeTag::DState);
    traverse_syntax_tree(
        ast,
        Some(sp),
        NodeTag::SChange.bit() | NodeTag::DWhen.bit(),
        EXPR_MASK,
        Some(sp),
        &mut |ast: &mut Ast, ep, _scope| {
            let target_state = match &ast[ep].extra {
                Extra::Change(n) => Some(*n),
                Extra::When(when) => when.next_state,
                // an S_CHANGE that analysis refused stays unconnected
                Extra::None => None,
                other => unreachable!("{other:?} in mark_states_reachable_from"),
            };
            if let Some(target) = target_state
                && !state_mut(ast, target).is_target
            {
                state_mut(ast, target).is_target = true;
                mark_states_reachable_from(ast, target);
            }
            ast[ep].tag == NodeTag::DWhen
        },
    );
}

/// Add a variable to a scope (append to the end of the var_list).
fn add_var(ast: &mut Ast, vp: VarId, scope: NodeId) {
    let var_list = ast.var_list_from_scope(scope);
    match ast[var_list].last {
        None => ast[var_list].first = Some(vp),
        Some(last) => ast[last].next = Some(vp),
    }
    ast[var_list].last = Some(vp);
    ast[vp].next = None;
    ast[vp].scope = Some(scope);
}

/// Find a variable by name, given a scope; first searches the given scope,
/// then the parent scope, and so on.
fn find_var(ast: &Ast, st: &SymTable<SymScope, Sym>, name: &str, scope: NodeId) -> Option<VarId> {
    let var_list = ast.var_list_from_scope(scope);
    match st.lookup(name, SymScope::VarList(var_list)) {
        Some(Sym::Var(vp)) => Some(vp),
        Some(_) => unreachable!("variable list holds variables only"),
        None => find_var(ast, st, name, ast[var_list].parent_scope?),
    }
}
