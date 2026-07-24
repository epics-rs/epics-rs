# Measurement — WHICH five epicsEvent blocks leak per rsrv client cycle

**Answer: the five leaked blocks per CA client connect/disconnect cycle are the
exact five predicted, now confirmed by address and by creating call site rather
than by count and size alone.** Every `epicsEventCreate` call in the running IOC
was intercepted, its target address and caller PC recorded, and each PC resolved
with `arm-rtems6-addr2line`. Over three cycles the tracer recorded exactly
`5 create + 5 destroy` records per cycle, and the five creates resolve to:

| # | caller PC | function | source (addr2line) | prediction table |
|---|---|---|---|---|
| 1 | `0x147e17` | `create_client` | `rsrv/caservertask.c:1263` | `client->blockSem` (`:1262`) |
| 2 | `0x12fdcb` | `db_init_events` | `db/dbEvent.c:314` | `evUser->ppendsem` (`:314`) |
| 3 | `0x12fded` | `db_init_events` | `db/dbEvent.c:320` | `evUser->pexitsem` (`:320`) |
| 4 | `0x16f49f` | `create_threadInfo` | `os/posix/osdThread.c:179` | CAS-client thread `suspendEvent` |
| 5 | `0x16f49f` | `create_threadInfo` | `os/posix/osdThread.c:179` | CAS-event thread `suspendEvent` |

This closes the second bullet of **"Not measured / open"** in
[`c-base-rtems-posix-event-leak.md`](c-base-rtems-posix-event-leak.md) — "**Which**
five blocks … no block's address was traced back to `epicsEventCreate`" — and the
matching first Limit of
[`measurement-c-event-leak-bytes-on-rtems-6.md`](measurement-c-event-leak-bytes-on-rtems-6.md)
("Attribution of the 5 blocks is by count and size, not by address").

Taken **2026-07-24** on `coding-agent@192.168.2.128`, under
`qemu-system-arm -M xilinx-zynq-a9` (RTEMS 6, BSP `xilinx_zynq_a9_qemu`, libbsd,
256 MB guest), same box and `~/rtems-cside/` tree as the two measurements it
extends.

---

## 1. Instrument — a link-time `--wrap` on the two event calls

The stock leak measurement read the heap; it could count and size the leaked
blocks but not name them. To name them, the image is linked with

```
cioc_LDFLAGS += -Wl,--wrap=epicsEventCreate -Wl,--wrap=epicsEventDestroy
```

so **every** reference to those two symbols — from libCom, dbCore, rsrv and ca
alike — resolves at final link to `__wrap_epicsEventCreate` /
`__wrap_epicsEventDestroy` in the added application file
[`repro/evleak/ciocEvTrace.c`](repro/evleak/ciocEvTrace.c). Each wrapper calls the
real symbol through `__real_epicsEvent*` and records one row:
`(op, event address, __builtin_return_address(0))`. Recording is **off** until
`evtrace on` is run from iocsh, so nothing in the boot path is touched.

Nothing in EPICS base or RTEMS is patched. This is the same discipline as the
`ciocEvLoop.c` variant image in the bytes measurement: application code plus, here,
one link flag. Declared in `~/rtems-cside/DEVIATIONS.md`.

The redirection was confirmed in the linked binary before booting —
`arm-rtems6-objdump -d` shows call sites such as `db_init_events` emitting
`bl __wrap_epicsEventCreate`, and `arm-rtems6-nm` shows all three symbols
(`epicsEventCreate` = the renamed real, `__wrap_epicsEventCreate`,
`__wrap_epicsEventDestroy`):

```
001714d0 T epicsEventCreate
001055e0 T __wrap_epicsEventCreate
00105638 T __wrap_epicsEventDestroy
```

Image: `~/rtems-cside/cioc-evtrace-fd64.exe`,
sha256 `014fd80b104a6c7a095743e1aab2a169079f8006209c9b985e2ddf2b7a1ad07d`.

