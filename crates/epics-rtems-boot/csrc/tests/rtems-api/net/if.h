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

#ifndef EPICS_RS_RECORDED_NET_IF_H
#define EPICS_RS_RECORDED_NET_IF_H

/* @rtems-api-local: struct if_data is recorded whole, so it needs the
 * fixed-width types, time_t and struct timeval its fields name. */
#include <stdint.h>
#include <sys/time.h>
#include <sys/types.h>

/* @rtems-api net/if.h */
#define		IF_NAMESIZE	16
/* @rtems-api-end */

/* @rtems-api net/if.h */
#define		IFNAMSIZ	IF_NAMESIZE
/* @rtems-api-end */

/* @rtems-api net/if.h */
struct if_data {
	/* generic interface information */
	uint8_t	ifi_type;		/* ethernet, tokenring, etc */
	uint8_t	ifi_physical;		/* e.g., AUI, Thinnet, 10base-T, etc */
	uint8_t	ifi_addrlen;		/* media address length */
	uint8_t	ifi_hdrlen;		/* media header length */
	uint8_t	ifi_link_state;		/* current link state */
	uint8_t	ifi_vhid;		/* carp vhid */
	uint16_t	ifi_datalen;	/* length of this data struct */
	uint32_t	ifi_mtu;	/* maximum transmission unit */
	uint32_t	ifi_metric;	/* routing metric (external only) */
	uint64_t	ifi_baudrate;	/* linespeed */
	/* volatile statistics */
	uint64_t	ifi_ipackets;	/* packets received on interface */
	uint64_t	ifi_ierrors;	/* input errors on interface */
	uint64_t	ifi_opackets;	/* packets sent on interface */
	uint64_t	ifi_oerrors;	/* output errors on interface */
	uint64_t	ifi_collisions;	/* collisions on csma interfaces */
	uint64_t	ifi_ibytes;	/* total number of octets received */
	uint64_t	ifi_obytes;	/* total number of octets sent */
	uint64_t	ifi_imcasts;	/* packets received via multicast */
	uint64_t	ifi_omcasts;	/* packets sent via multicast */
	uint64_t	ifi_iqdrops;	/* dropped on input */
	uint64_t	ifi_oqdrops;	/* dropped on output */
	uint64_t	ifi_noproto;	/* destined for unsupported protocol */
	uint64_t	ifi_hwassist;	/* HW offload capabilities, see IFCAP */

	/* Unions are here to make sizes MI. */
	union {				/* uptime at attach or stat reset */
		time_t		tt;
		uint64_t	ph;
	} __ifi_epoch;
#define	ifi_epoch	__ifi_epoch.tt
	union {				/* time of last administrative change */
		struct timeval	tv;
		struct {
			uint64_t ph1;
			uint64_t ph2;
		} ph;
	} __ifi_lastchange;
#define	ifi_lastchange	__ifi_lastchange.tv
};
/* @rtems-api-end */

/* @rtems-api net/if.h */
#define	LINK_STATE_UP		2	/* link is up */
/* @rtems-api-end */

/* @rtems-api net/if.h */
struct if_msghdr {
	u_short	ifm_msglen;	/* to skip over non-understood messages */
	u_char	ifm_version;	/* future binary compatibility */
	u_char	ifm_type;	/* message type */
	int	ifm_addrs;	/* like rtm_addrs */
	int	ifm_flags;	/* value of if_flags */
	u_short	ifm_index;	/* index for associated ifp */
	u_short	_ifm_spare1;
	struct	if_data ifm_data;/* statistics and other data about if */
};
/* @rtems-api-end */

/* @rtems-api net/if.h */
char			*if_indextoname(unsigned int, char *);
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_NET_IF_H */
