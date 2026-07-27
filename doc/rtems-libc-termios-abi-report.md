# `libc`'s RTEMS `struct termios` does not match RTEMS

Self-contained report for the `libc` crate. Everything below was measured on
2026-07-27 against RTEMS 6 (`6.0.0.2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`,
toolchain `arm-rtems6-gcc 13.3.0`, BSP `xilinx_zynq_a9_qemu`) and the `libc`
revision this workspace pins, `physwkim/libc @ 31d5776`, whose
`src/unix/newlib/` is unchanged from upstream in this area.

There are two independent defects. The first is silent and corrupts memory;
the second is loud and merely blocks compilation.

## 1. Wrong `struct termios` layout (silent)

`src/unix/newlib/mod.rs:200-212` declares, with its own `// Unverified`
comment on the first field:

```rust
pub struct termios {
    // Unverified
    pub c_iflag: crate::tcflag_t,
    pub c_oflag: crate::tcflag_t,
    pub c_cflag: crate::tcflag_t,
    pub c_lflag: crate::tcflag_t,
    pub c_line: crate::cc_t,
    pub c_cc: [crate::cc_t; crate::NCCS],
    #[cfg(target_os = "espidf")]
    pub c_ispeed: u32,
    #[cfg(target_os = "espidf")]
    pub c_ospeed: u32,
}
```

RTEMS's real declaration is BSD's, `arm-rtems6/include/sys/_termios.h:228-236`
with `NCCS` at `:78`:

```c
struct termios {
	tcflag_t	c_iflag;	/* input flags */
	tcflag_t	c_oflag;	/* output flags */
	tcflag_t	c_cflag;	/* control flags */
	tcflag_t	c_lflag;	/* local flags */
	cc_t		c_cc[NCCS];	/* control chars */
	speed_t		c_ispeed;	/* input speed */
	speed_t		c_ospeed;	/* output speed */
};
```

There is no `c_line` on RTEMS, and `c_ispeed`/`c_ospeed` are not
espidf-specific — they are how RTEMS carries the baud rate.

### Layout diff

| field | RTEMS (`_termios.h`) | `libc` newlib | note |
|---|---|---|---|
| `c_iflag` | 0 | 0 | agrees |
| `c_oflag` | 4 | 4 | agrees |
| `c_cflag` | 8 | 8 | agrees |
| `c_lflag` | 12 | 12 | agrees |
| `c_line` | *absent* | 16 | **extra field** |
| `c_cc[20]` | 16 | 17 | **displaced 1 byte** |
| `c_ispeed` | 36 | *absent* | **missing** |
| `c_ospeed` | 40 | *absent* | **missing** |
| `sizeof` | 44 | 37 | |

`NCCS` itself is correct — `src/unix/newlib/mod.rs:361-362` has an explicit
RTEMS arm at 20 — which is what makes the displacement so quiet: the four
flag words still land where they belong, so the struct looks right at a
glance and every `c_iflag`/`c_cflag` test behaves.

### Consequences

* Every `c_cc` index is off by one byte. Writing `c_cc[VMIN]` (index 16)
  through `libc`'s struct lands at absolute offset 33, which the kernel reads
  as `c_cc[VTIME]`. A serial driver that sets `VMIN=1, VTIME=0` therefore
  programs `VMIN=0, VTIME=1` and its blocking reads return 0 bytes.
* `cfsetispeed` / `cfsetospeed` write four bytes at offsets 36 and 40 — past
  the end of a 37-byte Rust struct. With a `struct termios` on the stack that
  is a stack write out of bounds.
* `tcgetattr` writes 44 bytes into a 37-byte object, unconditionally.

The termios **functions** are bound and are not the problem: they come from
the shared `src/unix/mod.rs:2088` block, not from newlib. So the calls
resolve, run, and mis-execute.

### Measured evidence

Field offsets read back from the running kernel. This is the first 48 bytes
of what `tcgetattr` wrote, on `/dev/ttyS0`, after a driver set `VMIN=1`,
`VSTART=^Q`, `VSTOP=^S` and 38400 baud:

```
offset  bytes (little-endian)          field / value
 0..4   05 08 00 00                    c_iflag = 0x0805
 4..8   00 00 00 00                    c_oflag = 0
 8..12  00 8a 00 00                    c_cflag = 0x8a00  (CS7|CREAD|CLOCAL)
12..16  00 00 00 00                    c_lflag = 0
16..36  00*12 11 13 00 00 01 00 00 00  c_cc[12]=0x11 c_cc[13]=0x13 c_cc[16]=0x01
36..40  00 96 00 00                    c_ispeed = 38400
40..44  00 96 00 00                    c_ospeed = 38400
```

`c_cc` begins at 16, not 17: `VSTART`/`VSTOP` (12/13) hold ^Q/^S and `VMIN`
(16) holds 1, exactly where the driver wrote them. Both speed members exist
and hold the rate. Under `libc`'s layout the same three bytes would be read
as `c_cc[11]`, `c_cc[12]` and `c_cc[15]`, and the speeds would not be part of
the object at all.

## 2. No termios constants bound (loud)

Newlib binds not one termios flag for RTEMS. Compiling a POSIX serial driver
against `libc` for `armv7-rtems-eabihf` fails with **102 errors** over **42
distinct names**, 79 `E0425` and 23 `E0531`:

