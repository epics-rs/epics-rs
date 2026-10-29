program hrCtl  ("P=xxx:, N=1, M_PHI1=m9, M_PHI2=m10")

/* SNL compiler options */
option +r;

/* arguments on the command line  :
 * 
 * ex.  seq &hrCtl, "P=xxx:, N=1, M_PHI1=m9, M_PHI2=m10"
 *
 *      P       - prefix     
 *      M_PHI1 - motor record for Theta 1
 *      M_PHI2 - motor record for Theta 2
 */


/*   EDITS & MODIFICATIONS ...
 *
 *
 * 07-16-96 tmm   created, based on kohzuCtl.st
 * 08-19-98 tmm   v2.0 Made reentrant (uglified escaped C code)
 * 08-26-98 tmm   v3.0 use ...Moving busy record. (Database asserts "busy", we
 *                assert "done"; this allows dbPutNotify to detect completion).
 * 02-24-00 tmm   v3.1 Test database's busy flag explicitly.  If someone tells
 *                the monochromator to go to a position it already is at, the
 *                only CA monitor we'll get is the busy flag.
 * 04-09-02 tmm   v3.2 Added Geom variable to select between nested (old) and
 *                symmetric (new) monochromator geometries.  In nested geometry,
 *                first and last xtal surfaces are of (H1,K1,L1), second and
 *                third are of (H2,K2,L2).  In symmetric geometry, first and
 *                second surfaces are (H1,K1,L1), third and fourth are
 *                (H2,K2,L2).
 * 08-05-02 tmm   v3.3 Fixed symmetric geometry.  Fixed phi2 initialization.
 *                Modified transition into "Set" mode so that phi offsets are
 *                automatically recalculated.  "Set" no longer wipes world
 *                offset.
 * 12-15-03 tmm   v3.4 Converted to EPICS 3.14 OSI.
 */

#define MAX(a,b) ((a)>(b)?(a):(b))
#define MIN(a,b) ((a)<(b)?(a):(b))

/* General Purpose PV's used by sequence */
short hrCtlDebug;
assign  hrCtlDebug to "{P}HR{N}CtlDebug"; 
monitor hrCtlDebug;

string  seqMsg1;
assign  seqMsg1 to "{P}HR{N}_SeqMsg1SI"; 

string  seqMsg2;
assign  seqMsg2 to "{P}HR{N}_SeqMsg2SI"; 

short   opAlert;
assign  opAlert to "{P}HR{N}_AlertBO";

short   opAck;
assign  opAck to "{P}HR{N}_OperAckBO";
monitor opAck;

short   putVals; 
assign  putVals to "{P}HR{N}_PutBO";
monitor putVals;

short   autoMode;
assign  autoMode to "{P}HR{N}_ModeBO";
monitor autoMode;

short   opMode;
assign  opMode to "{P}HR{N}_Mode2MO";
monitor opMode;
evflag  opMode_mon;
sync    opMode opMode_mon;

short   Geom;
assign  Geom to "{P}HR{N}_GeomMO";
monitor Geom;
evflag  Geom_mon;
sync    Geom Geom_mon;

short   hr_Moving; 
assign  hr_Moving to "{P}HR{N}_Moving";
monitor	hr_Moving;
evflag  hr_Moving_mon;
sync    hr_Moving hr_Moving_mon;

short	hr_Done;

/* crystal 1 configuration */
double  H1;
assign  H1 to "{P}HR{N}_H1AO";
monitor H1;
evflag  H1_mon;
sync    H1 H1_mon;

double  K1;
assign  K1 to "{P}HR{N}_K1AO";
monitor K1;
evflag  K1_mon;
sync    K1 K1_mon;

double  L1;
assign  L1 to "{P}HR{N}_L1AO";
monitor L1;
evflag  L1_mon;
sync    L1 L1_mon;

double  A1;
assign  A1 to "{P}HR{N}_A1AO";
monitor A1;
evflag  A1_mon;
sync    A1 A1_mon;

double  D1;
assign  D1 to "{P}HR{N}_2d1AO";

/* crystal 2 configuration */
double  H2;
assign  H2 to "{P}HR{N}_H2AO";
monitor H2;
evflag  H2_mon;
sync    H2 H2_mon;

double  K2;
assign  K2 to "{P}HR{N}_K2AO";
monitor K2;
evflag  K2_mon;
sync    K2 K2_mon;

double  L2;
assign  L2 to "{P}HR{N}_L2AO";
monitor L2;
evflag  L2_mon;
sync    L2 L2_mon;

double  A2;
assign  A2 to "{P}HR{N}_A2AO";
monitor A2;
evflag  A2_mon;
sync    A2 A2_mon;

double  D2;
assign  D2 to "{P}HR{N}_2d2AO";

/* E (energy) and Lambda (wavelength) */
double  E;
assign  E to "{P}HR{N}_EAO";
monitor E;
evflag  E_mon;
sync    E E_mon;

double  EHi;
assign  EHi to "{P}HR{N}_EAO.DRVH";

double  ELo;
assign  ELo to "{P}HR{N}_EAO.DRVL";

double  ERdbk;
assign  ERdbk to "{P}HR{N}_ERdbkAO";

double  lambda;
double  lambdaNom;
assign  lambda to "{P}HR{N}_LambdaAO";
monitor lambda;
evflag  lambda_mon;
sync    lambda lambda_mon;

double  lambdaHi;
assign  lambdaHi to "{P}HR{N}_LambdaAO.DRVH";

double  lambdaLo;
assign  lambdaLo to "{P}HR{N}_LambdaAO.DRVL";

double  lambdaRdbk;
assign  lambdaRdbk to "{P}HR{N}_LambdaRdbkAO";

/* theta 1 (angle between beam and crystal planes) */
double  theta1;
assign  theta1 to "{P}HR{N}_Theta1AO";
monitor theta1;
evflag  theta1_mon;
sync    theta1 theta1_mon;

double  theta1Hi;
assign  theta1Hi to "{P}HR{N}_Theta1AO.DRVH";

double  theta1Lo;
assign  theta1Lo to "{P}HR{N}_Theta1AO.DRVL";

double  theta1Rdbk;
assign  theta1Rdbk to "{P}HR{N}_Theta1RdbkAO";

/* phi 1 (angle between cyrstal planes and horizontal) */
double  phi1;
assign  phi1 to "{P}HR{N}_phi1AO";
monitor phi1;
evflag  phi1_mon;
sync    phi1 phi1_mon;

double  phi1Off;
assign  phi1Off to "{P}HR{N}_phi1OffAO";
monitor phi1Off;
evflag  phi1Off_mon;
sync    phi1Off phi1Off_mon;

double  phi1Hi;
assign  phi1Hi to "{P}HR{N}_phi1AO.DRVH";

double  phi1Lo;
assign  phi1Lo to "{P}HR{N}_phi1AO.DRVL";

double  phi1Rdbk;
assign  phi1Rdbk to "{P}HR{N}_phi1RdbkAO";

