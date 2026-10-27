//! Variable types: construction from declarators, queries, and C code for
//! a type (C `var_types.c`).

use std::sync::Arc;

use crate::snc::driver::{Exit, Snc};
use crate::snc::node::strtoui;
use crate::snc::seq_prim_types::PrimType;
use crate::snc::types::{Ast, Extra, NodeId, NodeTag, Tok, Token, TypeId, child};

/// Implicit first parameter of every SNL function (C `NM_ENV` in
/// `gen_code.h`).
pub const NM_ENV: &str = "seqg_env";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForeignTag {
    Enum,
    Struct,
    Union,
    Typename,
}

impl ForeignTag {
    /// C `foreign_type_prefix[tag]`.
    pub fn prefix(self) -> &'static str {
        match self {
            ForeignTag::Enum => "enum ",
            ForeignTag::Struct => "struct ",
            ForeignTag::Union => "union ",
            ForeignTag::Typename => "",
        }
    }
}

/// C `enum type_tag` with the `val` union.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    /// undeclared (or declared as foreign) variable
    None,
    /// event flags
    EvFlag,
    /// void type
    Void,
    /// primitive types: numbers, char, string
    Prim(PrimType),
    /// foreign types (declared in C code)
    Foreign {
        tag: ForeignTag,
        name: Arc<str>,
    },
    Pointer {
        value_type: TypeId,
    },
    Array {
        num_elems: u32,
        elem_type: TypeId,
    },
    Function {
        return_type: TypeId,
        param_decls: Option<NodeId>,
    },
    /// struct defined in SNL code
    Struct {
        name: Arc<str>,
        member_decls: Option<NodeId>,
    },
}

impl TypeKind {
    /// C `type_tag_names[tag]`.
    pub fn tag_name(&self) -> &'static str {
        match self {
            TypeKind::None => "T_NONE",
            TypeKind::EvFlag => "T_EVFLAG",
            TypeKind::Void => "T_VOID",
            TypeKind::Prim(_) => "T_PRIM",
            TypeKind::Foreign { .. } => "T_FOREIGN",
            TypeKind::Pointer { .. } => "T_POINTER",
            TypeKind::Array { .. } => "T_ARRAY",
            TypeKind::Function { .. } => "T_FUNCTION",
            TypeKind::Struct { .. } => "T_STRUCT",
        }
    }
}

/// C `struct type`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    pub kind: TypeKind,
    pub is_const: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assoc {
    L,
    R,
}

impl Ast {
    fn mk_type(&mut self, kind: TypeKind) -> TypeId {
        self.add_type(Type {
            kind,
            is_const: false,
        })
    }

    pub fn mk_prim_type(&mut self, tag: PrimType) -> TypeId {
        self.mk_type(TypeKind::Prim(tag))
    }

    pub fn mk_foreign_type(&mut self, tag: ForeignTag, name: Arc<str>) -> TypeId {
        self.mk_type(TypeKind::Foreign { tag, name })
    }

    pub fn mk_ef_type(&mut self) -> TypeId {
        self.mk_type(TypeKind::EvFlag)
    }

    pub fn mk_void_type(&mut self) -> TypeId {
        self.mk_type(TypeKind::Void)
    }

    pub fn mk_no_type(&mut self) -> TypeId {
        self.mk_type(TypeKind::None)
    }

    pub fn mk_pointer_type(&mut self, t: TypeId) -> TypeId {
        self.mk_type(TypeKind::Pointer { value_type: t })
    }

    pub fn mk_array_type(&mut self, t: TypeId, n: u32) -> TypeId {
        self.mk_type(TypeKind::Array {
            num_elems: n,
            elem_type: t,
        })
    }

    /// Marks `t` itself const: every declaration sharing `t` sees it.
    pub fn mk_const_type(&mut self, t: TypeId) -> TypeId {
        self[t].is_const = true;
        t
    }

