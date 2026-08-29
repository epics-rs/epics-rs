# What compatibility means in epics-rs

epics-rs is a from-scratch Rust implementation, not a binding over `libca` and
`libCom`. Nothing links against C EPICS, so "compatible" means different things
for the wire, for behaviour, and for the API.

## The wire

Channel Access and pvAccess are implemented to the byte, quirks included.
`caget`, `camonitor`, `pvget`, CSS, PyDM and Phoebus connect without knowing
the IOC is not C, and an epics-rs client talks to a C IOC the same way. When
epics-rs and C EPICS disagree about a byte, that is a defect in epics-rs.

## Behaviour

Record processing, link chains, scan scheduling, alarm and monitor posting,
access security and iocsh are held to what a running C IOC *does*, not to how
its source is organised. A Rust module rarely maps one-to-one onto a C file.
The differential oracle (`epics-oracle-rs`) drives an epics-rs IOC and a C
`softIoc` through the same inputs and compares the results; it needs a local C
EPICS checkout, so it is in the default test profile and out of the CI one.

## The API

The API is designed for Rust: a derive macro and a builder instead of `.dbd`
registration plus `Makefile` rules, traits instead of `void*`, an actor model
instead of callbacks. Porting a C *application* is a rewrite. Pointing an
existing *client* at an epics-rs IOC is nothing at all, and `.db`, `.dbd`,
`.substitutions`, ACF, autosave and `st.cmd` files carry over as data.

## Upstream sources

- **EPICS base 7** — records, database and links, iocsh, calc, access
  security, autosave, Channel Access. Boundary: the record and the wire.
- **pvxs** — pvAccess and normative types; QSRV follows the C++ group JSON.
- **asyn** — port driver model, interfaces, interposes, standard drivers.
  Boundary: the port driver API as a device sees it.
- **motor** — record state machine and transforms. Boundary: engineering
  units, so controller step scaling stays inside the driver.
- **areaDetector** — NDArray, driver base, plugin chain.
- **synApps** — std, scaler, optics, mca.

## Checking the current state

There is no status table here. A per-feature table has to be re-audited on
every change and rots faster than it is read. Search the crate for the C symbol
or field name, run the suite, or point a C client at a running IOC. Where the
answer is "different from C on purpose", the reason is in the code — see
[differences.md](./differences.md).
