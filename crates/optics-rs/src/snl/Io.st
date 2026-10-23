program Io ("P=7idd:Io:,MONO=7id:HHLM:energy_get,VSC=7idd:scaler1")

/*
          usage (VxWorks command):
 ld < bin/Io.o
 seq &Io
          (or to over-ride the defaults)
 seq &Io, "P=7idd:Io:,MONO=7id:HHLM:energy_get,VSC=7idBdscaler1"

             to kill the running sequence program (VxWorks):
 td "Io"

#  P     = prefix for ion chamber variables		"7idd:Io:"
#  MONO  = monochromator energy (keV)			"7id:HHLM:energy_get"
#  VSC   = scaler to use for input			"7idd:scaler1"
*/

%%#include "epicsThread.h"

/* #include "snlMacros.h" */
#define ASSIGN(SIZE, LOCAL_VAR, PV) \
        SIZE LOCAL_VAR;             \
        assign LOCAL_VAR to PV;

#define ASSIGN_MONITOR(SIZE, LOCAL_VAR, PV) \
        ASSIGN(SIZE, LOCAL_VAR, PV)         \
        monitor LOCAL_VAR;

#define ASSIGN_MON_FLAG(SIZE, LOCAL_VAR, PV, LOCAL_FLAG) \
        ASSIGN_MONITOR(SIZE, LOCAL_VAR, PV)              \
        evflag LOCAL_FLAG;                               \
        sync   LOCAL_VAR LOCAL_FLAG;

#define PVPUT(PV,VAL)  {PV = (VAL);  pvPut(PV);}

/* C defect: TEXT was the format, so a scaler channel name with a % in it
 * read arguments sprintf was never given */
#define PVPUTSTR(MSG,TEXT)  {sprintf(MSG, "%s", TEXT); pvPut(MSG);}

#define DEBUG_PRINT(DEBUG_LEVEL, MSG)	\
	if (debug_flag >= DEBUG_LEVEL) {	\
	   printf("<%s,%d,%s> %s\n", __FILE__, __LINE__, SNLtaskName, MSG);	\
	   epicsThreadSleep(0.1);	\
	}

/*   ------------------------   assignments   ------------------------ */

#define debug_flag Io_debug

	/*
	 * connect up with PV for result
	 */

ASSIGN_MON_FLAG	(double,	Emono,	"{MONO}", Emono_mon)			/* energy of mono (keV) */

ASSIGN_MON_FLAG	(short,		cnt,"{VSC}.CNT",cnt_mon)				/* flag for end of counting */
ASSIGN_MONITOR	(double,	clock_rate,"{VSC}.FREQ")						/* clock rate used by scaler */
ASSIGN_MON_FLAG	(double,	ticks,"{VSC}.S1", ticks_mon)					/* clock pulses counted (sec = ticks/clock) */
ASSIGN_MONITOR	(double,	s2,"{VSC}.S2")							/* pulses in scaler channel 2 */
ASSIGN_MONITOR	(double,	s3,"{VSC}.S3")
ASSIGN_MONITOR	(double,	s4,"{VSC}.S4")
ASSIGN_MONITOR	(double,	s5,"{VSC}.S5")
ASSIGN_MONITOR	(double,	s6,"{VSC}.S6")
ASSIGN_MONITOR	(double,	s7,"{VSC}.S7")
ASSIGN_MONITOR	(double,	s8,"{VSC}.S8")
ASSIGN_MONITOR	(double,	s9,"{VSC}.S9")
ASSIGN_MONITOR	(double,	s10,"{VSC}.S10")
ASSIGN_MONITOR	(double,	s11,"{VSC}.S11")
ASSIGN_MONITOR	(double,	s12,"{VSC}.S12")
ASSIGN_MONITOR	(double,	s13,"{VSC}.S13")
ASSIGN_MONITOR	(double,	s14,"{VSC}.S14")
ASSIGN_MONITOR	(double,	s15,"{VSC}.S15")

ASSIGN			(string,	nm2,"{VSC}.NM2")							/* name of channel 2 */
ASSIGN			(string,	nm3,"{VSC}.NM3")
ASSIGN			(string,	nm4,"{VSC}.NM4")
ASSIGN			(string,	nm5,"{VSC}.NM5")
ASSIGN			(string,	nm6,"{VSC}.NM6")
ASSIGN			(string,	nm7,"{VSC}.NM7")
ASSIGN			(string,	nm8,"{VSC}.NM8")
ASSIGN			(string,	nm9,"{VSC}.NM9")
ASSIGN			(string,	nm10,"{VSC}.NM10")
ASSIGN			(string,	nm11,"{VSC}.NM11")
ASSIGN			(string,	nm12,"{VSC}.NM12")
ASSIGN			(string,	nm13,"{VSC}.NM13")
ASSIGN			(string,	nm14,"{VSC}.NM14")
ASSIGN			(string,	nm15,"{VSC}.NM15")

