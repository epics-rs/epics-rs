//! The client's bounded monitor backlog — pvxs `MonitorOp`'s
//! `std::deque<Entry> queue` (`clientmon.cpp:69`) and its tail squash.
//!
//! Every other IOID slot is a *reply* slot: an op sends one request and
//! the server answers with a bounded number of frames. A MONITOR is the
//! one slot the server drives, so it is the one slot that needs a bound.
//! pvxs holds at most `queueSize` updates (builder default 4) and, when a
//! consumer falls behind, merges the newest INTO the tail:
//!
//! ```text
//! if(update.val && mon->queue.size() >= mon->queueSize
//!    && mon->queue.back().val && !mon->pipeline) {
//!     mon->queue.back().val.assign(update.val);
//!     mon->nCliSquash++;
//! } else if(update.exc || update.val) {
//!     mon->queue.emplace_back(std::move(update));
//! }                                        // clientmon.cpp:683-699
//! ```
//!
//! Three properties of that rule are load-bearing and are reproduced here:
//!
//! 1. the bound is on VALUE updates — the `Finished` marker is
//!    `emplace_back`'d unconditionally (`:701-707`), so a terminal can
//!    never be squashed away by a later post;
//! 2. `pipeline` disables the squash (`:686`): a credit window already
//!    stops the server outrunning the consumer, and squashing under it
//!    would drop updates the client has ACK'd room for;
//! 3. the squash is `Value::assign`, a MERGE — see
//!    [`crate::pvdata::monitor_squash`] for why last-wins is not it.
//!
//! The queue holds RAW frames so the raw-forward path (a gateway) never
//! decodes in steady state; the merge decodes only on overflow.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::decode::Frame;
use crate::proto::PvaHeader;
use crate::pvdata::FieldDesc;
use crate::pvdata::monitor_squash::{MonitorBody, squash_monitor_bodies};

/// Offset of the MONITOR body (`changed | value | overrun`) inside an
/// application frame payload: `ioid` (4) + `subcmd` (1).
const BODY_AT: usize = 5;

/// Is this a MONITOR **DATA** frame — the only squashable kind?
///
/// Subcmd `0x00`. INIT (`0x08`), FINISH (`0x10`) and any subcmd a server
/// must not send are control frames: they are appended verbatim so the
/// drain loop sees end-of-stream and protocol faults exactly as it would
/// on an unbounded channel.
fn is_data(frame: &Frame) -> bool {
    frame.payload.len() > BODY_AT && frame.payload[4] == 0x00
}

struct Inner {
    q: VecDeque<Frame>,
    /// pvxs `MonitorOp::queueSize` (`clientmon.cpp:52`, set from
    /// `record._options.queueSize` at `:763-766`).
    limit: usize,
    /// pvxs `MonitorOp::pipeline`. See rule 2 in the module docs.
    pipeline: bool,
    /// Introspection from the MONITOR INIT reply, installed by the drain
    /// loop ([`MonitorBacklog::arm`]) before it sends START.
    ///
    /// A DATA frame cannot legally arrive before START, and a server that
    /// sends one anyway is rejected by the loop's INIT-expected check
    /// (`decode_op_response` → `td.invalid`), which ends the subscription
    /// on that first frame. So "unarmed" is not a window in which the
    /// queue can grow: it holds the INIT reply and nothing else.
    intro: Option<Arc<FieldDesc>>,
    /// pvxs `nCliSquash` (`clientmon.cpp:73`).
    n_squash: u64,
    /// pvxs `queueMax` (`clientmon.cpp:74`, `:715-716`).
    max_queue: usize,
    closed: bool,
}

impl Inner {
    fn push(&mut self, frame: Frame) {
        if !is_data(&frame) || self.pipeline || self.q.len() < self.limit {
            self.q.push_back(frame);
            return;
        }
        // Full. pvxs squashes onto `queue.back()` only when that entry
        // holds a value (`mon->queue.back().val`); a control tail is not
        // assignable, and neither is a body we cannot decode. Every such
        // case appends, which is the pre-bound behaviour for that one
        // frame — the queue re-converges on the next DATA whose tail is
        // mergeable.
        let mergeable = self
            .intro
            .clone()
            .filter(|_| self.q.back().is_some_and(is_data));
        let Some(intro) = mergeable else {
            self.q.push_back(frame);
            return;
        };
        let tail = self.q.back().expect("filtered on `back().is_some_and`");
        let merged = squash_monitor_bodies(
            &intro,
            MonitorBody {
                bytes: &tail.payload[BODY_AT..],
                order: tail.order(),
            },
            MonitorBody {
                bytes: &frame.payload[BODY_AT..],
                order: frame.order(),
            },
            frame.order(),
        );
        match merged {
            Ok(body) => {
                let mut payload = Vec::with_capacity(BODY_AT + body.len());
                payload.extend_from_slice(&frame.payload[..BODY_AT]);
                payload.extend_from_slice(&body);
                let header = PvaHeader::application(
                    true,
                    frame.order(),
                    frame.header.command,
                    payload.len() as u32,
                );
                self.q.pop_back();
                self.q.push_back(Frame { header, payload });
                self.n_squash += 1;
            }
            Err(reason) => {
                // A malformed body cannot be merged. Deliver it: the drain
                // loop's own decode faults it and resets the circuit, which
                // is what pvxs does with a monitor message that is not good
                // (`clientmon.cpp:596`). Swallowing it here would hide an
                // upstream wire fault behind a squash.
                tracing::debug!(%reason, "monitor squash skipped — malformed body");
                self.q.push_back(frame);
            }
        }
    }
}

