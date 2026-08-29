/*
 * The boot command line's tokeniser — the one part of the RTEMS shim that
 * calls no RTEMS API.
 *
 * It lives in its own translation unit for exactly that reason, and the reason
 * is that a runner can RUN it. `scripts/csrc-check.sh` compiles it with any
 * host compiler and executes its boundary set on every push, so the tokeniser
 * is checked for behaviour and not merely for compiling. Before that split it
 * had no gate at all — the workspace's RTEMS gate (`scripts/rtems-check.sh`)
 * is a `cargo check`, which never runs `cc` — and its first reader was the
 * serial console of a board that would not boot.
 *
 * `rtems_init.c` does call the RTEMS API, so it can only be COMPILED on a
 * runner, never run: the same gate builds it for the host against the
 * declarations recorded in `tests/rtems-api/`. `rtems_config.c` and
 * `rtems_stats.c` are outside even that, and only a real image build compiles
 * them; see `tests/rtems-api/README.md`.
 */

#ifndef EPICS_RTEMS_BOOT_ARGS_H
#define EPICS_RTEMS_BOOT_ARGS_H

/* Room for the tokens of one command line, besides argv[0] and the NULL. */
#define EPICS_RTEMS_MAX_BOOT_ARGS 32

/*
 * Split `cmdline` into `argv`, in place, and return the argc that goes with it.
 * argv[0] is the program name, as it is under any shell; the tokens follow, and
 * `argv[argc]` is always NULL.
 *
 * Quote-aware, and that is not a nicety: EPICS's own values are
 * space-separated lists, so `EPICS_CA_ADDR_LIST="10.0.2.2 10.0.2.3"` has to be
 * ONE token. Split naively, the second address would arrive as a database file
 * name and fail the boot — the site would have configured the IOC and been told
 * its `.db` was missing. Base gets this from iocsh's own splitter reading
 * st.cmd; this target has no iocsh, so the shim owns it.
 *
 * Either quote character opens a group and is removed from the token, so
 * `X="a b"` and `"X=a b"` are the same two-word value.
 *
 * Rewriting in place is safe because the scanner only ever drops characters:
 * the write pointer is never ahead of the read pointer, and the NUL that ends
 * each token lands on the separator it just consumed.
 *
 * `argv_cap` is the number of slots in `argv`, one of which is spent on the
 * terminating NULL. Tokens past that are dropped, loudly.
 */
int epics_rtems_build_boot_argv(char *cmdline, char **argv, int argv_cap);

#endif /* EPICS_RTEMS_BOOT_ARGS_H */
