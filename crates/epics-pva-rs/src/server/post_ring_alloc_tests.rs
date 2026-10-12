//! A record monitor's post builds no NT descriptor, no value tree, no
//! metadata and no leaf set: the subscription's [`PostRing`](super::PostRing)
//! is filled in place and its marks were fixed at subscribe, so a scalar
//! value post allocates nothing. Encoding that tree walks it by reference, so
//! it allocates nothing beyond the output buffer either.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;

use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::{
    ControlInfo, DisplayInfo, EnumInfo, PropertySupport, Snapshot,
};
use epics_base_rs::types::EpicsValue;

use super::{LogLeaves, PostRing, marked_update, snapshot_to_pv_field};
use crate::pvdata::PvField;
use crate::server_native::{Marks, MonitorUpdate, PostValue};

thread_local! {
    /// Allocation calls made by this thread. Per thread so concurrently
    /// running tests cannot move each other's count.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counter is a
// const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = ALLOCS.with(Cell::get);
    let r = f();
    (r, ALLOCS.with(Cell::get) - before)
}

const LIMIT: u32 = 4;

fn ai(v: f64) -> Snapshot {
    let mut snap = Snapshot::new(EpicsValue::Double(v), 0, 0, std::time::UNIX_EPOCH);
    snap.properties = PropertySupport::NUMERIC;
    snap.display = Some(DisplayInfo {
        units: "mm".into(),
        description: "an ai".into(),
        upper_disp_limit: 10.0,
        ..Default::default()
    });
    snap.control = Some(ControlInfo::default());
    snap
}

fn bo(v: u16) -> Snapshot {
    let mut snap = Snapshot::new(EpicsValue::Enum(v), 0, 0, std::time::UNIX_EPOCH);
    snap.properties = PropertySupport {
        enum_strs: true,
        ..PropertySupport::NONE
    };
    snap.enums = Some(EnumInfo::new(vec!["Off".into(), "On".into()]));
    snap
}

fn event(mut snap: Snapshot) -> MonitorEvent {
    MonitorEvent::read_log(&mut snap, EventMask::VALUE | EventMask::ALARM)
}

/// The server's queue, as `push_squash_monitor` keeps it: below the limit a
/// post is appended; at the limit the newest replaces the tail, whose value
/// is dropped only after the new post exists.
fn push(queue: &mut VecDeque<MonitorUpdate>, update: MonitorUpdate) {
    if queue.len() >= LIMIT as usize {
        let tail = queue.pop_back();
        queue.push_back(update);
        drop(tail);
    } else {
        queue.push_back(update);
    }
}

/// Bursts longer than the queue, mapped before any is drained — PVASTASK's
/// batched condition, where the queue is full whenever an event is mapped.
/// Every warm post must allocate nothing, share a ring slot, and carry the
/// one leaf set its event class was given at subscribe.
fn assert_posts_fill_in_place(snap: impl Fn(u32) -> Snapshot) {
    let mut ring = PostRing::new(LIMIT, snap(0));
    let mut queue = VecDeque::new();
    let mut first_marks: Option<Marks> = None;
    let mut n = 0;
    for round in 0..8 {
        for _ in 0..3 * LIMIT {
            n += 1;
            let s = snap(n);
            let ev = event(s.clone());
            let (update, posted) = allocs(|| marked_update(&mut ring, ev, 0, || None));
            let update = update.expect("a VALUE post marks leaves");
            let marks = update
                .marked
                .clone()
                .expect("a record post declares its marks");
            assert_eq!(
                *marks,
                crate::nt::event_leaves(
                    EventMask::VALUE | EventMask::ALARM,
                    s.properties,
                    matches!(s.value, EpicsValue::Enum(_)),
                ),
                "post {n}: the shared set is the event class's leaves"
            );
            let first = first_marks.get_or_insert(marks.clone());
            assert!(
                first.same_set(&marks),
                "post {n}: every post of one class shares one leaf set"
            );
            assert!(
                matches!(update.value, PostValue::Shared(_)),
                "post {n}: a full queue still leaves the ring a free slot"
            );
            assert_eq!(
                update.value,
                snapshot_to_pv_field(&s),
                "post {n}: a slot filled in place equals a freshly built tree"
            );
            // Round 0 builds the slots; from then on the ring is warm.
            if round > 0 {
                assert_eq!(posted, 0, "post {n}: a warm post allocates nothing");
            }
            push(&mut queue, update);
        }
        assert!(ring.slots.len() <= LIMIT as usize + 1);
        queue.clear();
    }
}

