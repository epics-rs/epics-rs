/* pvSync.st:78's shape: an int-returning foreign call tested inside && in
   a when, then alone, negated and against 0 */
program foreignTruth
evflag ef;
int a[2];
ss s {
    state t {
        when (efTestAndClear(ef) && check(a[0] == 0)) {
            if (check(a[1]))
                a[0] = !check(1);
            a[1] = check(2) == 0;
        } state t
    }
}