    pub fn mk_function_type(&mut self, t: TypeId, ps: Option<NodeId>) -> TypeId {
        self.mk_type(TypeKind::Function {
            return_type: t,
            param_decls: ps,
        })
    }

    pub fn mk_structure_type(&mut self, name: Arc<str>, members: Option<NodeId>) -> TypeId {
        self.mk_type(TypeKind::Struct {
            name,
            member_decls: members,
        })
    }

    /// Base type for any combination of pointers and arrays.
    pub fn base_type(&self, t: TypeId) -> TypeId {
        match self[t].kind {
            TypeKind::Pointer { value_type: t } => self.base_type(t),
            TypeKind::Array { elem_type: t, .. } => self.base_type(t),
            TypeKind::Function { return_type: t, .. } => self.base_type(t),
            _ => t,
        }
    }

    /// Array length in the 1st dimension.
    pub fn type_array_length1(&self, t: TypeId) -> u32 {
        match self[t].kind {
            TypeKind::Array { num_elems, .. } => num_elems,
            _ => 1,
        }
    }

    /// Array length in the 2nd dimension.
    pub fn type_array_length2(&self, t: TypeId) -> u32 {
        match self[t].kind {
            TypeKind::Array { elem_type, .. } => self.type_array_length1(elem_type),
            _ => 1,
        }
    }

    fn type_assignable_array(&self, t: TypeId, depth: u32) -> bool {
        if depth > 2 || self[t].is_const {
            return false;
        }
        match self[t].kind {
            TypeKind::Array { elem_type, .. } => self.type_assignable_array(elem_type, depth + 1),
            TypeKind::Prim(_) => true,
            // T_STRUCT: "for now, at least"
            _ => false,
        }
    }

    /// Whether the type can be assign'ed to a PV.
    pub fn type_assignable(&self, t: TypeId) -> bool {
        self.type_assignable_array(t, 0)
    }
}

fn new_decl(ast: &mut Ast, k: Token, type_: TypeId) -> NodeId {
    let decl = ast.node(NodeTag::DDecl, k, &[None]);
    let var = ast.add_var(ast[decl].token.str.clone(), type_);
    ast[var].decl = Some(decl);
    ast[decl].extra = Extra::Decl(var);
    decl
}

fn add_implicit_parameters(
    snc: &mut Snc,
    ast: &mut Ast,
    fun_decl: NodeId,
    param_decls: Option<NodeId>,
) -> Result<Option<NodeId>, Exit> {
    // implicit parameter: "SS_ID " NM_ENV
    let t = Token {
        symbol: Tok::Name,
        str: Some(Arc::from(NM_ENV)),
        line: ast[fun_decl].token.line,
        file: ast[fun_decl].token.file.clone(),
    };
    let var = ast.node(NodeTag::EVar, t, &[]);
    let ss_id = ast.mk_foreign_type(ForeignTag::Typename, Arc::from("SS_ID"));
    let p1 = mk_decl(snc, ast, Some(var), ss_id)?;
    Ok(ast.link_node(Some(p1), param_decls))
}

fn decl_var(ast: &Ast, decl: NodeId) -> crate::snc::types::VarId {
    ast[decl]
        .e_decl()
        .expect("declaration node without variable")
}

fn remove_void_parameter(snc: &mut Snc, ast: &Ast, param_decls: Option<NodeId>) -> Option<NodeId> {
    let pd = param_decls?;
    let var = decl_var(ast, pd);
    if ast[ast[var].type_].kind != TypeKind::Void {
        return param_decls;
    }
    // no other params should be there
    if let Some(next) = ast[pd].next {
        snc.error_at_node(
            &ast[next],
            format_args!("void must be the only parameter\n"),
        );
    }
    if ast[var].name.is_some() {
        snc.error_at_node(
            &ast[pd],
            format_args!("void parameter should not have a name\n"),
        );
    }
    // void means empty parameter list
    None
}