Because `__builtin_return_address(0)` is the **return** address (the instruction
after the `bl`), addr2line names the line at or just past the call — which is why
the create lines land exactly on the prediction (`314`, `320`, `179`) while the
one call whose next instruction is on the following source line reads `1263`
against the predicted `1262`, and the destroy lines read one or two past
(`1132/397/396/236` against `1128/397/395/235`). All resolve inside the predicted
function; none is ambiguous.

## 2. The run — three counted cycles, tracer on

Driver [`repro/evleak/evattr.py`](repro/evleak/evattr.py): same external CA client
cycle as `evleak.py` (`connect → CA_PROTO_VERSION + CLIENT_NAME + HOST_NAME →
recv 16-byte reply → close`), concurrency 1, on port 5164. Tracing is switched on
only around the three counted cycles, so no boot or background event is recorded.

Raw log:
[`evidence/cioc-evattr-3cycles-2026-07-24.log`](evidence/cioc-evattr-3cycles-2026-07-24.log);
addr2line resolution:
[`evidence/cioc-evattr-addr2line-2026-07-24.txt`](evidence/cioc-evattr-addr2line-2026-07-24.txt).

`evtrace count` after the three cycles: **`n=30`** — exactly `3 × (5 create +
5 destroy)`, no overflow. The dump, one cycle shown (records 0–9):

```
EVT 0 C 0x7a06d8 0x147e17     <- create_client         blockSem
EVT 1 C 0x7a08d0 0x12fdcb     <- db_init_events         ppendsem
EVT 2 C 0x7a0948 0x12fded     <- db_init_events         pexitsem
EVT 3 C 0x7a0a58 0x16f49f     <- create_threadInfo      suspendEvent (thread 1)
EVT 4 C 0x7a0b88 0x16f49f     <- create_threadInfo      suspendEvent (thread 2)
EVT 5 D 0x7a0a58 0x16f893     <- free_threadInfo
EVT 6 D 0x7a0948 0x12fe73     <- db_close_events
EVT 7 D 0x7a08d0 0x12fe7b     <- db_close_events
EVT 8 D 0x7a06d8 0x147c39     <- destroy_client
EVT 9 D 0x7a0b88 0x16f893     <- free_threadInfo
```

Per cycle the four distinct create PCs appear with multiplicity
`1 + 1 + 1 + 2` = 5 — the doubled one is `create_threadInfo` (osdThread.c:179),
the two per-client threads (CAS-client, CAS-event). This holds identically in all
three cycles.

## 3. Why this is proof the destroy is the leak, at address granularity

Each created address gets a matching **`D` record at the same address** within the
cycle — `epicsEventDestroy` *is* called on every one. Yet the bytes measurement
shows the heap grows by exactly these 5 blocks per cycle. Both are true only
because the RTEMS-posix `epicsEventDestroy` runs `rtems_binary_semaphore_destroy`
but never `free(pSem)`: the destroy executes (hence the `D` record) and still
leaks (hence the growth). The addresses also climb across cycles — cycle 1's
`0x7a06d8…0x7a0b88` are not reissued to cycle 2, consistent with blocks that are
never returned to the allocator.

## 4. Limits

* **Three cycles, one boot, one image.** The attribution is structural (call
  site), so three cycles suffice to establish the per-cycle set; it is not an
  endurance claim. The slope itself is measured in the companion bytes document.
* **Thread identity (CAS-client vs CAS-event) is by count, not by name.** Both
  per-client thread `suspendEvent`s share the one call site osdThread.c:179, so
  the tracer sees "2 creates from osdThread.c:179 per cycle", and the *names*
  come from the held-connection thread census in the bytes measurement (§2
  there), not from this trace. `rtems_object_get_name` is blind to pthread names
  on this target, so no finer on-target label was available.
* **addr2line resolves the return address**, so line numbers sit on or just after
  the call (§1). Function attribution is exact; the ±1–2 line offset is inherent
  and stated, not corrected.
* **The `--wrap` image is not the stock image.** It is byte-for-byte the stock
  cioc plus `ciocEvTrace.c` and the two `--wrap` flags. The per-cycle *count* of 5
  is independently the stock-image result of the bytes measurement; this image
  only adds the naming.
