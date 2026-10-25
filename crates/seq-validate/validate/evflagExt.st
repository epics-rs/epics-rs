program evflagExtTest

%%#include "../testSupport.h"

evflag e;

%%static void extfun(SS_ID ssid);

entry {
    seq_test_init(2);
    efClear(e);
}

ss test {
    state testExt {
        entry {
            extfun(ssId);
        }
        when (efTestAndClear(e)) {
            testPass("got the event");
        } state testInt
        when (delay(0.2)) {
            testFail("timeout waiting for event");
        } state testInt
    }
    state testInt {
        entry {
            fun();
        }
        when (efTestAndClear(e)) {
            testPass("got the event");
        } exit
        when (delay(0.2)) {
            testFail("timeout waiting for event");
        } exit
    }
}

exit {
    seq_test_done();
}

%{
static void extfun(SS_ID ssid)
{
    seq_efSet(ssid, e);
}
}%

void fun(void)
{
    efSet(e);
}
