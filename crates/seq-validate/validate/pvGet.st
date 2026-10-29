/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program pvGetTest

%%#include "../testSupport.h"

option +s;

/* use literal C code to circumvent safe mode */
%%int shared = 0;
%%epicsMutexId mutex;

int anon;
assign anon;

#define MAX_GET 5

evflag ef_read_named, ef_read_anon, ef_read_named_sync, ef_read_anon_sync;

entry {
    seq_test_init(MAX_GET*4*3);
    mutex = epicsMutexMustCreate();
}

ss read_named {
    int named;
    assign named to "pvGet1";
    int expected;
    int n = 0;

    state get {
        when (n == MAX_GET) {
            efSet(ef_read_named);
        } state done
        when (delay(0.2)) {
            epicsMutexMustLock(mutex);
            expected = shared;
            pvGet(named, ASYNC);
            epicsMutexUnlock(mutex);
            if (pvGetComplete(named)) {
                testOk(expected==named,
                    "immediate completion:      expected=%d==%d=named", expected, named);
            } else {
                testPass("no immediate completion");
            }
            epicsThreadSleep(0.1);
            if (pvGetComplete(named)) {
                testOk(expected==named,
                    "completion after delay:    expected=%d==%d=named", expected, named);
            } else {
                testPass("no completion after delay");
            }
        } state wait
    }
    state wait {
        when (pvGetComplete(named)) {
            testOk(expected==named,
                "completion after sync:     expected=%d==%d=named", expected, named);
            n++;
        } state get
    }
    state done {
        when (FALSE) {} exit
    }
}

ss read_anon {
    int expected;
    int n = 0;

    state get {
        when (n == MAX_GET) {
            efSet(ef_read_anon);
        } state done
        when (delay(0.2)) {
            epicsMutexMustLock(mutex);
            expected = shared;
            pvGet(anon, ASYNC);
            epicsMutexUnlock(mutex);
            if (pvGetComplete(anon)) {
                testOk(expected==anon,
                    "immediate completion:      expected=%d==%d=anon", expected, anon);
            } else {
                testPass("no immediate completion");
            }
            epicsThreadSleep(0.1);
            if (pvGetComplete(anon)) {
                testOk(expected==anon,
                    "completion after delay:    expected=%d==%d=anon", expected, anon);
            } else {
                testPass("no completion after delay");
            }
        } state wait
    }
    state wait {
        when (pvGetComplete(anon)) {
            testOk(expected==anon,
                "completion after sync:     expected=%d==%d=anon", expected, anon);
            n++;
        } state get
    }
    state done {
        when (FALSE) {} exit
    }
}

ss read_named_sync {
    int named;
    assign named to "pvGet1";
    int expected;
    int n = 0;

    state get {
        when (n == MAX_GET) {
            efSet(ef_read_named_sync);
        } state done
        when (delay(0.2)) {
            epicsMutexMustLock(mutex);
            expected = shared;
            pvGet(named, SYNC);
            epicsMutexUnlock(mutex);
            testOk(expected==named,
                "synchronous get:           expected=%d==%d=named", expected, named);
            epicsThreadSleep(0.1);
            epicsMutexMustLock(mutex);
            expected = shared;
            pvGet(named, SYNC);
            epicsMutexUnlock(mutex);
            testOk(expected==named,
                "repeat get after delay:    expected=%d==%d=named", expected, named);
        } state wait
    }
    state wait {
        when () {
            testOk(expected==named,
                "after sync:                expected=%d==%d=named", expected, named);
            n++;
        } state get
    }
    state done {
        when (FALSE) {} exit
    }
}

ss read_anon_sync {
    int expected;
    int n = 0;

    state get {
        when (n == MAX_GET) {
            efSet(ef_read_anon_sync);
        } state done
        when (delay(0.2)) {
            epicsMutexMustLock(mutex);
            expected = shared;
            pvGet(anon, SYNC);
            epicsMutexUnlock(mutex);
            testOk(expected==anon,
                "synchronous get:           expected=%d==%d=anon", expected, anon);
            epicsThreadSleep(0.1);
            epicsMutexMustLock(mutex);
            expected = shared;
            pvGet(anon, SYNC);
            epicsMutexUnlock(mutex);
            testOk(expected==anon,
                "repeat get after delay:    expected=%d==%d=anon", expected, anon);
        } state wait
    }
    state wait {
        when () {
            testOk(expected==anon,
                "after sync:                expected=%d==%d=anon", expected, anon);
            n++;
        } state get
    }
    state done {
        when (FALSE) {} exit
    }
}

ss write {
    int out = 0;
    assign out to "pvGet1";

    state run {
        when (efTest(ef_read_named) && efTest(ef_read_anon) &&
            efTest(ef_read_named_sync) && efTest(ef_read_anon_sync)) {
        } exit
        when (delay(0.07)) {
            epicsMutexMustLock(mutex);
            shared = out;
            pvPut(out);
            anon = out;
            pvPut(anon);
            epicsMutexUnlock(mutex);
            out++;
        } state run
    }
}

exit {
    seq_test_done();
}
