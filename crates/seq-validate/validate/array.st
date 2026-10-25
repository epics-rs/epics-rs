/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program arrayTest

%%#include "../testSupport.h"

unsigned short longer[10] = {0,1,2,3,4,5,6,7,8,9};
assign longer to "array";

unsigned short shorter[3] = {10,11,12};
assign shorter;

entry {
    seq_test_init(33);
}

ss array {
    string a4a5s[4][5] = {
        { "00", "01", "02", "03", "04" },
        { "10", "11", "12", "13", "14" },
        { "20", "21", "22", "23", "24" },
        { "30", "31", "32", "33", "34" }
    };
    assign a4a5s to {};
    state init {
        string (*a4ps[4])[5] = {
            &a4a5s[0],
            &a4a5s[1],
            &a4a5s[2],
            &a4a5s[3]
        };
        when () {
            int i, j;
            for (i = 0; i < 4; i++) {
                for (j = 0; j < 5; j++) {
                    testOk(a4a5s[i][j] == (*a4ps[i])[j],
                        "a4a5s[%d][%d]=%s==%s=(*a4ps[%d])[%d])",
                        i, j, a4a5s[i][j], (*a4ps[i])[j], i, j);
                }
            }
            for (i = 0; i < 4; i++) {
                testDiag("a4ps[%d]=%p", i, a4ps[i]);
            }
            // test array variable that is longer than the PV
            pvPut(longer,SYNC);
            pvGet(longer);
            for (i = 0; i < 10; i++) {
                testOk1(longer[i]==i);
                longer[i] = 0;
            }
            pvAssign(shorter,"array");
        } state reconnect
    }
    state reconnect {
        when (pvConnectCount() == pvAssignCount()) {
            int i;
            // test array variable that is shorter than the PV
            pvPut(shorter,SYNC);
            pvGet(shorter);
            for (i = 0; i < 3; i++) {
                testOk1(shorter[i]==i+10);
            }
        } exit
    }
}

exit {
    seq_test_done();
}
