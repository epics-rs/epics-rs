# I/O readiness backends for the CA and PVA servers

**Status:** Mechanism comparison — input to a backend decision, not a decision
**Date:** 2026-07-28
**Scope:** How a server thread learns that a socket is ready, and what that
choice costs. Three candidates: blocking sockets with one thread per
connection, `poll`, and `kqueue`.

This document compares **mechanisms**. It deliberately says nothing about which
of the three a given target actually provides, or how well any of them behaves
there — that belongs in
[`rtems-runtime-portability-design.md`](rtems-runtime-portability-design.md) and
the platform bring-up notes. Nothing below is a port measurement; it is a
property-by-property comparison of the three designs, and the CA/PVA
recommendation that follows from the two protocols' shapes.

---

## 0. Two axes, not one

Readiness and locking are separate choices and are often conflated, because
"lightweight mutex" sounds like an alternative to `kqueue`. It is not.

- **Readiness** — how a thread waits for a socket to become readable/writable.
  That is what this document compares.
- **Locking** — what protects shared state (records, channel tables, the
  subscription registry) once a thread is running.

A blocking-socket design still needs locks, and a `kqueue` reactor still needs
them. The one place the axes touch is priority: a per-connection thread carries
its own scheduling priority, so a priority-inheriting mutex can actually act on
it, whereas every connection multiplexed onto one reactor thread shares that
thread's priority and inherits nothing finer. That is a consequence of the
readiness choice, not a substitute for it.

---

## 1. The three mechanisms

**Blocking + one thread per connection.** The thread calls `recv()` and the
kernel blocks it there. Readiness *is* the return from the syscall; there is no
separate notification step. A lightweight mutex/event is used only for handoff
between threads, not for waiting on the socket. The kernel holds one TCB and one
stack per connection.

**`poll`.** The caller passes the whole descriptor array on every call. The
kernel scans all of it, marks the ready ones, and returns. It keeps no state
between calls: each `poll` is a fresh registration and deregistration of every
descriptor in the set.

**`kqueue`.** Interest is registered once (`EV_ADD`), and the kernel keeps a
knote per registration. A `kevent()` wait returns only what actually fired, so
the call cost tracks the number of events, not the number of registrations.
Registration and waiting can ride in the same syscall.

## 2. Comparison

| | blocking, thread per connection | `poll` | `kqueue` |
|---|---|---|---|
| how a thread waits | blocked inside `recv()` | scans the whole set each call | sleeps on a kernel queue |
| kernel state | TCB + stack per connection | none (stateless) | one knote per registration (constant, small) |
| cost per event | O(1) | **O(N)** — one event still costs N copies plus an N-element scan | O(events fired), independent of N |
| registration cost | none | every call re-registers the whole set | once, then free |
| threads for N connections | **N** | 1 | 1, or M workers |
| priority granularity | **per connection** — the scheduler acts on it directly, and PI has something to inherit | all connections share one thread's priority | same as `poll` |
| wakeup path | 1 hop: kernel makes that thread runnable | 2 hops plus the scan | 2 hops, no scan |
| several threads on one wait set | n/a | **thundering herd** | kernel wakes one waiter — **M:N workers are practical** |
| slow handler isolation | **total** — a long operation delays only its own connection | none — one slow callback stalls the loop | none — same as `poll` |
| non-socket events | needs another thread | descriptors only | timers, signals and user events in the same queue, so no self-pipe |
| code shape | straight-line | state machine | state machine |

## 3. Where CA lands: blocking, one thread per connection

CA's server-side unit of work is per client and independent. Large array gets
and puts give a single connection a long occupancy, and that is the property
that decides it: on a reactor that occupancy is loop latency for every other
connection, while on a per-connection thread it is latency for that client only.

Per-connection threading is also the only one of the three that can give
different clients different priorities, and therefore the only one where a
priority-inheriting record lock has a priority to inherit.

The price is one thread — and one thread stack — per connection.

## 4. Where PVA lands: `kqueue`

Monitor fan-out is the opposite shape: many connections, few events on each, and
one update delivered to many subscribers. Walking the subscriber set on a single
thread is better for both lock contention and cache locality than waking N
threads to do a slice each.

PVA also needs UDP search and beacon timers, which `kqueue` takes into the same
queue rather than into more threads. When one reactor thread is not enough,
several workers can wait on the same queue and let the kernel distribute — an
option `poll` does not offer.

## 5. Why `poll` is never the pick

It pays O(N) to deliver one event, and it gives up all three of `kqueue`'s
structural advantages: constant-cost registration, kernel-side distribution
across workers, and non-socket events in the same queue. Portability is the only
argument for it, which makes it a fallback rather than a design choice.

## 6. The boundary

One line separates the two families:

> **connection count × per-thread cost** versus **event rate × isolation
> requirement**

When the left side dominates, multiplex (`kqueue`). When the right side does,
give each connection its own thread. CA and PVA sit on opposite sides of that
line, which is why one answer for both is the wrong shape of answer.
