# Where epics-rs differs from C EPICS, and why

The goal is broad compatibility, not line-for-line equivalence. Differences
fall into four kinds, and it matters which one you are looking at.

## Protocol limits, inherited

Channel Access is IPv4-only. The CA header carries the channel and response
identifiers as 32-bit fields and a beacon packs the server address into four
octets, so no CA message can hold a 128-bit address. IPv6 is therefore a
pvAccess feature here exactly as it is upstream: the PVA server and client bind
v6, join the default v6 multicast group, and emit and receive v6 beacons.

This is not a gap and will not close.

## Deliberate deviations

Places where Rust or the runtime makes a better choice available, and the
difference is either invisible to a client or is an improvement:

- **In-flight retargeting.** C parks a target that arrives mid-motion until the
  current move finishes. epics-rs dispatches it immediately and verifies at
  completion, which is what controllers capable of on-the-fly retargeting
  expect. The end state converges to the same place.
- **RTEMS priority bands.** C maps the EPICS priority range onto the RTOS
  linearly. epics-rs maps it onto a POSIX band offset, which keeps IOC threads
  correctly ordered against the high-priority libbsd network threads — the
  linear map does not.

Each one carries its reasoning in a comment at the site, not in a list here.
Find them with `deliberate deviation` or `deliberate divergence`.

A deviation is acceptable when a client cannot observe it, or when it is
observable and better and the reason is written down. "Awkward to express in
Rust" is not a reason.

## Rust extensions

A few interfaces have no C counterpart. `asynUInt64` / `asynUInt64Array` is the
clearest: upstream asyn ships only the signed 64-bit interfaces and the
unsigned proposal is unmerged, so a driver reading unsigned 64-bit hardware
registers would otherwise launder values through the signed path and lose the
sign bit.

These are labelled `Rust extension` in the module docs. The label exists so an
unmerged upstream proposal is never presented as compatibility.

## Known gaps

**Scaffolds without a transport.** FTDI, USBTMC and VXI-11 have their command
surface, protocol constants and iocsh registration, but no device path. Each is
behind a Cargo feature (`ftdi-mpsse`, `usbtmc`, `vxi11`); with the feature off
`connect()` returns "feature not enabled" so an application fails fast. Wiring
the transport waits on a real use case, which is what should pick the
dependency.

**Absent from C too.** HiSLIP, an asyn `getLimits` interface, a parameter-group
topology layer — upstream has discussed these but not merged them. Building
them would be invention presented as parity, and would likely not match
whatever upstream eventually ships.

Anything else that C does, that clients can see, and that epics-rs does not do
is a defect, not an entry here.
