/*************************************************************************\
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program opttVarTest

%%#include "../testSupport.h"

option +s;

/* snc test program to test state option -t, not to reset timers on entry
 * from self, where delay is a variable. Test is that;
 *   i. the variable is re-initialized on the iteration even 
 *      when -t is in effect.
 *   ii.The delay is timed from the states first entry when -t is in effect,
 *      and on each entry when +t is in effect (the default). 
 *   iii. Re-entrant code, variable d is assigned properly in entry{}.
 */

char msg[60];
assign msg;
monitor msg;
syncq msg 40;

entry {
    seq_test_init(40);
}

ss low_high {
    state low {
        double d; /* Delay variable for managing low state*/
        option -t; /* -t Don't reset timers on re-entry to state */
        entry {
            d = 0.4;
            sprintf(msg, "Entered low, initialize delay to %g sec.",d);
            pvPut(msg);
        }
        when(d>1.1) {
            sprintf(msg, "low, delay = %g, now changing to high",d);
            pvPut(msg);
        } state high
        when(delay(d)) {
            sprintf(msg, "low, delay timeout, reenter low");
            pvPut(msg);
            d += 0.2;
            sprintf(msg, "low, waiting %g secs to iterate",d);
            pvPut(msg);
        } state low
        exit {
            sprintf(msg, "low, print this on exit of low");
            pvPut(msg);
        }
    }

    state high {
        int v; /* iteration counter */
        entry {
            v = 3;
            sprintf(msg, "Entered high");
            pvPut(msg);
        }
        when(v==0) {
            sprintf(msg, "changing to low");
            pvPut(msg);
        } state low
        when(delay(0.3)) {
            sprintf(msg, "high, delay 0.3 timeout, decr v and re-enter high");
            pvPut(msg);
            v -= 1;
            sprintf(msg, "v = %d",v); 
            pvPut(msg);
        } state high
    }
}

ss check {
    char *expected[20] = {
        "Entered low, initialize delay to 0.4 sec.",
        "low, delay timeout, reenter low",
        "low, waiting 0.6 secs to iterate",
        "low, delay timeout, reenter low",
        "low, waiting 0.8 secs to iterate",
        "low, delay timeout, reenter low",
        "low, waiting 1 secs to iterate",
        "low, delay timeout, reenter low",
        "low, waiting 1.2 secs to iterate",
        "low, delay = 1.2, now changing to high",
        "low, print this on exit of low",
        "Entered high",
        "high, delay 0.3 timeout, decr v and re-enter high",
        "v = 2",
        "high, delay 0.3 timeout, decr v and re-enter high",
        "v = 1",
        "high, delay 0.3 timeout, decr v and re-enter high",
        "v = 0",
        "changing to low",
        0
    };
    char **xp;
    int repeat = 1;
    state check_msg {
        entry {
            xp = expected;
            testPass("start 1st round...");
        }
        when (repeat && !*xp) {
            xp = expected;
            testPass("one more time...");
            repeat = 0;
        } state check_msg
        when (!*xp) {
        } exit
        when (pvGetQ(msg)) {
            testOk(strcmp(*xp,msg)==0, "msg=%s", msg);
            xp++;
        } state check_msg
    }
}

exit {
    seq_test_done();
}
