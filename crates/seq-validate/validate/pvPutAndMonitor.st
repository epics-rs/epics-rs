program pvPutAndMonitorTest

%%#include "../testSupport.h"

option +s;

int x;
assign x to "counter";
monitor x;

entry {
    seq_test_init(1);
}

ss test {
    state put_then_incr {
        when () {
            x++;
            testDiag("before put: x==%d", x);
            pvPut(x);
            x++;
        } state test_x
    }
    state test_x {
        entry {
            testDiag("on entry to state test_x: x==%d", x);
        }
        when (delay(1) && x > 1) {
            testFail("when (x>1): x==%d (you should never see this)", x);
        } exit
        when (delay(1)) {
            testPass("when(): x==%d", x);
        } exit
    }
}

exit {
    seq_test_done();
}
