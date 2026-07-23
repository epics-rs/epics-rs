# RTEMS drains destructor-less pthread key values — measured, 2026-07-24

**Claim under test:** RTEMS destroys the value of a key created with a NULL
destructor before (or during) the run of *other* keys' destructors, so
`pthread_getspecific(A)` inside B's destructor reads NULL.

**Result: CONFIRMED on RTEMS, with a sharper rule than "always".** The drain
order is the order the thread called `pthread_setspecific`, not the order the
keys were created. And the glibc control is **not** clean either: it drains
destructor-less values too, ordered by key id.

| | round 1, A set first | round 1, B set first | round 2 (after re-arm) |
|---|---|---|---|
| RTEMS 6.0.0 armv7 | **A = 0x0** | A = 0xa5a5a5a5 | **A = 0x0** (both orders) |
| Linux glibc 2.39 | A = (nil) *when A's key id is lower* | A = 0xa5a5a5a5 *when A's key id is higher* | **A = (nil)** (all four variants) |

On RTEMS the key **creation** order changed nothing (`keydtor.c`, both
variants, A NULL in both rounds). On glibc the key **creation** order was the
whole difference, because it fixes the key slot index. Neither implementation
kept A alive to round 2 in any variant.

## Mechanism (RTEMS, from the source of the booted kernel)

`~/rtems-bringup/kernel` @ `2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc` — the
same rev the image prints in its shutdown banner.
`cpukit/posix/src/keycreate.c`:

```
113  static void _POSIX_Keys_Run_destructors( Thread_Control *the_thread )
122      node = _RBTree_Root( &the_thread->Keys.Key_value_pairs );
133      _RBTree_Extract( &the_thread->Keys.Key_value_pairs, ... );
139      _POSIX_Keys_Key_value_free( key_value_pair );      <-- value gone here
147      if ( destructor != NULL && value != NULL ) ( *destructor )( value );
170    .thread_terminate = _POSIX_Keys_Run_destructors
```

Each iteration takes the RBTree **root**, extracts it, **frees the pair**, and
only then calls the destructor *if there is one*. Two consequences, both
measured:

1. A pair whose key has no destructor is destroyed exactly like one that has
   one — it is simply freed with no callback. Nothing preserves it for the
   remaining destructors.
2. The pair processed first is the tree root, which for a two-node tree is the
   first node inserted — i.e. the first `pthread_setspecific` the thread made.
   That is why `keydtor.c` (which always set A then B) saw A drained in every
   variant, and why `keydtor-setorder.c` recovers A in round 1 exactly when B
   was set first.

The loop also has no `PTHREAD_DESTRUCTOR_ITERATIONS` cap — it runs until the
thread's tree is empty. Both platforms reported `dtor_iterations=4`; the
re-arm produced round 2 on both.

## Raw console lines

### RTEMS `keydtor.c` — `keydtor-rtems.log`

```
keydtor: POSIX_Init entered (RTEMS rtems-6.0.0 (ARM/ARMv4/xilinx_zynq_a9_qemu))
KEYDTOR-BEGIN platform=rtems6-armv7-xilinx_zynq_a9_qemu
KEYDTOR-VARIANT variant=Afirst created=A,B kA=318832641 kB=318832642 dtor_iterations=4
KEYDTOR-THREAD variant=Afirst set_A_rc=0 set_B_rc=0 back_A=0xa5a5a5a5 back_B=0xb0b0b0b0
KEYDTOR-R1-A=0x0 variant=Afirst B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-R1-REARM variant=Afirst rc=0
KEYDTOR-R2-A=0x0 variant=Afirst B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT-END variant=Afirst rounds=2 expected_rounds=2
KEYDTOR-VARIANT variant=Bfirst created=B,A kA=318832644 kB=318832643 dtor_iterations=4
KEYDTOR-THREAD variant=Bfirst set_A_rc=0 set_B_rc=0 back_A=0xa5a5a5a5 back_B=0xb0b0b0b0
KEYDTOR-R1-A=0x0 variant=Bfirst B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-R1-REARM variant=Bfirst rc=0
KEYDTOR-R2-A=0x0 variant=Bfirst B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT-END variant=Bfirst rounds=2 expected_rounds=2
KEYDTOR-DONE
```