/* theta 2 (angle between beam and crystal planes) */
double  theta2;
assign  theta2 to "{P}HR{N}_Theta2AO";
monitor theta2;
evflag  theta2_mon;
sync    theta2 theta2_mon;

double  theta2Nom;

double  theta2Hi;
assign  theta2Hi to "{P}HR{N}_Theta2AO.DRVH";

double  theta2Lo;
assign  theta2Lo to "{P}HR{N}_Theta2AO.DRVL";

double  theta2Rdbk;
assign  theta2Rdbk to "{P}HR{N}_Theta2RdbkAO";

/* phi 2 (angle between cyrstal planes and horizontal) */
double  phi2;
assign  phi2 to "{P}HR{N}_phi2AO";
monitor phi2;
evflag  phi2_mon;
sync    phi2 phi2_mon;

double  phi2Off;
assign  phi2Off to "{P}HR{N}_phi2OffAO";
monitor phi2Off;
evflag  phi2Off_mon;
sync    phi2Off phi2Off_mon;

double  phi2Hi;
assign  phi2Hi to "{P}HR{N}_phi2AO.DRVH";

double  phi2Lo;
assign  phi2Lo to "{P}HR{N}_phi2AO.DRVL";

double  phi2Rdbk;
assign  phi2Rdbk to "{P}HR{N}_phi2RdbkAO";

/*The following are "soft" PV's that contain motor information that is
  either echoed to or echoed from the actual motor records. The reason
  for these PV's is so the medm display can be independent from the
  motor PV names */

string  phi1MotName;
assign  phi1MotName to "{P}HR{N}_phi1PvSI"; 

string  phi2MotName;
assign  phi2MotName to "{P}HR{N}_phi2PvSI"; 

double  phi1MotCmdEcho;
assign  phi1MotCmdEcho to "{P}HR{N}_phi1CmdAO";

double  phi2MotCmdEcho;
assign  phi2MotCmdEcho to "{P}HR{N}_phi2CmdAO";

double  phi1MotRdbkEcho;
assign  phi1MotRdbkEcho to "{P}HR{N}_phi1RdbkAI";

double  phi2MotRdbkEcho;
assign  phi2MotRdbkEcho to "{P}HR{N}_phi2RdbkAI";

short   phi1DmovEcho;
assign  phi1DmovEcho to "{P}HR{N}_phi1DmovBI";

short   phi2DmovEcho;
assign  phi2DmovEcho to "{P}HR{N}_phi2DmovBI";

/* Confusion may result between the phi* motor variables and the
   user-entered phi*. To eliminate confusion, variables associated
   with the phi* motor are named phi*MotXXXXX, unless it is clear they
   refer to a motor.
*/ 
 
short   phi1MotStop;
assign  phi1MotStop to "{P}{M_PHI1}.STOP";

short   phi2MotStop;
assign  phi2MotStop to "{P}{M_PHI2}.STOP";

short   phi1Dmov;
assign  phi1Dmov to "{P}{M_PHI1}.DMOV";
monitor phi1Dmov;
evflag  phi1Dmov_mon;
sync    phi1Dmov phi1Dmov_mon;

short   phi2Dmov;
assign  phi2Dmov to "{P}{M_PHI2}.DMOV";
monitor phi2Dmov;
evflag  phi2Dmov_mon;
sync    phi2Dmov phi2Dmov_mon;

/* LIMIT SWITCHES */
short   phi1Hls;
short   phi1Lls;

short   phi2Hls;
short   phi2Lls;


/* Use the following assignments for the real system */
%%  static short  hrCtl_LS_TEST = 0;
assign  phi1Hls to "{P}{M_PHI1}.HLS";
assign  phi1Lls to "{P}{M_PHI1}.LLS";
assign  phi2Hls to "{P}{M_PHI2}.HLS";
assign  phi2Lls to "{P}{M_PHI2}.LLS";

/* Use the following assignment to simulate switches with soft PV's */
/*
%%  static short  hrCtl_LS_TEST = 1;
assign  phi1Hls to "{P}HR{N}_phi1SimHlsBI";
assign  phi1Lls to "{P}HR{N}_phi1SimLlsBI";
assign  phi2Hls to "{P}HR{N}_phi2SimHlsBI";
assign  phi2Lls to "{P}HR{N}_phi2SimLlsBI";
*/

monitor phi1Hls;
monitor phi1Lls;

monitor phi2Hls;
monitor phi2Lls;

/* C defect: monitored the values calcMovements puts here, so moveHR took
 * what the PV held by then: the late echo of an earlier pass's put, or a
 * user's edit landing after the pass assigned them. The move goes where
 * calcMovements put. */
double  phi1MotDesired;
assign  phi1MotDesired to "{P}HR{N}_phi1SetAO";

double  phi2MotDesired;
assign  phi2MotDesired to "{P}HR{N}_phi2SetAO";

double  phi1MotHiLim;
assign  phi1MotHiLim to "{P}{M_PHI1}.HLM";
monitor phi1MotHiLim;
evflag  phi1MotHiLim_mon;
sync    phi1MotHiLim phi1MotHiLim_mon;

double  phi1MotLoLim;
assign  phi1MotLoLim to "{P}{M_PHI1}.LLM";
monitor phi1MotLoLim;
evflag  phi1MotLoLim_mon;
sync    phi1MotLoLim phi1MotLoLim_mon;

double  phi2MotHiLim;
assign  phi2MotHiLim to "{P}{M_PHI2}.HLM";
monitor phi2MotHiLim;
evflag  phi2MotHiLim_mon;
sync    phi2MotHiLim phi2MotHiLim_mon;

double  phi2MotLoLim;
assign  phi2MotLoLim to "{P}{M_PHI2}.LLM";
monitor phi2MotLoLim;
evflag  phi2MotLoLim_mon;
sync    phi2MotLoLim phi2MotLoLim_mon;

double  phi1MotCmd;
assign  phi1MotCmd to "{P}{M_PHI1}";
int phi1PutComplete;

double  phi2MotCmd;
assign  phi2MotCmd to "{P}{M_PHI2}";
int phi2PutComplete;

double  phi1MotCmdMon;
assign  phi1MotCmdMon to "{P}{M_PHI1}";
monitor phi1MotCmdMon;
evflag  phi1MotCmdMon_mon;
sync    phi1MotCmdMon phi1MotCmdMon_mon;

double  phi2MotCmdMon;
assign  phi2MotCmdMon to "{P}{M_PHI2}";
monitor phi2MotCmdMon;
evflag  phi2MotCmdMon_mon;
sync    phi2MotCmdMon phi2MotCmdMon_mon;

double  phi1MotRdbk;
assign  phi1MotRdbk to "{P}{M_PHI1}.RBV";
monitor phi1MotRdbk;
evflag  phi1MotRdbk_mon;
sync    phi1MotRdbk phi1MotRdbk_mon;
evflag  phi1MotRdbkPseudo_mon;

double  phi2MotRdbk;
assign  phi2MotRdbk to "{P}{M_PHI2}.RBV";
monitor phi2MotRdbk;
evflag  phi2MotRdbk_mon;
sync    phi2MotRdbk phi2MotRdbk_mon;
evflag  phi2MotRdbkPseudo_mon;