ASSIGN			(double,	flux,		"{P}flux")					/* photons/sec before ion chamber */
ASSIGN			(double,	ionPhotons,	"{P}ionPhotons")			/* photons absorbed in the active part of the ion chamber */
ASSIGN			(double,	ionAbs,		"{P}ionAbs")				/* transmission factor of ion chamber gas, absorbed is (1-ionAbs) */
ASSIGN			(double,	detector,	"{P}detector")				/* photons/sec at detector location */

ASSIGN_MON_FLAG	(short,		IoDebug,"{P}debug", IoDebug_mon)		/* debug flag */
ASSIGN_MON_FLAG	(short,		icChannel,"{P}scaler", icChannel_mon)	/* scaler channel to use for ion chamber [2-8] */
ASSIGN			(string,	icName,  "{P}scaler.DESC")
ASSIGN_MON_FLAG	(double,	E_using,"{P}E_using", E_using_mon)		/* energy of mono (keV) */
ASSIGN_MON_FLAG	(double,	VperA,"{P}VperA", VperA_mon)			/* Current gain of Kiethley */
ASSIGN_MON_FLAG	(double,	v2f,"{P}v2f", v2f_mon)					/* Voltage to frequency (Hz/V) */
ASSIGN_MON_FLAG	(double,	xN2,"{P}xN2", xN2_mon)					/* fraction of N2 in ion chamber (atm) */
ASSIGN_MON_FLAG	(double,	xAr,"{P}xAr", xAr_mon)					/* fraction of Argon in ion chamber (atm) */
ASSIGN_MON_FLAG	(double,	xHe,"{P}xHe", xHe_mon)					/* fraction of He in ion chamber (atm) */
ASSIGN_MON_FLAG	(double,	xAir,"{P}xAir", xAir_mon)				/* fraction of air in ion chamber (atm) */
ASSIGN_MON_FLAG	(double,	xCO2,"{P}xCO2", xCO2_mon)				/* fraction of CO2 in ion chamber (atm) */
ASSIGN_MON_FLAG	(double,	activeLen,"{P}activeLen", activeLen_mon)/* active length (mm) */
ASSIGN_MON_FLAG	(double,	deadFront,"{P}deadFront", deadFront_mon)/* dead length before active region (mm) */
ASSIGN_MON_FLAG	(double,	deadRear,"{P}deadRear", deadRear_mon)	/* dead length after active region (mm) */
ASSIGN_MON_FLAG	(double,	kapton1,"{P}kapton1", kapton1_mon)		/* Kapton before ion chamber (inches) */
ASSIGN_MON_FLAG	(double,	kapton2,"{P}kapton2",kapton2_mon)		/* Kapton after ion chamber (inches) */
ASSIGN_MON_FLAG	(double,	HePath,"{P}HePath", HePath_mon)			/* He path after ion chamber (mm) */
ASSIGN_MON_FLAG	(double,	airPath,"{P}airPath", airPath_mon)		/* Air path after ion chamber (mm) */
ASSIGN_MON_FLAG	(double,	Be,"{P}Be", Be_mon)						/* Be after ion chamber (inches) */
ASSIGN_MON_FLAG	(double,	dEff,"{P}efficiency", dEff_mon)			/* Detector efficiency */
ASSIGN_MON_FLAG	(short,		ArPcntr,"{P}ArPcntr", ArPcntr_mon)		/* using Argon Proportional counter */

char	new_msg[256];  
double	updateRate;			/* delay time between forced updates */
char	*SNLtaskName;
short	Io_debug;

%% #include <stdlib.h>
%% #include <math.h>
/* %% #include <taskLib.h> */
%% static void EvalFlux(double aln, double dln1, double dln2, double dHe, double dAir, double dBe, double energy);
%% static double photon(double cps, double work, double VtoF, double Volt_per_A, double aln, double deadL, double Kapton, int gasId, double energy);
%% static double absorb(int id, double energy);
%% static double absH(double energy);		/* mass absorption coefficient for Hydrogen */
%% static double absHe(double energy);
%% static double absBe(double energy);
%% static double absC(double energy);
%% static double absN(double energy);
%% static double absO(double energy);
%% static double absAr(double energy);
%% static double absArPhoto(double energy);

