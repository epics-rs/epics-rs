# sscan-rs

Rust port of the synApps [sscan](https://github.com/epics-modules/sscan)
module: the `sscan` and `scanparm` record types, `recDynLink`, `saveData`
and the `scanProgress` sequence.

`db/` and `dbd/` are vendored from `sscanApp/Db` and `sscanApp/src`
unchanged, so an `st.cmd` that loads `$(SSCAN)/db/standardScans.db` resolves
when `SSCAN` is set to [`SSCAN_DIR`](src/lib.rs).
