# CA-RS Source Review - 2026-05-26

Scope:

- Crate: `crates/epics-ca-rs`
- Upstream reference (read-only): EPICS base C at `/Users/stevek/codes/epics-base`
  - CA server protocol: `modules/database/src/ioc/rsrv/camessage.c`
  - CA client: `modules/ca/src/client/cac.cpp`, `modules/ca/src/client/udpiiu.cpp`
  - Protocol header: `modules/ca/src/client/caProto.h`, `modules/ca/src/client/caerr.h`
- Areas reviewed: CA wire protocol framing, CA_PROTO_ERROR reply layout, DBR type
  conversions, monitor flow control, search/UDP/repeater, ECA status codes,
  extended-header handling, beacon, client transport, access rights.
- Finding-ID series: `R-N` (the global parity-round series; this document records the
  epics-ca-rs slice — R45 here). IDs are globally unique by prefix and never reused;
  see `docs/review-tagging-conventions.md`.

## References

- `caProto.h` – wire constants, `mon_info` struct, CA_V4x macros
- `camessage.c` – server-side command handlers (`vsend_err`, `search_reply_udp`,
  `event_add_action`, `clear_channel_reply`, `read_sync_reply`, etc.)
- `cac.cpp` – client-side response parsers (`exceptionRespAction`, `searchRespAction`,
  `eventAddRespAction`, etc.)
- `udpiiu.cpp` – UDP search client

## Method

For each focus area: read both the Rust source and the C reference, compare field
assignments, payload sizes, and control flow. A finding is recorded only when both
a Rust path:line and a C path:line are cited as bilateral evidence.

## Findings

### R45 — `send_ca_error` declares 8 bytes too few in outer header for extended-original requests

Severity: High

Status: Fixed

Evidence:

- **Rust**: `crates/epics-ca-rs/src/server/tcp.rs:4657` —
  `let payload_size = CaHeader::SIZE + error_msg_bytes.len();` always uses 16 regardless
  of whether `original_hdr.to_bytes_extended()` returns 16 or 24 bytes.
- **C**: `modules/database/src/ioc/rsrv/camessage.c:201-214` (`vsend_err`) —
  when `curp->m_postsize >= 0xffff || curp->m_count >= 0xffff`, C computes
  `size = sizeof(caHdr) + 2*sizeof(*pLW) = 24`; otherwise `size = sizeof(caHdr) = 16`.
  `cas_commit_msg(client, size)` uses the correct size to set `m_postsize` in the
  outer CA_PROTO_ERROR reply header.

Impact:

When a CA_PROTO_ERROR reply is sent in response to a large-array request (one that
used the extended 24-byte header — i.e. `m_postsize == 0xFFFF`), the outer
CA_PROTO_ERROR response header declares `m_postsize = 16 + N` (where N is the padded
diagnostic string length), but the actual payload sent on the wire is
`24 + N` bytes (the extended echoed request header plus the diagnostic). The TCP
receiver (C libca `exceptionRespAction`) advances by `align8(16 + N)` bytes after
reading the outer CA_PROTO_ERROR header, leaving 8 orphan bytes (the extended annex
of the echoed request header) in the TCP stream. These orphan bytes are then parsed
as the opcode of the next message, causing all subsequent messages on the connection
to be mis-framed. Affected commands: any CA_PROTO_ERROR sent in response to a
large-array READ_NOTIFY, WRITE_NOTIFY, or EVENT_ADD whose element count or payload
size was >= 0xFFFF.

Fix direction:

Move `let orig_bytes = original_hdr.to_bytes_extended()` before the `payload_size`
calculation and change `payload_size` to use `orig_bytes.len()` instead of
`CaHeader::SIZE`. This makes the declared `m_postsize` exactly equal to the actual
bytes sent (echo header length + padded diagnostic), covering both the 16-byte and
24-byte echo cases with one formula.

## Uncertain Candidates

None identified. All other areas checked (EVENT_ADD mask extraction at offset 12,
CREATE_CHAN response field layout, READ_NOTIFY field layout, WRITE_NOTIFY field
layout, beacon m_available = 0, repeater registration noop, client CA_PROTO_ERROR
parsing with extended-echo handling, search reply cid sentinel, ECA status code
table, CLEAR_CHANNEL reply field echo, READ_SYNC echo, ECHO full-payload round-trip,
monitor flow-control lost-wake-safe gate) were found to match C behavior or be
intentional, documented deviations.