#define _HELIUM_	1
#define _NITROGEN_	2
#define _ARGON_		3
#define _AIR_		4
#define _BERYLLIUM_	5
#define _KAPTON_	6
#define _CO2_		7
#define CMperINCH	2.54
/* 
 * state set:  ionChamber
 */
ss ionChamber {

	state init {
		when () {
			SNLtaskName = macValueGet("name");
			/* C defect: without a name macro, as io.iocsh runs it, DEBUG_PRINT
			 * passes this NULL to %s */
			if (!SNLtaskName) SNLtaskName = "Io";
			Io_debug=0;
			DEBUG_PRINT(1, "init start");
			IoDebug=Io_debug;
			efClear(IoDebug_mon);
			updateRate = 10.0;				/* forced update time in seconds */
			pvGet(Emono);
			pvGet(VperA);
			pvGet(clock_rate);
			pvGet(ticks);
			pvGet(s2);
			pvGet(s3);
			pvGet(s4);
			pvGet(s5);
			pvGet(s6);
			pvGet(s7);
			pvGet(s8);
			pvGet(s9);
			pvGet(s10);
			pvGet(s11);
			pvGet(s12);
			pvGet(s13);
			pvGet(s14);
			pvGet(s15);

			PVPUT(icChannel,2)				/* likely scaler channel for ion chamber */
			PVPUT(VperA,100000000.)			/* likely setting for current gain */
			PVPUT(v2f,100000.)				/* for Nova V to F (Hz/V) */
			PVPUT(xAir,1.)					/* assume 1 atmosphere of air in chamber */
			PVPUT(xN2,0.)
			PVPUT(xAr,0.)
			PVPUT(xHe,0.)
			PVPUT(xCO2,0.)
			PVPUT(activeLen,60.)			/* assume CHESS ion chambers */
			PVPUT(deadFront,17.5)
			PVPUT(deadRear,17.5)
			PVPUT(kapton1,.001)				/* front window */
			PVPUT(kapton2,.001)				/* rear window */
			PVPUT(HePath,0.)				/* assume no air or He paths */
			PVPUT(airPath,0.)
			PVPUT(Be,0.)
			PVPUT(dEff,1.)					/* assume NaI(Tl) */
			PVPUT(ArPcntr,0)
			PVPUT(flux,0.)
			PVPUT(ionPhotons,0.)
			PVPUT(ionAbs,1.)
			PVPUT(detector,0.)

			PVPUT(E_using,Emono);
			efClear(Emono_mon);
			efClear(E_using_mon);
			efClear(VperA_mon);
			efClear(cnt_mon);
			efClear(ticks_mon);
			efClear(icChannel_mon);
			efClear(v2f_mon);
			efClear(xN2_mon);
			efClear(xAr_mon);
			efClear(xHe_mon);
			efClear(xAir_mon);
			efClear(xCO2_mon);
			efClear(activeLen_mon);
			efClear(deadFront_mon);
			efClear(deadRear_mon);
			efClear(kapton1_mon);
			efClear(kapton2_mon);
			efClear(HePath_mon);
			efClear(airPath_mon);
			efClear(Be_mon);
			efClear(dEff_mon);
			efClear(ArPcntr_mon);

			DEBUG_PRINT(1, "init complete");
		} state UpdateDisplay				/* update display */
	}


	state idle {							/* main program loop */
		when (efTestAndClear(IoDebug_mon)) {
			pvGet(IoDebug);
			Io_debug = IoDebug;
			efClear(IoDebug_mon);
			sprintf(new_msg, "changed debug flag to %d", Io_debug);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(E_using_mon)) {
			DEBUG_PRINT(1, "monochromator energy (pretend) changed");
			pvGet(E_using);
			efClear(E_using_mon);
		} state UpdateDisplay

		when (efTestAndClear(Emono_mon)) {
			DEBUG_PRINT(2, "monochromator (actual) changed");
			pvGet(Emono);
			PVPUT(E_using,Emono);
			efClear(E_using_mon);
			efClear(Emono_mon);
		} state UpdateDisplay

		when (efTestAndClear(VperA_mon)) {
			pvGet(VperA);
			efClear(VperA_mon);
			sprintf(new_msg, "changed, Current gain to =%g", VperA);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(ArPcntr_mon)) {
			pvGet(ArPcntr);
			efClear(ArPcntr_mon);
			if (ArPcntr) {
				dEff = 1. - exp( -(4*CMperINCH) * absArPhoto(E_using) );	/* photo-electric part of Argon */
				dEff *= exp( -(.005*CMperINCH) * absorb(_BERYLLIUM_,E_using) );	/* window on detector */
				pvPut(dEff);
				efClear(dEff_mon);
			}
			sprintf(new_msg, "changed, ArPcntr=%d,  set dEff = %g", ArPcntr,dEff);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(dEff_mon)) {
			pvGet(dEff);
			PVPUT(ArPcntr,0)										/* set efficiency, turn off check box */
			efClear(ArPcntr_mon);
			efClear(dEff_mon);
			sprintf(new_msg, "changed, dEff=%g", dEff);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(icChannel_mon) || efTestAndClear(v2f_mon)) {
			pvGet(icChannel);
			pvGet(v2f);
			efClear(icChannel_mon);
			efClear(v2f_mon);
			sprintf(new_msg, "changed, icChannel=%d, v2f=%g", icChannel,v2f);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(xN2_mon) || efTestAndClear(xHe_mon) || efTestAndClear(xAr_mon) || efTestAndClear(xAir_mon) || efTestAndClear(xCO2_mon)) {
			pvGet(xN2);
			pvGet(xHe);
			pvGet(xAr);
			pvGet(xAir);
			pvGet(xCO2);
			efClear(xN2_mon);
			efClear(xHe_mon);
			efClear(xAr_mon);
			efClear(xAir_mon);
			efClear(xCO2_mon);
			sprintf(new_msg, "changed, xN2=%g, xAr=%g, xHe=%g, xAir=%g, xCO2=%g", xN2,xAr,xHe,xAir,xCO2);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(activeLen_mon) || efTestAndClear(deadFront_mon) || efTestAndClear(deadRear_mon)) {
			pvGet(activeLen);
			pvGet(deadFront);
			pvGet(deadRear);
			efClear(activeLen_mon);
			efClear(deadFront_mon);
			efClear(deadRear_mon);
			sprintf(new_msg, "changed, activeLen=%g, deadFront=%g, deadRear=%g", activeLen,deadFront,deadRear);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(HePath_mon) || efTestAndClear(airPath_mon)) {
			pvGet(HePath);
			pvGet(airPath);
			efClear(HePath_mon);
			efClear(airPath_mon);
			sprintf(new_msg, "changed, HePath=%g, airPath=%g", HePath,airPath);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(kapton1_mon) || efTestAndClear(kapton2_mon) || efTestAndClear(Be_mon)) {
			pvGet(kapton1);
			pvGet(kapton2);
			pvGet(Be);
			efClear(kapton1_mon);
			efClear(kapton2_mon);
			efClear(Be_mon);
			sprintf(new_msg, "changed, kapton1=%g, kapton2=%g, Be=%g", kapton1,kapton2,Be);
			DEBUG_PRINT(1, new_msg);
		} state UpdateDisplay

		when (efTestAndClear(cnt_mon) || efTestAndClear(ticks_mon)) {
			efClear(cnt_mon);
			efClear(ticks_mon);
			sprintf(new_msg, "counter cycled");
			DEBUG_PRINT(2, new_msg);
		} state UpdateDisplay

		when (delay(updateRate)) {				/* after a wait, check chi just to be sure */
			DEBUG_PRINT(3, "periodic update");
		} state UpdateDisplay					/* update displayed calculation of sz, schi, sup */
	}


	state UpdateDisplay {
		when() {
			pvGet(E_using);
			efClear(E_using_mon);
			EvalFlux(activeLen,deadFront,deadRear,HePath,airPath,Be,E_using);
			if (icChannel==2) { pvGet(nm2); PVPUTSTR(icName,nm2) }
			if (icChannel==3) { pvGet(nm3); PVPUTSTR(icName,nm3) }
			if (icChannel==4) { pvGet(nm4); PVPUTSTR(icName,nm4) }
			if (icChannel==5) { pvGet(nm5); PVPUTSTR(icName,nm5) }
			if (icChannel==6) { pvGet(nm6); PVPUTSTR(icName,nm6) }
			if (icChannel==7) { pvGet(nm7); PVPUTSTR(icName,nm7) }
			if (icChannel==8) { pvGet(nm8); PVPUTSTR(icName,nm8) }
			if (icChannel==9) { pvGet(nm9); PVPUTSTR(icName,nm9) }
			if (icChannel==10) { pvGet(nm10); PVPUTSTR(icName,nm10) }
			if (icChannel==11) { pvGet(nm11); PVPUTSTR(icName,nm11) }
			if (icChannel==12) { pvGet(nm12); PVPUTSTR(icName,nm12) }
			if (icChannel==13) { pvGet(nm13); PVPUTSTR(icName,nm13) }
			if (icChannel==14) { pvGet(nm14); PVPUTSTR(icName,nm14) }
			if (icChannel==15) { pvGet(nm15); PVPUTSTR(icName,nm15) }
			pvPut(flux);
			pvPut(ionPhotons);
			pvPut(ionAbs);
			pvPut(detector);
			sprintf(new_msg, "   in Updatedisplay, flux = %g,   detector = %g", flux,detector);
			DEBUG_PRINT(3, new_msg);
		} state idle
	}

}