#[test]
fn an_ai_post_builds_no_descriptor_and_no_tree() {
    assert_posts_fill_in_place(|n| ai(f64::from(n)));
}

#[test]
fn an_enum_post_builds_no_descriptor_and_no_tree() {
    assert_posts_fill_in_place(|n| bo((n % 2) as u16));
}

/// A snapshot whose shape differs — the value type, or the property leaves
/// the channel serves — starts a new tree rather than inheriting leaves the
/// old shape wrote.
#[test]
fn a_new_shape_starts_a_new_tree() {
    let a = ai(1.0);
    let mut ring = PostRing::new(LIMIT, a.clone());
    let _ = ring.post(&LogLeaves::of_snapshot(&a));
    let mut fewer = ai(2.0);
    fewer.properties = PropertySupport::NONE;
    ring.set_meta(fewer.clone());
    let posted = ring.post(&LogLeaves::of_snapshot(&fewer));
    assert_eq!(posted, snapshot_to_pv_field(&fewer));
    let long = Snapshot::new(EpicsValue::Long(3), 0, 0, std::time::UNIX_EPOCH);
    ring.set_meta(long.clone());
    let posted = ring.post(&LogLeaves::of_snapshot(&long));
    assert_eq!(posted, snapshot_to_pv_field(&long));
    assert!(!matches!(&*posted, PvField::Null));
}

/// Metadata is the GET door's, read at subscribe and on `DBE_PROPERTY` only:
/// N value posts read it 0 times and ignore whatever metadata the event
/// carries; one property post reads it once, and every slot takes the new
/// read before its next post.
#[test]
fn only_a_property_post_reads_metadata() {
    let mut ring = PostRing::new(LIMIT, ai(0.0));
    let mut queue = VecDeque::new();
    let mut reads = 0;
    let units = |v: &PvField| {
        let PvField::Structure(root) = v else {
            panic!("an NTScalar root")
        };
        match root.get_field("display") {
            Some(PvField::Structure(d)) => d.get_field("units").cloned(),
            _ => None,
        }
    };
    for n in 0..20 {
        let mut s = ai(f64::from(n));
        s.display.as_mut().expect("display").units = "WRONG".into();
        let update = marked_update(&mut ring, event(s), 0, || {
            reads += 1;
            None
        })
        .expect("a value post");
        assert_eq!(units(&update.value), units(&snapshot_to_pv_field(&ai(0.0))));
        push(&mut queue, update);
    }
    assert_eq!(reads, 0, "a value post reads no metadata");

    let mut egu = ai(0.0);
    egu.display.as_mut().expect("display").units = "um".into();
    let property = MonitorEvent::read_log(&mut ai(0.0), EventMask::PROPERTY);
    let update = marked_update(&mut ring, property, 0, || {
        reads += 1;
        Some(egu.clone())
    })
    .expect("a property post");
    assert_eq!(reads, 1, "a property post reads the metadata once");
    let want = units(&snapshot_to_pv_field(&egu));
    assert_eq!(units(&update.value), want);
    push(&mut queue, update);
    for n in 0..2 * LIMIT {
        let update =
            marked_update(&mut ring, event(ai(f64::from(n))), 0, || None).expect("a value post");
        assert_eq!(units(&update.value), want, "every slot takes the new read");
        push(&mut queue, update);
    }
}