/// Build a declaration (a syntax node with tag `D_DECL`) from declarator
/// node `d` and child type `t`.
pub fn mk_decl(snc: &mut Snc, ast: &mut Ast, d: Option<NodeId>, t: TypeId) -> Result<NodeId, Exit> {
    let Some(d) = d else {
        // abstract declarator
        return Ok(new_decl(ast, Token::zero(), t));
    };
    let dn = &ast[d];
    let impossible = |snc: &mut Snc, ast: &Ast, what: &str| {
        snc.assert_at_node(false, &ast[d], format_args!("{what}\n"))
    };
    match dn.tag {
        NodeTag::EBinop => {
            // initializer
            assert_eq!(dn.token.symbol, Tok::Equal);
            let (left, right) = (
                dn.children[child::BINOP_LEFT],
                dn.children[child::BINOP_RIGHT],
            );
            let r = mk_decl(snc, ast, left, t)?;
            assert_eq!(ast[r].tag, NodeTag::DDecl); // post condition
            ast[r].children[child::DECL_INIT] = right;
            Ok(r)
        }
        NodeTag::EVar => {
            let k = dn.token.clone();
            Ok(new_decl(ast, k, t))
        }
        NodeTag::EPre => {
            let operand = dn.children[child::PRE_OPERAND];
            match dn.token.symbol {
                Tok::Asterisk => {
                    match ast[t].kind {
                        TypeKind::None => impossible(snc, ast, "pointer to foreign entity")?,
                        TypeKind::EvFlag => {
                            snc.error_at_node(&ast[d], format_args!("pointer to event flag\n"))
                        }
                        _ => {}
                    }
                    let p = ast.mk_pointer_type(t);
                    mk_decl(snc, ast, operand, p)
                }
                Tok::Const => {
                    match ast[t].kind {
                        TypeKind::None => impossible(snc, ast, "constant foreign entity")?,
                        TypeKind::EvFlag => {
                            snc.error_at_node(&ast[d], format_args!("constant event flag\n"))
                        }
                        TypeKind::Array { .. } => {
                            snc.error_at_node(&ast[d], format_args!("constant array\n"))
                        }
                        TypeKind::Function { .. } => {
                            snc.warning_at_node(
                                &ast[d],
                                format_args!("discarding redundant const from function type\n"),
                            );
                            return mk_decl(snc, ast, operand, t);
                        }
                        _ => {}
                    }
                    if ast[t].is_const {
                        snc.warning_at_node(&ast[d], format_args!("discarding repeated const\n"));
                        mk_decl(snc, ast, operand, t)
                    } else {
                        let c = ast.mk_const_type(t);
                        mk_decl(snc, ast, operand, c)
                    }
                }
                s => panic!("mk_decl: prefix declarator {s:?}"),
            }
        }
        NodeTag::ESubscr => {
            match ast[t].kind {
                TypeKind::None => impossible(snc, ast, "array of foreign entities")?,
                TypeKind::EvFlag => {
                    snc.error_at_node(&ast[d], format_args!("array of event flags\n"))
                }
                TypeKind::Void => snc.error_at_node(&ast[d], format_args!("array of void\n")),
                _ => {}
            }
            let dn = &ast[d];
            let operand = dn.children[child::SUBSCR_OPERAND];
            let index = dn.children[child::SUBSCR_INDEX].expect("subscript without index");
            assert_eq!(ast[index].tag, NodeTag::EConst);
            let num_elems = match strtoui(ast[index].token.text(), u32::MAX) {
                Some(n) if n != 0 => n,
                _ => {
                    snc.error_at_node(&ast[d], format_args!("invalid array size (must be >= 1)\n"));
                    1
                }
            };
            let a = ast.mk_array_type(t, num_elems);
            mk_decl(snc, ast, operand, a)
        }
        NodeTag::EFunc => {
            match ast[t].kind {
                TypeKind::None => impossible(snc, ast, "function returning foreign entity")?,
                TypeKind::EvFlag => {
                    snc.error_at_node(&ast[d], format_args!("function returning event flag\n"))
                }
                _ => {}
            }
            let dn = &ast[d];
            let (expr, args) = (dn.children[child::FUNC_EXPR], dn.children[child::FUNC_ARGS]);
            let params = remove_void_parameter(snc, ast, args);
            let params = add_implicit_parameters(snc, ast, d, params)?;
            let f = ast.mk_function_type(t, params);
            mk_decl(snc, ast, expr, f)
        }
        tag => panic!("mk_decl: declarator {}", tag.name()),
    }
}

