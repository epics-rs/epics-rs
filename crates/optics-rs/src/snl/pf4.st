/*  XIA PF4 Filter Control
    Calling seqeunce:
        seq &pf4,"name=pf41,P=,H=,B=,BP=,B1=,B2=,B3=,B4=,M="
 
    Where:
        P  = prefix of database and sequencer
        H  = hardware (e.g. "pf401")
        B  = bank indicator (e.g. "A","B")
		BP = Filter control bit PV prefix
        B1 = Filter control bit 0 number
        B2 = Filter control bit 1 number
        B3 = Filter control bit 2 number
        B4 = Fitler control bit 3 number
        M  = Monochromator energy PV

 */
program pf4("name=pf4,P=,H=,B=,BP=,B1=,B2=,B3=,B4=,M=")

option +d;
option -c;
option +r;
option -w;

%%#include <stdio.h>
%%#include <math.h>
%%#include <string.h>
#include "seqPVmacros.h"

%%#include "chantler.h"

#define IDLE_WAIT   (0.10)
#define PVPUT_WAIT  (0.01)
#define ENERGY_WAIT (1.50)
#define FILTER_WAIT (0.20)


/*****************************************************************************
 ***                            Passed parameters                          ***
 *****************************************************************************/
PV(int,b1,"{BP}{B1}",EvFlag);			/* filter control bit0 */
PV(int,b2,"{BP}{B2}",EvFlag);			/* filter control bit1 */
PV(int,b3,"{BP}{B3}",EvFlag);			/* filter control bit2 */
PV(int,b4,"{BP}{B4}",EvFlag);			/* filter control bit3 */
PV(double,monoenergy,"{M}",EvFlag);	/* monochromator energy */

PV(int,displayBit1,"{P}{H}displayBit1{B}",NoMon);		/* filter display bit0 */
PV(int,displayBit2,"{P}{H}displayBit2{B}",NoMon);		/* filter display bit1 */
PV(int,displayBit3,"{P}{H}displayBit3{B}",NoMon);		/* filter display bit2 */
PV(int,displayBit4,"{P}{H}displayBit4{B}",NoMon);		/* filter display bit3 */
PV(double,displayEnergy,"{P}{H}displayEnergy",NoMon);	/* monochromator energy */


/*****************************************************************************
 ***                                Common PVs                             ***
 *****************************************************************************/
PV(int,selectenergy,"{P}{H}useMono",EvFlag);    /* energy select */
PV(double,localenergy,"{P}{H}E:local",EvFlag);  /* local energy */

/* combined thickness (and units) of selected filters, by material, mm */
PV(double,filterAl,"{P}{H}filterAl",NoMon);
PV(double,filterTi,"{P}{H}filterTi",NoMon);
PV(double,filterGl,"{P}{H}filterGlass",NoMon);


/*****************************************************************************
 ***                             Bank Specific PVs                         ***
 *****************************************************************************/
PV(double,trans,"{P}{H}trans{B}",NoMon);
PV(double,invtrans,"{P}{H}invTrans{B}",NoMon);
PV(int,debug_flag,"{P}{H}debug{B}",EvFlag);

/*PV(long,bitflg,"{P}{H}bitFlag{B}",NoMon);*/
PV(int,bitflg,"{P}{H}bitFlag{B}",NoMon);
PV(short,bitsts,"{P}{H}status{B}",NoMon);
PV(short,bankctl,"{P}{H}bank{B}",EvFlag);

PV(short,filpos,"{P}{H}fPos{B}",EvFlag);
PV(string,a00,"{P}{H}fPos{B}.ZRST",NoMon);
PV(string,a01,"{P}{H}fPos{B}.ONST",NoMon);
PV(string,a02,"{P}{H}fPos{B}.TWST",NoMon);
PV(string,a03,"{P}{H}fPos{B}.THST",NoMon);
PV(string,a04,"{P}{H}fPos{B}.FRST",NoMon);
PV(string,a05,"{P}{H}fPos{B}.FVST",NoMon);
PV(string,a06,"{P}{H}fPos{B}.SXST",NoMon);
PV(string,a07,"{P}{H}fPos{B}.SVST",NoMon);
PV(string,a08,"{P}{H}fPos{B}.EIST",NoMon);
PV(string,a09,"{P}{H}fPos{B}.NIST",NoMon);
PV(string,a10,"{P}{H}fPos{B}.TEST",NoMon);
PV(string,a11,"{P}{H}fPos{B}.ELST",NoMon);
PV(string,a12,"{P}{H}fPos{B}.TVST",NoMon);
PV(string,a13,"{P}{H}fPos{B}.TTST",NoMon);
PV(string,a14,"{P}{H}fPos{B}.FTST",NoMon);
PV(string,a15,"{P}{H}fPos{B}.FFST",NoMon);