%{


void EvalFlux(
double	aln,		/* active length of the ion chamber (cm) */
double	dln1,		/* dead length of the chamber in front of the active length */
double	dln2,		/* dead length of the chamber in after of the active length */
double	dHe,		/* Helium path after ion chamber (mm on input) */
double	dAir,		/* air path after ion chamber (mm on input) */
double	dBe,		/* thickness of Be betwen ion chamber and detector (inches on input) */
double	energy)		/* enegy  (KeV) */
{
	/* local variables */
	double	AirAbs;
	double	HeAbs;
	double	KapAbs;
	double	BeAbs;
	double	fillAbsorb;		/* absorption of the fill gasses */
	double	frontAbs;
	double	cps;			/* counts per second */

	/* Convert all internal thickness and distnces to cm */
	aln = aln / 10.;		/* convert mm to cm */
	dln1 = dln1 / 10.;
	dln2 = dln2 / 10.;
	dHe = dHe / 10.;
	dAir = dAir / 10.;
	dBe = dBe * CMperINCH;		/* convert inches to cm */

	switch (icChannel) {
	case 2: cps = s2; break;
	case 3: cps = s3; break;
	case 4: cps = s4; break;
	case 5: cps = s5; break;
	case 6: cps = s6; break;
	case 7: cps = s7; break;
	case 8: cps = s8; break;
	case 9: cps = s9; break;
	case 10: cps = s10; break;
	case 11: cps = s11; break;
	case 12: cps = s12; break;
	case 13: cps = s13; break;
	case 14: cps = s14; break;
	case 15: cps = s15; break;
	default:
		cps = 0;
	}
	cps /= (ticks/clock_rate);

	/* Call the routine to calculate the flux (photons/sec just before ion chamber) */
	flux = 0.;
	flux += xHe * photon(cps,29.6, v2f,VperA,aln,dln1,kapton1,_HELIUM_,energy);
	flux += xN2 * photon(cps,36.3, v2f,VperA,aln,dln1,kapton1,_NITROGEN_,energy);
	flux += xAr * photon(cps,24.4, v2f,VperA,aln,dln1,kapton1,_ARGON_,energy);
	flux += xAir* photon(cps,35.36,v2f,VperA,aln,dln1,kapton1,_AIR_,energy);
	flux += xCO2* photon(cps,35.36,v2f,VperA,aln,dln1,kapton1,_CO2_,energy);

	/* Calculate absorption from front of ion chamber to detector */
	AirAbs = exp(-dAir*absorb(_AIR_,energy));
	HeAbs = exp(-dHe * absorb(_HELIUM_,energy) );
	KapAbs = exp(-(kapton2+kapton1)*CMperINCH * absorb(_KAPTON_,energy) );
	BeAbs = exp(-dBe * absorb(_BERYLLIUM_,energy) );
	fillAbsorb = absorb(_HELIUM_,energy)*xHe + absorb(_NITROGEN_,energy)*xN2 + absorb(_ARGON_,energy)*xAr + absorb(_AIR_,energy)*xAir + absorb(_CO2_,energy)*xCO2;

	ionAbs = exp(-(dln1+dln2+aln) * fillAbsorb);						/* transmission factor of ion chamber gas, absorbed is (1-ionAbs) */
	detector = flux * AirAbs * HeAbs * KapAbs * BeAbs * ionAbs * dEff;	/* photons/sec at detector location */
	frontAbs =  exp( -CMperINCH*kapton1*absorb(_KAPTON_,energy) - dln1*fillAbsorb );/* absorption before acitve region */
	ionPhotons = (1.- exp(-aln * fillAbsorb)) * frontAbs * flux;		/* number of photons absorbed in active region */
}


double	photon(		/* routine to calculate the xray flux in photons/sec for an ion chamber */
double	cps,		/* counts in scaler (fropm VtoF) divided by count time (sec) */
double	work,		/* work function in eV/ion pair for the ion chamber gas */
double	VtoF,		/* pulses/sec/volt output by the V-F converter */
double	Volt_per_A,	/* current amplifier gain in volts/amp */
double	aln,		/* active length of the ion chamber (cm) */
double	deadL,		/* dead length of the chamber in front of the active length */
double	Kapton,		/* kapton thickness on upstream end of chamber in inches */
int		gasId,		/* id of gas (see #defines -- e.g., "_HELIUM_" above) */
double	energy)		/* enegy  (KeV) */
{
/*	flux is calculated at a point just upstream of the first kapton window of the chamber.*/

	double	convHe = 6.647;
	double	rhoHe  = 0.0001785;
	double	convN  = 23.26;
	double	rhoN   = 0.00125;
	double	convAr = 66.32;
	double	rhoAr  = 0.001784;
	double	edgeAr = 3.202;
	double	sum,E1,E2,E3;
	double	part1,part2,part3;
	double	photo;

	static double	aHe[4]  = { 6.06488, -3.29055, -0.107256, 0.0144465 };
	static double	aN[4]   = { 11.2765, -2.65400, -0.200445, 0.0200765 };
	static double	aAr1[4] = { 13.9491, -1.82276, -0.328827, 0.0274382 };
	static double	aAr2[4] = { 12.2960, -2.63279, -0.073660, 0.0       };
	static double	aO[4]   = { 11.7130, -2.57229, -0.205893, 0.0199244 };
	static double	aC[4]   = { 10.6879, -2.71400, -0.200530, 0.0207248 };

	E1 = log(energy);
	E2 = E1 * E1;
	E3 = E2 * E1;
	photo = 0;						/* photo-electron absorption length  (1/cm) */

	/* 	Calculate the photo-electric cross section for the ion chamber gas */
	if (gasId == _HELIUM_) {					/* Photo-electric cross section for Helium */
		sum = exp(aHe[0] + aHe[1]*E1 + aHe[2]*E2 + aHe[3]*E3);
		photo = sum * rhoHe / convHe;
	}
	else if (gasId == _NITROGEN_) {			/* Photo-electric cross section for Nitrogen */
		sum = exp(aN[0] + aN[1]*E1 + aN[2]*E2 + aN[3]*E3);
		photo = sum * rhoN / convN;
	}
	else if (gasId == _ARGON_) {			/* Photo-electric cross section for Argon */
		if (E1 > edgeAr) sum = exp(aAr1[0] + aAr1[1]*E1 + aAr1[2]*E2 + aAr1[3]*E3);
		else sum = exp(aAr2[0] + aAr2[1]*E1 + aAr2[2]*E2 + aAr2[3]*E3);
		photo = sum * rhoAr / convAr;
	}
	else if (gasId == _AIR_) {			/* Photo-electric cross section for Air .79N2, .20O2, .01Ar */
		sum = exp(aN[0] + aN[1]*E1 + aN[2]*E2 + aN[3]*E3);
		photo = sum * 0.000922 / convN;
		sum = exp(aO[0] + aO[1]*E1 + aO[2]*E2 + aO[3]*E3);
		photo += sum * 0.000266 / convN;			/* should use convO here, but I do not know it */
		if (E1 > edgeAr) sum = exp(aAr1[0] + aAr1[1]*E1 + aAr1[2]*E2 + aAr1[3]*E3);
		else sum = exp(aAr2[0] + aAr2[1]*E1 + aAr2[2]*E2 + aAr2[3]*E3);
		photo += sum * 1.66E-5 / convAr;
	}
	else if (gasId == _CO2_) {			/* Photo-electric cross section for CO2 */
		sum = exp(aC[0] +  aC[1]*E1 +  aC[2]*E2 +  aC[3]*E3);
		photo = sum * 0.0005396 / convN;			/* should use convC here, but I do not know it */
		sum = exp(aO[0] +  aO[1]*E1 +  aO[2]*E2 +  aO[3]*E3);
		photo += sum * 0.0014374 / convN;			/* should use convO here, but I do not know it */
	}

	/*	Parts 1,2, and 3 calculate the flux in photons/sec */
	part1 = cps * work / (1.602E-19 * VtoF * Volt_per_A * energy * 1000.);
	part2 = 1 - exp(-photo * aln);
	part3 = exp(absorb(gasId,energy) * deadL) * exp(absorb(_KAPTON_,energy) * Kapton * CMperINCH);

	return (part1 * part3 / part2);
}


double	absorb(				/* linear absorption coefficient (1/cm) */
int		id,					/* integer specifying type of absorber */
double	energy)				/* enegy  (KeV) */
{
/*
*	This routine calculates the absorbtion coefficient in 1/cm for
*	a chosen material and a given photon energy.
*
*	Variable definitions:
*
*	id = integer value which specifies the xray absorber
*	
*	energy = photon energy in KeV
*/
	double	rhoH;				/* for Hydrogen, density  (gm/cm^3) */
	double	rhoHe;				/*  "  Helium */
	double	rhoBe;				/*  "  Beryllium */
	double	rhoC;				/*  "  Carbon */
	double	rhoN;				/*  "  Nitrogen */
	double	rhoO;				/*  "  Oxygen */
	double	rhoAr;				/*  "  Argon */

	switch (id) {
	case _HELIUM_:								/* Helium */
		rhoHe = 0.0001785;
		return(absHe(energy)*rhoHe);
	case _BERYLLIUM_:								/* Beryllium */
		rhoBe = 1.848;
		return(absBe(energy)*rhoBe);
	case _NITROGEN_:								/* Nitrogen */
		rhoN  = 0.00125;
		return(absN(energy)*rhoN);
	case _AIR_:								/* Dry Air */
		rhoN  = 0.000922;				/* 79% N2, 20% O2 AND 1% Ar by volume*/
		rhoO  = 0.000266;				/* rho Air = 1.2047E-3 gm/cc at 20C */
		rhoAr = 1.66E-5;
		return(absN(energy)*rhoN + absO(energy)*rhoO + absAr(energy)*rhoAr);
	case _ARGON_:								/* Argon */
		rhoAr = 0.001784;
		return(absAr(energy)*rhoAr);
	case _KAPTON_:								/* Kapton */
		rhoC  = 0.981;					/* C22 H10 O5 N2 */
		rhoH  = 0.037;					/* MolWt=382 gm/mole    rho=1.42 gm/cc */
		rhoO  = 0.297;
		rhoN  = 0.105;
		return(absC(energy)*rhoC + absH(energy)*rhoH + absO(energy)*rhoO + absN(energy)*rhoN);
	case _CO2_:								/* CO2, carbon dioxide density = 0.001977 g/ccn */
		rhoC = 0.0005396;				/* 12.01115/44.00995 * 0.001977 */
		rhoO = 0.0014374;				/* 2*15.9994/44.00995 * 0.001977 */
		return(absC(energy)*rhoC + absO(energy)*rhoO);
	}
	return 0;
}


/***********************************************************************/


double	absH(				/* Mass absorption coefficient for Hydrogen (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;			/* photo-electron scattering */
	double	coherent;		/* coherent scattering */
	double	compton;		/* compton scattering */
	static double	a[4] = { 2.44964E+00,-3.34953E+00,-4.71370E-02, 7.09962E-03 };
	static double	b[4] = {-1.19075E-01,-9.37086E-01,-2.00538E-01, 1.06587E-02 };
	static double	c[4] = {-2.15772E+00, 1.32685E+00,-3.05620E-01, 1.85025E-02 };

/*	rho = 8.987E-5;	*/
	conv = 1.674;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3);	/* Photoelectron */
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);/* Coherent */
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);	/* Compton */
	return( (photo+coherent+compton)/conv);				/* Mass absorption coef (cm^2/gm) */
}