/// Multi-variable declaration.
pub fn mk_decls(
    snc: &mut Snc,
    ast: &mut Ast,
    ds: Option<NodeId>,
    t: TypeId,
) -> Result<Option<NodeId>, Exit> {
    let mut r = None;
    let mut d = ds;
    while let Some(id) = d {
        let decl = mk_decl(snc, ast, Some(id), t)?;
        r = ast.link_node(r, Some(decl));
        d = ast[id].next;
    }
    Ok(r)
}

fn gen_pre(
    snc: &mut Snc,
    ast: &Ast,
    t: TypeId,
    prev_assoc: Assoc,
    letter: bool,
    honor_const: bool,
) {
    let sep = if letter { " " } else { "" };
    if honor_const && ast[t].is_const {
        gen_pre(snc, ast, t, Assoc::L, true, false);
        snc.gen_code(format_args!("const{sep}"));
        return;
    }
    match &ast[t].kind {
        TypeKind::None => panic!("gen_type: T_NONE"),
        TypeKind::Pointer { value_type } => {
            gen_pre(snc, ast, *value_type, Assoc::L, true, true);
            snc.gen_code(format_args!("*"));
        }
        TypeKind::Array {
            elem_type: inner, ..
        }
        | TypeKind::Function {
            return_type: inner, ..
        } => {
            gen_pre(
                snc,
                ast,
                *inner,
                Assoc::R,
                letter || prev_assoc == Assoc::L,
                true,
            );
            if prev_assoc == Assoc::L {
                snc.gen_code(format_args!("("));
            }
        }
        TypeKind::EvFlag => snc.gen_code(format_args!("evflag{sep}")),
        TypeKind::Void => snc.gen_code(format_args!("void{sep}")),
        TypeKind::Prim(p) => snc.gen_code(format_args!("{}{sep}", p.name())),
        TypeKind::Foreign { tag, name } => {
            snc.gen_code(format_args!("{}{name}{sep}", tag.prefix()))
        }
        TypeKind::Struct { name, .. } => snc.gen_code(format_args!("struct {name}{sep}")),
    }
}

fn gen_post(snc: &mut Snc, ast: &Ast, t: TypeId, prev_assoc: Assoc, honor_const: bool) {
    if honor_const && ast[t].is_const {
        gen_post(snc, ast, t, Assoc::L, false);
        return;
    }
    match &ast[t].kind {
        TypeKind::Pointer { value_type } => gen_post(snc, ast, *value_type, Assoc::L, true),
        TypeKind::Array {
            num_elems,
            elem_type,
        } => {
            if prev_assoc == Assoc::L {
                snc.gen_code(format_args!(")"));
            }
            // C prints the unsigned count with %d.
            snc.gen_code(format_args!("[{}]", *num_elems as i32));
            gen_post(snc, ast, *elem_type, Assoc::R, true);
        }
        TypeKind::Function {
            return_type,
            param_decls,
        } => {
            if prev_assoc == Assoc::L {
                snc.gen_code(format_args!(")"));
            }
            snc.gen_code(format_args!("("));
            for pd in ast.list(*param_decls) {
                let var = &ast[decl_var(ast, pd)];
                gen_type(snc, ast, var.type_, "", var.name.as_deref());
                if ast[pd].next.is_some() {
                    snc.gen_code(format_args!(", "));
                }
            }
            snc.gen_code(format_args!(")"));
            gen_post(snc, ast, *return_type, Assoc::R, true);
        }
        _ => {}
    }
}