/* filter bank thickness */
PV(double,f1,"{P}{H}f1{B}",EvFlag);
PV(double,f2,"{P}{H}f2{B}",EvFlag);
PV(double,f3,"{P}{H}f3{B}",EvFlag);
PV(double,f4,"{P}{H}f4{B}",EvFlag);

/* filter blade material index: 0=Al, 1=Ti, 2=borosilicate glass, 3=Other */
PV(short,z1,"{P}{H}Z1{B}",EvFlag);
PV(short,z2,"{P}{H}Z2{B}",EvFlag);
PV(short,z3,"{P}{H}Z3{B}",EvFlag);
PV(short,z4,"{P}{H}Z4{B}",EvFlag);

PV(string,other1,"{P}{H}Other1{B}",EvFlag);
PV(string,other2,"{P}{H}Other2{B}",EvFlag);
PV(string,other3,"{P}{H}Other3{B}",EvFlag);
PV(string,other4,"{P}{H}Other4{B}",EvFlag);
PV(string,otherShow,"{P}{H}OtherShow",EvFlag);
#define NUM_SHOW 300
float showEnergy[NUM_SHOW];
assign showEnergy to "{P}{H}E";
float showTransmission[NUM_SHOW];
assign showTransmission to "{P}{H}T";
PV(int,numShow,"{P}{H}E.NELM",Monitor);

PV(short,otherLegal1,"{P}{H}OtherLegal1{B}",NoMon);
PV(short,otherLegal2,"{P}{H}OtherLegal2{B}",NoMon);
PV(short,otherLegal3,"{P}{H}OtherLegal3{B}",NoMon);
PV(short,otherLegal4,"{P}{H}OtherLegal4{B}",NoMon);

char msg[256];
char* SNLtaskName;
short bits[16];             /* bit patterns for each of the thicknesses */
double xmit[16];            /* transmission of all of the filter combinations */
/* C defect fixed: C took an fPos outside 0..15, which the mbbo accepts, as
** an index into bits/xmit and as a shift count; such a position selects
** nothing. pf4 runs without +s, so a monitor can rewrite filpos between a
** check and its use. The state set copies it once into pos, which only it
** writes, and checks and indexes the copy. */
short pos;
#define LEGAL_POS(p) ((p) >= 0 && (p) < 16)
double energytimeout;

%{
static double TiAbsorptionLength(double);
static double GlassAbsorptionLength(double);
static double AlAbsorptionLength(double);
static int isLegalOther(char *s);
static double OtherAbsorptionLength(double keV, char *species);
static int fillShowArrays(char *species, int N, float *E, float *T);
/*static long numInArray(long,short*,short);*/
static int numInArray(int,short*,short);
static void sortDecreasing(int,double*,short*,int);
static double thickZ(int,int,int,int,int,int,int,int,int,int,double,double,double,double);
static void RecalcFilters(double,short*,double*,int,int,int,int,int,int,double,double,double,double, char *, char *, char *, char *);
}%

