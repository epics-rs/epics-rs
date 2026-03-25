# autosave-rs

Pure Rust implementation of [EPICS autosave](https://github.com/epics-modules/autosave) — automatic periodic and triggered saving/restoring of PV values to persistent storage.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Features

- **C-compatible st.cmd commands** — same syntax as C autosave
- **Pass0/Pass1 restore** — restore before and after device support init
- **Request file parsing** — `.req` files with `file` includes, macro expansion, search path resolution, cycle detection
- **Save strategies** — Periodic, Triggered (AnyChange/NonZero), OnChange, Manual
- **Multiple save sets** with independent configurations
- **Atomic file writes** (tmp → fsync → rename)
- **Backup rotation** — .savB, sequence files (.sav0–.savN), dated backups
- **Restore with priority** — .sav > .savB > .sav0/1/...
- **Macro expansion** — `$(KEY)`, `${KEY}`, `$(KEY=default)`, `$$` escape, environment variable fallback
- **C autosave compatible** save file format (`@array@` notation)
- **iocsh commands** — fdbrestore, fdbsave, fdblist
- **Status PV updates** after each save cycle

## How It Works

Autosave operates in three phases during the IOC lifecycle:

```
Phase 1: st.cmd execution          Phase 2: iocInit              Phase 3: Runtime
────────────────────────           ─────────────────             ────────────────
set_requestfile_path ──┐
set_savefile_path ─────┤
save_restoreSet_...  ──┤
set_pass0_restoreFile ─┤──→ Config ──→ Pass0 restore ──→ AutosaveManager
set_pass1_restoreFile ─┤              device support       .start(db)
create_monitor_set ────┘              Pass1 restore        periodic saves
```

### Phase 1: Configuration (st.cmd)

During st.cmd execution, seven iocsh commands collect the autosave configuration:

| Command | Purpose |
|---------|---------|
| `set_requestfile_path(path)` | Add a search path for `.req` files |
| `set_savefile_path(path)` | Set directory for `.sav` files |
| `save_restoreSet_status_prefix(prefix)` | Set prefix for status PVs |
| `set_pass0_restoreFile(file, macros)` | Register file for pre-init restore |
| `set_pass1_restoreFile(file, macros)` | Register file for post-init restore |
| `create_monitor_set(file, period, macros)` | Create periodic save set |
| `create_triggered_set(file, period, macros)` | Create triggered save set |

### Phase 2: Restore (iocInit)

1. **Pass0** — Restore saved values *before* device support is wired. This sets initial field values (e.g., gains, offsets) that device support `init_record` methods may read.
2. **Device support init** — Records are connected to their drivers.
3. **Pass1** — Restore saved values *after* device support is wired. Write callbacks fire, pushing restored values to hardware drivers.

### Phase 3: Runtime

The `AutosaveManager` runs periodic save tasks in the background. Each save set independently:
1. Reads current PV values from the database
2. Writes to a `.sav` file (atomic: write temp file → fsync → rename)
3. Rotates backup files (`.savB`, sequence files, dated backups)

### Request Files (.req)

Request files list PV names to save, one per line. They support `file` includes for modular composition:

```
# simDetector_settings.req
$(P)$(R)GainX
$(P)$(R)SimMode
file "ADBase_settings.req", P=$(P), R=$(R)
file "commonPlugins_settings.req", P=$(P)
```

Macros are expanded through the include chain — `P=$(P)` in a `file` directive passes the parent's `P` value to the child. Environment variables set via `epicsEnvSet()` are also available as macro values.

Search paths set by `set_requestfile_path()` are used to locate both top-level and included `.req` files.

### Save Files (.sav)

Save files use C autosave-compatible format:

```
# File header with version
SIM1:cam1:GainX 1.000000
SIM1:cam1:SimMode 1
SIM1:Stats1:EnableCallbacks 1
<END>
```

Array values use `@array@` notation matching the C format.

## Architecture

```
autosave-rs/
  src/
    lib.rs          # Public API
    manager.rs      # AutosaveManager — orchestrates save sets
    save_set.rs     # SaveSet configuration, save/restore operations
    request.rs      # Request file parser with includes and macros
    save_file.rs    # Save file I/O (atomic write, parse)
    backup.rs       # Backup rotation and recovery
    macros.rs       # Macro expansion engine
    verify.rs       # File validation
    format.rs       # Constants (version, markers)
    iocsh.rs        # iocsh command registration
    error.rs        # Error types
  tests/
    save_restore.rs
    backup.rs
    manager.rs
    request_parsing.rs
    verify.rs
  opi/
    medm/           # MEDM .adl screens (from C++ autosave)
    pydm/           # PyDM .ui screens (converted via adl2pydm)
```

The startup config (`AutosaveStartupConfig`) lives in `epics-base-rs` because it integrates with the IOC lifecycle (iocsh command registration, Phase 1/2 orchestration in `IocApplication`).

## Example st.cmd

```bash
# Search paths for .req files
set_requestfile_path("$(TOP)/db")
set_requestfile_path("$(ADCORE)/db")
set_requestfile_path("$(CALC)/db")

# Save file directory
set_savefile_path("$(TOP)/ioc/autosave")

# Status PV prefix
save_restoreSet_status_prefix("$(P)")

# Restore on startup
set_pass0_restoreFile("settings.req", "P=$(P),R=$(R)")
set_pass1_restoreFile("settings.req", "P=$(P),R=$(R)")

# Periodic save every 5 seconds
create_monitor_set("settings.req", 5, "P=$(P),R=$(R)")
```

## Runtime iocsh Commands

Once the IOC is running:

| Command | Description |
|---------|-------------|
| `fdblist` | List all save sets with status (PV count, last save time, errors) |
| `fdbsave <set_name>` | Trigger immediate save for a set |
| `fdbrestore <filename>` | Restore PVs from a `.sav` file |

## Quick Start (Library API)

```rust
use autosave_rs::{AutosaveManager, SaveSetConfig, SaveStrategy};
use std::time::Duration;

let config = SaveSetConfig {
    name: "positions".into(),
    save_path: "/tmp/positions.sav".into(),
    strategy: SaveStrategy::Periodic(Duration::from_secs(30)),
    request_file: "positions.req".into(),
    ..Default::default()
};

let manager = AutosaveManager::new(vec![config]);
manager.restore_all(&db).await?;
manager.start(db.clone()).await;
```

## Testing

```bash
cargo test
```

45 tests covering save/restore operations, backup rotation, manager lifecycle, request file parsing, and file validation.

## Dependencies

- epics-base-rs — PvDatabase, EpicsValue
- tokio — async runtime
- chrono — timestamps

## Requirements

- Rust 1.70+
- tokio runtime

## License

The Rust code authored in this crate is licensed under MIT.

This crate also bundles third-party OPI/UI assets related to synApps autosave.
See [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES) for attribution and upstream
license text.
