/*  femto - Femto Amplifier Control
    Calling seqeunce:
        seq &femto,"name=,P=,H=,F=G1=,G2=,G3=,NO="
 
    Where:
        P  = prefix of database and sequencer
        H  = hardware (i.e. fem01)
        F  = functionality (i.e. seq01)
        G1 = PV name for gain bit 0
        G2 = PV name for gain bit 1
        G3 = PV name for gain bit 2
        NE = PV name for noise ctl bit

 */
program femto("name=femto,P=,H=,F=,G1=,G2=,G3=,NO=")

option +d;
option -c;
option +r;
option -w;

%%#include <stdio.h>
%%#include <math.h>
#include "seqPVmacros.h"

#define MIN_GAIN      0
#define MAX_GAIN      15
#define UNUSED_GAIN   7

PV(int,t0,"{G1}",EvFlag);
PV(int,t1,"{G2}",EvFlag);
PV(int,t2,"{G3}",EvFlag);
PV(int,tx,"{NO}",EvFlag);
PV(double,gain,"{P}{H}{F}gain",NoMon);
PV(int,gainidx,"{P}{H}{F}gainidx",EvFlag);
PV(int,debug_flag,"{P}{H}{F}debug",EvFlag);

int curgain;
/* femto runs without +s, so a monitor can rewrite gainidx or a bit PV at any
   time. C re-read them between checking and using them, so a value that passed
   a check could index powers[] out of bounds. The state set copies each once
   into these variables, which only it writes, and checks and indexes the copy. */
int req;
int newidx;
int b0;
int b1;
int b2;
int bx;
char msg[256];
char* SNLtaskName;

%%static int powers[] = {5,6,7,8,9,10,11,0,3,4,5,6,7,8,9,0};