ss pf4
{

    state init
    {
        when()
        {
            energytimeout = 0.0;
            SNLtaskName = macValueGet("name");

            pvGet(b1,SYNC); PVPUT(displayBit1, b1);
            pvGet(b2,SYNC); PVPUT(displayBit2, b2);
            pvGet(b3,SYNC); PVPUT(displayBit3, b3);
            pvGet(b4,SYNC); PVPUT(displayBit4, b4);
            pvGet(filpos,SYNC);
            pvGet(bankctl,SYNC);
            pvGet(monoenergy,SYNC); PVPUT(displayEnergy, monoenergy);
            pvGet(debug_flag,SYNC);

            pvGet(f1,SYNC);
            pvGet(f2,SYNC);
            pvGet(f3,SYNC);
            pvGet(f4,SYNC);

            pvGet(z1,SYNC);
            pvGet(z2,SYNC);
            pvGet(z3,SYNC);
            pvGet(z4,SYNC);

            PVPUT(localenergy, monoenergy);

            efClear(b1_mon);
            efClear(b2_mon);
            efClear(b3_mon);
            efClear(b4_mon);

            efClear(f1_mon);
            efClear(f2_mon);
            efClear(f3_mon);
            efClear(f4_mon);

            efClear(z1_mon);
            efClear(z2_mon);
            efClear(z3_mon);
            efClear(z4_mon);

            efClear(filpos_mon);
            efClear(bankctl_mon);
            efClear(selectenergy_mon);
            efClear(localenergy_mon);
            efClear(monoenergy_mon);
            efClear(debug_flag_mon);
            DEBUG_PRINT(2,"init complete");

        } state idle

    }

    state filterBits
    {
        when()
        {
			PVPUT(displayBit1, b1);
			PVPUT(displayBit2, b2);
			PVPUT(displayBit3, b3);
			PVPUT(displayBit4, b4);

            pos = numInArray(16,bits,((b4<<3)|(b3<<2)|(b2<<1)|b1));
            PVPUT(filpos,pos);
            PVPUT(bitflg,(1 << pos));
            PVPUT(bitsts,bits[pos]);

            bankctl = (bankctl) ? 2 : 0;
            pvPut(bankctl,SYNC);

            efClear(filpos_mon);
        } state idle

    }

    state filterPos
    {
        when( !LEGAL_POS(pos) ) {} state idle

        /* C defect fixed: bankControl 3 writes the fPos labels, and a write
        ** of the current position's label posts fPos (mbboRecord.c:288-290),
        ** so with the bank on C ran filterPos -> recalcBank -> bankControl
        ** 3, 2, 1 -> filterPos without end, and idle never reached its later
        ** clauses (otherShow). A position the blades already hold moves
        ** nothing and recalculates nothing. */
        when( bits[pos] == ((b4<<3)|(b3<<2)|(b2<<1)|b1) ) {} state idle

        when()
        {
            /* Put new filters in */
            if (bits[pos] & 0x1) {PVPUT(b1,1); PVPUT(displayBit1,1);}
            if (bits[pos] & 0x2) {PVPUT(b2,1); PVPUT(displayBit2,1);}
            if (bits[pos] & 0x4) {PVPUT(b3,1); PVPUT(displayBit3,1);}
            if (bits[pos] & 0x8) {PVPUT(b4,1); PVPUT(displayBit4,1);}

            /* Give time for filters to engage */
            epicsThreadSleep(FILTER_WAIT);

            /* Take old filters out */
            if (!(bits[pos] & 0x1)) {PVPUT(b1,0); PVPUT(displayBit1,0);}
            if (!(bits[pos] & 0x2)) {PVPUT(b2,0); PVPUT(displayBit2,0);}
            if (!(bits[pos] & 0x4)) {PVPUT(b3,0); PVPUT(displayBit3,0);}
            if (!(bits[pos] & 0x8)) {PVPUT(b4,0); PVPUT(displayBit4,0);}

            PVPUT(bitflg,(1 << pos));
            PVPUT(bitsts,bits[pos]);

            efClear(b1_mon);
            efClear(b2_mon);
            efClear(b3_mon);
            efClear(b4_mon);
        } state recalcBank

    }

    state recalcBank
    {
        when( )
        {
            DEBUG_PRINT(2,"recalcBank");

            bankctl = (bankctl) ? 3 : 0;
            pvPut(bankctl,SYNC);
        } state idle
    }

    state bankControl
    {
        when( bankctl==0 )
        {
            sprintf(msg,"filter bank switched off (%d)",bankctl);
            DEBUG_PRINT(2,msg);
        } state bankOff

        when( )
        {
            if( bankctl == 1 )
                sprintf(msg,"filter bank changed value to %d",bankctl);
            else if( bankctl == 2 )
            {
                if( z1==0 || z2==0 || z3==0 || z4==0 ) PVPUT(filterAl,thickZ(0,bankctl,b1,b2,b3,b4,z1,z2,z3,z4,f1,f2,f3,f4));
                if( z1==1 || z2==1 || z3==1 || z4==1 ) PVPUT(filterTi,thickZ(1,bankctl,b1,b2,b3,b4,z1,z2,z3,z4,f1,f2,f3,f4));
                if( z1==2 || z2==2 || z3==2 || z4==2 ) PVPUT(filterGl,thickZ(2,bankctl,b1,b2,b3,b4,z1,z2,z3,z4,f1,f2,f3,f4));

                pos = filpos;
                if( LEGAL_POS(pos) )
                {
                    PVPUT(trans,xmit[pos]);
                    if( trans > 0.0 ) PVPUT(invtrans,1/trans);

                    PVPUT(bitsts,bits[pos]);
                    PVPUT(bitflg,(1 << pos));
                }

                bankctl = (bankctl) ? 1 : 0;
                pvPut(bankctl,SYNC);

                sprintf(msg,"filter bank changed value to %d",bankctl);
            }
            else if( bankctl == 3 )
            {
                pvGet(localenergy,SYNC);
                efClear(localenergy_mon);
                RecalcFilters(localenergy,bits,xmit,debug_flag,bankctl,z1,z2,z3,z4,f1,f2,f3,f4,other1,other2,other3,other4);

                /* set button names to transmission factors */
                sprintf(a00,"%.3e",xmit[0]); pvPut(a00,SYNC);
                sprintf(a01,"%.3e",xmit[1]); pvPut(a01,SYNC);
                sprintf(a02,"%.3e",xmit[2]); pvPut(a02,SYNC);
                sprintf(a03,"%.3e",xmit[3]); pvPut(a03,SYNC);
                sprintf(a04,"%.3e",xmit[4]); pvPut(a04,SYNC);
                sprintf(a05,"%.3e",xmit[5]); pvPut(a05,SYNC);
                sprintf(a06,"%.3e",xmit[6]); pvPut(a06,SYNC);
                sprintf(a07,"%.3e",xmit[7]); pvPut(a07,SYNC);
                sprintf(a08,"%.3e",xmit[8]); pvPut(a08,SYNC);
                sprintf(a09,"%.3e",xmit[9]); pvPut(a09,SYNC);
                sprintf(a10,"%.3e",xmit[10]); pvPut(a10,SYNC);
                sprintf(a11,"%.3e",xmit[11]); pvPut(a11,SYNC);
                sprintf(a12,"%.3e",xmit[12]); pvPut(a12,SYNC);
                sprintf(a13,"%.3e",xmit[13]); pvPut(a13,SYNC);
                sprintf(a14,"%.3e",xmit[14]); pvPut(a14,SYNC);
                sprintf(a15,"%.3e",xmit[15]); pvPut(a15,SYNC);

                PVPUT(bankctl,2);
                sprintf(msg,"filter bank changed value to %d",bankctl);
            }
            else
                sprintf(msg,"invalid filter bank value of %d",bankctl);

            DEBUG_PRINT(2,msg);
        } state idle
    }

    state bankOff
    {
        when( bankctl )
        {
            pvGet(selectenergy,SYNC);
            pvGet(localenergy,SYNC);
            pvGet(monoenergy,SYNC); PVPUT(displayEnergy, monoenergy);

            efClear(localenergy_mon);

            sprintf(msg,"filter bank switched on (%d)",bankctl);
            DEBUG_PRINT(2,msg);
        } state recalcBank

        when( delay(IDLE_WAIT) ) {} state bankOff
    }

    state idle
    {
        /* Debug level change */
        when( efTestAndClear(debug_flag_mon) )
        {
            sprintf(msg,"Debug level changed to %d",debug_flag);
            DEBUG_PRINT(1,msg);
        } state idle

        /* Local energy change */
        when( efTestAndClear(localenergy_mon) )
        {
            PVPUT(selectenergy,0);
            epicsThreadSleep(PVPUT_WAIT);
            efClear(selectenergy_mon);

            sprintf(msg,"local energy changed to %f",localenergy);
            DEBUG_PRINT(2,msg);
        } state recalcBank

        /* Energy select (local/monochromator) change */
        when( efTestAndClear(selectenergy_mon) )
        {
            if( selectenergy )
            {
                PVPUT(localenergy, monoenergy);
                epicsThreadSleep(PVPUT_WAIT);
                efClear(localenergy_mon);

                bankctl = (bankctl) ? 3 : 0;
                pvPut(bankctl,SYNC);
            }

            sprintf(msg,"using energy local/monochromator changed to %d",selectenergy);
            DEBUG_PRINT(2,msg);
        } state idle

        /* Monochromator energy change */
        when (efTestAndClear(monoenergy_mon)) {
            PVPUT(displayEnergy,monoenergy);
            if (selectenergy) {
				PVPUT(localenergy,monoenergy);
            	epicsThreadSleep(PVPUT_WAIT);
            	efClear(localenergy_mon);
	            energytimeout = ENERGY_WAIT;
	            sprintf(msg,"monochromator energy changed to %f",monoenergy);
	            DEBUG_PRINT(2,msg);
			}
		} state idle

        /* Filter bank control change */
        when( efTestAndClear(bankctl_mon) )
        {
            sprintf(msg,"filter bank changed to %d, going to state bankControl",bankctl);
            DEBUG_PRINT(2,msg);
        } state bankControl

        /* Filter bank thicknesses change */
        when( efTestAndClear(f1_mon) || efTestAndClear(f2_mon) || efTestAndClear(f3_mon) ||
			efTestAndClear(f4_mon) )
        {
            sprintf(msg,"filters thickness changed to f1=%g, f2=%g, f3=%g, f3=%g",f1,f2,f3,f4);
            DEBUG_PRINT(2,msg);

            bankctl = (bankctl) ? 3 : 0;
            pvPut(bankctl,SYNC);
        } state idle

        /* Filter bank material index change */
        when( efTestAndClear(z1_mon) || efTestAndClear(z2_mon) || efTestAndClear(z3_mon) ||
			efTestAndClear(z4_mon))
        {
            sprintf(msg,"filters material index changed to z1=%d, z2=%d, z3=%d, z4=%d",z1,z2,z3,z4);
            DEBUG_PRINT(2,msg);

            bankctl = (bankctl) ? 3 : 0;
            pvPut(bankctl,SYNC);
        } state idle

        /* Filter bank "other" material index change */
        when( efTestAndClear(other1_mon) || efTestAndClear(other2_mon) ||
			efTestAndClear(other3_mon) || efTestAndClear(other4_mon))
        {
            sprintf(msg,"filters material index changed to other1='%s', o2='%s', o3='%s', o4='%s'",
				other1,other2,other3,other4);
            DEBUG_PRINT(2,msg);
			PVPUT(otherLegal1, isLegalOther(other1));
			PVPUT(otherLegal2, isLegalOther(other2));
			PVPUT(otherLegal3, isLegalOther(other3));
			PVPUT(otherLegal4, isLegalOther(other4));
            bankctl = (bankctl) ? 3 : 0;
            pvPut(bankctl,SYNC);
        } state idle

        /* Filter bit change */
        when( efTestAndClear(b4_mon) || efTestAndClear(b3_mon) || efTestAndClear(b2_mon) ||
			efTestAndClear(b1_mon) )
        {
            sprintf(msg,"filter bits changed to (%1.1d%1.1d%1.1d%1.1d), going to state filterBits",
				b4,b3,b2,b1);
            DEBUG_PRINT(2,msg);
        } state filterBits

        /* Filter position change */
        when( efTestAndClear(filpos_mon) && bankctl )
        {
            pos = filpos;
            sprintf(msg,"filter position changed to %d, going to state filterPos",pos);
            DEBUG_PRINT(2,msg);
        } state filterPos

        when( delay(IDLE_WAIT) )
        {
            if( energytimeout > 0.0 && (energytimeout = energytimeout - IDLE_WAIT) <= 0.0 )
            {
                bankctl = (bankctl) ? 3 : 0;
                pvPut(bankctl,SYNC);

                DEBUG_PRINT(2,"energy timeout expired, force recalculation");
            }

        } state idle

		when (efTestAndClear(otherShow_mon)) {
			/* C defect fixed: C passed E.NELM as the count, writing past
			** showEnergy/showTransmission for a NELM above NUM_SHOW. */
			if (fillShowArrays(otherShow, (numShow < NUM_SHOW) ? numShow : NUM_SHOW,
					showEnergy, showTransmission) == 0) {
				pvPut(showEnergy);
				pvPut(showTransmission);
			}
		} state idle
    }

}


