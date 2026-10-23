//! Expression types (C `type_check.c`), a first approximation that code
//! generation uses to tell function calls from calls through foreign
//! objects.

use crate::driver::Snc;
use crate::seq_prim_types::PrimType;
use crate::types::{Ast, Extra, NodeId, NodeTag, Tok, TypeId, child};
use crate::var_types::TypeKind;

/// A type as `type_of` infers it: a type of the syntax tree, one of C's
/// static type constants, or a pointer type it allocates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeOf {
    /// C `num_type`: `int`
    Num,
    /// C `str_type`: `string`
    Str,
    /// C `no_type`: unknown
    No,
    /// a type of the syntax tree
    Type(TypeId),
    /// C `new_pointer_type(t)`
    Pointer(Box<TypeOf>),
}

static NUM_KIND: TypeKind = TypeKind::Prim(PrimType::Int);
static STR_KIND: TypeKind = TypeKind::Prim(PrimType::String);
static NO_KIND: TypeKind = TypeKind::None;

impl TypeOf {
    /// The type's tag and value, `None` for an allocated pointer type.
    fn kind<'a>(&self, ast: &'a Ast) -> Option<&'a TypeKind> {
        match self {
            TypeOf::Num => Some(&NUM_KIND),
            TypeOf::Str => Some(&STR_KIND),
            TypeOf::No => Some(&NO_KIND),
            TypeOf::Type(t) => Some(&ast[*t].kind),
            TypeOf::Pointer(_) => None,
        }
    }

    fn is_pointer(&self, ast: &Ast) -> bool {
        matches!(self, TypeOf::Pointer(_))
            || matches!(self.kind(ast), Some(TypeKind::Pointer { .. }))
    }

    fn pointer_value_type(self, ast: &Ast) -> Option<TypeOf> {
        match self {
            TypeOf::Pointer(t) => Some(*t),
            t => match t.kind(ast) {
                Some(TypeKind::Pointer { value_type }) => Some(TypeOf::Type(*value_type)),
                _ => None,
            },
        }
    }
}

/// C `type_error_at`.
fn type_error_at(snc: &mut Snc, ast: &Ast, e: NodeId) -> TypeOf {
    // C's "type error" lacks the newline and runs into the next message.
    snc.error_at_node(&ast[e], format_args!("type error\n"));
    TypeOf::No
}

fn member_type(ast: &Ast, members: Option<NodeId>, name: &str) -> TypeOf {
    for member in ast.list(members) {
        if ast[member].tag == NodeTag::DDecl && ast[member].token.text() == name {
            let var = ast[member].e_decl().expect("member decl var");
            return TypeOf::Type(ast[var].type_);
        }
    }
    TypeOf::No
}

fn c(ast: &Ast, e: NodeId, i: usize) -> NodeId {
    ast[e].child(i).expect("expression operand")
}

