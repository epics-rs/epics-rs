# Apply the FramePool lend/return change to the E8 rig tree by ANCHOR, not by
# patch: that tree is at 43ff13c7 without 871b2de6 and carries other rounds'
# uncommitted probe edits, so a `git apply` would either fail or take those
# edits with it.  Every anchor below is text present in BOTH versions.
#
# Backs up each file it touches to <file>.bak-poolprobe first; restore with
# `python3 rigpool.py --restore`.
import re
import shutil
import sys

WT = "/home/coding-agent/vx-rig-e8/wt/crates/epics-ca-rs/src/server/"
FILES = ["frame.rs", "outbox.rs", "monitor.rs", "tcp.rs", "blocking.rs"]

if "--restore" in sys.argv:
    for f in FILES:
        try:
            shutil.copy(WT + f + ".bak-poolprobe", WT + f)
            print("restored", f)
        except FileNotFoundError:
            print("no backup for", f)
    sys.exit(0)

for f in FILES:
    shutil.copy(WT + f, WT + f + ".bak-poolprobe")

POOL_TYPES = '''
/// One connection's reusable send buffer.
pub(crate) struct FramePool {
    slot: Mutex<Option<Vec<u8>>>,
}

impl FramePool {
    pub(crate) fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    fn take(&self) -> Option<Vec<u8>> {
        self.slot.try_lock().ok().and_then(|mut slot| slot.take())
    }

    fn put(&self, mut buf: Vec<u8>) {
        buf.clear();
        if let Ok(mut slot) = self.slot.try_lock() {
            let keep = match slot.take() {
                Some(resident) if resident.capacity() >= buf.capacity() => resident,
                _ => buf,
            };
            *slot = Some(keep);
        }
    }
}

/// A finished frame that returns its allocation to the pool when the drain
/// owner drops it.
pub(crate) struct PooledFrame {
    buf: Vec<u8>,
    home: Option<Arc<FramePool>>,
}

impl std::fmt::Debug for PooledFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledFrame")
            .field("len", &self.buf.len())
            .field("pooled", &self.home.is_some())
            .finish()
    }
}

impl std::ops::Deref for PooledFrame {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl From<Vec<u8>> for PooledFrame {
    fn from(buf: Vec<u8>) -> Self {
        Self { buf, home: None }
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        if let Some(home) = self.home.take() {
            home.put(std::mem::take(&mut self.buf));
        }
    }
}

'''


def edit(path, pairs, allow_missing=()):
    s = open(path).read()
    for old, new in pairs:
        n = s.count(old)
        if n == 0 and old in allow_missing:
            print("  skip (absent): %r" % old[:50])
            continue
        assert n == 1, "%s: %d matches for %r" % (path, n, old[:70])
        s = s.replace(old, new)
    open(path, "w").write(s)


print("== frame.rs")
edit(WT + "frame.rs", [
    ("use crate::protocol::{CaHeader, align8};",
     "use crate::protocol::{CaHeader, align8};\nuse std::sync::{Arc, Mutex};"),
    ("/// A CA frame under construction:",
     POOL_TYPES.lstrip("\n") + "/// A CA frame under construction:"),
    ("pub(crate) struct FrameBuf {\n    buf: Vec<u8>,\n}",
     "pub(crate) struct FrameBuf {\n    buf: Vec<u8>,\n    home: Option<Arc<FramePool>>,\n}"),
    ("    pub(crate) fn new(payload_hint: usize) -> Self {\n"
     "        let mut buf = Vec::with_capacity(HDR_RESERVE + payload_hint);\n"
     "        buf.resize(HDR_RESERVE, 0);\n"
     "        Self { buf }\n"
     "    }",
     "    pub(crate) fn acquire(pool: &Arc<FramePool>, payload_hint: usize) -> Self {\n"
     "        let mut buf = pool\n"
     "            .take()\n"
     "            .unwrap_or_else(|| Vec::with_capacity(HDR_RESERVE + payload_hint));\n"
     "        buf.clear();\n"
     "        buf.resize(HDR_RESERVE, 0);\n"
     "        Self {\n"
     "            buf,\n"
     "            home: Some(pool.clone()),\n"
     "        }\n"
     "    }"),
    ("    pub(crate) fn seal(mut self, hdr: &CaHeader) -> Vec<u8> {",
     "    pub(crate) fn seal(mut self, hdr: &CaHeader) -> PooledFrame {"),
    ("        if start > 0 {\n            self.buf.drain(..start);\n        }\n        self.buf\n    }",
     "        if start > 0 {\n            self.buf.drain(..start);\n        }\n"
     "        PooledFrame {\n            buf: self.buf,\n            home: self.home,\n        }\n    }"),
])

