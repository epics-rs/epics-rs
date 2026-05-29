# 02 — PVA wire protocol

This doc covers the on-the-wire layout `epics-pva-rs` emits and
parses. It is a working reference, not a spec — the canonical
spec is the EPICS V4 pvAccess document and the pvxs source code.

## Frame header (8 bytes)

```text
0   1   2   3   4   5   6   7
+---+---+---+---+---+---+---+---+
|0xCA|ver|flags|cmd|  payload_length (u32, byte order from flags)  |
+---+---+---+---+---+---+---+---+
```

| Byte | Field | Notes |
|------|-------|-------|
| 0    | magic | always `0xCA` |
| 1    | version | wire version, currently `2` |
| 2    | flags | bit 0 = control(0)/application(1), bit 7 = byte order (LE=0, BE=1), bit 4 = segmented |
| 3    | command | application: `Command` enum; control: `ControlCommand` enum |
| 4–7  | payload_length | u32, encoded in the byte order from byte 2 |

Code: `proto/header.rs` `PvaHeader`. Constants: `PVA_VERSION` = 2.

## Application commands (header byte 3 when flags.bit0 = 1)

| Code | Command | Direction |
|------|---------|-----------|
| 0x01 | `ConnectionValidation` | server → client (req), client → server (reply) |
| 0x02 | `Echo` | both — body echoed back |
| 0x03 | `Search` | client UDP / TCP-via-name-server |
| 0x04 | `SearchResponse` | server UDP / TCP |
| 0x05 | `AuthNZ` | unused at the application layer (handled in CONNECTION_VALIDATION) |
| 0x06 | `AclChange` | unused |
| 0x07 | `CreateChannel` | client → server |
| 0x08 | `DestroyChannel` | client → server |
| 0x09 | `ConnectionValidated` | server → client |
| 0x0A | `Get` | both, INIT/DATA/DESTROY phased on subcmd |
| 0x0B | `Put` | both, phased |
| 0x0C | `PutGet` | both |
| 0x0D | `Monitor` | both, INIT/START/STOP/DATA/FINISH |
| 0x0E | `Array` | not implemented |
| 0x0F | `DestroyRequest` | client → server (free an op slot) |
| 0x10 | `Process` | both, phased |
| 0x11 | `GetField` | both — schema-only fetch |
| 0x12 | `Message` | client → server, severity-tagged log line |
| 0x13 | `MultipleData` | unused |
| 0x14 | `Rpc` | both, phased |
| 0x15 | `CancelRequest` | client → server |
| 0x16 | `OriginTag` | unused |
| 0x17 | `Beacon` | server UDP only |

Code: `proto/command.rs`.

## Control commands (header byte 3 when flags.bit0 = 0)

| Code | Command |
|------|---------|
| 0x00 | `SetMarker` |
| 0x01 | `AckMarker` |
| 0x02 | `SetByteOrder` |
| 0x03 | `EchoRequest` |
| 0x04 | `EchoResponse` |

`SetByteOrder` flips the byte-order bit in its own header to declare
which byte order the rest of the connection will use. Server sends
this immediately after accept.

## Size encoding

PVA uses a 1-or-5 byte size:

```
0..=253      → 1 byte (the value itself)
254..=u32::MAX → 0xFE prefix + u32 (in current byte order)
null         → 0xFF
```

Code: `proto/size.rs`.

## String encoding

```
size + UTF-8 bytes        (size encoded as above)
0xFF                       (null string)
```

Code: `proto/string.rs`.

## BitSet

LSB-first within each byte; on encode, trailing zero bytes are
trimmed and the remaining length is written as a Size. Zero bits
set encodes as `0x00`.

Code: `proto/bitset.rs`. `BitSet::all_set(n)` masks the unused
high bits in the last byte.

## Status

```
status_byte (0=OK, 1=WARN, 2=ERROR, 3=FATAL)
String message       (only when status_byte != 0)
String call_tree     (only when status_byte != 0)
```

Code: `proto/status.rs` `Status::write_into` / `Status::decode`.

## FieldDesc / PvField encoding

The pvData type tree is encoded as a stream of type tags:

```
0x00          boolean
0x20..0x27    signed   (Byte / UByte / Short / UShort / Int / UInt / Long / ULong)
0x42 / 0x43   float / double
0x60          string
0x80 + body   structure
0x81 + body   union
0x82          variant
0x88 + 0x80 + body  structure array
0x89 + 0x81 + body  union array
0x8A          variant array
<any> | 0x08  scalar array (e.g. 0x68 = string array)
0xFD <slot:u16> <body>   define cache slot + inline body
0xFE <slot:u16>          lookup cache slot
0xFF          null (caller-context dependent)
```

Code: `pvdata/encode.rs::{encode_type_desc,decode_type_desc}` and the
`*_cached` variants. See [`07-introspection-cache.md`](07-introspection-cache.md)
for cache details.

The value bytes (after the type tag) follow the structure depth-first.
For `Get` / `Monitor` `DATA` frames, a `BitSet` precedes the value to
mark which fields are present.

## Op subcommand bits

Application commands `Get` / `Put` / `Monitor` / `Rpc` / `Process`
multiplex phase-of-life on a single byte after sid + ioid:

| Bit  | Meaning |
|------|---------|
| 0x08 | INIT — client sends pvRequest, server replies with introspection |
| 0x10 | DESTROY (`Get`) / FINISH (`Monitor`, end-of-stream) |
| 0x40 | combine flag — `Put` "GET-after-PUT" or `Monitor` pipeline ack |
| 0x80 | legacy pipeline ack (`Monitor`, pvxs / spirit) |

Code: `server_native/tcp.rs::handle_op` switches on these.

## Search request layout

After the application header:

```
seq:u32
flags:u8           (bit 7 = SEARCH_DISCOVER ≡ no PV names)
3 reserved bytes
addr:[16]          (replier address, 0.0.0.0 for "use source IP")
port:u16
String[] protos    (length-prefixed list, "tcp" / "tls")
u16 n_queries
n_queries × (cid:u32, String pv_name)
```

Code: `client_native/search_engine.rs::send_search` /
`server_native/udp.rs::parse_search_request`.

## Search response layout

```
guid:[12]
seq:u32
addr:[16]          (server TCP address; 0.0.0.0 means "use receiver IP")
port:u16
String proto
found:u8           (1 = found, 0 = NOT_FOUND)
u16 n_cids
n_cids × cid:u32
```

Code: `server_native/udp.rs::build_search_response_proto`.

## Beacon layout

```
guid:[12]
flags:u8            (0 / undefined)
sequence:u8         (rolling)
change_count:u16    (incremented on PV-set churn)
addr:[16]
port:u16
String proto
0xFF                (null serverStatus marker)
```

Code: `server_native/udp.rs::build_beacon`.

## CONNECTION_VALIDATION

Server → client (after `SetByteOrder`):

```
buffer_size:u32
introspection_registry_size:u16
Size n
n × String          (offered auth methods)
```

Client → server reply:

```
buffer_size:u32
introspection_registry_size:u16
qos:u16
String auth_method
<auth_method-specific payload>     ; "ca" → struct{user, host, groups}
```

Server → client final ack:

```
Status status   (only emitted; OK on success)
```

Code: `server_native/tcp.rs::parse_client_credentials` /
`client_native/server_conn.rs::handshake`.
