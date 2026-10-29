/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
/* Test state change statement */
program changeTest

option +r;

%%#include <time.h>
%%#include <stdlib.h>
%%#include "../testSupport.h"

#define MAX_TEST 50

entry {
    seq_test_init(MAX_TEST);
    /* uncomment for repeatability */
    /* srandom(12345); */
}

ss main {
    int x = 0;
    int n = 1;
    state zero {
        when (n > MAX_TEST) {
        } exit
        when (delay(0.01)) {
            testOk(x == 0, "state=zero, x=%d", x);
            x = rand() % 10 - 5;
            testDiag("new random x=%d", x);
            n++;
            if (x > 0) {
                state pos;
            } else if (x < 0) {
                state neg;
            }
        } state zero
    }
    state pos {
        when (n > MAX_TEST) {
        } exit
        when (delay(0.01)) {
            testOk(x > 0, "state=pos, x=%d", x);
            n++;
            x--;
            if (x == 0) state zero;
        } state pos
    }
    state neg {
        when (n > MAX_TEST) {
        } exit
        when (delay(0.01)) {
            testOk(x < 0, "state=neg, x=%d", x);
            n++;
            x++;
            if (x == 0) state zero;
        } state neg
    }
}

exit {
    seq_test_done();
}