### Linux glibc `keydtor.c` — `keydtor-linux.log`

```
KEYDTOR-BEGIN platform=linux-glibc
KEYDTOR-VARIANT variant=Afirst created=A,B kA=0 kB=1 dtor_iterations=4
KEYDTOR-THREAD variant=Afirst set_A_rc=0 set_B_rc=0 back_A=0xa5a5a5a5 back_B=0xb0b0b0b0
KEYDTOR-R1-A=(nil) variant=Afirst B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-R1-REARM variant=Afirst rc=0
KEYDTOR-R2-A=(nil) variant=Afirst B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT-END variant=Afirst rounds=2 expected_rounds=2
KEYDTOR-VARIANT variant=Bfirst created=B,A kA=1 kB=0 dtor_iterations=4
KEYDTOR-THREAD variant=Bfirst set_A_rc=0 set_B_rc=0 back_A=0xa5a5a5a5 back_B=0xb0b0b0b0
KEYDTOR-R1-A=0xa5a5a5a5 variant=Bfirst B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=yes
KEYDTOR-R1-REARM variant=Bfirst rc=0
KEYDTOR-R2-A=(nil) variant=Bfirst B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT-END variant=Bfirst rounds=2 expected_rounds=2
KEYDTOR-DONE
```

### RTEMS `keydtor-setorder.c` — `keydtor-setorder-rtems.log`

```
KEYDTOR-BEGIN platform=rtems6-armv7-xilinx_zynq_a9_qemu probe=setorder
KEYDTOR-VARIANT variant=Afirst-setAB created=A,B set_order=A,B kA=318832641 kB=318832642
KEYDTOR-R1-A=0x0 variant=Afirst-setAB B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-R2-A=0x0 variant=Afirst-setAB B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT variant=Afirst-setBA created=A,B set_order=B,A kA=318832643 kB=318832644
KEYDTOR-R1-A=0xa5a5a5a5 variant=Afirst-setBA B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=yes
KEYDTOR-R2-A=0x0 variant=Afirst-setBA B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT variant=Bfirst-setAB created=B,A set_order=A,B kA=318832646 kB=318832645
KEYDTOR-R1-A=0x0 variant=Bfirst-setAB B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-R2-A=0x0 variant=Bfirst-setAB B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-VARIANT variant=Bfirst-setBA created=B,A set_order=B,A kA=318832648 kB=318832647
KEYDTOR-R1-A=0xa5a5a5a5 variant=Bfirst-setBA B_arg=0xb0b0b0b0 A_expected=0xa5a5a5a5 A_live=yes
KEYDTOR-R2-A=0x0 variant=Bfirst-setBA B_arg=0xb1b1b1b1 A_expected=0xa5a5a5a5 A_live=NO
KEYDTOR-DONE
```
(THREAD / REARM / VARIANT-END lines elided here only; the log has them all.)

### Linux glibc `keydtor-setorder.c` — `keydtor-setorder-linux.log`

```
KEYDTOR-R1-A=(nil)      variant=Afirst-setAB  A_live=NO
KEYDTOR-R1-A=(nil)      variant=Afirst-setBA  A_live=NO
KEYDTOR-R1-A=0xa5a5a5a5 variant=Bfirst-setAB  A_live=yes
KEYDTOR-R1-A=0xa5a5a5a5 variant=Bfirst-setBA  A_live=yes
KEYDTOR-R2-A=(nil)      all four variants     A_live=NO
```
(abridged; full lines in the log)

## Reproducibility and hashes

Every image was run twice; the four run-2 logs are byte-identical to run 1
(same sha256), so nothing here is a scheduling artefact.

