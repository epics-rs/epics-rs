# newlib header permalinks for libc PRs #5307 / #5308

Source of truth verified 2026-07-22: the arm-rtems6 toolchain headers on the
QEMU box (`tools/arm-rtems6/include`) are byte-identical (md5) to
`gitlab.rtems.org/contrib/newlib-cygwin` at commit
`1b3dcfdc6f1fd2bd3e1ef2f8b7df736c076c6042` — the exact source RSB built from
(`rtems-gcc-13.3-newlib-head.cfg` line 22 downloads the archive of that ref).
11 files verified against GitLab raw fetches, 3 (socket.h,
_sockaddr_storage.h, tcp.h) against the RSB source tarball itself after
GitLab rate-limited (HTTP 429). Do NOT link sourceware — RSB does not build
from it.

Base URL (prefix every path below):

    https://gitlab.rtems.org/contrib/newlib-cygwin/-/blob/1b3dcfdc6f1fd2bd3e1ef2f8b7df736c076c6042/

`R/` = `newlib/libc/sys/rtems/include/` (RTEMS-specific, FreeBSD-derived)
`G/` = `newlib/libc/include/` (generic newlib)

## PR #5307 commit 1 — sockaddr types

| item | path#line |
|---|---|
| `struct sockaddr` `sa_len` | `R/sys/socket.h#L322` |
| `sockaddr_in` `sin_len` | `R/netinet/in.h#L98` |
| `sockaddr_in6` `sin6_len` | `R/netinet6/in6.h#L121` |
| `sockaddr_un` `sun_len` | `R/sys/un.h#L58` |
| `sockaddr_storage` (`_SS_MAXSIZE 128` / `ss_len` / `ss_family`) | `R/sys/_sockaddr_storage.h#L41-53` |

## PR #5307 commit 2 — constants

| item | path#line |
|---|---|
| `AF_INET6 28` (same file: `SOCK_CLOEXEC` L114, `PF_INET6` L375, `MSG_DONTWAIT` L443, `MSG_NOSIGNAL` L454) | `R/sys/socket.h#L246` |
| `IP_TTL 4` / `IP_ADD_MEMBERSHIP 12` | `R/netinet/in.h#L424` / `#L434` |
| `TCP_NODELAY 1` / `TCP_MAXSEG 2` | `R/netinet/tcp.h#L168` / `#L170` |
| `POLLOUT 0x4` / `POLLHUP 0x10` | `R/sys/poll.h#L65` / `#L82` |
| `FIONBIO` (`_IOW('f',126,int)` = 0x8004667e) | `R/sys/filio.h#L50` |
| `NCCS 20` | `R/sys/_termios.h#L78` |
| `TRY_AGAIN 2` / `NO_DATA 4` / `NO_ADDRESS` / `EAI_FAMILY 5` / `AI_NUMERICSERV 8` / `NI_NUMERICSERV 8` | `R/netdb.h#L156` (block L156–L217) |
| `O_CLOEXEC` = `_FNOINHERIT` (0x40000) | `G/sys/_default_fcntl.h#L63` |
| `FD_SETSIZE 256` — explicit `defined(__rtems__)` arm | `G/sys/select.h#L34` |

## PR #5308 — scalar type widths

| item | path#line |
|---|---|
| `__dev_t` = `__uint64_t` | `R/machine/_types.h#L12` |
| `__ino_t` = `__uint64_t` | `R/machine/_types.h#L21` |
| `_CLOCK_T_` = `__uint64_t` (unsigned!) | `R/machine/_types.h#L27` |
| `_CLOCKID_T_` = `int` (signed) | `R/machine/_types.h#L30` |
| `__rlim_t` = `__int64_t` ("intentionally signed" comment upstream) | `R/machine/_types.h#L39` |
| `_TIME_T_` = `__int_least64_t` on 32-bit (no `_USE_LONG_TIME_T`; measured size=8 confirms the arm taken) | `G/sys/_types.h#L187` (typedef L189) |

Note: `machine/_types.h` sits in the RTEMS sys dir, so one permalink file
covers five of the six #5308 types; only `time_t` comes from the generic
`sys/_types.h`.