/// Generate code for a type; `name` is an optional variable name.
pub fn gen_type(snc: &mut Snc, ast: &Ast, t: TypeId, prefix: &str, name: Option<&str>) {
    gen_pre(snc, ast, t, Assoc::R, name.is_some(), true);
    if let Some(name) = name {
        snc.gen_code(format_args!("{prefix}{name}"));
    }
    gen_post(snc, ast, t, Assoc::R, true);
}

fn ind(snc: &mut Snc, level: i32) {
    for _ in 0..level {
        snc.report(format_args!("  "));
    }
}

pub fn dump_type(snc: &mut Snc, ast: &Ast, t: TypeId, l: i32) {
    ind(snc, l);
    snc.report(format_args!(
        "dump_type(): tag={}\n",
        ast[t].kind.tag_name()
    ));
    match &ast[t].kind {
        TypeKind::None | TypeKind::EvFlag | TypeKind::Void | TypeKind::Prim(_) => {}
        TypeKind::Foreign { tag, name } => {
            ind(snc, l + 1);
            snc.report(format_args!("foreign.tag={}, name={name}\n", tag.prefix()));
        }
        TypeKind::Pointer { value_type } => dump_type(snc, ast, *value_type, l + 1),
        TypeKind::Array {
            num_elems,
            elem_type,
        } => {
            ind(snc, l + 1);
            snc.report(format_args!("array.num_elems={}", *num_elems as i32));
            dump_type(snc, ast, *elem_type, l + 1);
        }
        TypeKind::Function { return_type, .. } => dump_type(snc, ast, *return_type, l + 1),
        TypeKind::Struct { name, .. } => {
            ind(snc, l + 1);
            snc.report(format_args!("struct.name={name}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Captures generated code for assertions.
    #[derive(Clone, Default)]
    struct Out(Arc<Mutex<Vec<u8>>>);

    impl Write for Out {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn render(ast: &Ast, t: TypeId, name: Option<&str>) -> String {
        let mut snc = Snc::capturing();
        let out = Out::default();
        snc.set_output(Box::new(out.clone()));
        gen_type(&mut snc, ast, t, "", name);
        String::from_utf8(out.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn gen_type_declarator_syntax() {
        let mut ast = Ast::new();
        let int = ast.mk_prim_type(PrimType::Int);
        let arr = ast.mk_array_type(int, 3);
        let ptr_to_arr = ast.mk_pointer_type(arr);
        assert_eq!(render(&ast, ptr_to_arr, Some("x")), "int (*x)[3]");
        let ptr = ast.mk_pointer_type(int);
        let arr_of_ptr = ast.mk_array_type(ptr, 2);
        assert_eq!(render(&ast, arr_of_ptr, Some("y")), "int *y[2]");
        let cptr = ast.mk_pointer_type(int);
        ast.mk_const_type(cptr);
        assert_eq!(render(&ast, cptr, Some("z")), "int *const z");
        assert_eq!(render(&ast, cptr, None), "int *const");
        let big = ast.mk_array_type(int, u32::MAX - 1);
        assert_eq!(render(&ast, big, Some("b")), "int b[-2]");
    }

    #[test]
    fn assignable_and_lengths() {
        let mut ast = Ast::new();
        let d = ast.mk_prim_type(PrimType::Double);
        let a1 = ast.mk_array_type(d, 4);
        let a2 = ast.mk_array_type(a1, 5);
        let a3 = ast.mk_array_type(a2, 6);
        assert!(ast.type_assignable(a2));
        assert!(!ast.type_assignable(a3));
        assert_eq!(ast.type_array_length1(a2), 5);
        assert_eq!(ast.type_array_length2(a2), 4);
        assert_eq!(ast.base_type(a3), d);
        let e = ast.mk_ef_type();
        assert!(!ast.type_assignable(e));
    }
}