ss femto
{

    state init
    {
        when()
        {
            SNLtaskName = macValueGet("name");

            pvGet(t0,SYNC);
            pvGet(t1,SYNC);
            pvGet(t2,SYNC);
            pvGet(tx,SYNC);

            b0 = t0; b1 = t1; b2 = t2; bx = tx;

            if( (bx+b2+b1+b0) == 0 )
            {
                newidx = 8;   /* most common undefined case 1e3 */
                DEBUG_PRINT(2,"gainidx = default (1e3 since all bits were OFF)");
            }
            else
            {
               /* C: a negative bit is a left shift of a negative int, and a
                  bit past its field overflows the index; both undefined.
                  Such a PV value is refused (MAX_GAIN, reset below). */
               if( b0 < 0 || b1 < 0 || b2 < 0 || bx < 0 || b0 > 15 || b1 > 7 || b2 > 3 || bx > 1 )
               {
                  newidx = MAX_GAIN;
               }
               else
               {
                  newidx = (bx << 3) | (b2 << 2) | (b1 << 1) | b0;
               }
               sprintf(msg,"gainidx = %d",newidx);
               DEBUG_PRINT(2,msg);
            }

            if( newidx >= MAX_GAIN || newidx == UNUSED_GAIN )
            {
                newidx = 6;
                DEBUG_PRINT(4,"resetting gain to 1e11");
            }

            curgain = -1;
            req = newidx;
            gainidx = newidx;
            PVPUT(gainidx,gainidx);
            PVPUT(gain,pow(10,powers[newidx]));
            sprintf(msg,"gainidx = %d",newidx);
            DEBUG_PRINT(2,msg);

            efClear(t0_mon);
            efClear(t1_mon);
            efClear(t2_mon);
            efClear(tx_mon);
            efClear(gainidx_mon);
            efClear(debug_flag_mon);

            DEBUG_PRINT(2,"init complete");
        } state changeGain

    }

    state updateGain
    {
        when()
        {
            /* C shifted the bit PVs unchecked, see init */
            b0 = t0; b1 = t1; b2 = t2; bx = tx;
            if( b0 < 0 || b1 < 0 || b2 < 0 || bx < 0 || b0 > 15 || b1 > 7 || b2 > 3 || bx > 1 )
            {
                newidx = MAX_GAIN;
            }
            else
            {
                newidx = (bx << 3) | (b2 << 2) | (b1 << 1) | b0;
            }
            sprintf(msg,"gainidx = %d",newidx);
            DEBUG_PRINT(2,msg);

            if( newidx >= MAX_GAIN || newidx == UNUSED_GAIN )
            {
                newidx = 6;
                DEBUG_PRINT(4,"resetting gain to 1e11");
            }

            curgain = newidx;
            gainidx = newidx;
            PVPUT(gainidx,gainidx);
            PVPUT(gain,pow(10,powers[newidx]));
            sprintf(msg,"gainidx = %d",newidx);
            DEBUG_PRINT(2,msg);

        } state idle

    }

    state changeGain
    {
        /* C: until a gain is applied curgain is -1, and the reject clauses
           below index powers[curgain], an out-of-bounds read. A request
           that would be rejected then takes init's invalid-gain fallback.
           Every clause tests req, the copy taken on entry, never gainidx. */
        when( curgain < MIN_GAIN && ( req < MIN_GAIN || req > MAX_GAIN ||
              req == UNUSED_GAIN || req == MAX_GAIN ) )
        {
            req = 6;
            gainidx = req;
            PVPUT(gainidx,gainidx);
            DEBUG_PRINT(4,"resetting gain to 1e11");
        } state changeGain

        when( curgain == req )
        {
            DEBUG_PRINT(2,"No gain change required");
        } state idle

        when( req < MIN_GAIN )
        {
            PVPUT(gainidx,curgain);
            PVPUT(gain,pow(10,powers[curgain]));
            sprintf(msg,"Requested gain (%d) too low!",req);
            DEBUG_PRINT(2,msg);
        } state idle

        when( req > MAX_GAIN )
        {
            PVPUT(gainidx,curgain);
            PVPUT(gain,pow(10,powers[curgain]));
            sprintf(msg,"Requested gain (%d) too high!",req);
            DEBUG_PRINT(2,msg);
        } state idle

        when( req == UNUSED_GAIN || req == MAX_GAIN )
        {
            PVPUT(gainidx,curgain);
            PVPUT(gain,pow(10,powers[curgain]));
            sprintf(msg,"Requested gain (%d) invalid!",req);
            DEBUG_PRINT(2,msg);
        } state idle

        when()
        {
            sprintf(msg,"gain %d selected",req);
            DEBUG_PRINT(2,msg);

            /* 1e5 V/A gain, high speed */
            if( req == 0 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 0);
                PVPUT(t2, 0);
                PVPUT(tx, 0);
            }

            /* 1e6 V/A gain, high speed */
            if( req == 1 )
            {
                PVPUT(t0, 1);
                PVPUT(t1, 0);
                PVPUT(t2, 0);
                PVPUT(tx, 0);
            }

            /* 1e7 V/A gain, high speed */
            if( req == 2 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 1);
                PVPUT(t2, 0);
                PVPUT(tx, 0);
            }

            /* 1e8 V/A gain, high speed */
            if( req == 3 )
            {
                PVPUT(t0, 1);
                PVPUT(t1, 1);
                PVPUT(t2, 0);
                PVPUT(tx, 0);
            }

            /* 1e9 V/A gain, high speed */
            if( req == 4 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 0);
                PVPUT(t2, 1);
                PVPUT(tx, 0);
            }

            /* 1e10 V/A gain, high speed */
            if( req == 5 )
            {
                PVPUT(t0, 1);
                PVPUT(t1, 0);
                PVPUT(t2, 1);
                PVPUT(tx, 0);
            }

            /* 1e11 V/A gain, high speed */
            if( req == 6 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 1);
                PVPUT(t2, 1);
                PVPUT(tx, 0);
            }

            /* 1e3 V/A gain, low noise */
            if( req == 8 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 0);
                PVPUT(t2, 0);
                PVPUT(tx, 1);
            }

            /* 1e4 V/A gain, low noise */
            if( req == 9 )
            {
                PVPUT(t0, 1);
                PVPUT(t1, 0);
                PVPUT(t2, 0);
                PVPUT(tx, 1);
            }

            /* 1e5 V/A gain, low noise */
            if( req == 10 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 1);
                PVPUT(t2, 0);
                PVPUT(tx, 1);
            }

            /* 1e6 V/A gain, low noise */
            if( req == 11 )
            {
                PVPUT(t0, 1);
                PVPUT(t1, 1);
                PVPUT(t2, 0);
                PVPUT(tx, 1);
            }

            /* 1e7 V/A gain, low noise */
            if( req == 12 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 0);
                PVPUT(t2, 1);
                PVPUT(tx, 1);
            }

            /* 1e8 V/A gain, low noise */
            if( req == 13 )
            {
                PVPUT(t0, 1);
                PVPUT(t1, 0);
                PVPUT(t2, 1);
                PVPUT(tx, 1);
            }

            /* 1e9 V/A gain, low noise */
            if( req == 14 )
            {
                PVPUT(t0, 0);
                PVPUT(t1, 1);
                PVPUT(t2, 1);
                PVPUT(tx, 1);
            }

            curgain = req;
            PVPUT(gain,pow(10,powers[req]));

            efClear(t0_mon);
            efClear(t1_mon);
            efClear(t2_mon);
            efClear(tx_mon);
            efClear(gainidx_mon);

        } state idle

    }

    state idle
    {
        when( efTestAndClear(debug_flag_mon) )
        {
            sprintf(msg,"Debug level changed to %d",debug_flag);
            DEBUG_PRINT(1,msg);
        } state idle

        when( efTestAndClear(tx_mon) || efTestAndClear(t2_mon) ||
              efTestAndClear(t1_mon) || efTestAndClear(t0_mon)
            )
        {
            DEBUG_PRINT(2,"bits changed, updating");
        } state updateGain

        when( efTestAndClear(gainidx_mon) )
        {
            req = gainidx;
            DEBUG_PRINT(2,"gain change");
        } state changeGain

        when( delay(.1) ) {} state idle
    }

}