/// Infer the type of an expression. A result of [`TypeOf::No`] means we
/// can't say, either because of a type error, or because we don't have
/// enough information, e.g. due to involvement of foreign types,
/// functions, or variables.
pub fn type_of(snc: &mut Snc, ast: &Ast, e: NodeId) -> TypeOf {
    let node = &ast[e];
    match node.tag {
        NodeTag::EBinop => {
            let l = type_of(snc, ast, c(ast, e, child::BINOP_LEFT));
            let r = type_of(snc, ast, c(ast, e, child::BINOP_RIGHT));
            match node.token.symbol {
                Tok::Sub | Tok::Add => {
                    if l.is_pointer(ast) {
                        l
                    } else {
                        // TODO: what if it's a foreign pointer?
                        TypeOf::Num
                    }
                }
                Tok::Equal
                | Tok::Addeq
                | Tok::Subeq
                | Tok::Andeq
                | Tok::Oreq
                | Tok::Diveq
                | Tok::Muleq
                | Tok::Modeq
                | Tok::Lsheq
                | Tok::Rsheq
                | Tok::Xoreq
                | Tok::Comma => r,
                // arithmetic, comparison, logical, shift and bit operators;
                // C falls through to E_BUILTIN for any other token
                _ => TypeOf::Num,
            }
        }
        // TODO: create built-in functions with their real type
        NodeTag::EBuiltin => TypeOf::Num,
        NodeTag::ECast => {
            let decl = c(ast, e, child::CAST_TYPE);
            let var = ast[decl].e_decl().expect("cast type decl var");
            TypeOf::Type(ast[var].type_)
        }
        NodeTag::EConst => TypeOf::Num,
        NodeTag::EFunc => {
            let t = type_of(snc, ast, c(ast, e, child::FUNC_EXPR));
            match t.kind(ast) {
                Some(TypeKind::Function { return_type, .. }) => TypeOf::Type(*return_type),
                _ => TypeOf::No,
            }
        }
        NodeTag::EParen => type_of(snc, ast, c(ast, e, child::PAREN_EXPR)),
        NodeTag::EPost => type_of(snc, ast, c(ast, e, child::POST_OPERAND)),
        NodeTag::EPre => {
            let t = type_of(snc, ast, c(ast, e, child::PRE_OPERAND));
            match node.token.symbol {
                Tok::Add | Tok::Sub | Tok::Incr | Tok::Decr => t,
                Tok::Asterisk => match t.kind(ast) {
                    Some(TypeKind::Array { elem_type, .. }) => TypeOf::Type(*elem_type),
                    Some(TypeKind::Prim(PrimType::String)) => TypeOf::Num,
                    _ => t.pointer_value_type(ast).unwrap_or(TypeOf::No),
                },
                Tok::Ampersand => TypeOf::Pointer(Box::new(t)),
                Tok::Not | Tok::Tilde | Tok::Sizeof => TypeOf::Num,
                tok => unreachable!("prefix operator {tok:?}"),
            }
        }
        NodeTag::ESelect => {
            let mut t = type_of(snc, ast, c(ast, e, child::SELECT_LEFT));
            match node.token.symbol {
                Tok::Pointer => match t.pointer_value_type(ast) {
                    Some(v) => t = v,
                    None => return TypeOf::No,
                },
                Tok::Period => {}
                tok => unreachable!("member selection {tok:?}"),
            }
            match t.kind(ast) {
                Some(TypeKind::Struct { member_decls, .. }) => {
                    let member = c(ast, e, child::SELECT_RIGHT);
                    member_type(ast, *member_decls, ast[member].token.text())
                }
                _ => TypeOf::No,
            }
        }
        // just a first approximation, of course
        NodeTag::EString => TypeOf::Str,
        NodeTag::ESubscr => {
            let operand = c(ast, e, child::SUBSCR_OPERAND);
            let t = type_of(snc, ast, operand);
            match t.kind(ast) {
                Some(TypeKind::Array { elem_type, .. }) => TypeOf::Type(*elem_type),
                _ => match t.pointer_value_type(ast) {
                    Some(v) => v,
                    None => type_error_at(snc, ast, operand),
                },
            }
        }
        NodeTag::ETernop => type_of(snc, ast, c(ast, e, child::TERNOP_THEN)),
        NodeTag::EVar => match node.extra {
            Extra::Var(vp) => TypeOf::Type(ast[vp].type_),
            ref extra => unreachable!("unconnected E_VAR with {extra:?}"),
        },
        // E_INIT and E_MEMBER only occur where no expression is typed
        tag => unreachable!("type_of({})", tag.name()),
    }
}

/// Whether the given expression has type function or pointer to function.
pub fn type_is_function(snc: &mut Snc, ast: &Ast, e: NodeId) -> bool {
    let t = type_of(snc, ast, e);
    if matches!(t.kind(ast), Some(TypeKind::Function { .. })) {
        return true;
    }
    t.pointer_value_type(ast)
        .is_some_and(|v| matches!(v.kind(ast), Some(TypeKind::Function { .. })))
}
