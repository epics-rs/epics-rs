//! The ONE monitor squash-the-tail rule, written once for both ends of
//! the wire.
//!
//! pvxs bounds a monitor backlog at `queueSize` and, on overflow, merges
//! the newest post INTO the queue tail rather than appending or dropping:
//!
//! * client — `clientmon.cpp:683-699`
//!   `mon->queue.back().val.assign(update.val); mon->nCliSquash++;`
//! * server — `servermon.cpp:270-287`
//!   `mon->queue.back().assign(val); mon->nSquash++;`
//!
//! Both are `Value::assign`, which copies the *marked* fields of the newer
//! value into the older one and marks them there — so the surviving entry
//! carries the UNION of the two changed sets, with the newer value winning
//! on any leaf both touched. Dropping the older entry instead (a last-wins
//! squash) silently loses every leaf the older post changed and the newer
//! one did not: the peer's cached `Value` never sees that transition and
//! never will, because the source posts only deltas.
//!
//! pvxs can write `assign` in one line because its queue holds decoded
//! `Value`s. Both queues here hold RAW `changed | value | overrun` bodies —
//! the client so a raw-forwarding gateway never decodes in steady state,
//! the server so a fan-out is N refcount bumps rather than N re-encodes. So
//! the merge is done here on the wire form, and only on overflow: decode
//! both bodies, overlay, re-encode. Steady state stays decode-free.

use std::io::Cursor;

use crate::proto::{BitSet, ByteOrder};
use crate::pvdata::FieldDesc;
use crate::pvdata::encode::{
    TypeCache, decode_pv_field_with_bitset, decode_pv_field_with_bitset_into,
    encode_pv_field_with_bitset,
};

/// One MONITOR DATA body — `changed | value | overrun` — plus the byte
/// order it was encoded in. This is the payload of a MONITOR data frame
/// *after* its `ioid`/`subcmd` prefix, which is what both queues store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MonitorBody<'a> {
    pub bytes: &'a [u8],
    pub order: ByteOrder,
}

/// Expand a wire changed-bitset to its leaf-only form.
///
/// A set STRUCTURE bit is pvData's parent compression for "this whole
/// subtree changed" — both [`encode_pv_field_with_bitset`] and pvxs's
/// `from_wire_valid` read it that way — and bit 0 (the root) means the
/// whole value. Expanding before the union is what makes the merge total:
/// a parent bit on one side and a child bit on the other must not produce
/// a bitset where the parent silently swallows the sibling leaves.
///
/// The result is the leaf-only canonical form
/// ([`crate::pvdata::encode::canonical_changed_bitset`] produces), which is
/// what pvxs's own `to_wire_valid` emits: `Value::mark` cannot set a
/// structure bit (`data.cpp:256-270`).
fn leaf_bits(desc: &FieldDesc, src: &BitSet, bit: usize, whole: bool, out: &mut BitSet) {
    let whole = whole || src.get(bit);
    match desc {
        FieldDesc::Structure { fields, .. } => {
            let mut child = bit + 1;
            for (_, f) in fields {
                leaf_bits(f, src, child, whole, out);
                child += f.total_bits();
            }
        }
        _ => {
            if whole {
                out.set(bit);
            }
        }
    }
}

/// Union of two bitsets, in place into `a`.
fn union_into(a: &mut BitSet, b: &BitSet) {
    for i in b.iter() {
        a.set(i);
    }
}

