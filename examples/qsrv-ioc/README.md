# qsrv-ioc

Dual-protocol IOC example serving EPICS records over **Channel Access** and **pvAccess** simultaneously from a shared database. Demonstrates the QSRV bridge with group PV support.

## PV List

| PV | Protocol | Type | Description |
|----|----------|------|-------------|
| `DEMO:AI` | CA + PVA | NTScalar (ai) | Simulated sine wave temperature |
| `DEMO:AO` | CA + PVA | NTScalar (ao) | Writable setpoint |
| `DEMO:BI` | CA + PVA | NTEnum (bi) | Beam status (Off/On toggle) |
| `DEMO:GROUP` | PVA only | Group PV | Composite of AI + AO + BI + const |

### Group PV Structure

`DEMO:GROUP` is defined in `db/group.json` and combines four members:

```
DEMO:GROUP  (demo:group/v1, atomic)
  temperature  NTScalar  "+channel": "DEMO:AI",  "+type": "scalar",  "+trigger": "*"
  setpoint     plain     "+channel": "DEMO:AO",  "+type": "plain",   "+trigger": "setpoint"
  status       NTEnum    "+channel": "DEMO:BI",  "+type": "scalar",  "+trigger": "status"
  version      const     "+const": 1
```

- **scalar**: full Normative Type with alarm, timestamp, display, control
- **plain**: value only, no metadata
- **const**: fixed value, no backing record

## Build & Run

```bash
cargo run --release -p qsrv-ioc -- examples/qsrv-ioc/ioc/st.cmd
```

Output:

```
qsrv-ioc: dual-protocol IOC
  CA  port: 5064
  PVA port: 5075
  PVs (4):
    DEMO:AI
    DEMO:AO
    DEMO:BI
    DEMO:GROUP
epics>
```

The `epics>` prompt is an interactive iocsh. Type `exit` or Ctrl-D to stop the server.

## Testing

### Channel Access (port 5064)

```bash
caget DEMO:AI
caput DEMO:AO 42.5
camonitor DEMO:AI
```

### pvAccess (port 5075)

```bash
# Using epics-pva-rs tools
pvget-rs DEMO:AI
pvget-rs DEMO:GROUP

# Using pvxs tools (if installed)
pvget DEMO:AI
pvget DEMO:GROUP
pvmonitor DEMO:GROUP
pvput DEMO:AO 42.5
```

### iocsh Commands

```
epics> dbl                  # list all PVs
epics> dbgf DEMO:AI         # read AI value
epics> dbpf DEMO:AO 42.5    # write AO value
epics> dbpf DEMO:BI 1       # set BI to "On"
```

## Port Configuration

Override default ports via environment variables:

```bash
EPICS_CA_SERVER_PORT=5064 EPICS_PVA_SERVER_PORT=5075 \
  cargo run --release -p qsrv-ioc -- examples/qsrv-ioc/ioc/st.cmd
```

## File Structure

```
qsrv-ioc/
  Cargo.toml          # default feature = ioc (CA + PVA + QSRV bridge)
  ioc/
    st.cmd            # startup script (epicsEnvSet, dbLoadRecords, qsrvGroupLoadConfig)
  db/
    qsrv_demo.db      # EPICS database: ai, ao, bi records with metadata
    group.json         # pvxs-compatible group PV definition (JSON)
  src/
    main.rs           # st.cmd parser, dual-server startup, simulator
```

## Architecture

```
                     +------------------+
                     |   PvDatabase     |
                     |  (shared state)  |
                     +--------+---------+
                              |
              +---------------+---------------+
              |                               |
      +-------+-------+             +--------+--------+
      |   CaServer    |             |   PvaServer     |
      | (port 5064)   |             | (port 5075)     |
      |               |             |                 |
      | caget/caput   |             | QsrvPvStore     |
      | camonitor     |             |   BridgeProvider|
      +---------------+             |   GroupChannel  |
                                    |                 |
                                    | pvget/pvmonitor |
                                    +-----------------+
```

Both servers read and write the same `PvDatabase`. Changes made via CA (`caput`) are immediately visible over PVA (`pvget`), and vice versa. Group PVs are PVA-only since CA has no equivalent concept.