/// Bounded, tail-squashing backlog for one MONITOR IOID.
pub(crate) struct MonitorBacklog {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl MonitorBacklog {
    fn new(limit: usize, pipeline: bool) -> Self {
        Self {
            inner: Mutex::new(Inner {
                q: VecDeque::new(),
                limit: limit.max(1),
                pipeline,
                intro: None,
                n_squash: 0,
                max_queue: 0,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    /// Install the INIT reply's introspection. Until this runs the queue
    /// cannot merge (see [`Inner::intro`]).
    pub(crate) fn arm(&self, intro: Arc<FieldDesc>) {
        self.inner.lock().intro = Some(intro);
    }

    /// Producer side, called from the connection reader task.
    fn push(&self, frame: Frame) {
        {
            let mut g = self.inner.lock();
            if g.closed {
                return;
            }
            g.push(frame);
            let depth = g.q.len();
            if depth > g.max_queue {
                g.max_queue = depth;
            }
        }
        self.notify.notify_one();
    }

    /// End of stream — the consumer's next [`Self::recv`] returns `None`.
    fn close(&self) {
        self.inner.lock().closed = true;
        self.notify.notify_one();
    }

    /// Pop the oldest frame, waiting for one. `None` once the backlog is
    /// closed AND drained, matching an `mpsc::UnboundedReceiver` whose
    /// senders have dropped.
    ///
    /// Cancel-safe: the waiter is registered before the queue is
    /// inspected, so a frame pushed in between cannot be missed, and
    /// `Notify::notify_one` stores a permit when no waiter is parked.
    pub(crate) async fn recv(&self) -> Option<Frame> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let mut g = self.inner.lock();
                if let Some(f) = g.q.pop_front() {
                    return Some(f);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// `(n_cli_squash, max_queue, n_queue)` — pvxs `nCliSquash`,
    /// `queueMax` and `nQueue`.
    pub(crate) fn counters(&self) -> (u64, u32, u32) {
        let g = self.inner.lock();
        (g.n_squash, g.max_queue as u32, g.q.len() as u32)
    }
}

/// Producer handle held by the IOID routing slot. Dropping it closes the
/// backlog, so a slot removal — `unregister_ioid`, the channel-destroy
/// sweep, or the reader task's `by_ioid.clear()` on connection death —
/// ends the consumer exactly as dropping an `mpsc::UnboundedSender` did.
pub(crate) struct MonitorSink(Arc<MonitorBacklog>);

impl MonitorSink {
    pub(crate) fn new(limit: usize, pipeline: bool) -> (Self, Arc<MonitorBacklog>) {
        let q = Arc::new(MonitorBacklog::new(limit, pipeline));
        (Self(q.clone()), q)
    }

    pub(crate) fn push(&self, frame: Frame) {
        self.0.push(frame);
    }
}

impl Drop for MonitorSink {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{BitSet, ByteOrder, Command, WriteExt};
    use crate::pvdata::encode::{
        changed_bitset_paths, decode_pv_field_with_bitset, encode_pv_field_with_bitset,
        marked_changed_bitset,
    };
    use crate::pvdata::{PvField, PvStructure, ScalarType, ScalarValue};

    const IOID: u32 = 77;
    const ORDER: ByteOrder = ByteOrder::Little;

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

    fn frame(subcmd: u8, body: Vec<u8>) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(IOID, ORDER);
        payload.put_u8(subcmd);
        payload.extend_from_slice(&body);
        let header =
            PvaHeader::application(true, ORDER, Command::Monitor.code(), payload.len() as u32);
        Frame { header, payload }
    }

    /// A MONITOR DATA frame marking `marked` with values `(a, b, c)`.
    fn data(marked: &[&str], v: (i32, i32, i32)) -> Frame {
        let d = intro();
        let paths: Vec<String> = marked.iter().map(|s| (*s).to_string()).collect();
        let changed = marked_changed_bitset(&d, &paths);
        let mut body = Vec::new();
        changed.write_into(ORDER, &mut body);
        encode_pv_field_with_bitset(&value(v.0, v.1, v.2), &d, &changed, 0, ORDER, &mut body);
        BitSet::new().write_into(ORDER, &mut body);
        frame(0x00, body)
    }

    fn read(f: &Frame) -> (Vec<String>, i32, i32, i32) {
        let d = intro();
        let mut cur = std::io::Cursor::new(&f.payload[BODY_AT..]);
        let changed = BitSet::decode(&mut cur, f.order()).expect("changed");
        let v = decode_pv_field_with_bitset(&d, &changed, 0, &mut cur, f.order()).expect("value");
        let get = |n: &str| match &v {
            PvField::Structure(s) => s
                .fields
                .iter()
                .find(|(k, _)| k == n)
                .and_then(|(_, x)| match x {
                    PvField::Scalar(ScalarValue::Int(i)) => Some(*i),
                    _ => None,
                })
                .expect("leaf"),
            _ => panic!("not a structure"),
        };
        (
            changed_bitset_paths(&d, &changed),
            get("a"),
            get("b"),
            get("c"),
        )
    }

    fn armed(limit: usize, pipeline: bool) -> (MonitorSink, Arc<MonitorBacklog>) {
        let (sink, q) = MonitorSink::new(limit, pipeline);
        q.arm(Arc::new(intro()));
        (sink, q)
    }

    /// PVX-45: the backlog stops at `queueSize` however far the consumer
    /// lags, and every overflow is a MERGE, not a drop. Pre-fix this was an
    /// unbounded `mpsc` that grew one entry per DATA frame.
    #[test]
    fn backlog_bounds_at_queue_size_and_squashes_the_tail() {
        let (sink, q) = armed(4, false);
        for i in 1..=20 {
            sink.push(data(&["a"], (i, 0, 0)));
        }
        let (n_squash, max_queue, n_queue) = q.counters();
        assert_eq!(n_queue, 4, "the backlog must stop at queueSize");
        assert_eq!(max_queue, 4, "and must never have been deeper");
        assert_eq!(n_squash, 16, "every frame past the bound is one squash");
    }

    /// The squash is `Value::assign`: the surviving tail carries the union
    /// of the merged frames' changed leaves. A last-wins squash would drop
    /// `a` and `b` here.
    #[test]
    fn the_squashed_tail_carries_the_union_of_the_merged_leaves() {
        let (sink, q) = armed(1, false);
        sink.push(data(&["a"], (1, 0, 0)));
        sink.push(data(&["b"], (0, 2, 0)));
        sink.push(data(&["c"], (0, 0, 3)));
        assert_eq!(q.counters(), (2, 1, 1));
        let f = q.inner.lock().q.pop_front().expect("one entry");
        let (changed, a, b, c) = read(&f);
        assert_eq!(
            changed,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!((a, b, c), (1, 2, 3));
    }

    /// pvxs `emplace_back`s the terminal unconditionally
    /// (`clientmon.cpp:701-707`). A FINISH must never be squashed away by
    /// a later post, and a DATA must never be squashed ONTO a FINISH.
    #[test]
    fn a_control_frame_is_appended_past_the_bound_and_never_squashed() {
        let (sink, q) = armed(2, false);
        sink.push(data(&["a"], (1, 0, 0)));
        sink.push(data(&["a"], (2, 0, 0)));
        sink.push(frame(0x10, Vec::new())); // FINISH, queue already full
        sink.push(data(&["a"], (3, 0, 0))); // tail is the FINISH
        let g = q.inner.lock();
        assert_eq!(g.q.len(), 4, "the FINISH and the post-FINISH DATA append");
        assert_eq!(g.q[2].payload[4], 0x10, "the FINISH survives in place");
        assert_eq!(g.n_squash, 0);
    }

    /// pvxs disables the squash under `pipeline` (`clientmon.cpp:686`) —
    /// the credit window is the bound there, and squashing would drop
    /// updates the client has already ACK'd room for.
    #[test]
    fn pipeline_disables_the_squash() {
        let (sink, q) = armed(2, true);
        for i in 1..=6 {
            sink.push(data(&["a"], (i, 0, 0)));
        }
        let (n_squash, _, n_queue) = q.counters();
        assert_eq!((n_squash, n_queue), (0, 6));
    }

    /// Dropping the routing slot ends the consumer, exactly as dropping an
    /// `mpsc::UnboundedSender` did — `unregister_ioid`, the channel-destroy
    /// sweep and the reader's `by_ioid.clear()` all rely on it. Queued
    /// frames drain first.
    #[epics_macros_rs::epics_test]
    async fn dropping_the_sink_drains_then_ends_recv() {
        let (sink, q) = armed(4, false);
        sink.push(data(&["a"], (1, 0, 0)));
        drop(sink);
        assert!(q.recv().await.is_some(), "queued frames drain first");
        assert!(q.recv().await.is_none(), "then the stream ends");
    }

    /// An unarmed backlog cannot merge, so it appends. Reachable only
    /// through a server that sends DATA before the INIT reply, which the
    /// drain loop rejects on that first frame.
    #[test]
    fn an_unarmed_backlog_appends_rather_than_merging() {
        let (sink, q) = MonitorSink::new(1, false);
        sink.push(data(&["a"], (1, 0, 0)));
        sink.push(data(&["b"], (0, 2, 0)));
        let (n_squash, _, n_queue) = q.counters();
        assert_eq!((n_squash, n_queue), (0, 2));
    }
}