/// pvxs `Value::assign` on the MONITOR DATA wire form: merge `newer` into
/// `older` and return the body the surviving queue entry must carry.
///
/// * **value** — `older` decoded, then `newer` decoded *into* it
///   ([`decode_pv_field_with_bitset_into`] overwrites exactly the fields
///   `newer` marked), so a leaf both posts changed takes the newer value
///   and a leaf only `older` changed survives. That is `assign`.
/// * **changed** — the union of the two leaf-expanded sets, which is the
///   marked set `Value::assign` leaves behind.
/// * **overrun** — the union of the two, and NOTHING else. The overrun
///   bitset means "an upstream server squashed", and the port must never
///   fabricate it: pvxs's server writes a hard-empty overrun on every data
///   frame (`servermon.cpp:174-176`) and a pvxs client that sees bits sets
///   `servSquash` / bumps `nSrvSquash` (`clientmon.cpp:554-564`). Only bits
///   an upstream frame already carried go on the wire, so a squash carries
///   both inputs' bits forward and adds none.
///
/// Returns `Err` when either body is malformed under its own byte order.
/// The caller decides the policy — neither queue may silently synthesise a
/// well-formed frame out of a corrupt one.
pub(crate) fn squash_monitor_bodies(
    intro: &FieldDesc,
    older: MonitorBody<'_>,
    newer: MonitorBody<'_>,
    out_order: ByteOrder,
) -> Result<Vec<u8>, String> {
    let mut cur_a = Cursor::new(older.bytes);
    let changed_a = BitSet::decode(&mut cur_a, older.order)
        .map_err(|e| format!("older changed bitset: {e}"))?;
    let mut value = decode_pv_field_with_bitset(intro, &changed_a, 0, &mut cur_a, older.order)
        .map_err(|e| format!("older value: {e}"))?;
    let overrun_a = BitSet::decode(&mut cur_a, older.order)
        .map_err(|e| format!("older overrun bitset: {e}"))?;

    let mut cur_b = Cursor::new(newer.bytes);
    let changed_b = BitSet::decode(&mut cur_b, newer.order)
        .map_err(|e| format!("newer changed bitset: {e}"))?;
    let mut cache = TypeCache::new();
    decode_pv_field_with_bitset_into(
        &mut value,
        intro,
        &changed_b,
        0,
        &mut cur_b,
        newer.order,
        &mut cache,
    )
    .map_err(|e| format!("newer value: {e}"))?;
    let overrun_b = BitSet::decode(&mut cur_b, newer.order)
        .map_err(|e| format!("newer overrun bitset: {e}"))?;

    let mut changed = BitSet::new();
    leaf_bits(intro, &changed_a, 0, false, &mut changed);
    let mut leaves_b = BitSet::new();
    leaf_bits(intro, &changed_b, 0, false, &mut leaves_b);
    union_into(&mut changed, &leaves_b);

    let mut overrun = overrun_a;
    union_into(&mut overrun, &overrun_b);

    let mut out = Vec::with_capacity(older.bytes.len().max(newer.bytes.len()));
    changed.write_into(out_order, &mut out);
    encode_pv_field_with_bitset(&value, intro, &changed, 0, out_order, &mut out);
    overrun.write_into(out_order, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::encode::marked_changed_bitset;
    use crate::pvdata::{PvField, PvStructure, ScalarType, ScalarValue};

    fn intro() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "test:nt/Triple:1.0".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("c".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        }
    }

    fn value(a: i32, b: i32, c: i32) -> PvField {
        let mut s = PvStructure::new("test:nt/Triple:1.0");
        for (n, v) in [("a", a), ("b", b), ("c", c)] {
            s.fields
                .push((n.into(), PvField::Scalar(ScalarValue::Int(v))));
        }
        PvField::Structure(s)
    }

    /// `changed | value | overrun` for the named leaves of `(a, b, c)`.
    fn body(marked: &[&str], v: (i32, i32, i32), overrun: &[&str], order: ByteOrder) -> Vec<u8> {
        let d = intro();
        let paths: Vec<String> = marked.iter().map(|s| (*s).to_string()).collect();
        let changed = marked_changed_bitset(&d, &paths);
        let over: Vec<String> = overrun.iter().map(|s| (*s).to_string()).collect();
        let mut out = Vec::new();
        changed.write_into(order, &mut out);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &value(v.0, v.1, v.2),
            &d,
            &changed,
            0,
            order,
            &mut out,
        );
        marked_changed_bitset(&d, &over).write_into(order, &mut out);
        out
    }

    /// Read a merged body back as `(changed leaf paths, a, b, c, overrun
    /// leaf paths)`.
    #[allow(clippy::type_complexity)]
    fn read(bytes: &[u8], order: ByteOrder) -> (Vec<String>, i32, i32, i32, Vec<String>) {
        let d = intro();
        let mut cur = Cursor::new(bytes);
        let changed = BitSet::decode(&mut cur, order).expect("changed");
        let v = decode_pv_field_with_bitset(&d, &changed, 0, &mut cur, order).expect("value");
        let overrun = BitSet::decode(&mut cur, order).expect("overrun");
        assert_eq!(
            cur.position() as usize,
            bytes.len(),
            "merged body must have no trailing bytes"
        );
        let get = |n: &str| match &v {
            PvField::Structure(s) => s
                .fields
                .iter()
                .find(|(k, _)| k == n)
                .and_then(|(_, f)| match f {
                    PvField::Scalar(ScalarValue::Int(i)) => Some(*i),
                    _ => None,
                })
                .expect("leaf"),
            _ => panic!("not a structure"),
        };
        (
            crate::pvdata::encode::changed_bitset_paths(&d, &changed),
            get("a"),
            get("b"),
            get("c"),
            crate::pvdata::encode::changed_bitset_paths(&d, &overrun),
        )
    }

    /// pvxs `Value::assign` keeps the marks the OLDER post carried. A
    /// last-wins squash loses `a` here — the downstream peer never learns
    /// `a` moved to 1, because the source posts deltas and will not repeat
    /// it.
    #[test]
    fn squash_keeps_a_leaf_only_the_older_post_changed() {
        let o = ByteOrder::Little;
        let older = body(&["a"], (1, 0, 0), &[], o);
        let newer = body(&["b"], (0, 2, 0), &[], o);
        let out = squash_monitor_bodies(
            &intro(),
            MonitorBody {
                bytes: &older,
                order: o,
            },
            MonitorBody {
                bytes: &newer,
                order: o,
            },
            o,
        )
        .expect("merge");
        let (changed, a, b, _c, _) = read(&out, o);
        assert_eq!(changed, vec!["a".to_string(), "b".to_string()]);
        assert_eq!((a, b), (1, 2));
    }

    /// A leaf both posts changed takes the NEWER value — `assign` copies
    /// the newer marked fields over the older ones.
    #[test]
    fn squash_takes_the_newer_value_on_a_shared_leaf() {
        let o = ByteOrder::Little;
        let older = body(&["a", "b"], (1, 1, 0), &[], o);
        let newer = body(&["b", "c"], (0, 9, 9), &[], o);
        let out = squash_monitor_bodies(
            &intro(),
            MonitorBody {
                bytes: &older,
                order: o,
            },
            MonitorBody {
                bytes: &newer,
                order: o,
            },
            o,
        )
        .expect("merge");
        let (changed, a, b, c, _) = read(&out, o);
        assert_eq!(
            changed,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!((a, b, c), (1, 9, 9));
    }

    /// The overrun bitset means "an UPSTREAM server squashed". A squash
    /// carries both inputs' bits forward and fabricates none — `b` changed
    /// in both posts here and must NOT appear in the merged overrun, or a
    /// pvxs client would bump `nSrvSquash` against a server that never
    /// reported one (`clientmon.cpp:554-564`).
    #[test]
    fn squash_unions_overrun_and_invents_none() {
        let o = ByteOrder::Little;
        let older = body(&["a", "b"], (1, 1, 0), &["a"], o);
        let newer = body(&["b"], (0, 2, 0), &["c"], o);
        let out = squash_monitor_bodies(
            &intro(),
            MonitorBody {
                bytes: &older,
                order: o,
            },
            MonitorBody {
                bytes: &newer,
                order: o,
            },
            o,
        )
        .expect("merge");
        let (_, _, b, _, overrun) = read(&out, o);
        assert_eq!(b, 2);
        assert_eq!(overrun, vec!["a".to_string(), "c".to_string()]);
    }

    /// A parent-compressed bitset (the whole structure changed) expands to
    /// its leaves before the union, so a sibling leaf marked only by the
    /// other side survives instead of being swallowed.
    #[test]
    fn squash_expands_a_parent_compressed_changed_bitset() {
        let o = ByteOrder::Little;
        let d = intro();
        // Hand-build "root bit set" — every leaf changed.
        let mut whole = BitSet::new();
        whole.set(0);
        let mut older = Vec::new();
        whole.write_into(o, &mut older);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &value(1, 1, 1),
            &d,
            &whole,
            0,
            o,
            &mut older,
        );
        BitSet::new().write_into(o, &mut older);

        let newer = body(&["b"], (0, 7, 0), &[], o);
        let out = squash_monitor_bodies(
            &d,
            MonitorBody {
                bytes: &older,
                order: o,
            },
            MonitorBody {
                bytes: &newer,
                order: o,
            },
            o,
        )
        .expect("merge");
        let (changed, a, b, c, _) = read(&out, o);
        assert_eq!(
            changed,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!((a, b, c), (1, 7, 1));
    }

    /// Each body is decoded under its OWN order (a server may re-negotiate
    /// endianness mid-stream) and the result is written in `out_order`.
    #[test]
    fn squash_merges_across_byte_orders() {
        let older = body(&["a"], (1, 0, 0), &[], ByteOrder::Big);
        let newer = body(&["c"], (0, 0, 3), &[], ByteOrder::Little);
        let out = squash_monitor_bodies(
            &intro(),
            MonitorBody {
                bytes: &older,
                order: ByteOrder::Big,
            },
            MonitorBody {
                bytes: &newer,
                order: ByteOrder::Little,
            },
            ByteOrder::Little,
        )
        .expect("merge");
        let (changed, a, _b, c, _) = read(&out, ByteOrder::Little);
        assert_eq!(changed, vec!["a".to_string(), "c".to_string()]);
        assert_eq!((a, c), (1, 3));
    }

    /// A truncated body is an error, never a fabricated frame — the caller
    /// owns the malformed-raw policy.
    #[test]
    fn squash_rejects_a_truncated_body() {
        let o = ByteOrder::Little;
        let older = body(&["a"], (1, 0, 0), &[], o);
        let newer = body(&["b"], (0, 2, 0), &[], o);
        assert!(
            squash_monitor_bodies(
                &intro(),
                MonitorBody {
                    bytes: &older,
                    order: o
                },
                MonitorBody {
                    bytes: &newer[..1],
                    order: o
                },
                o,
            )
            .is_err()
        );
    }
}