```
B110 B115200 B1200 B134 B150 B1800 B19200 B200 B230400 B2400 B300 B38400
B4800 B50 B57600 B600 B75 B9600 CLOCAL CREAD CRTSCTS CS5 CS6 CS7 CS8 CSIZE
CSTOPB IGNBRK IGNPAR IXANY IXOFF IXON O_NOCTTY PARENB PARODD TCIFLUSH
TCIOFLUSH TCSANOW VMIN VSTART VSTOP VTIME
```

(`O_NOCTTY` is `sys/_default_fcntl.h:25` via `:59`, `_FNOCTTY == 0x8000`;
`O_RDWR` and `O_NONBLOCK` next to it *are* bound, and `O_NONBLOCK`'s bound
value 16384 matches `_FNONBLOCK == 0x4000`.)

This half is benign in the sense that it fails the build rather than
producing wrong behaviour — but it is also why the layout defect went
unnoticed: nobody could get far enough to hit it.

The values are BSD's, not Linux's — a binding that copied Linux's numbers
would compile and then misconfigure the line:

| name | RTEMS | Linux | `_termios.h` |
|---|---|---|---|
| `CSIZE` | `0x300` | `0x30` | `:128` |
| `CS8` | `0x300` | `0x30` | `:132` |
| `CSTOPB` | `0x400` | `0x40` | `:133` |
| `CREAD` | `0x800` | `0x80` | `:134` |
| `PARENB` | `0x1000` | `0x100` | `:135` |
| `CLOCAL` | `0x8000` | `0x800` | `:138` |
| `CRTSCTS` | `0x30000` | `0x80000000` | `:140-142` |
| `IXANY` | `0x800` | `0x800` | `:97` |
| `VSTART` | 12 | 8 | `:66` |
| `VSTOP` | 13 | 9 | `:67` |
| `VMIN` | 16 | 6 | `:72` |
| `VTIME` | 17 | 5 | `:73` |
| `B9600` | 9600 | 13 | `:199` |

Note the last row: on RTEMS the `Bxxx` codes **are** the literal rates.

## Affected symbols

All of these take or return `struct termios` and are bound today, so all of
them are affected by defect 1. Every one is defined in the BSP's
`librtemscpu.a` (`arm-rtems6-nm --defined-only`, 2360 `T` symbols in that
archive):

```
cfgetispeed cfgetospeed cfmakeraw cfsetispeed cfsetospeed cfsetspeed
tcdrain tcflow tcflush tcgetattr tcsendbreak tcsetattr
```

`tcdrain`, `tcflow` and `cfmakeraw` are all present — RTEMS is not a reduced
termios platform.

## Reproduction

Compilation (defect 2), from a crate with a POSIX termios call site:

```
cargo +nightly check --target armv7-rtems-eabihf \
      -Zbuild-std=std,panic_abort -p <crate> --lib
```

Layout (defect 1), on target — needs a bootable RTEMS image, e.g. the
`xilinx_zynq_a9_qemu` BSP under `qemu-system-arm -M xilinx-zynq-a9`:

```rust
// Read the raw bytes the kernel writes, without assuming a layout.
unsafe extern "C" {
    fn open(p: *const libc::c_char, f: libc::c_int, ...) -> libc::c_int;
    fn tcgetattr(fd: libc::c_int, t: *mut u8) -> libc::c_int;
}
let fd = unsafe { open(c"/dev/ttyS0".as_ptr(), 2) };
let mut raw = [0u8; 64];
assert_eq!(unsafe { tcgetattr(fd, raw.as_mut_ptr()) }, 0);
// c_cc occupies 16..36 and the speeds 36..44 -- not 17..37 with nothing after.
```

The BSP registers `/dev/ttyS0`, `/dev/ttyS1` and `/dev/console`; on QEMU the
first two are the two `-serial` chardevs, so `/dev/ttyS0` can be driven
without disturbing the console.

## Suggested fix

Give RTEMS its own `struct termios` arm rather than sharing newlib's:

```rust
#[cfg(target_os = "rtems")]
pub struct termios {
    pub c_iflag: crate::tcflag_t,
    pub c_oflag: crate::tcflag_t,
    pub c_cflag: crate::tcflag_t,
    pub c_lflag: crate::tcflag_t,
    pub c_cc: [crate::cc_t; crate::NCCS],
    pub c_ispeed: crate::speed_t,
    pub c_ospeed: crate::speed_t,
}
```

and bind the 42 constants above from `sys/_termios.h` / `termios.h`. Until
that lands, a consumer has to declare the ABI itself; this workspace does so
in `crates/asyn-rs/src/drivers/serial_port.rs` (`mod platform`), with
`const` assertions on every field offset so the declaration cannot drift
from the header.

## One consumer-visible consequence worth stating

`B300 == 300` on RTEMS, so C's own test for "the code is the rate"
(`#if defined(B300) && (B300 == 300) && ...`) selects the passthrough branch
and any rate can be *named*. That is not the same as any rate being
*settable*: `cfsetospeed` enforces RTEMS's own table, and
`rtems_termios_baud_to_number(31250)` is 0, so the call fails with `EINVAL`
and leaves the previous rate. Measured on target; a `tcsetattr` carrying
31250 in `c_ospeed` directly is accepted and reads back, so the refusal is
`cfsetospeed`'s alone.
