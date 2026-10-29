/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program reassignTest

option +s;

%%#include "../testSupport.h"

int x, y, z;
assign x to "pv1";
assign y to "";
assign z to "pv1";

#define NTESTS 30

entry {
    seq_test_init(NTESTS);
}

ss reassign {
    state start {
        when () {
            testDiag("start");
            testOk1(pvChannelCount()==3);
            testOk1(pvAssignCount()==2);
            testOk1(pvConnectCount()==2);
            testOk1(pvAssign(x,"")==pvStatOK);
        } state wait_x_nil
    }
    state wait_x_nil {
        when (pvAssignCount()==1) {
            testDiag("wait_x_nil");
            testOk1(pvChannelCount()==3);
            testOk1(pvAssignCount()==1);
            testOk1(pvAssign(x,"pv1")==pvStatOK);
            testOk1(pvAssignCount()==2);
        } state wait_x_pv1
    }
    state wait_x_pv1 {
        when (pvConnected(x)) {
            testDiag("wait_x_pv1");
            testOk1(pvGet(x)==pvStatOK);
            testOk1(pvChannelCount()==3);
            testOk1(pvConnectCount()==2);
            testOk1(pvAssignCount()==2);
            testOk1(pvAssign(x,"pv2")==pvStatOK);
        } state wait_x_pv2
    }
    state wait_x_pv2 {
        when (pvConnected(x)) {
            testDiag("wait_x_pv2");
            testOk1(pvGet(x)==pvStatOK);
            testOk1(pvChannelCount()==3);
            testOk1(pvConnectCount()==2);
            testOk1(pvAssignCount()==2);

            testOk1(pvAssign(y,"")==pvStatOK);
            testOk1(pvConnectCount()==2);
            testOk1(pvAssignCount()==2);
            testOk1(pvAssign(y,"pv1")==pvStatOK);
            testOk1(pvAssignCount()==3);
        } state wait_y_pv1
    }
    state wait_y_pv1 {
        when (pvConnected(y)) {
            testDiag("wait_y_pv1");
            testOk1(pvGet(y)==pvStatOK);
            testOk1(pvChannelCount()==3);
            testOk1(pvConnectCount()==3);
            testOk1(pvAssign(z,"pv2")==pvStatOK);
        } state wait_z_pv2
    }
    state wait_z_pv2 {
        when (pvConnected(z)) {
            testDiag("wait_z_pv2");
            testOk1(pvChannelCount()==3);
            testOk1(pvConnectCount()==3);
            testOk1(pvAssignCount()==3);
            seqShow(epicsThreadGetIdSelf());
        } state done
    }
    state done {
        when(1) {
            testPass("ok");
        } exit
    }
}

exit {
    seq_test_done();
}