/// An event carrying metadata of its own, as R1.2's `from_snapshot` can
/// still build one, changes no display or control leaf: the post is the
/// ring's read with only the event's value, alarm and timeStamp.
#[test]
fn an_events_own_metadata_changes_no_display_or_control_leaf() {
    let meta = ai(0.0);
    let mut ring = PostRing::new(LIMIT, meta.clone());
    let mut stale = ai(7.0);
    stale.alarm.status = 3;
    stale.alarm.severity = 2;
    stale.user_tag = 5;
    let d = stale.display.as_mut().expect("display");
    d.units = "WRONG".into();
    d.description = "stale".into();
    d.precision = 9;
    d.upper_disp_limit = -1.0;
    d.upper_alarm_limit = -2.0;
    let c = stale.control.as_mut().expect("control");
    c.upper_ctrl_limit = 99.0;
    c.lower_ctrl_limit = -99.0;
    let update = marked_update(&mut ring, event(stale.clone()), 0, || {
        panic!("a value post reads no metadata")
    })
    .expect("a value post");
    let mut want = meta;
    want.value = stale.value;
    want.alarm = stale.alarm;
    want.timestamp = stale.timestamp;
    want.user_tag = stale.user_tag;
    assert_eq!(update.value, snapshot_to_pv_field(&want));
}

/// Encode `v` under `desc` and `bits` into a buffer sized by a first pass:
/// the second pass must allocate nothing and write the same bytes.
fn assert_encodes_by_reference(
    v: &PvField,
    desc: &crate::pvdata::FieldDesc,
    bits: &crate::proto::BitSet,
) {
    use crate::proto::ByteOrder;
    let encode = |out: &mut Vec<u8>| {
        crate::pvdata::encode::encode_pv_field_with_bitset(v, desc, bits, 0, ByteOrder::Little, out)
    };
    let mut first = Vec::new();
    encode(&mut first);
    let mut second = Vec::with_capacity(first.len());
    let ((), n) = allocs(|| encode(&mut second));
    assert_eq!(n, 0, "encoding a tree by reference allocates nothing");
    assert_eq!(first, second, "one tree, one encoding");
}

/// The whole tree (root bit), and a value+alarm+timeStamp delta that walks
/// the structure leaf by leaf.
#[test]
fn an_nt_tree_encodes_by_reference() {
    let snap = ai(1.5);
    let desc = super::NtShape::of(&snap).desc();
    let v = snapshot_to_pv_field(&snap);
    let mut root = crate::proto::BitSet::new();
    root.set(0);
    assert_encodes_by_reference(&v, &desc, &root);
    let mut delta = crate::proto::BitSet::new();
    for path in ["value", "alarm", "timeStamp"] {
        let (start, end) = desc.bit_span_for_path(path).expect("an NT leaf");
        (start..end).for_each(|b| delta.set(b));
    }
    assert_encodes_by_reference(&v, &desc, &delta);
}

/// A structure array's elements encode in place, not as copies.
#[test]
fn a_structure_array_encodes_by_reference() {
    use crate::pvdata::{FieldDesc, PvStructure, ScalarType, ScalarValue};
    let fields = vec![("x".to_string(), FieldDesc::Scalar(ScalarType::Int))];
    let desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![(
            "a".to_string(),
            FieldDesc::StructureArray {
                struct_id: "e".into(),
                fields,
            },
        )],
    };
    let element = |x| {
        let mut e = PvStructure::new("e");
        e.fields
            .push(("x".into(), PvField::Scalar(ScalarValue::Int(x))));
        Some(e)
    };
    let mut root = PvStructure::new("");
    root.fields.push((
        "a".into(),
        PvField::StructureArray(vec![element(1), None, element(2)]),
    ));
    let mut bits = crate::proto::BitSet::new();
    bits.set(0);
    assert_encodes_by_reference(&PvField::Structure(root), &desc, &bits);
}
