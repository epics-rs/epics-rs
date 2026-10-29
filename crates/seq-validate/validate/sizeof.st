/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program sizeofTest

%%#include "../testSupport.h"

entry {
    seq_test_init(9);
}

ss test {
    state sizes {
        when () {
            char c;
            char *p = &c;
            testOk1(sizeof(char)==1);
            testOk1(sizeof(*p)==1);
            testOk1(sizeof(string) >= 40);
            testOk1(sizeof(int8_t) == 1);
            testOk1(sizeof(uint8_t) == 1);
            testOk1(sizeof(int16_t) == 2);
            testOk1(sizeof(uint16_t) == 2);
            testOk1(sizeof(int32_t) == 4);
            testOk1(sizeof(uint32_t) == 4);
            c = *p;
        } exit
    }
}

exit {
    seq_test_done();
}
