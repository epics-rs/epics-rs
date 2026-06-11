# PVA parity test harness

End-to-end and byte-level parity tests against the upstream EPICS C++
reference implementation [`pvxs`](https://github.com/epics-base/pvxs).

This directory holds the byte-exact golden-wire fixtures and the
cross-implementation interop runners. The per-command encode/decode parity
checks themselves live as in-module `#[test]`s next to the code they cover
(see the **Current coverage** table); this harness layers the captured
`pvxs` fixtures and the interop matrix on top of them.

## Layout

```
tests/parity/
├── README.md                 # this file
├── fixtures/
│   └── golden_wire/          # captured byte-exact PVA messages
│       (drop *.bin files here, see "Capturing fixtures" below)
└── (cross-implementation interop runners — added incrementally)
```

## Goals

1. **Byte-exact wire format** — every command we emit/decode is verified
   against a fixture captured from `pvxs` 1.x.
2. **4-way interop matrix** — confirm that all combinations of clients and
   servers (ours / pvxs) talk to each other:
   - our pvget-rs ↔ pvxs `softIocPVX`
   - pvxs `pvget`  ↔ our `PvaServer`
   - our pvget-rs ↔ our `PvaServer`
   - pvxs `pvget`  ↔ pvxs `softIocPVX` (control)
3. **NormativeType conformance** — `structure_id`, field order, and
   alarm/timeStamp payloads must match pvxs byte-for-byte for NTScalar,
   NTScalarArray, NTEnum, NTTable, NTNDArray.
4. **BitSet semantics** — first event has all bits set; subsequent events
   carry only the changed-field bitset; nested structure changes use the
   correct depth-first field index.

## Current coverage

| Area | Covered by (in-module tests) |
|---|---|
| Size encoding parity | ✅ `src/proto/size.rs` |
| String encoding parity | ✅ `src/proto/string.rs` |
| Header parity | ✅ `src/proto/header.rs` |
| Status OK parity | ✅ `src/proto/status.rs` |
| IPv4 wire conversion | ✅ `src/proto/ip.rs` |
| FieldDesc structure encoding | ✅ `src/pvdata/encode.rs` |
| `pvRequest` builder | ✅ `src/pv_request.rs` |
| SEARCH command | ✅ `src/codec.rs` |
| CREATE_CHANNEL | ✅ `src/codec.rs` |
| GET/PUT/MONITOR/GET_FIELD/DESTROY | ✅ `src/codec.rs` |
| CONNECTION_VALIDATED | ✅ `src/codec.rs` |
| BitSet decode of all-bits-set | ✅ `src/proto/bitset.rs` |
| GET response decode | ⏳ Phase 3 |
| MONITOR delta apply | ⏳ Phase 3 |
| NTScalar conformance | ⏳ Phase 5 |
| NTScalarArray conformance | ⏳ Phase 5 |
| NTEnum conformance | ⏳ Phase 5 |
| NTTable conformance | ⏳ Phase 5 |
| NTNDArray conformance | ⏳ Phase 5 |
| 4-way interop matrix | ⏳ Phase 4 (needs server) |

## Capturing fixtures

Once a `pvxs` build is available locally:

```bash
# Build pvxs
cd ~/codes/pvxs && make -j

# Run softIocPVX and capture wire bytes
sudo tcpdump -i lo -w fixtures/golden_wire/get_double.pcap \
    'tcp port 5075 or udp port 5076'

# In another shell:
~/codes/pvxs/bundle/usr/local/lib/perl/PVXS/softIocPVX -d test.db &
~/codes/pvxs/bundle/usr/local/bin/pvget MY:DOUBLE
```

Then convert `.pcap` → raw bytes (one file per direction per command):

```bash
# Strip TCP/UDP framing, save reassembled application payloads
tshark -r get_double.pcap -T fields -e tcp.payload -e udp.payload \
    | xxd -r -p > fixtures/golden_wire/get_double.bin
```

Test code under `tests/parity/golden_wire.rs` (added in Phase 3) loads
these `.bin` files and asserts byte-exact decode + re-encode.

## Running

```bash
cargo test -p epics-pva-rs                   # in-module wire-parity unit tests
cargo test -p epics-pva-rs --test 'parity_*' # golden-wire + interop runners
```

The in-module unit tests are the per-command cross-checks; the golden-wire
fixtures under `fixtures/golden_wire/` remain the long-term parity ground
truth as the harness fills in the ⏳ rows above.