/* Support for chasing wandering beam: rotate both crystals by same angle. */
double  worldOff;
assign  worldOff to "{P}HR{N}_worldOffAO";
monitor worldOff;
evflag  worldOff_mon;
sync    worldOff worldOff_mon;

double  worldTweakVal;
assign  worldTweakVal to "{P}HR{N}_worldTweakAI";
monitor worldTweakVal;

short   worldTweakInc;
assign  worldTweakInc to "{P}HR{N}_worldIncBO";
monitor worldTweakInc;

short   worldTweakDec;
assign  worldTweakDec to "{P}HR{N}_worldDecBO";
monitor worldTweakDec;

/* Set/Use support */
short   useSetMode;
assign  useSetMode to "{P}HR{N}_UseSetBO";
monitor useSetMode;
evflag  useSetMode_mon;
sync    useSetMode useSetMode_mon;

/* tweaking PV's */
double  ETweakVal;
assign  ETweakVal to "{P}HR{N}_ETweakAI";
monitor ETweakVal;

short   ETweakInc;
assign  ETweakInc to "{P}HR{N}_EIncBO";
monitor ETweakInc;

short   ETweakDec;
assign  ETweakDec to "{P}HR{N}_EDecBO";
monitor ETweakDec;

double  LTweakVal;
assign  LTweakVal to "{P}HR{N}_LambdaTweakAI";
monitor LTweakVal;

short   LTweakInc;
assign  LTweakInc to "{P}HR{N}_LambdaIncBO";
monitor LTweakInc;

short   LTweakDec;
assign  LTweakDec to "{P}HR{N}_LambdaDecBO";
monitor LTweakDec;

double  th1TweakVal;
assign  th1TweakVal to "{P}HR{N}_Theta1TweakAI";
monitor th1TweakVal;

short   th1TweakInc;
assign  th1TweakInc to "{P}HR{N}_Theta1IncBO";
monitor th1TweakInc;

short   th1TweakDec;
assign  th1TweakDec to "{P}HR{N}_Theta1DecBO";
monitor th1TweakDec;

double  th2TweakVal;
assign  th2TweakVal to "{P}HR{N}_Theta2TweakAI";
monitor th2TweakVal;

short   th2TweakInc;
assign  th2TweakInc to "{P}HR{N}_Theta2IncBO";
monitor th2TweakInc;

short   th2TweakDec;
assign  th2TweakDec to "{P}HR{N}_Theta2DecBO";
monitor th2TweakDec;

/* min/max theta */
double  thetaMin;
assign  thetaMin to "{P}HR{N}_thetaMin";
monitor thetaMin;
double  thetaMax;
assign  thetaMax to "{P}HR{N}_thetaMax";
monitor thetaMax;


%%#include <string.h>
%%#include <math.h>

%%static double hrCtl_calc2dSpacing(struct UserVar *pVar, int xtal, double A, double H, double K, double L);
%%static void hrCtl_calcReadback(struct UserVar *pVar);

short  toggle;
short  causedMove;
short  initSeq;
short  initSeqDone;
double hc;
double PI;
double R2D;
double uR2D;
double D2R;
double D2uR;
double temp;

