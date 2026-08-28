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

#ifndef EPICS_RS_RECORDED_NET_ROUTE_H
#define EPICS_RS_RECORDED_NET_ROUTE_H

#include <sys/types.h>

/* @rtems-api net/route.h */
struct rt_metrics {
	u_long	rmx_locks;	/* Kernel must leave these values alone */
	u_long	rmx_mtu;	/* MTU for this path */
	u_long	rmx_hopcount;	/* max hops expected */
	u_long	rmx_expire;	/* lifetime for route, e.g. redirect */
	u_long	rmx_recvpipe;	/* inbound delay-bandwidth product */
	u_long	rmx_sendpipe;	/* outbound delay-bandwidth product */
	u_long	rmx_ssthresh;	/* outbound gateway buffer limit */
	u_long	rmx_rtt;	/* estimated round trip time */
	u_long	rmx_rttvar;	/* estimated rtt variance */
	u_long	rmx_pksent;	/* packets sent using this route */
	u_long	rmx_weight;	/* route weight */
	u_long	rmx_nhidx;	/* route nexhop index */
	u_long	rmx_filler[2];	/* will be used for T/TCP later */
};
/* @rtems-api-end */

/* @rtems-api net/route.h */
struct rt_msghdr {
	u_short	rtm_msglen;	/* to skip over non-understood messages */
	u_char	rtm_version;	/* future binary compatibility */
	u_char	rtm_type;	/* message type */
	u_short	rtm_index;	/* index for associated ifp */
	u_short _rtm_spare1;
	int	rtm_flags;	/* flags, incl. kern & message, e.g. DONE */
	int	rtm_addrs;	/* bitmask identifying sockaddrs in msg */
	pid_t	rtm_pid;	/* identify sender */
	int	rtm_seq;	/* for sender to identify action */
	int	rtm_errno;	/* why failed */
	int	rtm_fmask;	/* bitmask used in RTM_CHANGE message */
	u_long	rtm_inits;	/* which metrics we are initializing */
	struct	rt_metrics rtm_rmx; /* metrics themselves */
};
/* @rtems-api-end */

/* @rtems-api net/route.h */
#define	RTM_IFINFO	0xe	/* (3) iface going up/down etc. */
/* @rtems-api-end */

/* @rtems-api-local: PF_ROUTE lives in the toolchain sysroot's <sys/socket.h>,
 * not the BSP include tree, and the host's <sys/socket.h> has no routing
 * socket at all. Recorded here so `#include <net/route.h>` — the header that
 * gives the routing socket its meaning — carries it. */

/* @rtems-api sys/socket.h */
#define	AF_ROUTE	17		/* Internal Routing Protocol */
/* @rtems-api-end */

/* @rtems-api sys/socket.h */
#define	PF_ROUTE	AF_ROUTE
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_NET_ROUTE_H */
