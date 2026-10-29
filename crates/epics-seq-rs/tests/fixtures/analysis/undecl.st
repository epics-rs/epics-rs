program undecl
int x = TRUE;
ss s1 {
    state a {
        when (efTest(nope) && delay(1.0)) {
            undef1 = undef1 + 1;
            pvPut(x, SYNC);
            exit(0);
            x = sizeof(int);
            x = pvStatOK + NOEVFLAG;
        } exit
    }
}