ss hr_Ctl
{
	state init {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:init: entry\n");
		}
		when () {
			initSeq = 0;
		} state initSequence
	}

	state initSequence {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:initSequence: entry\n");
		}
		when (initSeq == 0) {
			hc = 12.3984244;
			PI = 4*atan(1.0);
			R2D = 180/PI;
			uR2D = R2D/1000000;
			D2R = PI/180;
			D2uR = 1000000*PI/180;
			phi1MotStop = 1;
			phi2MotStop = 1;
			phi1PutComplete = 1;
			phi2PutComplete = 1;
			putVals = 0;
			pvPut(putVals);
			autoMode = 0;
			pvPut(autoMode);
			opAck = 0;
			pvPut(opAck);
			/* C defect: strcpy and strcat wrote P and M_PHI* unbounded into
			 * the 40-byte names, past them when the two exceed 39 chars. */
			/* C defect: hrCtl_pmacP and hrCtl_pmacM were statics shared by all
			 * instances of this reentrant program; two instances starting at
			 * once could build their names from each other's P. */
			%{
				char *hrCtl_pmacP = seq_macValueGet(ssId,"P");
				char *hrCtl_pmacM = seq_macValueGet(ssId,"M_PHI1");
				if (hrCtl_pmacP == 0 || hrCtl_pmacM == 0) {
					sprintf(pVar->phi1MotName,"??????");
				} else {
					snprintf(pVar->phi1MotName, sizeof(pVar->phi1MotName), "%s%s", hrCtl_pmacP, hrCtl_pmacM);
				}
				hrCtl_pmacM = seq_macValueGet(ssId,"M_PHI2");
				if (hrCtl_pmacP == 0 || hrCtl_pmacM == 0) {
					sprintf(pVar->phi2MotName,"??????");
				} else {
					snprintf(pVar->phi2MotName, sizeof(pVar->phi2MotName), "%s%s", hrCtl_pmacP, hrCtl_pmacM);
				}
			}%
			pvPut(phi1MotName);
			pvPut(phi2MotName);
			initSeq = 1;
		} state dInputChanged

        when (initSeq == 1) {
			initSeq = 2;
		} state chkMotorLimits

		when (initSeq == 2) {
			initSeq = 3;
		} state updateRdbk

		when (initSeq == 3) {
			sprintf(seqMsg1, "HR Control Ready");
			pvPut(seqMsg1);
			%%if (hrCtl_LS_TEST) {
				sprintf(seqMsg2, "LIMIT SWITCH TEST VERSION !!!!");
				pvPut(seqMsg2);
				opAlert = 1;
				pvPut(opAlert);
			%%} else {
				sprintf(seqMsg2, " ");
				pvPut(seqMsg2);
			%%}
			initSeqDone = 1;
			phi1 = phi1MotRdbk * uR2D + (phi1Off + worldOff*uR2D);
			pvPut(phi1);
			theta1 = phi1;
			pvPut(theta1);
			if (Geom == 0) {
				phi2 = phi2MotRdbk * uR2D + phi2Off + worldOff*uR2D;
				theta2 = phi2 - phi1 - theta1;
			} else {
				phi2 = phi2MotRdbk * uR2D + phi2Off - worldOff*uR2D;
				theta2 = phi2;
			}
			if (hrCtlDebug) %%printf("hrCtl:initSequence: opMode=%d, Geom=%d, phi2=%f\n", pVar->opMode, pVar->Geom, pVar->phi2);
			pvPut(phi2);
			pvPut(theta2);
		} state thChanged
	}

	state waitForCmndEnter {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmndEnter: entry\n");
		}
		when (initSeqDone && delay(.1)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmdEnter: initSeqDone\n");
			efClear(H1_mon);
			efClear(K1_mon);
			efClear(L1_mon);
			efClear(A1_mon);
			efClear(H2_mon);
			efClear(K2_mon);
			efClear(L2_mon);
			efClear(A2_mon);
			efClear(E_mon);
			efClear(lambda_mon);
			efClear(theta1_mon);
			efClear(theta2_mon);
			efClear(phi1Off_mon);
			efClear(phi2Off_mon);
			efClear(worldOff_mon);
			efClear(useSetMode_mon);
			efClear(opMode_mon);
			efClear(Geom_mon);

			phi1MotRdbkEcho = phi1MotRdbk;
			pvPut(phi1MotRdbkEcho);
			phi2MotRdbkEcho = phi2MotRdbk;
			pvPut(phi2MotRdbkEcho);

			phi1MotCmdEcho = phi1MotCmdMon;
			pvPut(phi1MotCmdEcho);
			phi1DmovEcho = phi1Dmov;
			pvPut(phi1DmovEcho);

			phi2MotCmdEcho = phi2MotCmdMon;
			pvPut(phi2MotCmdEcho);
			phi2DmovEcho = phi2Dmov;
			pvPut(phi2DmovEcho);

			if (autoMode && 0) {
				/* Calc and put drive values to screen */
				phi1 = phi1MotCmdMon * uR2D + (phi1Off + worldOff*uR2D);
				pvPut(phi1);
				theta1 = phi1;

				if (Geom == 0) {
					phi2 = phi2MotCmdMon * uR2D + phi2Off + worldOff*uR2D;
				} else {
					phi2 = phi2MotCmdMon * uR2D + phi2Off - worldOff*uR2D;
				}
				if (hrCtlDebug) %%printf("hrCtl:waitForCmdEnter: Geom=%d, phi2=%f\n", pVar->Geom, pVar->phi2);
				pvPut(phi2);

				if ((opMode == 0) || (opMode == 1)) {
					lambda = D1*sin(theta1*D2R);
				} else {
					theta2Nom = asin(D1*sin(theta1*D2R)/D2)*R2D;
					lambda = D1*sin(theta1*D2R) +
						D2R * (phi2 - phi1 - theta1 - theta2Nom) /
						( 1/(D1*cos(theta1*D2R)) +
						  1/(D2*cos(theta2Nom*D2R)) );
				}
				pvPut(lambda);
				theta1 = asin(lambda/D1)*R2D;
				pvPut(theta1);
				theta2 = asin(lambda/D2)*R2D;
				pvPut(theta2);
				E = hc/lambda;
				pvPut(E);

				/* Calc and put readback values */
				%%hrCtl_calcReadback(pVar);
				pvPut(phi1Rdbk);
				pvPut(theta1Rdbk);
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
				pvPut(lambdaRdbk);
				pvPut(ERdbk);
			}
			opAck = 0;
			pvPut(opAck);
			if (hrCtlDebug) %%printf("hrCtl:waitForCmndEnter: Moving=%d; Done=%d\n", pVar->hr_Moving, pVar->hr_Done);
			if (hr_Moving && hr_Done) {
				if (hrCtlDebug) %%printf("hrCtl:waitForCmndEnter: asserting 'done'.\n");
				hr_Moving=0; hr_Done = 0;
				pvPut(hr_Moving);
			}
		} state waitForCmnd  

	    when (!initSeqDone) {
		} state initSequence
	}

	state waitForCmnd {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmnd: entry\n");
		}
		when (efTestAndClear(phi1MotHiLim_mon) || efTestAndClear(phi1MotLoLim_mon) ||
				efTestAndClear(phi2MotHiLim_mon) || efTestAndClear(phi2MotLoLim_mon)) {
			sprintf(seqMsg1, "Motor Soft Limit Changed");
			pvPut(seqMsg1);
			sprintf(seqMsg2, " ");
			pvPut(seqMsg2);
		} state phiLimits

		when (efTestAndClear(H1_mon) || efTestAndClear(K1_mon) || efTestAndClear(L1_mon) ||
				efTestAndClear(A1_mon) || efTestAndClear(H2_mon) || efTestAndClear(K2_mon) ||
				efTestAndClear(L2_mon) || efTestAndClear(A2_mon)) {
			sprintf(seqMsg1, "Lattice Spacing Changed");
			pvPut(seqMsg1);
			sprintf(seqMsg2, " ");
			pvPut(seqMsg2);
		} state dInputChanged
 
		when (efTestAndClear(phi1MotRdbkPseudo_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: phi1MotRdbkPseudo_mon\n");
			efSet(phi1MotRdbkPseudo_mon);
		} state updateRdbk

		when (efTestAndClear(phi2MotRdbkPseudo_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: phi2MotRdbkPseudo_mon\n");
			efSet(phi2MotRdbkPseudo_mon);
		} state updateRdbk

		when (efTest(E_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: E_mon\n");
			efClear(E_mon);
		} state eChanged
   
		when (efTest(lambda_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: lambda_mon\n");
			efClear(lambda_mon);
		} state lChanged
   
		when (efTest(theta1_mon) || efTest(theta2_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: theta*_mon\n");
			efClear(theta1_mon);
			efClear(theta2_mon);
		} state thChanged
   
		when (efTest(worldOff_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: worldOff_mon\n");
			efClear(worldOff_mon);
		} state calcMovements

		when (efTest(phi1Off_mon) || efTest(phi2Off_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmd: phi*Off_mon\n");
			efClear(phi1Off_mon);
			efClear(phi2Off_mon);
			/* Calc and put readback values. */
			%%hrCtl_calcReadback(pVar);        
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			if (opMode != 0) {
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
			}
			pvPut(lambdaRdbk);
			pvPut(ERdbk);
		} state phiLimits

		when (putVals && delay(.1)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmnd:putVals\n");
		} state calcMovements

		when (efTest(useSetMode_mon) && useSetMode) {
			efClear(useSetMode_mon);
			EHi=0; pvPut(EHi);
			ELo=0; pvPut(ELo);
			lambdaHi=0; pvPut(lambdaHi);
			lambdaLo=0; pvPut(lambdaLo);
			theta1Hi=0; pvPut(theta1Hi);
			theta1Lo=0; pvPut(theta1Lo);
			phi1Hi=0; pvPut(phi1Hi);
			phi1Lo=0; pvPut(phi1Lo);
			theta2Hi=0; pvPut(theta2Hi);
			theta2Lo=0; pvPut(theta2Lo);
			phi2Hi=0; pvPut(phi2Hi);
			phi2Lo=0; pvPut(phi2Lo);
		} state eChanged

		when (efTest(useSetMode_mon) && !useSetMode) {
			efClear(useSetMode_mon);
		} state phiLimits

		when (efTestAndClear(opMode_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmnd: opMode event.\n");
			autoMode = 0;
			pvPut(autoMode);
			sprintf(seqMsg2, "Set to Manual Mode");
			pvPut(seqMsg2);
		} state calcMovements

		when (efTestAndClear(Geom_mon)) {
			if (hrCtlDebug) %%printf("hrCtl:waitForCmnd: Geom event.\n");
			autoMode = 0;
			pvPut(autoMode);
			sprintf(seqMsg1, "New geometry.  Switch Phi 2 motor dir.");
			pvPut(seqMsg1);
			sprintf(seqMsg2, "and recalibrate monochromator.");
			pvPut(seqMsg2);
			opAlert = 1;
			pvPut(opAlert);
		} state calcMovements

		when (ETweakInc||ETweakDec||LTweakInc||LTweakDec||th1TweakInc||
				th1TweakDec||th2TweakInc||th2TweakDec||
				worldTweakInc||worldTweakDec) {
		} state tweak

		when (opAck) {
			opAlert = 0;
			pvPut(opAlert);
			sprintf(seqMsg1, " ");
			pvPut(seqMsg1);
			sprintf(seqMsg2, " ");
			pvPut(seqMsg2);
		} state waitForCmndEnter

		when (efTest(hr_Moving_mon)) {
			efClear(hr_Moving_mon);
		} state checkAutoMode
		
		when ((!phi1Dmov || !phi2Dmov) && delay(.1)) {
		} state updateRdbk
	}

	state checkAutoMode {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:checkAutoMode: entry\n");
		}
		when (autoMode) {
		} state checkDone
		when (1) {
		} state waitForCmnd
	}

	state phiLimits {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:phiLimits: entry\n");
		}
		when () {
			if (hrCtlDebug) %%printf("hrCtl:phiLimits\n");
			if (!useSetMode) {
				/* translate phi1 motor limits into phi1, theta1 limits */
				phi1Hi = phi1MotHiLim * uR2D + (phi1Off + worldOff*uR2D);
				theta1Hi = phi1Hi;
				phi1Lo = phi1MotLoLim * uR2D + (phi1Off + worldOff*uR2D);
				theta1Lo = phi1Lo;
				/* C defect: sprintf wrote these %.3f messages unbounded into the
				 * 40-byte seqMsg2, past it for a limit of 1e12 or more, or -1e11
				 * or less. */
				if (theta1Hi > thetaMax) {
					theta1Hi = thetaMax;
					%%snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta1HiLim clipped at %.3f", pVar->thetaMax);
					pvPut(seqMsg2);
				}
				if (theta1Lo < thetaMin) {
					theta1Lo = thetaMin;
					%%snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta1LoLim clipped at %.3f", pVar->thetaMin);
					pvPut(seqMsg2);
				}
				pvPut(theta1Hi);
				pvPut(theta1Lo);

				/* translate phi2 motor limits into phi2, theta2 limits */
				if (Geom == 0) {
					/* nested */
					phi2Hi = phi2MotHiLim * uR2D + phi2Off + worldOff*uR2D;
					phi2Lo = phi2MotLoLim * uR2D + phi2Off + worldOff*uR2D;
					theta2Hi = phi2Hi - phi1Lo - theta1Lo;
					theta2Lo = phi2Lo - phi1Hi - theta1Hi;
				} else {
					/* symmetric */
					phi2Hi = phi2MotHiLim * uR2D + phi2Off - worldOff*uR2D;
					phi2Lo = phi2MotLoLim * uR2D + phi2Off - worldOff*uR2D;
					theta2Hi = phi2Hi;
					theta2Lo = phi2Lo;
				}
				if (theta2Hi > thetaMax) {
					theta2Hi = thetaMax;
					%%snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta2HiLim clipped at %.3f", pVar->thetaMax);
					pvPut(seqMsg2);
				}
				if (theta2Lo < thetaMin) {
					theta2Lo = thetaMin;
					%%snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta2LoLim clipped at %.3f", pVar->thetaMin);
					pvPut(seqMsg2);
				}
				pvPut(theta2Hi);
				pvPut(theta2Lo);

				/* calc limits on lambda, energy from theta 1/2 limits */
				lambdaHi = MIN(D1*sin(theta1Hi*D2R), D2*sin(theta2Hi*D2R));
				pvPut(lambdaHi);
				lambdaLo = MAX(D1*sin(theta1Lo*D2R), D2*sin(theta2Lo*D2R));
				pvPut(lambdaLo);
				EHi = hc/lambdaLo;
				pvPut(EHi);
				ELo = hc/lambdaHi;
				pvPut(ELo);
			} else {
				/* ignore limits entirely during setup */
				EHi=0; pvPut(EHi);
				ELo=0; pvPut(ELo);
				lambdaHi=0; pvPut(lambdaHi);
				lambdaLo=0; pvPut(lambdaLo);
				theta1Hi=0; pvPut(theta1Hi);
				theta1Lo=0; pvPut(theta1Lo);
				phi1Hi=0; pvPut(phi1Hi);
				phi1Lo=0; pvPut(phi1Lo);
				theta2Hi=0; pvPut(theta2Hi);
				theta2Lo=0; pvPut(theta2Lo);
				phi2Hi=0; pvPut(phi2Hi);
				phi2Lo=0; pvPut(phi2Lo);
			}
		} state checkDone /*waitForCmndEnter*/
	}

	state checkDone {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:checkDone: entry\n");
			if (!phi1PutComplete) phi1PutComplete = pvPutComplete(phi1MotCmd);
			if (!phi2PutComplete) phi2PutComplete = pvPutComplete(phi2MotCmd);
		}
		/* when (phi1Dmov && phi2Dmov) { */
		when (phi1PutComplete && phi2PutComplete) {
			if (hrCtlDebug) %%printf("hrCtl:checkDone: done\n");
			hr_Done = 1;
		} state waitForCmndEnter

		when (1) {
			if (hrCtlDebug) %%printf("hrCtl:checkDone: not done\n");
		} state updateRdbk
	}

	state dInputChanged {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:dInputChanged: entry\n");
		}
		when () {
			autoMode = 0;
			pvPut(autoMode);
			sprintf(seqMsg2, "Set to Manual Mode");
			pvPut(seqMsg2);
			%%pVar->D1 = hrCtl_calc2dSpacing(pVar, 1, pVar->A1, pVar->H1, pVar->K1, pVar->L1);
			pvPut(D1);
			pvPut(seqMsg1);
			%%pVar->D2 = hrCtl_calc2dSpacing(pVar, 2, pVar->A2, pVar->H2, pVar->K2, pVar->L2);
            pvPut(D2);
            pvPut(seqMsg1);

			if (useSetMode) {
				lambdaHi = D1*sin(theta1Hi*D2R);
				lambdaLo = D1*sin(theta1Lo*D2R);
				EHi = hc/lambdaLo;
				ELo = hc/lambdaHi;
			} else {
				lambdaHi = 0;
				lambdaLo = 0;
				EHi = 0;
				ELo = 0;
			}
			pvPut(lambdaLo);
			pvPut(lambdaHi);
			pvPut(EHi);
			pvPut(ELo);
			pvPut(opAlert);
		} state thChanged
	}

	state tweak {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:tweak: entry\n");
		}
		when (ETweakInc || ETweakDec) {
			/* Note ETweakVal is in meV (E is in keV) */
			if (ETweakInc) {
				temp = E + ETweakVal/1000000;
			} else {
				temp = E - ETweakVal/1000000;
			}
			if (useSetMode || ((temp>=ELo) && (temp<=EHi))) {
				E = temp;
				pvPut(E);
				sprintf(seqMsg1, "Tweaked E");
				pvPut(seqMsg1);
			} else {
				sprintf(seqMsg1, "Exceeds Limit");
				pvPut(seqMsg1);
				opAlert = 1;
				pvPut(opAlert);
			}
			if (ETweakInc) {
				ETweakInc = 0; pvPut(ETweakInc);
			} else {
				ETweakDec = 0; pvPut(ETweakDec);
			}
		} state eChanged

        when (LTweakInc || LTweakDec) {
			/* Note LTweakVal is in fm (L is in Angstroms) */
			if (LTweakInc) {
				temp = lambda + LTweakVal/100000;
			} else {
				temp = lambda - LTweakVal/100000;
			}
			if (useSetMode || ((temp>=lambdaLo) && (temp<=lambdaHi))) {
				lambda = temp;
				pvPut(lambda);
				sprintf(seqMsg1, "Tweaked Lambda");
				pvPut(seqMsg1);
			} else {
				sprintf(seqMsg1, "Exceeds Limit");
				pvPut(seqMsg1);
				opAlert = 1;
				pvPut(opAlert);
			}
			if (LTweakInc) {
				LTweakInc = 0; pvPut(LTweakInc);
			} else {
				LTweakDec = 0; pvPut(LTweakDec);
			}
		} state lChanged

		when (th1TweakInc || th1TweakDec) {
			if (th1TweakInc) {
				temp = theta1 + th1TweakVal;
			} else {
				temp = theta1 - th1TweakVal;
			}
			if (useSetMode || ((temp>=theta1Lo) && (temp<=theta1Hi))) {
				theta1 = temp;
				pvPut(theta1);
				sprintf(seqMsg1, "Tweaked Theta 1");
				pvPut(seqMsg1);
			} else {
				sprintf(seqMsg1, "Exceeds Limit");
				pvPut(seqMsg1);
				opAlert = 1;
				pvPut(opAlert);
			}
			if (th1TweakInc) {
				th1TweakInc = 0; pvPut(th1TweakInc);
			} else {
				th1TweakDec = 0; pvPut(th1TweakDec);
			}
		} state thChanged

		when (th2TweakInc || th2TweakDec) {
			if (th2TweakInc) {
				temp = theta2 + th2TweakVal;
			} else {
				temp = theta2 - th2TweakVal;
			}
			if (useSetMode || ((temp>=theta2Lo) && (temp<=theta2Hi))) {
				theta2 = temp;
				pvPut(theta2);
				sprintf(seqMsg1, "Tweaked Theta 2");
				pvPut(seqMsg1);
			} else {
				sprintf(seqMsg1, "Exceeds Limit");
				pvPut(seqMsg1);
				opAlert = 1;
				pvPut(opAlert);
			}
			if (th2TweakInc) {
				th2TweakInc = 0; pvPut(th2TweakInc);
			} else {
				th2TweakDec = 0; pvPut(th2TweakDec);
			}
		} state thChanged

        when (worldTweakInc || worldTweakDec) {
			if (worldTweakInc) {
				worldOff = worldOff + worldTweakVal;
			} else {
				worldOff = worldOff - worldTweakVal;
			}
			pvPut(worldOff);
			sprintf(seqMsg1, "Tweaked world");
			pvPut(seqMsg1);
			if (worldTweakInc) {
				worldTweakInc = 0; pvPut(worldTweakInc);
			} else {
				worldTweakDec = 0; pvPut(worldTweakDec);
			}
		} state calcMovements

    }

	state eChanged {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:eChanged: entry\n");
		}
		when () {                
			if (hrCtlDebug) %%printf("hrCtl:eChanged\n");
			lambda = hc/E;
			pvPut(lambda);
		} state lChanged
	}

	state lChanged {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:lChanged: entry\n");
		}
		when (lambda > D1) {
			/* Don't change Theta, notify operator */
			sprintf(seqMsg1, "Wavelength > 2d spacing of crystal 1.");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
		} state thChanged 

		when ((lambda > D2) && (opMode != 0)) {
			/* Don't change Theta, notify operator */
			sprintf(seqMsg1, "Wavelength > 2d spacing of crystal 2.");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
		} state thChanged 

		when (opMode == 0) {
			if (hrCtlDebug) %%printf("hrCtl:lChanged, opMode 0\n");
			/* single-crystal mode */
			theta1 = asin(lambda/D1) * R2D;
			pvPut(theta1);
		} state thChanged

		when (opMode == 1) {
			if (hrCtlDebug) %%printf("hrCtl:lChanged, opMode 1\n");
			/* two crystals locked together */
			theta1 = asin(lambda/D1) * R2D;
			pvPut(theta1);
			theta2 = asin(lambda/D2) * R2D;
			pvPut(theta2);
		} state thChanged

		when (opMode == 2) {
			/* C defect: this branch's debug line said opMode 1. */
			if (hrCtlDebug) %%printf("hrCtl:lChanged, opMode 2\n");
			/*
			 * two crystals: user may change theta 1, but we may not.
			 * We regard theta 1 as defining the nominal energy, and
			 * can move theta 2 to vary energy by a small amount.
			 */
			theta2 = asin(lambda/D2) * R2D;
			pvPut(theta2);
		} state thChanged
	}

	state thChanged {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:thChanged: entry\n");
		}
		when () {
			if (hrCtlDebug) %%printf("hrCtl:thChanged\n");
			if (!useSetMode && ((theta1 <= theta1Lo) || (theta1 >= theta1Hi))) {
				sprintf(seqMsg1, "Theta 1 constrained to LIMIT");
				pvPut(seqMsg1);
				opAlert = 1;
				pvPut(opAlert);
				autoMode = 0;
				pvPut(autoMode);
				sprintf(seqMsg2, "Set to Manual Mode");
				pvPut(seqMsg2);
			}

			if (!useSetMode && (opMode != 0) && ((theta2 <= theta2Lo) || (theta2 >= theta2Hi))) {
				sprintf(seqMsg1, "Theta 2 constrained to LIMIT");
				pvPut(seqMsg1);
				opAlert = 1;
				pvPut(opAlert);
				autoMode = 0;
				pvPut(autoMode);
				sprintf(seqMsg2, "Set to Manual Mode");
				pvPut(seqMsg2);
			}

			if ((opMode == 0) || (opMode == 1)) {
				lambda = D1*sin(theta1*D2R);
				phi1 = theta1;
				if (opMode == 1) {
					if (Geom == 0) {
						phi2 = phi1 + theta1 + theta2;
					} else {
						phi2 = theta2;
					}
					if (hrCtlDebug) %%printf("hrCtl:thChanged: opMode=%d, Geom=%d, theta2=%f\n", pVar->opMode, pVar->Geom, pVar->theta2);
				}
			} else {
				lambdaNom = D1*sin(theta1*D2R);
				theta2Nom = asin(lambdaNom/D2) * R2D;
				lambda = D2*sin(theta2*D2R);
				phi1 = theta1;
				if (Geom == 0) {
					phi2 = (phi1 + theta1 + theta2Nom) + R2D * (lambda-lambdaNom) *
							(1/(D1*cos(theta1*D2R)) + 1/(D2*cos(theta2Nom*D2R)));
				} else {
					phi2 = theta2Nom + R2D * (lambda-lambdaNom) *
							(1/(D1*cos(theta1*D2R)) + 1/(D2*cos(theta2Nom*D2R)));
				}
			}
			pvPut(phi1);
			if (opMode != 0) {
				if (hrCtlDebug) %%printf("hrCtl:thChanged: opMode=%d, Geom=%d, phi2=%f\n", pVar->opMode, pVar->Geom, pVar->phi2);
				pvPut(phi2);
			}
			pvPut(lambda);

			E = hc/lambda;
			pvPut(E); 

			/*
			 * Update the readback values as well, since we may have gotten
			 * here because D1 or D2 changed, for example.
			 */
			%%hrCtl_calcReadback(pVar);        
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			if (opMode != 0) {
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
			}
			pvPut(lambdaRdbk);
			pvPut(ERdbk);
		} state calcMovements
	}

	state calcMovements {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:calcMovements: entry\n");
		}
		when (useSetMode == 0) {
			if (hrCtlDebug) %%printf("hrCtl:calcMovements, USE\n");
			/* Calculate new phi* */
			phi1MotDesired = (phi1 - phi1Off - worldOff*uR2D) * D2uR;
			pvPut(phi1MotDesired);
			if (opMode != 0) {
				if (Geom == 0) {
					phi2MotDesired = (phi2 - phi2Off - worldOff*uR2D) * D2uR;
				} else {
					phi2MotDesired = (phi2 - phi2Off + worldOff*uR2D) * D2uR;
				}
				pvPut(phi2MotDesired);
			}
		} state moveHR

		when (useSetMode == 1) {
			if (hrCtlDebug) %%printf("hrCtl:calcMovements, SET\n");
			/* Calculate new phiOff* */
			/* phi1Off = phi1 - phi1MotRdbk/D2uR - worldOff*uR2D; */
			phi1Off = phi1 - phi1MotRdbk/D2uR;
			pvPut(phi1Off);
			if (opMode != 0) {
				/* Don't consider world.  It's not part of calibration
				 * if (Geom == 0) {
				 * 	phi2Off = (phi2 - phi2MotRdbk/D2uR - worldOff*uR2D) * D2uR;
				 * } else {
				 * 	phi2Off = (phi2 - phi2MotRdbk/D2uR + worldOff*uR2D) * D2uR;
				 * }
				 */
				phi2Off = phi2 - phi2MotRdbk/D2uR;
				pvPut(phi2Off);
			}
			/* Calculate new phi* */
			phi1MotDesired = (phi1 - phi1Off - worldOff*uR2D) * D2uR;
			pvPut(phi1MotDesired);
			if (opMode != 0) {
				if (Geom == 0) {
					phi2MotDesired = (phi2 - phi2Off - worldOff*uR2D) * D2uR;
				} else {
					phi2MotDesired = (phi2 - phi2Off + worldOff*uR2D) * D2uR;
				}
				pvPut(phi2MotDesired);
			}
			putVals = 0;
			pvPut(putVals);

			/* Calc and put readback values. */
			%%hrCtl_calcReadback(pVar);        
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			if (opMode != 0) {
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
			}
			pvPut(lambdaRdbk);
			pvPut(ERdbk);
		} state waitForCmndEnter
	}

	state moveHR {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:moveHR entry\n");
		}
		when (autoMode || putVals) {
			if (hrCtlDebug) %%printf("hrCtl:moveHR: putting to motors\n");
			phi1MotCmd = phi1MotDesired;
			if (opMode == 0) {
				phi1PutComplete = 0;
				pvPut(phi1MotCmd, ASYNC);
			} else if ((opMode == 1) || (opMode == 2)) {
				phi2MotCmd = phi2MotDesired;
				phi1PutComplete = 0;
				phi2PutComplete = 0;
				pvPut(phi2MotCmd, ASYNC);
				pvPut(phi1MotCmd, ASYNC);
            }
			putVals = 0;
			pvPut(putVals);
			causedMove = 1;
		} state updateRdbk

		when (1) {
			if (hrCtlDebug) %%printf("hrCtl:moveHR: NOT putting to motors\n");
		} state waitForCmndEnter
	}

	state updateRdbk {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:updateRdbk: entry\n");
			if (!phi1PutComplete) phi1PutComplete = pvPutComplete(phi1MotCmd);
			if (!phi2PutComplete) phi2PutComplete = pvPutComplete(phi2MotCmd);
		}
		when (phi1Hls || phi1Lls || phi2Hls || phi2Lls) {
			if (phi1Hls || phi1Lls) {
				sprintf(seqMsg1, "Theta 1 motor hit a limit switch !");
				pvPut(seqMsg1);
			} 
			if ((opMode != 0) && (phi2Hls || phi2Lls)) {
				sprintf(seqMsg1, "Theta 2 motor hit a limit switch !");
				pvPut(seqMsg1);
			} 
			opAlert = 1;
			pvPut(opAlert);
			autoMode = 0;
			pvPut(autoMode);
			sprintf(seqMsg2, "Setting to Manual Mode");
			pvPut(seqMsg2);
		} state stopHR

		when (!phi1Dmov || !phi2Dmov) {
			if (hrCtlDebug) %%printf("hrCtl:updateRdbk: a motor is moving\n");
			hr_Moving = 1;
			pvPut(hr_Moving);
            
			/* Calc and put readback values. */
			%%hrCtl_calcReadback(pVar);        
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			if (opMode != 0) {
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
			}
			pvPut(lambdaRdbk);
			pvPut(ERdbk);
		} state updateRdbkAfterDelay

		when (phi1PutComplete && phi2PutComplete) {
			if (hrCtlDebug) %%printf("hrCtl:updateRdbk: motors stopped\n");
			/* Calc and put readback values. */
			%%hrCtl_calcReadback(pVar);        
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			if (opMode != 0) {
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
			}
			pvPut(lambdaRdbk);
			pvPut(ERdbk);
			hr_Done = 1;
		} state motorsStopped
	}

	state updateRdbkAfterDelay {
		when (delay(.1)) {
		} state updateRdbk
	}

	state motorsStopped {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:motorsStopped: entry\n");
		}
		when (causedMove) {
			if (hrCtlDebug) %%printf("hrCtl:motorsStopped (we started them)\n");
			causedMove = 0;
		}  state waitForCmndEnter

		when (!causedMove) {
			if (hrCtlDebug) %%printf("hrCtl:motorsStopped (we didn't start them)\n");
			/* Some external agent moved one of our motors */
			if (initSeqDone) {
				opAlert = 1;
				pvPut(opAlert);
				sprintf(seqMsg1, "Somebody moved one of my motors.");
				pvPut(seqMsg1);
				autoMode = 0;
				pvPut(autoMode);
			}
			/* Calc and put readback values. */
			%%hrCtl_calcReadback(pVar);        
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			if (opMode != 0) {
				pvPut(phi2Rdbk);
				pvPut(theta2Rdbk);
			}
			pvPut(lambdaRdbk);
			pvPut(ERdbk);
		}  state waitForCmndEnter /*thChanged*/
	}

	state chkMotorLimits {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:chkMotorLimits: entry\n");
		}
		when () {
			opAlert = 0;
			/* Calc and put readback values */
			%%hrCtl_calcReadback(pVar);
			pvPut(phi1Rdbk);
			pvPut(theta1Rdbk);
			pvPut(phi2Rdbk);
			pvPut(theta2Rdbk);
			pvPut(lambdaRdbk);
			pvPut(ERdbk);

			if ((phi1MotRdbk<phi1MotLoLim)||(phi1MotRdbk>phi1MotHiLim)) {
				sprintf(seqMsg1, "Theta 1 motor is outside its limits");
				pvPut(seqMsg1);
				opAlert = 1;
			}
			if ((opMode != 0) &&
				((phi2MotRdbk<phi2MotLoLim)||(phi2MotRdbk>phi2MotHiLim))) {
				sprintf(seqMsg1, "Theta 2 motor is outside its limits");
				pvPut(seqMsg1);
				opAlert = 1;
			}
			if (opAlert) {
				pvPut(opAlert);
				autoMode = 0;
				pvPut(autoMode);
				sprintf(seqMsg2, "Set to manual mode");
				pvPut(seqMsg2);
			}
		} state chkMotorLimitsExit
	}

	state chkMotorLimitsExit {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:chkMotorLimitsExit: entry\n");
		}
		when (!opAlert) {
			pvPut(opAlert);
			sprintf(seqMsg1, "All motors within limits");
			pvPut(seqMsg1);
		} state waitForCmndEnter

		when (opAlert && delay(2.0)) {
			pvPut(opAlert);
			sprintf(seqMsg2, "Modify motor positions and/or limits");
			pvPut(seqMsg2);          
		} state chkMotorLimits
	}

	state stopHR {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:stopHR: entry\n");
		}
		when () {
			pvPut(phi1MotStop);
			if (opMode != 0) pvPut(phi2MotStop);
		} state stoppedHRWait
	}

	state stoppedHRWait {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:stoppedHRWait: entry\n");
		}
		when (delay(1.0)) {
		} state updateRdbk
	}

} /* end of state set hr_Ctl  */