/*****************************************************************************
 ***                                C Functions                            ***
 *****************************************************************************/

%{


/* calculate absorption length of Al (microns) */
double AlAbsorptionLength(double keV)
{
    double Wcoef0[]={1.90195,-0.00120447,4.3745e-7,8.68635e-11,3.40793e-15,-1.05816e-19,5.83389e-25};
    double Wcoef1[]={-1625.33,0.328256,-2.68391e-5,1.26554e-9,-2.41557e-14,2.12864e-19,-7.28743e-25};
    double kink = 26797.5;
    double* pointer;
    double sum,power;
    double eV;
    int    i;

    eV = keV * 1000;    /* convert to eV */
    if (eV>60000.) eV = 60000.;
    pointer = (eV<kink) ? Wcoef0 : Wcoef1;
    sum = 0.;
    power=1.;
    for( i=0; i<7; i++ )
    {
        sum += pointer[i] * power;
        power *= eV;
    }
    return sum;
}


/* absorption length of Ti in microns, (= 1/mu) */
double TiAbsorptionLength(double keV)
{
    double eV;                  /* energy in eV */
    double mu;                  /* mu in 1/micron */
                                /* for Ti, Kedge = 4966.4,   Ledge = 563.7 */
    eV = keV * 1000.;           /* convert keV to eV */
    if( eV < 1e3 ) mu = 0;      /* this routine only good above 1000 eV */
    else if( eV < 4966.4 ) 		/* above L-edge, and below K-edge */
    {
        double c0=0.00092284;
        double c1=2.5891e+08;
        double powA=-2.6651;
        mu = c0+c1*pow(eV,powA);
    }
    else                        /* above the K-edge */
    {
        /* coefs={5.63768167444831e-05  24061652313.4169  -2.91380053083527  -0.268162843203489  3.74221014277593e-05  -1.05663835782997  -0.000570785180739491} */
        double offset = 5.63768167444831e-5;
        double amp = 24061652313.4169;
        double powB = -2.91380053083527;
        double intercept = -0.268162843203489;
        double slope = 3.74221014277593e-5;

        double ampExp = -1.05663835782997;
        double invTau = -0.000570785180739491;
        double extra = (eV < 6456) ? (intercept+slope*eV) : (ampExp*exp(invTau*eV));
        mu = (offset + amp*pow(eV,powB) + extra);
    }
    return( 1./mu );
}


/* calculation for glass, see the Igor experiment "glass absorption.pxp" for the coefficients.
 *
 *	SiO2	0.722	wt fraction
 *	Na2O	0.143
 *	CaO	0.064
 *	MgO	0.043
 *	Al2O3	0.012
 *	K2O	0.012
 *	SO2	0.003
 *	Fe2O3	0.0003
 */
/* absorption length of Glass in microns, (= 1/mu) */
double GlassAbsorptionLength(double keV)
{
    double absLength;           /* absorption length (micron) */
    double c0,c1,c2;
    double c3,c4,c5,c6,c7;      /* more coefficients for the last energy range */
    double keV2, keVn;          /* square of energy, and powers of energy */
    keV2 = keV*keV;

    if( keV < 2 )
        absLength = 0;          /* this routine only good above 2 keV */
    else if( keV < 2.472 )      /* below Sulphur K edge */
    {
        c0 = 0.5059463974;
        c1 = -0.1259565387;
        c2 = 0.01763933889;
        absLength = c0 + c1*keV + c2*keV2;
    }
    else if( keV < 3.6084 )     /* above Sulphur K and below Potassium K */
    {
        c0 = 0.4570603245;
        c1 = -0.08869920063;
        c2 = 0.01032934773;
        absLength = c0 + c1*keV + c2*keV2;
    }
    else if( keV < 4.0385 )     /* above Potassium K and below Calcium K */
    {
        c0 =  0.3708574258;
        c1 =  -0.04453063888;
        c2 =  3.979930821e-3;
        absLength = c0 + c1*keV + c2*keV2;
    }
    else if( keV < 7.112 )      /* above Calcium K and below Iron K */
    {
        c0 = 0.2830642538;
        c1 = -0.0223186563;
        c2 = 1.412011413e-3;
        absLength = c0 + c1*keV + c2*keV2;
    }
    else                        /* above Iron K */
    {
        c0 = 0.2715022686;
        c1 = -0.02428526798;
        c2 = 2.984228845e-3;
        c3 = -2.003675391e-4;
        c4 = 7.983398893e-6;
        c5 = -1.869726202e-7;
        c6 = 2.378962632e-9;
        c7 = -1.270082060e-11;
        absLength = c0 + c1*keV;
        keVn  = keV2; absLength += c2*keVn;
        keVn *= keV;  absLength += c3*keVn;
        keVn *= keV;  absLength += c4*keVn;
        keVn *= keV;  absLength += c5*keVn;
        keVn *= keV;  absLength += c6*keVn;
        keVn *= keV;  absLength += c7*keVn;
    }
    absLength *= keV*keV*keV;   /* finally scale by energy^3 */
    return( absLength );
}

int isLegalOther(char *s) {
	int i;
	for (i=0; i<NUM_SPECIES; i++) {
		if (strcmp(filtermat[i].name, s) == 0) break;
	}
	if (i >= NUM_SPECIES) return(0);
	return(1);
}

double OtherAbsorptionLength(double keV, char *species) {
	int i, j;
	double mu, frac, absLen;

	for (i=0; i<NUM_SPECIES; i++) {
		if (strcmp(filtermat[i].name, species) == 0) break;
	}
	if (i >= NUM_SPECIES) {
		/* printf("pf4.st: Filter material '%s' not found\n", species);*/
		return(0.);
	}
	/*printf("pf4.st: species='%s', index=%d\n", species, i);*/
	for (j=0; j<filtermat[i].numEntries; j++) {
		if (keV < filtermat[i].keV[j]) break;
	}
	/* C defect fixed: C interpolated on [j, j+1], the interval above the
	** energy, reading keV/mu[numEntries] past the table in its top interval.
	** This is calcTrans's [j-1, j] (filterDrive.st). */
	if ((j < 1) || (j >= filtermat[i].numEntries)) {
		/*printf("pf4.st: Energy '%f' not found in filter data\n", keV);*/
		return(0.);
	}
	/* mu is in cm^2/g, matdensity is g/cm^3, want absorption length in microns */
	frac = (keV - filtermat[i].keV[j-1]) / (filtermat[i].keV[j] - filtermat[i].keV[j-1]);
	mu = filtermat[i].mu[j-1] + frac * (filtermat[i].mu[j] - filtermat[i].mu[j-1]);
	absLen = 1/(matdensity[i]*mu);
	absLen *= 1.e4; /* cm to microns */
	return(absLen);
}

/* recalculate all of the filters and attenuations */
void RecalcFilters(double keV,short* bits,double* xmit,int d,int b,int z1,
                    int z2,int z3,int z4,double f1,double f2,double f3,double f4,
					char *other1, char *other2, char *other3, char *other4)
{
    double absLenAl,absLenTi,absLenGlass;   /* absorption length for this energy (micron) */
    double xAl,xTi,xGlass;                  /* thickneses of Al, Ti, and Glass (mm) */
	double xOther1, xOther2, xOther3, xOther4;
    int i;
	double absLenOther1=0, absLenOther2=0, absLenOther3=0, absLenOther4=0;

    if (d >= 10) printf("\nRecalcFilters: keV=%g\n",keV);
    absLenAl = AlAbsorptionLength(keV);
    absLenTi = TiAbsorptionLength(keV);
    absLenGlass = GlassAbsorptionLength(keV);
	if (z1==3) absLenOther1 = OtherAbsorptionLength(keV, other1);
	if (z2==3) absLenOther2 = OtherAbsorptionLength(keV, other2);
	if (z3==3) absLenOther3 = OtherAbsorptionLength(keV, other3);
	if (z4==3) absLenOther4 = OtherAbsorptionLength(keV, other4);
	/* C defect fixed: OtherAbsorptionLength answers an unknown species or an
	** energy outside its table with 0., which C divided by, reporting the
	** blade opaque without a word. Such a blade is reported and left out. */
	if (z1==3 && f1 > 0 && absLenOther1 == 0)
		printf("RecalcFilters: no data for Other blade 1 '%s' at %g keV; it is left out\n", other1, keV);
	if (z2==3 && f2 > 0 && absLenOther2 == 0)
		printf("RecalcFilters: no data for Other blade 2 '%s' at %g keV; it is left out\n", other2, keV);
	if (z3==3 && f3 > 0 && absLenOther3 == 0)
		printf("RecalcFilters: no data for Other blade 3 '%s' at %g keV; it is left out\n", other3, keV);
	if (z4==3 && f4 > 0 && absLenOther4 == 0)
		printf("RecalcFilters: no data for Other blade 4 '%s' at %g keV; it is left out\n", other4, keV);

    for (i=0;i<16;i++)
    {
        if (b)
        {
            xAl     = (z1==0 && i&1) ? f1 : 0;
            xAl    += (z2==0 && i&2) ? f2 : 0;
            xAl    += (z3==0 && i&4) ? f3 : 0;
            xAl    += (z4==0 && i&8) ? f4 : 0;
            xTi    =  (z1==1 && i&1) ? f1 : 0;
            xTi    += (z2==1 && i&2) ? f2 : 0;
            xTi    += (z3==1 && i&4) ? f3 : 0;
            xTi    += (z4==1 && i&8) ? f4 : 0;
            xGlass  = (z1==2 && i&1) ? f1 : 0;
            xGlass += (z2==2 && i&2) ? f2 : 0;
            xGlass += (z3==2 && i&4) ? f3 : 0;
            xGlass += (z4==2 && i&8) ? f4 : 0;
			xOther1 = (z1==3 && i&1) ? f1 : 0;
			xOther2 = (z2==3 && i&2) ? f2 : 0;
			xOther3 = (z3==3 && i&4) ? f3 : 0;
			xOther4 = (z4==3 && i&8) ? f4 : 0;
           if (d >= 10)
            {
                printf("       in RecalcFilters %3d, Aluminum = %g,   Titanium = %g,   Glass = %g\n",i,xAl,xTi,xGlass);
            }
            xmit[i] =  exp(-xAl*1000./absLenAl);
            xmit[i] *= exp(-xTi*1000./absLenTi);
            /* C defect fixed: below 2 keV absLenGlass is 0, and C multiplied
            ** every combination, glass or not, by exp(-0/0), NaN. */
            if (xGlass > 0) xmit[i] *= exp(-xGlass*1000./absLenGlass);
			if (xOther1 > 0 && absLenOther1 > 0) xmit[i] *= exp(-xOther1*1000./absLenOther1);
			if (xOther2 > 0 && absLenOther2 > 0) xmit[i] *= exp(-xOther2*1000./absLenOther2);
			if (xOther3 > 0 && absLenOther3 > 0) xmit[i] *= exp(-xOther3*1000./absLenOther3);
			if (xOther4 > 0 && absLenOther4 > 0) xmit[i] *= exp(-xOther4*1000./absLenOther4);
        }
        else
            xmit[i] =  1.;
        bits[i] = i;
    }
    if (b) sortDecreasing(d,xmit,bits,16);    /* sort by transmission, transmission and bits */
}


void sortDecreasing(int d,double* arr,short* bits,int n)
{
    int	ii, jj;
    double a;
    short b;

    if (d >= 9)
    {
        printf("       before sorting, bits=%d",bits[0]);
        for (ii=1; ii<n; ii++)
        {
            printf(",%d", bits[ii]);
        }
        printf("\n");
        if (d >= 10)
        {
            printf("                   transmit=%g",arr[0]);
            for (ii=1; ii<n/3; ii++)
            {
                printf(", %g", arr[ii]);
            }
            printf("\n");
        }
    }

    for (jj=1;jj<n;jj++)
    {
        a = arr[jj];
        b = bits[jj];
        ii = jj-1;
        while(ii>=0 && arr[ii]<a)
        {
            arr[ii+1] = arr[ii];
            bits[ii+1] = bits[ii];
            ii--;
        }
        arr[ii+1] = a;
        bits[ii+1] = b;
    }

    if (d >= 8)
    {
        printf("       after sorting, bits=%d",bits[0]);
        for (ii=1; ii<n; ii++)
        {
            printf(",%d", bits[ii]);
        }
        printf("\n");
        if (d >= 10)
        {
            printf("                   transmit=%g",arr[0]);
            for (ii=1; ii<n/3; ii++)
            {
                printf(", %g", arr[ii]);
            }
            printf("\n");
        }
    }
}


/* add up current thickness of all filters with material number Z */
double thickZ(int z,int b,int b1,int b2,int b3,int b4,int z1,int z2,int z3,
              int z4,double f1,double f2,double f3,double f4)
{
    double sum = 0.0;

    if( b )
    {
        sum += (b1 && (z1==z)) ? f1 : 0;
        sum += (b2 && (z2==z)) ? f2 : 0;
        sum += (b3 && (z3==z)) ? f3 : 0;
        sum += (b4 && (z4==z)) ? f4 : 0;
    }

    return sum;
}


/* find the arrays element contianing value, return index, arr[i]==value */
/*long numInArray(long N,short arr[],short value)*/
int numInArray(int N,short arr[],short value)
{
    int i;

    for( i=0; i<N; i++ ) if( value==arr[i] ) return i;
    return 0;
}

static int fillShowArrays(char *species, int N, float *E, float *T) {
	int i, j, k;

	for (i=0; i<NUM_SPECIES; i++) {
		if (strcmp(filtermat[i].name, species) == 0) break;
	}
	if (i >= NUM_SPECIES) {
		return(1);
	}
	/*printf("pf4.st: species='%s', index=%d\n", species, i);*/
	for (j=0; j<filtermat[i].numEntries; j++) {
		E[j] = filtermat[i].keV[j];
		T[j] = filtermat[i].mu[j];
	}
	k = j-1;
	for (; j<N; j++) {
		E[j] = filtermat[i].keV[k];
		T[j] = filtermat[i].mu[k];
	}
	return(0);
}

}%
