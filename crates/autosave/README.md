# autosave-rs

Pure Rust implementation of [EPICS autosave](https://github.com/epics-modules/autosave) — automatic periodic and triggered saving/restoring of PV values to persistent storage.

No C dependencies. Just `cargo build`.

## Features

- **Save strategies**: Periodic, Triggered (AnyChange/NonZero), OnChange, Manual
- **Multiple save sets** with independent configurations
- **Request file parsing** with `file` includes, macro expansion, cycle detection
- **Atomic file writes** (tmp → fsync → rename)
- **Backup rotation**: .savB, sequence files (.sav0–.savN), dated backups
- **Restore with priority**: .sav > .savB > .sav0/1/...
- **Macro expansion**: `$(KEY)`, `${KEY}`, `$(KEY=default)`, `$$` escape
- **C autosave compatible** save file format (`@array@` notation)
- **iocsh commands**: fdbrestore, fdbsave, fdblist
- **Status PV updates** after each save cycle

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

## Quick Start

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

MIT