/***********************************************************************/


double	absHe(				/* Mass absorption coefficient for Helium (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	coherent;
	double	compton;
	static double	a[4] = { 6.06488E+00,-3.29055E+00,-1.07256E-01, 1.44465E-02 };
	static double	b[4] = { 1.04768E+00,-8.51805E-02,-4.03527E-01, 2.69398E-02 };
	static double	c[4] = {-2.56357E+00, 2.02536E+00,-4.48710E-01, 2.79691E-02 };

/*	rho = 1.785E-04;	*/
	conv = 6.647;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3);
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);
	return((photo+coherent+compton)/conv);
}


/***********************************************************************/


double	absBe(				/* Mass absorption coefficient for Beryllium (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	coherent;
	double	compton;
	static double	a[4] = { 9.04511E+00,-2.83487E+00,-2.10021E-01, 2.29526E-02 };
	static double	b[4] = { 2.00860E+00,-4.61920E-02,-3.37018E-01, 1.86939E-02 };
	static double	c[4] = {-6.90079E-01, 9.46448E-01,-1.71142E-01, 6.51413E-03 };

/*	rho = 1.848;	*/
	conv = 14.96;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3);
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);
	return((photo+coherent+compton)/conv);
}


/***********************************************************************/


double	absC(				/* Mass absorption coefficient for Carbon (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	coherent;
	double	compton;
	static double	a[4] = { 1.06879E+01,-2.71400E+00,-2.00530E-01, 2.07248E-02 };
	static double	b[4] = { 3.10861E+00,-2.60580E-01,-2.71974E-01, 1.35181E-02 };
	static double	c[4] = {-9.82878E-01, 1.46693E+00,-2.93743E-01, 1.56005E-02 };

/*	rho = 1.580;	*/
	conv = 19.94;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3);
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);
	return((photo+coherent+compton)/conv);
}


