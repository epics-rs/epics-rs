/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program bittypesTest

%%#include "../testSupport.h"

int8_t i8;
uint8_t ui8;
int16_t i16;
uint16_t ui16;
int32_t i32;
uint32_t ui32;

assign i8 to "ushort";
assign ui8 to "ushort";
assign i16 to "ushort";
assign ui16 to "ushort";
assign i32 to "ushort";
assign ui32 to "ushort";

entry {
    seq_test_init(24);
}

ss test {
    state sizes {
        when () {
            testOk1(sizeof(i8)==1);
            testOk1(sizeof(ui8)==1);
            testOk1(sizeof(i16)==2);
            testOk1(sizeof(ui16)==2);
            testOk1(sizeof(i32)==4);
            testOk1(sizeof(ui32)==4);
        } state conversion
    }
    state conversion {
        int8_t xi8 = -1;
        uint8_t xui8 = -1;
        int16_t xi16 = -1;
        uint16_t xui16 = -1;
        int32_t xi32 = -1;
        uint32_t xui32 = -1;
        when () {
            int pvstat;
            i8 = xi8;
            pvstat = pvPut(i8, SYNC);
            testOk1(pvstat==pvStatOK);
            pvstat = pvGet(i8);
            testOk1(pvstat==pvStatOK);
            testOk1(i8==xi8);
            ui8 = xui8;
            pvstat = pvPut(ui8, SYNC);
            testOk1(pvstat==pvStatOK);
            pvstat = pvGet(ui8);
            testOk1(pvstat==pvStatOK);
            testOk1(ui8==xui8);
            i16 = xi16;
            pvstat = pvPut(i16, SYNC);
            testOk1(pvstat==pvStatOK);
            pvstat = pvGet(i16);
            testOk1(pvstat==pvStatOK);
            testOk1(i16==xi16);
            ui16 = xui16;
            pvstat = pvPut(ui16, SYNC);
            testOk1(pvstat==pvStatOK);
            pvstat = pvGet(ui16);
            testOk1(pvstat==pvStatOK);
            testOk1(ui16==xui16);
            i32 = xi32;
            pvstat = pvPut(i32, SYNC);
            testOk1(pvstat==pvStatOK);
            pvstat = pvGet(i32);
            testOk1(pvstat==pvStatOK);
            testOk1(i32!=xi32);
            ui32 = xui32;
            pvstat = pvPut(ui32, SYNC);
            testOk1(pvstat==pvStatOK);
            pvstat = pvGet(ui32);
            testOk1(pvstat==pvStatOK);
            testOk1(ui32!=xui32);
        } exit
    }
}

exit {
    seq_test_done();
}
