//! Parser support routines (C `node.c`).

use crate::snc::types::{Ast, Extra, Node, NodeId, NodeTag, State, StateSet, Token, When};

impl Ast {
    /// C `node()`: a new node with the given children, which must number
    /// `tag.num_children()`.
    pub fn node(&mut self, tag: NodeTag, tok: Token, children: &[Option<NodeId>]) -> NodeId {
        assert_eq!(children.len(), tag.num_children(), "node({})", tag.name());
        let extra = match tag {
            NodeTag::DSs => Extra::Ss(StateSet::default()),
            NodeTag::DState => Extra::State(State::default()),
            NodeTag::DWhen => Extra::When(When::default()),
            _ => Extra::None,
        };
        let id = NodeId(self.nodes.len() as u32);
        self.add_node(Node {
            next: None,
            last: Some(id),
            children: children.to_vec(),
            tag,
            token: tok,
            extra,
        })
    }

    /// C `opt_defn`.
    pub fn opt_defn(&mut self, name: Token, value: &Token) -> NodeId {
        let opt = self.node(NodeTag::DOption, name, &[]);
        self[opt].extra = Extra::Option(value.text().starts_with('+'));
        opt
    }

    /// C `link_node`: append list `ep2` to list `ep1` and return the
    /// combined list. `last` stays correct only for the first element.
    pub fn link_node(&mut self, ep1: Option<NodeId>, ep2: Option<NodeId>) -> Option<NodeId> {
        let (Some(ep1), Some(ep2)) = (ep1, ep2) else {
            return ep1.or(ep2);
        };
        let last1 = self[ep1].last.expect("link_node: ep1 is not a list head");
        self[last1].next = Some(ep2);
        self[ep1].last = self[ep2].last;
        self[ep2].last = None;
        Some(ep1)
    }
}

/// C `strtoui`: the number `strtoul(str, 0, 0)` reads from `s`, if it
/// converts without overflow and is below `limit`.
pub fn strtoui(s: &str, limit: u32) -> Option<u32> {
    let result = strtoul(s)?;
    if result >= u64::from(limit) {
        return None;
    }
    Some(result as u32)
}

/// glibc `strtoul(s, 0, 0)` on LP64; `None` where it sets `ERANGE`.
fn strtoul(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let negative = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let base: u64 = if b.get(i) == Some(&b'0') {
        if matches!(b.get(i + 1), Some(b'x' | b'X'))
            && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            i += 2;
            16
        } else {
            8
        }
    } else {
        10
    };
    let mut value: u64 = 0;
    let mut overflow = false;
    while let Some(d) = b.get(i).and_then(|&c| (c as char).to_digit(36)) {
        let d = u64::from(d);
        if d >= base {
            break;
        }
        match value.checked_mul(base).and_then(|v| v.checked_add(d)) {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if overflow {
        return None;
    }
    Some(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtoui_matches_strtoul_base_0() {
        assert_eq!(strtoui("10", u32::MAX), Some(10));
        assert_eq!(strtoui("0x1fU", u32::MAX), Some(31));
        assert_eq!(strtoui("010", u32::MAX), Some(8));
        assert_eq!(strtoui("08", u32::MAX), Some(0));
        assert_eq!(strtoui("'a'", u32::MAX), Some(0));
        assert_eq!(strtoui("4294967294", u32::MAX), Some(u32::MAX - 1));
        assert_eq!(strtoui("4294967295", u32::MAX), None);
        assert_eq!(strtoui("99999999999999999999", u32::MAX), None);
    }

    #[test]
    fn link_node_keeps_last_on_head() {
        let mut ast = Ast::new();
        let a = ast.node(NodeTag::EVar, Token::zero(), &[]);
        let b = ast.node(NodeTag::EVar, Token::zero(), &[]);
        let c = ast.node(NodeTag::EVar, Token::zero(), &[]);
        let ab = ast.link_node(Some(a), Some(b));
        let abc = ast.link_node(ab, Some(c));
        assert_eq!(abc, Some(a));
        assert_eq!(ast.list(abc).collect::<Vec<_>>(), [a, b, c]);
        assert_eq!(ast[a].last, Some(c));
        assert_eq!(ast[b].last, None);
        assert_eq!(ast.link_node(None, Some(c)), Some(c));
    }
}
