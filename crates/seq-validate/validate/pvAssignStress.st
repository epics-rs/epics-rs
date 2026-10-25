program pvAssignStressTest

%%#include "../testSupport.h"

string names[3] = {
    "pvAssignStress0",
    "pvAssignStress1",
    "pvAssignStress2"
};
int x[3];
assign x to {};
monitor x;

entry {
    seq_test_init(27);
}

ss test {
    int i;
    int shift = 0;
    state disconn {
        option -t;
        entry {
            for (i=0; i<3; i++) {
                pvAssign(x[i], names[(i+shift)%3]);
            }
        }
        when (pvConnected(x[0]) && pvConnected(x[1]) && pvConnected(x[2])) {
            for (i=0; i<3; i++) {
                pvGet(x[i],SYNC);
                testOk(x[i] == (i+shift)%3,"%d==%d",x[i],(i+shift)%3);
            }
            shift++;
        } state conn
        when (delay(2)) {
            testFail("timeout");
        } exit
    }
    state conn {
        entry {
            for (i=0; i<3; i++) {
                pvAssign(x[i], "");
            }
        }
        when (shift == 9) {
        } exit
        when (!pvConnected(x[0]) && !pvConnected(x[1]) && !pvConnected(x[2])) {
        } state disconn
    }
}

exit {
    seq_test_done();
}
