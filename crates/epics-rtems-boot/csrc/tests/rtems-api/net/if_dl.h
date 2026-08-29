/*
 * Recorded RTEMS 6 / rtems-libbsd declarations — the subset that
 * csrc/rtems_init.c names, and nothing else.
 *
 * This is NOT a copy of the real header. Every block introduced by an
 * `@rtems-api <header>` marker is verbatim text from that header in an
 * installed RTEMS 6 BSP, running to its `@rtems-api-end`, and
 * `scripts/rtems-api-check.sh` proves it line for line against a real
 * $RTEMS_BSP_PREFIX. Anything introduced by `@rtems-api-local` is ours and
 * carries its own justification.
 *
 * The recorded text stays under its authors' licences: RTEMS (BSD-2-Clause,
 * (c) OAR Corporation and contributors) and FreeBSD via rtems-libbsd
 * (BSD-3-Clause, (c) The Regents of the University of California and
 * contributors).
 *
 * See tests/rtems-api/README.md for why this exists.
 */

#ifndef EPICS_RS_RECORDED_NET_IF_DL_H
#define EPICS_RS_RECORDED_NET_IF_DL_H

#include <sys/types.h>

/* @rtems-api net/if_dl.h */
struct sockaddr_dl {
	u_char	sdl_len;	/* Total length of sockaddr */
	u_char	sdl_family;	/* AF_LINK */
	u_short	sdl_index;	/* if != 0, system given index for interface */
	u_char	sdl_type;	/* interface type */
	u_char	sdl_nlen;	/* interface name length, no trailing 0 reqd. */
	u_char	sdl_alen;	/* link level address length */
	u_char	sdl_slen;	/* link layer selector length */
	char	sdl_data[46];	/* minimum work area, can be larger;
				   contains both if name and ll address */
};
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_NET_IF_DL_H */