/***********************************************************************/


double	absN(				/* Mass absorption coefficient for Nitrogen (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	coherent;
	double	compton;
	static double	a[4] = { 1.12765E+01,-2.65400E+00,-2.00445E-01, 2.00765E-02 };
	static double	b[4] = { 3.47760E+00,-2.15762E-01,-2.88874E-01, 1.51312E-02 };
	static double	c[4] = {-1.23693E+00, 1.74510E+00,-3.54660E-01, 1.98705E-02 };

/*	rho = 0.001250;	*/
	conv = 23.26;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3);
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);
	return((photo+coherent+compton)/conv);
}


/***********************************************************************/


double	absO(				/* Mass absorption coefficient for Oxygen (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	coherent;
	double	compton;
	static double	a[4] = { 1.17130E+01,-2.57229E+00,-2.05893E-01, 1.99244E-02 };
	static double	b[4] = { 3.77239E+00,-1.48539E-01,-3.07124E-01, 1.67303E-02 };
	static double	c[4] = {-1.73679E+00, 2.17686E+00,-4.49050E-01, 2.64733E-02 };

/*	rho = 0.001429;	*/
	conv = 26.57;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3);
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);
	return((photo+coherent+compton)/conv);
}


/***********************************************************************/


double	absAr(				/* Mass absorption coefficient for Argon (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	coherent;
	double	compton;
	double	edge;			/* K-edge in KeV */
	static double	a1[4] = { 1.39491E+01,-1.82276E+00,-3.28827E-01, 2.74382E-02 };
	static double	a2[4] = { 1.22960E+01,-2.63279E+00,-7.36600E-02, 0.};
	static double	b[4]  = { 5.21079E+00, 1.35618E-01,-3.47214E-01, 1.84333E-02 };
	static double	c[4]  = {-6.82105E-01, 1.74279E+00,-3.17646E-01, 1.56467E-02 };

/*	rho = 0.001784;	*/
	conv = 66.32;
	edge = 3.202;

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	if (energy>edge) photo = exp( a1[0] + a1[1]*E1 + a1[2]*E2 + a1[3]*E3);
	else photo = exp( a2[0] + a2[1]*E1 + a2[2]*E2 + a2[3]*E3);
	coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3);
	compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3);
	return((photo+coherent+compton)/conv);
}


double	absArPhoto(			/* Mass absorption coeff for photo-electric part of Argon (cm^2/gm) */
double	energy)				/* enegy  (KeV) */
{
	double	conv;
	double	E1,E2,E3;
	double	photo;
	double	rho;
	double	edge;
	static double	aAr1[4] = {13.9491, -1.82276, -0.328827, 0.0274382 };
	static double	aAr2[4] = {12.2960, -2.63279, -0.073660, 0.0 };

	conv = 66.32;
	rho = 0.001784;			/* density of Ar */
	edge = 3.202;			/* K-edge in KeV */

	E1 = log(energy);
	E2 = E1*E1;
	E3 = E2*E1;

	if (energy>edge) photo = exp( aAr1[0] + aAr1[1]*E1 + aAr1[2]*E2 + aAr1[3]*E3);
	else photo = exp( aAr2[0] + aAr2[1]*E1 + aAr2[2]*E2 + aAr2[3]*E3);

	return (photo / conv * rho);
}


}%