/**************************************************************************
 *
 * This State Set keeps the motor independent fields updated 
 *
 **************************************************************************/
ss updatePsuedo
{
	state init {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:updatePsuedo:init: entry\n");
		}
		when () {
		} state update
	}

	state update {
		entry {
			if (hrCtlDebug) %%printf("hrCtl:updatePsuedo:update: entry\n");
		}
		when (efTestAndClear(phi1MotRdbkPseudo_mon)) {
			phi1MotRdbkEcho = phi1MotRdbk;
			pvPut(phi1MotRdbkEcho);
		} state update

		when (efTestAndClear(phi2MotRdbkPseudo_mon)) {
			phi2MotRdbkEcho = phi2MotRdbk;
			pvPut(phi2MotRdbkEcho);
		} state update

		when (efTest(phi1MotCmdMon_mon)) {
			phi1MotCmdEcho = phi1MotCmdMon;
			pvPut(phi1MotCmdEcho);
			efClear(phi1MotCmdMon_mon);
		} state update

		when (efTest(phi2MotCmdMon_mon)) {
			phi2MotCmdEcho = phi2MotCmdMon;
			pvPut(phi2MotCmdEcho);
			efClear(phi2MotCmdMon_mon);
		} state update

		when (efTest(phi1Dmov_mon)) {
			phi1DmovEcho = phi1Dmov;
			pvPut(phi1DmovEcho);
			efClear(phi1Dmov_mon);
		} state update

		when (efTest(phi2Dmov_mon)) {
			phi2DmovEcho = phi2Dmov;
			pvPut(phi2DmovEcho);
			efClear(phi2Dmov_mon);
		} state update
    }

}

