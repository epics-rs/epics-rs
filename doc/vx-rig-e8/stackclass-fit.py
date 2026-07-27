# E8 rig: fit the measured CA admission wall against the declared stack size of
# a client's two threads.  Data are the six booted points of
# doc/vxworks-ca-admission-wall-vs-declared-stack.md, all at -m 1024M on
# x86_64-wrs-vxworks; the wall is the number of concurrent client sets the
# server sustained before it stopped taking connections.
#
# Prints three things, because one of them alone would mislead:
#   * the reciprocal-linear fit, i.e. "each set costs its declared stack plus a
#     constant" -- the model the pool accounting wants to use;
#   * the straight line N = a + b*D, whose systematic residuals are what refute
#     "the wall moves linearly with declared stack size";
#   * every pairwise exact solve, whose 2.3x spread in K is the honest
#     uncertainty on the fitted per-thread overhead.
pts = [  # (declared bytes per set, wall in sets, label)
    (1048576, 80, "client=Small event=Small"),
    (1572864, 67, "client=Small event=Medium"),
    (2097152, 58, "client=Medium event=Medium"),
    (2621440, 53, "client=Big    event=Small"),
    (3145728, 49, "client=Big    event=Medium"),
    (3145728, 49, "client=Medium event=Big"),
]
n = len(pts)
# --- reciprocal-linear: 1/N = D/B + K/B  (fixed budget B, fixed per-set overhead K)
xs = [p[0] for p in pts]; ys = [1.0 / p[1] for p in pts]
mx = sum(xs) / n; my = sum(ys) / n
sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
sxx = sum((x - mx) ** 2 for x in xs)
b = sxy / sxx; a = my - b * mx
B = 1.0 / b; K = a * B
print("reciprocal-linear fit  1/N = D/B + K/B")
print("  budget B           = %d B  (%.2f MiB)" % (round(B), B / 1048576))
print("  per-set overhead K = %d B  (%.3f MiB)  -> per thread %d B (%.3f MiB)"
      % (round(K), K / 1048576, round(K / 2), K / 2 / 1048576))
ssr = sst = 0.0
print("  %-28s %10s %5s %7s %7s" % ("config", "declared", "wall", "pred", "resid"))
for (d, N, lab), y in zip(pts, ys):
    pred = 1.0 / (a + b * d)
    ssr += (N - pred) ** 2
    print("  %-28s %10d %5d %7.2f %+7.2f" % (lab, d, N, pred, N - pred))
mN = sum(p[1] for p in pts) / n
sst = sum((p[1] - mN) ** 2 for p in pts)
print("  R^2 (on N) = %.4f    max |resid| = %.2f sets" % (1 - ssr / sst, max(abs(p[1] - 1.0/(a+b*p[0])) for p in pts)))
# --- straight line N = a2 + b2*D, for contrast
my2 = mN
sxy2 = sum((x - mx) * (p[1] - my2) for x, p in zip(xs, pts))
b2 = sxy2 / sxx; a2 = my2 - b2 * mx
ssr2 = sum((p[1] - (a2 + b2 * p[0])) ** 2 for p in pts)
print("\nstraight line N = a + b*D  (the model 'wall is linear in declared stack')")
print("  slope = %.6e sets/B   intercept = %.2f sets   R^2 = %.4f" % (b2, a2, 1 - ssr2 / sst))
for d, N, lab in pts:
    print("  %-28s %10d %5d %7.2f %+7.2f" % (lab, d, N, a2 + b2 * d, N - (a2 + b2 * d)))
# --- pairwise constant-budget solves
print("\npairwise solves of  N1(D1+K) = N2(D2+K) = B")
seen = set()
for i in range(n):
    for j in range(i + 1, n):
        d1, n1, l1 = pts[i]; d2, n2, l2 = pts[j]
        if n1 == n2: continue
        Kp = (n1 * d1 - n2 * d2) / (n2 - n1)
        Bp = n1 * (d1 + Kp)
        key = (d1, d2)
        if key in seen: continue
        seen.add(key)
        print("  D=%d(N=%d) vs D=%d(N=%d):  K=%9d B/set (%9d B/thread)  B=%d B (%.1f MiB)"
              % (d1, n1, d2, n2, round(Kp), round(Kp / 2), round(Bp), Bp / 1048576))
# --- total declared stack at the wall, to show it is not the invariant
print("\ntotal DECLARED stack held at the wall (N*D):")
for d, N, lab in pts:
    print("  %-28s %d B (%.1f MiB)" % (lab, N * d, N * d / 1048576))