```
7d978a65485e2a6a317e90c5de4f376175cd973d4dd221b62c820254dacf6dd7  keydtor.c
413ee42145d89f7a7a8a1789cdd96006451571ece53a50917b9aee5f1cf017a6  keydtor-setorder.c
8502e46eccd392b6c8c051261f9e558e8fc10cb4e7490a2d3d2241c8019e9c0f  build-keydtor.sh
df6667705075e03aa8ee6612af4a114fd4d1ecd30ef27d0ed62d8955489ebfad  boot-keydtor.sh
d3bbfadf8865150a194b05848f5eabe0c0a9acad4d0686da9334ab235a90056a  boot-keydtor-setorder.sh
9a5d3146ee81106b27c3b6db07c9ce6e1e08591cc894288121a6d4b278e9b86a  keydtor-linux.log
9a5d3146ee81106b27c3b6db07c9ce6e1e08591cc894288121a6d4b278e9b86a  keydtor-linux-run2.log
fce1a9543f7cb84ac17743bee207e026ccb56b2564e098a63a028f0c1b1626da  keydtor-rtems.log
fce1a9543f7cb84ac17743bee207e026ccb56b2564e098a63a028f0c1b1626da  keydtor-rtems-run2.log
39f7192f98855f08d7d3e5c6facfa3c56e029d6d88593b19542a14ebc6d82631  keydtor-setorder-linux.log
39f7192f98855f08d7d3e5c6facfa3c56e029d6d88593b19542a14ebc6d82631  keydtor-setorder-linux-run2.log
1aaf70c346b2863fbb5ae7632bd245401016e7974259b053f8d5452836866ce9  keydtor-setorder-rtems.log
1aaf70c346b2863fbb5ae7632bd245401016e7974259b053f8d5452836866ce9  keydtor-setorder-rtems-run2.log
aa4f5937974f6b4f853d877785524b0cb6659e1e5b19491416bb78fee2b77578  keydtor-rtems.exe
79e407a4841b3a6efd8210a92d5735844c41467e0e0662f22200499f3d834b7b  keydtor-linux
7f2a4b305c485d93762ae2bba2c908e3a6a038c8a36ba5c052a4e13a127c1970  keydtor-setorder-rtems.exe
cd1018166a923f6a8242d9cc384c753ac1426f9aa6297cd335d09c25a043b619  keydtor-setorder-linux
```

## Build / run

```
arm-rtems6-gcc -march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard -mtune=cortex-a9 \
  -I<bsp>/lib/include -O2 -g -Wall -Wextra -DKEYDTOR_PLATFORM=... \
  -B<bsp>/lib -qrtems -Wl,--gc-sections -u POSIX_Init keydtor.c -o keydtor-rtems.exe
gcc -O2 -g -Wall -Wextra -pthread -DKEYDTOR_PLATFORM='"linux-glibc"' keydtor.c -o keydtor-linux
```
Zero warnings from either compiler. The RTEMS image is kernel + libc only — no
libbsd, no network — and boots with `-net none`, taking no host port. qemu
writes its own pid to `./qemu.pid` and that pid is the only process the boot
script can signal; no `pkill` appears anywhere. `pgrep -a qemu-system-arm` was
empty before the first boot and after the last.

Toolchain: arm-rtems6-gcc 13.3.0 (RSB 5dbc1e08, Newlib 1b3dcfd), RTEMS
6.0.0.2faafecb, BSP xilinx_zynq_a9_qemu, qemu-system-arm 8.2.2; control
gcc 13.3.0 / glibc 2.39 on x86-64.

## What this does and does not establish

* Establishes: on RTEMS 6 armv7, a pthread key value with **no** destructor is
  extracted and freed by the same loop that runs destructors, before the
  destructor of a key whose pair sits deeper in the tree; and by the second
  destructor round every such value is gone. Two creation orders x two
  setspecific orders, all four measured, twice each.
* Does **not** establish anything about Rust std's own keys or about the
  128 B/thread leak — no Rust code was involved in this experiment. It
  characterises the C primitive only.
* The glibc control did not behave as POSIX-clean either. Whatever conclusion
  is drawn about RTEMS conformance, "glibc keeps destructor-less values alive"
  is contradicted by `keydtor-linux.log` variant Afirst.
* Mechanism is cited from RTEMS source; the glibc round-1 order correlates with
  the key id in all four variants, but no glibc source was read, so that
  correlation is stated as observed, not explained.