print("== outbox.rs")
edit(WT + "outbox.rs", [
    ("use epics_base_rs::runtime::sync::mpsc;",
     "use crate::server::frame::{FramePool, PooledFrame};\n"
     "use epics_base_rs::runtime::sync::mpsc;\nuse std::sync::Arc;"),
    ("pub(crate) struct Outbox {\n    tx: mpsc::UnboundedSender<Vec<u8>>,\n}",
     "pub(crate) struct Outbox {\n    tx: mpsc::UnboundedSender<PooledFrame>,\n"
     "    pool: Arc<FramePool>,\n}"),
    ("pub(crate) struct OutboxDrain {\n    rx: mpsc::UnboundedReceiver<Vec<u8>>,\n}",
     "pub(crate) struct OutboxDrain {\n    rx: mpsc::UnboundedReceiver<PooledFrame>,\n}"),
    ("    let (tx, rx) = mpsc::unbounded_channel();\n    (Outbox { tx }, OutboxDrain { rx })",
     "    let (tx, rx) = mpsc::unbounded_channel();\n    (\n        Outbox {\n            tx,\n"
     "            pool: Arc::new(FramePool::new()),\n        },\n        OutboxDrain { rx },\n    )"),
    ("    pub(crate) fn push(&self, frame: Vec<u8>) {\n        let _ = self.tx.send(frame);\n    }",
     "    pub(crate) fn pool(&self) -> &Arc<FramePool> {\n        &self.pool\n    }\n\n"
     "    pub(crate) fn push(&self, frame: impl Into<PooledFrame>) {\n"
     "        let _ = self.tx.send(frame.into());\n    }"),
    ("    pub(crate) fn try_next(&mut self) -> Option<Vec<u8>> {",
     "    pub(crate) fn try_next(&mut self) -> Option<PooledFrame> {"),
    ("    pub(crate) async fn recv(&mut self) -> Option<Vec<u8>> {",
     "    pub(crate) async fn recv(&mut self) -> Option<PooledFrame> {"),
])

print("== monitor.rs")
edit(WT + "monitor.rs", [
    ("    let mut frame = FrameBuf::new(0);",
     "    let mut frame = FrameBuf::acquire(outbox.pool(), 0);"),
])

print("== tcp.rs")
s = open(WT + "tcp.rs").read()
n = s.count("let mut frame = FrameBuf::new(0);")
s = s.replace("                        let mut frame = FrameBuf::new(0);",
              "                        let mut frame = FrameBuf::acquire(outbox_clone.pool(), 0);")
s = s.replace("    let mut frame = FrameBuf::new(0);\n    encode_dbr_into(frame.dst(), data_type, snapshot)?;",
              "    let mut frame = FrameBuf::acquire(writer.pool(), 0);\n"
              "    encode_dbr_into(frame.dst(), data_type, snapshot)?;")
# build_read_reply: pool in, PooledFrame out
s = s.replace("fn build_read_reply(\n    requested_type: u16,",
              "#[allow(clippy::too_many_arguments)]\nfn build_read_reply(\n"
              "    pool: &std::sync::Arc<crate::server::frame::FramePool>,\n    requested_type: u16,")
s = s.replace(") -> Result<Vec<u8>, ReadReplyError> {",
              ") -> Result<crate::server::frame::PooledFrame, ReadReplyError> {")
s = s.replace("    let mut frame = FrameBuf::new(0);\n"
              "    encode_dbr_into(frame.dst(), requested_type, snapshot).map_err(|_| ReadReplyError::BadType)?;",
              "    let mut frame = FrameBuf::acquire(pool, 0);\n"
              "    encode_dbr_into(frame.dst(), requested_type, snapshot).map_err(|_| ReadReplyError::BadType)?;")
s = s.replace("            match build_read_reply(\n                requested_type,",
              "            match build_read_reply(\n                writer.pool(),\n                requested_type,")
left = s.count("FrameBuf::new(")
open(WT + "tcp.rs", "w").write(s)
print("  FrameBuf::new sites before=%d, left=%d (non-test leftovers must be 0)" % (n, left))
print("done")