/***************************************************************************/
/***************************************************************************/
/*   C subroutines */

%{

static double hrCtl_calc2dSpacing(struct UserVar *pVar, int xtal, double A, double H, double K, double L)
{
	double result;
	float  avg;
	double nint;

	result = (2*A)/sqrt(H*H + K*K + L*L);

	/* determine validity of H, K, L */
	pVar->opAlert = 0;
	if (((fmod(H,2)) != (fmod(K,2))) || ((fmod(H,2)) != (fmod(L,2)))) 
		pVar->opAlert = 1;
	avg = (H+K+L)/2.0;
	/* C defect: nint was NINT(avg), an int cast undefined once |avg|
	 * passes INT_MAX; round() gives the same value below that. */
	nint = round(avg);
	if ((fabs(avg-nint) <= .25) && (fmod(nint,2)))  
		pVar->opAlert = 1;
	if (pVar->opAlert) {
		sprintf(pVar->seqMsg1, "Invalid HKL combination for crystal %1d", xtal);
	}
    return(result);
}

static void hrCtl_calcReadback(struct UserVar *pVar)
{

	pVar->phi1Rdbk = pVar->phi1MotRdbk * pVar->uR2D +
		(pVar->phi1Off + pVar->worldOff*pVar->uR2D);
	pVar->theta1Rdbk = pVar->phi1Rdbk;

	switch (pVar->opMode) {
	case 0:
		pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R);
		break;

	case 1:
		if (pVar->Geom == 0) {
			pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D +
				(pVar->phi2Off + pVar->worldOff*pVar->uR2D);
		} else {
			pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D +
				(pVar->phi2Off - pVar->worldOff*pVar->uR2D);
		}
		pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R);
		break;

	case 2:
		if (pVar->Geom == 0) {
			pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D +
				(pVar->phi2Off + pVar->worldOff*pVar->uR2D);
		} else {
			pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D +
				(pVar->phi2Off - pVar->worldOff*pVar->uR2D);
		}
		pVar->theta2Nom = asin(pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R)/pVar->D2) * pVar->R2D;
		if (pVar->Geom == 0) {
			pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R) +
				pVar->D2R * (pVar->phi2Rdbk - pVar->phi1Rdbk - pVar->theta1Rdbk - pVar->theta2Nom) /
				( 1/(pVar->D1*cos(pVar->theta1Rdbk*pVar->D2R)) + 1/(pVar->D2*cos(pVar->theta2Nom*pVar->D2R)) );
		} else {
			pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R) +
				pVar->D2R * (pVar->phi2Rdbk - pVar->theta2Nom) /
				( 1/(pVar->D1*cos(pVar->theta1Rdbk*pVar->D2R)) + 1/(pVar->D2*cos(pVar->theta2Nom*pVar->D2R)) );
		}
		break;
	}
	pVar->theta1Rdbk = asin(pVar->lambdaRdbk/pVar->D1) * pVar->R2D;
	if (pVar->opMode != 0) pVar->theta2Rdbk = asin(pVar->lambdaRdbk/pVar->D2) * pVar->R2D;
	pVar->ERdbk = pVar->hc/pVar->lambdaRdbk;

	return;
}

}%
