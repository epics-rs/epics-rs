program kohzuCtl_soft("P=xxx:, MONO=Kohzu1:, M_THETA=m9, M_Y=m10, M_Z=m11, GEOM=1")

/* arguments on the command line  :
 * 
 * ex.  seq &kohzuCtl_soft, "P=xxx:, MONO=Kohzu1:, M_THETA=m9, M_Y=m10, M_Z=m11"
 *
 *      P       - prefix
 *      MONO    - which monochromator
 *      M_THETA - motor record for Theta    
 *      M_Y     - motor record for Y       
 *      M_Z     - motor record for Z       
 */


/*   EDITS & MODIFICATIONS ...
 *
 *
 * 06-02-95 nda   created 
 * 07-24-95 nda   made interaction with Kohzu independent of actual motors
 * 08-15-95 tmm   reversed positive sense of Y motion
 * 08-17-95 tmm   frozen motors take zero time to "move"
 * 08-18-95 tmm   don't put speeds to frozen motors
 * 01-24-96 tmm   v2.0 made compatible with SNL 1.9
 * 02-23-96 tmm   v2.1 added "Freeze Y" to "{P}KohzuMode2MO" (ccMode)
 * 02-26-96 tmm   v2.2 added Use/Set support.
 * 02-26-96 tmm   v2.3 many changes to make things work right in "Set" mode.
 * ??-??-?? tmm   v2.4 debug output.
 * 04-21-97 tmm   v2.5 don't check limits for frozen motion
 * 03-06-98 tmm   v2.6 removed redundant pvPut(thetaMotCmd), write kohzuMovingBO
 *                before telling motors to move, so it can be used as an
 *                overall "busy" flag for the monochromator.
 * 07-07-98 tmm   v3.0 use KohzuMoving busy record. (Database asserts "busy", we
 *                assert "done"; this allows dbPutNotify to detect completion).
 *                Combined two monochromator geometries (kohzu/PSL & kohzu2).
 * 11-02-00 tmm   v3.1 if a motor is frozen, ignore its limit conditions
 * 12-12-03 tmm   v3.2 Converted to EPICS 3.14 OSI.  Fixed long-standing bug:
 *                if thetaMotRdbk changed by small amount, program didn't react
 *                The problem was a single event flag tested in two state sets
 *                (though cleared in only one).  Now only one ss tests the event
 *                and it posts a different event thetaMotRdbkPseudo_mon to alert
 *                the other ss.
 */

/* General Purpose PV's used by sequence */
short kohzuCtlDebug;
assign  kohzuCtlDebug to "{P}{MONO}CtlDebug"; 
monitor kohzuCtlDebug;

string  seqMsg1;
assign  seqMsg1 to "{P}{MONO}SeqMsg1"; 

string  seqMsg2;
assign  seqMsg2 to "{P}{MONO}SeqMsg2"; 

short   opAlert;
assign  opAlert to "{P}{MONO}Alert";

short   opAck;
assign  opAck to "{P}{MONO}OperAck";
monitor opAck;

/* {P}{MONO}Put is the user's "Move" button.  If {P}{MONO}Mode==0, user
 * can write whatever values he wants to whatever fields, and set
 * {P}{MONO}Put=1 to say "make it so".
 */
short   putVals; 
assign  putVals to "{P}{MONO}Put";
monitor putVals;

/* {P}{MONO}Mode is the user's "Manual/Auto" menu.  If {P}{MONO}Mode==0,
 * nothing happens to motors until the user sets {P}{MONO}Put=1.  If
 * {P}{MONO}Mode==1, we respond directly to changes in {P}{MONO}E, etc. 
 */
short   autoMode;
assign  autoMode to "{P}{MONO}Mode";
monitor autoMode;

/* Menu: Normal, Channel Cut, Freeze Z, Freeze Y */
short   ccMode;
assign  ccMode to "{P}{MONO}Mode2";
monitor ccMode;

short   kohzuMoving; 
assign  kohzuMoving to "{P}{MONO}Moving";
monitor kohzuMoving;

short   kohzuDone; 

/* crystal configuration */
double  H;
assign  H to "{P}{MONO}H";
monitor H;
evflag  H_mon;
sync    H H_mon;

double  K;
assign  K to "{P}{MONO}K";
monitor K;
evflag  K_mon;
sync    K K_mon;

double  L;
assign  L to "{P}{MONO}L";
monitor L;
evflag  L_mon;
sync    L L_mon;

double  A;
assign  A to "{P}{MONO}A";
monitor A;
evflag  A_mon;
sync    A A_mon;

double  D;
assign  D to "{P}{MONO}2dSpacing";

/* E, Lambda, and Theta */
/* C defect: monitored E and lambda themselves, so a request landing
 * between a pass's assign and its put was put as the pass's value.
 * Requests land in EReq and lambdaReq, which the request arms take. */
double  E;
assign  E to "{P}{MONO}E";

double  EReq;
assign  EReq to "{P}{MONO}E";
monitor EReq;
evflag  E_mon;
sync    EReq E_mon;

double  EHi;
assign  EHi to "{P}{MONO}E.HLM";

double  ELo;
assign  ELo to "{P}{MONO}E.LLM";

double  ERdbk;
assign  ERdbk to "{P}{MONO}ERdbk";

double  lambda;
assign  lambda to "{P}{MONO}Lambda";

double  lambdaReq;
assign  lambdaReq to "{P}{MONO}Lambda";
monitor lambdaReq;
evflag  lambda_mon;
sync    lambdaReq lambda_mon;

double  lambdaHi;
assign  lambdaHi to "{P}{MONO}Lambda.HLM";

double  lambdaLo;
assign  lambdaLo to "{P}{MONO}Lambda.LLM";

double  lambdaRdbk;
assign  lambdaRdbk to "{P}{MONO}LambdaRdbk";

/* C defect: monitored theta itself, so a theta event landing after
 * thChanged clamped it, such as the unclamped echo of the put that led
 * there, replaced the theta calcMovements moved to. Requests land in
 * thetaReq, which the request arms take into theta. */
double  theta;
assign  theta to "{P}{MONO}Theta";

double  thetaReq;
assign  thetaReq to "{P}{MONO}Theta";
monitor thetaReq;
evflag  theta_mon;
sync    thetaReq theta_mon;

double  thetaHi;
assign  thetaHi to "{P}{MONO}Theta.HLM";

double  thetaLo;
assign  thetaLo to "{P}{MONO}Theta.LLM";

double  thetaRdbk;
assign  thetaRdbk to "{P}{MONO}ThetaRdbk";

/* The following are "soft" PV's that contain motor information that is
 * either echoed to or echoed from the actual motor records. The reason
 * for these PV's is so the medm display can be independent from the
 * motor PV names
 */

string  thetaMotName;
assign  thetaMotName to "{P}{MONO}ThetaPv"; 

string  yMotName;
assign  yMotName to "{P}{MONO}YPv"; 

string  zMotName;
assign  zMotName to "{P}{MONO}ZPv"; 

double  thetaMotCmdEcho;
assign  thetaMotCmdEcho to "{P}{MONO}ThetaCmd";

double  yMotCmdEcho;
assign  yMotCmdEcho to "{P}{MONO}YCmd";

double  zMotCmdEcho;
assign  zMotCmdEcho to "{P}{MONO}ZCmd";

double  thetaMotRdbkEcho;
assign  thetaMotRdbkEcho to "{P}{MONO}ThetaMotRdbk";

double  yMotRdbkEcho;
assign  yMotRdbkEcho to "{P}{MONO}YRdbk";

double  zMotRdbkEcho;
assign  zMotRdbkEcho to "{P}{MONO}ZRdbk";

double  thetaMotVelEcho;
assign  thetaMotVelEcho to "{P}{MONO}ThetaVel";

double  yMotVelEcho;
assign  yMotVelEcho to "{P}{MONO}YVel";

double  zMotVelEcho;
assign  zMotVelEcho to "{P}{MONO}ZVel";

short   thetaDmovEcho;
assign  thetaDmovEcho to "{P}{MONO}ThetaDmov";

short   yDmovEcho;
assign  yDmovEcho to "{P}{MONO}YDmov";

short   zDmovEcho;
assign  zDmovEcho to "{P}{MONO}ZDmov"; 

/* Confusion may result between the theta motor variables and the
 * user-entered theta. To eliminate confusion, variables associated
 * with the theta motor are named thetaMotXXXXX  unless it is clear they
 * refer to a motor (e.g. thSpeed, etc)
 */

short   thetaMotStop;
assign  thetaMotStop to "{P}{M_THETA}.STOP";

short   yStop;
assign  yStop to "{P}{M_Y}.STOP";

short   zStop;
assign  zStop to "{P}{M_Z}.STOP";

short   thetaDmov;
assign  thetaDmov to "{P}{M_THETA}.DMOV";
monitor thetaDmov;
evflag  thetaDmov_mon;
sync    thetaDmov thetaDmov_mon;

short   yDmov;
assign  yDmov to "{P}{M_Y}.DMOV";
monitor yDmov;
evflag  yDmov_mon;
sync    yDmov yDmov_mon;

short   zDmov;
assign  zDmov to "{P}{M_Z}.DMOV";
monitor zDmov;
evflag  zDmov_mon;
sync    zDmov zDmov_mon;

/* LIMIT SWITCHES */
short   thetaHls;
short   thetaLls;
short   yHls;
short   yLls;
short   zHls;
short   zLls;


/* Use the following assignments for the real system */
%%  short  kohzuCtl_soft_LS_TEST = 0;
assign  thetaHls to "{P}{M_THETA}.HLS";
assign  thetaLls to "{P}{M_THETA}.LLS";
assign  yHls to "{P}{M_Y}.HLS";
assign  yLls to "{P}{M_Y}.LLS";
assign  zHls to "{P}{M_Z}.HLS";
assign  zLls to "{P}{M_Z}.LLS";


/* Use the following assignment to simulate switches with soft PV's */
/*
%%  short  kohzuCtl_soft_LS_TEST = 1;
assign  thetaHls to "{P}{MONO}thetaSimHls";
assign  thetaLls to "{P}{MONO}thetaSimLls";
assign  yHls to "{P}{MONO}ySimHls";
assign  yLls to "{P}{MONO}ySimLls";
assign  zHls to "{P}{MONO}zSimHls";
assign  zLls to "{P}{MONO}zSimLls";
*/

monitor thetaHls;
monitor thetaLls;
monitor yHls;
monitor yLls;
monitor zHls;
monitor zLls;

double  thetaMotDesired;
assign  thetaMotDesired to "{P}{MONO}ThetaSet";
monitor thetaMotDesired;

double  yMotDesired;
assign  yMotDesired to "{P}{MONO}YSet";
monitor yMotDesired;
double  yMotDesiredHi;
assign  yMotDesiredHi to "{P}{MONO}YSet.DRVH";
double  yMotDesiredLo;
assign  yMotDesiredLo to "{P}{MONO}YSet.DRVL";

double  zMotDesired;
assign  zMotDesired to "{P}{MONO}ZSet";
monitor zMotDesired;

double  thetaMotHiLim;
assign  thetaMotHiLim to "{P}{M_THETA}.HLM";
monitor thetaMotHiLim;
evflag  thetaMotHiLim_mon;
sync    thetaMotHiLim thetaMotHiLim_mon;

double  thetaMotLoLim;
assign  thetaMotLoLim to "{P}{M_THETA}.LLM";
monitor thetaMotLoLim;
evflag  thetaMotLoLim_mon;
sync    thetaMotLoLim thetaMotLoLim_mon;

double  yMotHiLim;
assign  yMotHiLim to "{P}{M_Y}.HLM";
monitor yMotHiLim;

double  yMotLoLim;
assign  yMotLoLim to "{P}{M_Y}.LLM";
monitor yMotLoLim;

double  zMotHiLim;
assign  zMotHiLim to "{P}{M_Z}.HLM";
monitor zMotHiLim;

double  zMotLoLim;
assign  zMotLoLim to "{P}{M_Z}.LLM";
monitor zMotLoLim;

double  thetaMotCmd;
assign  thetaMotCmd to "{P}{M_THETA}";

double  yMotCmd;
assign  yMotCmd to "{P}{M_Y}";

double  zMotCmd;
assign  zMotCmd to "{P}{M_Z}";

double  thetaMotCmdMon;
assign  thetaMotCmdMon to "{P}{M_THETA}";
monitor thetaMotCmdMon;
evflag  thetaMotCmdMon_mon;
sync    thetaMotCmdMon thetaMotCmdMon_mon;

double  yMotCmdMon;
assign  yMotCmdMon to "{P}{M_Y}";
monitor yMotCmdMon;
evflag  yMotCmdMon_mon;
sync    yMotCmdMon yMotCmdMon_mon;

double  zMotCmdMon;
assign  zMotCmdMon to "{P}{M_Z}";
monitor zMotCmdMon;
evflag  zMotCmdMon_mon;
sync    zMotCmdMon zMotCmdMon_mon;

double  thTime;
double  oldThSpeed;
assign  oldThSpeed to "{P}{M_THETA}.VELO";
double  newThSpeed;
double  thSpeed;
assign  thSpeed to "{P}{M_THETA}.VELO";
monitor thSpeed;
evflag  thetaMotVel_mon;
sync    thSpeed thetaMotVel_mon;

double  yTime;
double  oldYSpeed;
assign  oldYSpeed to "{P}{M_Y}.VELO";
double  newYSpeed;
double  ySpeed;
assign  ySpeed to "{P}{M_Y}.VELO";
monitor ySpeed;
evflag  yMotVel_mon;
sync    ySpeed yMotVel_mon;

double  zTime;
double  oldZSpeed;
assign  oldZSpeed to "{P}{M_Z}.VELO";
double  newZSpeed;
double  zSpeed;
assign  zSpeed to "{P}{M_Z}.VELO";
monitor zSpeed;
evflag  zMotVel_mon;
sync    zSpeed zMotVel_mon;

/* C defect: put thSpeed, ySpeed and zSpeed themselves, which are
 * monitored: the echo of the previous speed put could land before a put,
 * which then wrote that speed. The puts go through these. */
double  thSpeedCmd;
assign  thSpeedCmd to "{P}{M_THETA}.VELO";

double  ySpeedCmd;
assign  ySpeedCmd to "{P}{M_Y}.VELO";

double  zSpeedCmd;
assign  zSpeedCmd to "{P}{M_Z}.VELO";

short speedControl;
assign  speedControl to "{P}{MONO}SpeedCtrl";
monitor speedControl;

double  thetaMotRdbk;
assign  thetaMotRdbk to "{P}{M_THETA}.RBV";
monitor thetaMotRdbk;
evflag  thetaMotRdbk_mon;
sync    thetaMotRdbk thetaMotRdbk_mon;
evflag  thetaMotRdbkPseudo_mon;

double  yMotRdbk;
assign  yMotRdbk to "{P}{M_Y}.RBV";
monitor yMotRdbk;
evflag  yMotRdbk_mon;
sync    yMotRdbk yMotRdbk_mon;

double  zMotRdbk;
assign  zMotRdbk to "{P}{M_Z}.RBV";
monitor zMotRdbk;
evflag  zMotRdbk_mon;
sync    zMotRdbk zMotRdbk_mon;

/* Set/Use support */
short   useSetMode;
assign  useSetMode to "{P}{MONO}UseSet";
monitor useSetMode;
evflag  useSetMode_mon;
sync    useSetMode useSetMode_mon;

short   thetaMotSet;
assign  thetaMotSet to "{P}{M_THETA}.SET";
monitor thetaMotSet;
evflag  thetaMotSet_mon;
sync    thetaMotSet thetaMotSet_mon;

short   yMotSet;
assign  yMotSet to "{P}{M_Y}.SET";
monitor yMotSet;
evflag  yMotSet_mon;
sync    yMotSet yMotSet_mon;

short   zMotSet;
assign  zMotSet to "{P}{M_Z}.SET";
monitor zMotSet;
evflag  zMotSet_mon;
sync    zMotSet zMotSet_mon;

/* C defect: put useSetMode and the motor .SET flags themselves, which
 * are monitored: the echo of the previous put could land before a put,
 * which then wrote that flag. The puts go through these. */
short   useSetModeCmd;
assign  useSetModeCmd to "{P}{MONO}UseSet";

short   thetaMotSetCmd;
assign  thetaMotSetCmd to "{P}{M_THETA}.SET";

short   yMotSetCmd;
assign  yMotSetCmd to "{P}{M_Y}.SET";

short   zMotSetCmd;
assign  zMotSetCmd to "{P}{M_Z}.SET";

/* yOffset (y dist from incident/exit beams to center of rotation) */
double  yOffset;
assign  yOffset to "{P}{MONO}yOffset";
monitor yOffset;
evflag  yOffset_mon;
sync    yOffset yOffset_mon;

double  yOffsetHi;
assign  yOffsetHi to "{P}{MONO}yOffset.DRVH";

double  yOffsetLo;
assign  yOffsetLo to "{P}{MONO}yOffset.DRVL";

double  yOffsetInit;
assign  yOffsetInit to "{P}{MONO}yOffset";

%%#include <string.h>
%%#include <math.h>

%%double kohzuCtl_soft_calc2dSpacing();
%%char *kohzuCtl_soft_pmacP;
%%char *kohzuCtl_soft_pmacM;
%%char *kohzuCtl_soft_pmacGEOM;
short geom;

short  causedMove;
short  initSeq;
short  initSeqDone;
float  avg;
double nint;
double hc;
double radConv;

int riskAverse;
double prev_E;
double prev_theta;
double prev_lambda;
double prev_thetaMotDesired;
double prev_yMotDesired;
double prev_zMotDesired;
int willViolateLimit;

ss kohzuCtl_soft {
state init {
	when () {
		initSeq = 0;
		riskAverse = 0;	/* don't set to manual mode for limit errors */
	} state initSequence
}

state initSequence {
	when (initSeq == 0) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:initSequence:initSeq=0\n");
		hc = 12.3984244;
		radConv = 57.2958;
		thetaMotStop = 1;
		yStop = 1;
		zStop = 1;
		putVals = 0;
		pvPut(putVals);
		autoMode = 0;
		pvPut(autoMode);
		opAck = 0;
		pvPut(opAck);
		/* C defect: strcpy and strcat wrote P and M_* unbounded into the
		 * 40-byte names, past them when the two exceed 39 chars. */
%{
		kohzuCtl_soft_pmacP = seq_macValueGet(ssId,"P");
		kohzuCtl_soft_pmacM = seq_macValueGet(ssId,"M_THETA");
		if (kohzuCtl_soft_pmacP == 0 || kohzuCtl_soft_pmacM == 0) {
			sprintf(thetaMotName,"??????");
		} else {
			snprintf(thetaMotName, sizeof(thetaMotName), "%s%s", kohzuCtl_soft_pmacP, kohzuCtl_soft_pmacM);
		}
		kohzuCtl_soft_pmacM = seq_macValueGet(ssId,"M_Y");
		if (kohzuCtl_soft_pmacP == 0 || kohzuCtl_soft_pmacM == 0) {
			sprintf(yMotName,"??????");
		} else {
			snprintf(yMotName, sizeof(yMotName), "%s%s", kohzuCtl_soft_pmacP, kohzuCtl_soft_pmacM);
		}
		kohzuCtl_soft_pmacM = seq_macValueGet(ssId,"M_Z");
		if (kohzuCtl_soft_pmacP == 0 || kohzuCtl_soft_pmacM == 0) {
			sprintf(zMotName,"??????");
		} else {
			snprintf(zMotName, sizeof(zMotName), "%s%s", kohzuCtl_soft_pmacP, kohzuCtl_soft_pmacM);
		}
		kohzuCtl_soft_pmacGEOM = seq_macValueGet(ssId,"GEOM");
		geom = 1;
		if (*kohzuCtl_soft_pmacGEOM == '2') geom = 2;
}%
		if (geom == 1) {
			yOffsetHi = 17.5 + .000001;
			pvPut(yOffsetHi);
			yOffsetLo = 17.5 - .000001;
			pvPut(yOffsetLo);
			yOffset = 17.5;
			/* C defect: put yOffset itself, which is monitored: the
			 * yOffsetLo put clamps VAL to 17.499999 and posts it, and that
			 * event could land before the put, which then wrote 17.499999. */
			yOffsetInit = 17.5;
			pvPut(yOffsetInit);
			yMotDesiredHi = 0.;
			pvPut(yMotDesiredHi);
			yMotDesiredLo = -35;
			pvPut(yMotDesiredLo);
		} else {
			yMotDesiredHi = 60.;
			pvPut(yMotDesiredHi);
			yMotDesiredLo = 0.;
			pvPut(yMotDesiredLo);
		}
		pvPut(thetaMotName);
		pvPut(yMotName);
		pvPut(zMotName);
		initSeq = 1;
	} state dInputChanged

	when (initSeq == 1) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:initSequence:initSeq=1\n");
		initSeq = 2;
	} state chkMotorLimits

	when (initSeq == 2) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:initSequence:initSeq=2\n");
		initSeq = 3;
	} state updateRdbk

	when ((initSeq == 3) && delay(1.)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:initSequence:initSeq=3\n");
		sprintf(seqMsg1, "Kohzu Control Ready");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		initSeqDone=1;
		theta = thetaMotRdbk;
		pvPut(theta);
	} state thChanged
}

state waitForCmndEnter {
	when (initSeqDone && delay(.1)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmndEnter:initSeqDone\n");
		efClear(H_mon);
		efClear(K_mon);
		efClear(L_mon);
		efClear(A_mon);
		efClear(E_mon);
		efClear(lambda_mon);
		efClear(theta_mon);

		thetaMotRdbkEcho = thetaMotRdbk;
		pvPut(thetaMotRdbkEcho);
		yMotRdbkEcho = yMotRdbk;
		pvPut(yMotRdbkEcho);
		zMotRdbkEcho = zMotRdbk;
		pvPut(zMotRdbkEcho);
		thetaMotVelEcho = thSpeed;
		pvPut(thetaMotVelEcho);
		yMotVelEcho = ySpeed;
		pvPut(yMotVelEcho);
		zMotVelEcho = zSpeed;
		pvPut(zMotVelEcho);
		thetaMotCmdEcho = thetaMotCmdMon;
		pvPut(thetaMotCmdEcho);
		yMotCmdEcho = yMotCmdMon;
		pvPut(yMotCmdEcho);
		zMotCmdEcho = zMotCmdMon;
		pvPut(zMotCmdEcho);
		thetaDmovEcho = thetaDmov;
		pvPut(thetaDmovEcho);
		yDmovEcho = yDmov;
		pvPut(yDmovEcho);
		zDmovEcho = zDmov;
		pvPut(zDmovEcho);
		if (autoMode || useSetMode) {
			thetaRdbk = thetaMotRdbk;
			pvPut(thetaRdbk);
			lambdaRdbk = D*sin(thetaRdbk/radConv);
			pvPut(lambdaRdbk);
			ERdbk = hc/lambdaRdbk;
			pvPut(ERdbk);
		}
		opAck = 0;
		pvPut(opAck);
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmndEnter: kohzuMoving=%d; kohzuDone=%d\n", kohzuMoving, kohzuDone);
		if (kohzuMoving && kohzuDone) {
			if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmndEnter: asserting 'done'.\n");
			kohzuMoving=0; kohzuDone = 0;
			pvPut(kohzuMoving);
		}
		/* store previous values, in case we have to restore them */
		prev_theta = theta;
		prev_E = E;
		prev_lambda = lambda;
		prev_thetaMotDesired = thetaMotDesired;
		prev_yMotDesired = yMotDesired;
		prev_zMotDesired = zMotDesired;
		willViolateLimit = 0;
	} state waitForCmnd  

	when (!initSeqDone) {
	} state initSequence
}

state waitForCmnd {
	when (efTest(H_mon) || efTest(K_mon) || efTest(L_mon) || efTest(A_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:HKLA_mon\n");
		sprintf(seqMsg1, "Lattice Spacing Changed");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		efClear(H_mon);
		efClear(K_mon);
		efClear(L_mon);
		efClear(A_mon);
	} state dInputChanged

	when (efTest(thetaMotHiLim_mon) || efTest(thetaMotLoLim_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:thetaMotXxLim_mon\n");
		sprintf(seqMsg1, "Theta Motor Soft Limits Changed");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		efClear(thetaMotHiLim_mon);
		efClear(thetaMotLoLim_mon);
	} state thetaLimits

	when (efTestAndClear(thetaMotRdbk_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:thetaMotRdbk_mon\n");
		efSet(thetaMotRdbkPseudo_mon);
	} state updateRdbk

	when (efTestAndClear(E_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:E_mon\n");
		E = EReq;
	} state eChanged

	when (efTestAndClear(lambda_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:lambda_mon\n");
		lambda = lambdaReq;
	} state lChanged

	when (efTestAndClear(theta_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:theta_mon\n");
		theta = thetaReq;
	} state thChanged

	when (putVals && delay(.1)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:putVals\n");
	} state moveKohzu

	when (efTestAndClear(yOffset_mon)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:yOffset_mon\n");
		autoMode = 0;
		pvPut(autoMode);
		/* C defect: sprintf wrote this %f message unbounded into the 40-byte
		 * seqMsg1, past it for a yOffset of 1e12 or more, or -1e11 or less. */
		%%snprintf(seqMsg1, sizeof(seqMsg1), "y offset changed to %f", yOffset);
		pvPut(seqMsg1);
		sprintf(seqMsg2, "Set to Manual Mode");
		pvPut(seqMsg2);
	} state calcMovements

	when (opAck) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:waitForCmnd:opAck\n");
		opAlert = 0;
		pvPut(opAlert);
		sprintf(seqMsg1, " ");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
	} state waitForCmndEnter
}

state dInputChanged {
	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:dInputChanged\n");
		/* C's monitored theta already held a request that landed. */
		if (efTest(theta_mon)) theta = thetaReq;
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Set to Manual Mode");
		pvPut(seqMsg2);
		D = kohzuCtl_soft_calc2dSpacing();
		pvPut(D);
		pvPut(seqMsg1);
		lambdaHi = D*sin(thetaHi/radConv);
		pvPut(lambdaHi);
		lambdaLo = D*sin(thetaLo/radConv);
		pvPut(lambdaLo);
		EHi = hc/lambdaLo;
		pvPut(EHi);
		ELo = hc/lambdaHi;
		pvPut(ELo);
		pvPut(opAlert);
	} state thChanged
}

state thetaLimits {
	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:thetaLimits\n");
		thetaHi = thetaMotHiLim;
		thetaLo = thetaMotLoLim;
		if (thetaHi > 89.0) {
			thetaHi = 89.0;
			sprintf(seqMsg2, "ThetaHiLim > 89 deg. (Using 89 deg.)");
			pvPut(seqMsg2);
		}
		if (thetaLo < 1.0) {
			thetaLo = 1.0;
			sprintf(seqMsg2, "ThetaLoLim < 1 deg. (Using 1 deg.)");
			pvPut(seqMsg2);
		}
		pvPut(thetaHi);
		pvPut(thetaLo);
		lambdaHi = D*sin(thetaHi/radConv);
		pvPut(lambdaHi);
		lambdaLo = D*sin(thetaLo/radConv);
		pvPut(lambdaLo);
		EHi = hc/lambdaLo;
		pvPut(EHi);
		ELo = hc/lambdaHi;
		pvPut(ELo);
	} state checkDone
}

state eChanged {
	when () { 
		if (kohzuCtlDebug) %%printf("kohzuCtl:eChanged\n");
		lambda = hc/E;
		pvPut(lambda);
	} state lChanged
}

state lChanged {
	when (lambda > D) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:lChanged: lambda>D\n");
		/* Don't change Theta, notify operator */
		sprintf(seqMsg1, "Wavelength > 2d spacing.");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
	} state thChanged 

	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:lChanged\n");
		theta = radConv * asin(lambda/D);
		pvPut(theta);
	} state thChanged
}

state thChanged {
	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:thChanged\n");
		if ((theta <= thetaLo) || (theta >= thetaHi)) {
			sprintf(seqMsg1, "Theta constrained to LIMIT");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
			if (theta < thetaLo) theta = thetaLo;
			if (theta > thetaHi) theta = thetaHi;
			if (riskAverse) {
				autoMode = 0;
				pvPut(autoMode);
				sprintf(seqMsg2, "Set to Manual Mode");
				pvPut(seqMsg2);
			} else {
				willViolateLimit = 1;
			}
		}
		lambda = D*sin(theta/radConv);
		pvPut(lambda);
		E = hc/lambda;
		pvPut(E);            

		/* Update the readback values as well */
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = D * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);
	} state calcMovements
}

state calcMovements {
	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:calcMovements\n");

		/* Calculate new Y and Z, check against limits */
		thetaMotDesired = theta;
		if (geom == 1) {
			yMotDesired = -yOffset/cos(theta/radConv);
			zMotDesired = yOffset/sin(theta/radConv);
		} else {
			yMotDesired = yOffset/(2*cos(theta/radConv));
			zMotDesired = yOffset/(2*sin(theta/radConv));
		}
		if (ccMode != 1 && ccMode != 3 &&
		   ((yMotDesired < yMotLoLim)||(yMotDesired > yMotHiLim))) {
			sprintf(seqMsg1, "Y will exceed soft limits");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
			if (riskAverse) {
				sprintf(seqMsg2, "Setting to Manual Mode");
				pvPut(seqMsg2);
				opAlert = 1;
				pvPut(opAlert);
				autoMode = 0;
				pvPut(autoMode);
			} else {
				willViolateLimit = 1;
			}
		}
		if (ccMode != 1 && ccMode != 2 &&
		   ((zMotDesired < zMotLoLim)||(zMotDesired > zMotHiLim))) {
			sprintf(seqMsg1, "Z will exceed soft limits");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
			if (riskAverse) {
				sprintf(seqMsg2, "Setting to Manual Mode");
				pvPut(seqMsg2);
				opAlert = 1;
				pvPut(opAlert);
				autoMode = 0;
				pvPut(autoMode);
			} else {
				willViolateLimit = 1;
			}
		}
		pvPut(thetaMotDesired);
		pvPut(yMotDesired);
		pvPut(zMotDesired);

		/***** Calculate required speeds ******/
		/* C defect: took the old speeds from thSpeed, ySpeed and zSpeed,
		 * whose monitors can still hold the previous move's speed after its
		 * restore, which this move's restore then put back. They are read
		 * from VELO, and the monitored ones only feed the VelEcho mirrors. */
		pvGet(oldThSpeed);
		pvGet(oldYSpeed);
		pvGet(oldZSpeed);
		/* C defect: divided by each motor's speed, which speed control
		 * itself sets to 0 for a motor with no travel. A zero speed gave
		 * an inf time, which made every speed 0, or 0/0, a NaN, which
		 * failed every comparison below and left the Z branch dividing by
		 * a zTime of 0 or NaN. A motor with no speed takes zero time. */
		thTime = 0.0;
		if (oldThSpeed > 0) thTime = fabs(theta-thetaMotRdbk)/oldThSpeed;
		yTime = 0.0;
		if (oldYSpeed > 0) yTime = fabs(yMotDesired-yMotRdbk)/oldYSpeed;
		zTime = 0.0;
		if (oldZSpeed > 0) zTime = fabs(zMotDesired-zMotRdbk)/oldZSpeed;
		/* Frozen motors take zero time to move. (tmm) */
		if (ccMode == 1) {
			/* channel-cut mode: Y,Z frozen */
			yTime = 0.0;
			zTime = 0.0;
		} else if (ccMode == 2) {
			/* Z frozen */
			zTime = 0.0;
		} else if (ccMode == 3) {
			/* Y frozen */
			yTime = 0.0;
		}
		/*** Test for longest time ***/
		if ((thTime >= yTime) && (thTime >= zTime)) {
			newThSpeed = oldThSpeed;
			/* C defect: divided Y's and Z's travel by thTime when it was 0,
			 * putting 0/0, a NaN, into their VELO when no motor had to move. */
			if (thTime > 0) {
				newYSpeed = fabs(yMotDesired-yMotRdbk)/thTime;
				newZSpeed = fabs(zMotDesired-zMotRdbk)/thTime;
			} else {
				newYSpeed = oldYSpeed;
				newZSpeed = oldZSpeed;
			}
		} else if ((yTime >= thTime) && (yTime >= zTime)) {
			newThSpeed = fabs(theta-thetaMotRdbk)/yTime;
			newYSpeed = oldYSpeed;
			newZSpeed = fabs(zMotDesired-zMotRdbk)/yTime;
		} else {
			newThSpeed = fabs(theta-thetaMotRdbk)/zTime;
			newYSpeed = fabs(yMotDesired-yMotRdbk)/zTime;
			newZSpeed = oldZSpeed;
		}

	} state moveKohzu
}

state moveKohzu {
	when (autoMode || putVals || useSetMode) {
		if (willViolateLimit) {
			E = prev_E;
			pvPut(E);
			theta = prev_theta;
			pvPut(theta);
			lambda = prev_lambda;
			pvPut(lambda);
			thetaMotDesired = prev_thetaMotDesired;
			pvPut(thetaMotDesired);
			yMotDesired = prev_yMotDesired;
			pvPut(yMotDesired);
			zMotDesired = prev_zMotDesired;
			pvPut(zMotDesired);
			sprintf(seqMsg2, "Command ignored");
			pvPut(seqMsg2);
		} else {
		if (kohzuCtlDebug) %%printf("kohzuCtl:moveKohzu:auto|put\n");
			if (speedControl) {
				thSpeedCmd = newThSpeed;
				ySpeedCmd = newYSpeed;
				zSpeedCmd = newZSpeed;
				pvPut(thSpeedCmd);
				if (ccMode == 0 || ccMode == 2) {
					pvPut(ySpeedCmd);
				}
				if (ccMode == 0 || ccMode == 3) {
					pvPut(zSpeedCmd);
				}
			}
			thetaMotCmd = thetaMotDesired;
			yMotCmd = yMotDesired;
			zMotCmd = zMotDesired;
			pvPut(thetaMotCmd);
			if (ccMode == 0) {
				pvPut(yMotCmd);
				pvPut(zMotCmd);
			} else if (ccMode == 1) {
				;
			} else if (ccMode == 2) {
				pvPut(yMotCmd);
			} else if (ccMode == 3) {
				pvPut(zMotCmd);
			}
			putVals = 0;
			pvPut(putVals);
			causedMove = 1;
		}
	} state updateRdbk

	when (1) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:moveKohzu\n");
	} state waitForCmndEnter
}

state checkDone {
	when (thetaDmov && yDmov && zDmov) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:checkDone: done\n");
		kohzuDone = 1;
	} state waitForCmndEnter

	when (!thetaDmov || !yDmov || !zDmov) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:checkDone: not done\n");
	} state updateRdbk

}

state updateRdbk {
	when (willViolateLimit == 1) {
	} state waitForCmndEnter

	when (thetaMotRdbk == 0) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:updateRdbk:thetaMotRdbk==0\n");
		sprintf(seqMsg1, "Theta Motor Readback is 0 !");
		pvPut(seqMsg1);
		sprintf(seqMsg2, "Monochromator stopped");
		pvPut(seqMsg2);
		opAlert = 1;
		pvPut(opAlert);
	} state waitForCmndEnter

	/* when (thetaDmov && yDmov && zDmov) {  */  /* on linux, some motor readback monitors arrive after *Dmov */
	when (thetaDmov && yDmov && zDmov && delay(0.01)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:updateRdbk:*Dmov (done=%d,moving=%d)\n", kohzuDone, kohzuMoving);
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = D * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);
		kohzuDone = 1;
		efSet(thetaMotRdbkPseudo_mon);
		efClear(thetaMotRdbk_mon);
		if (kohzuCtlDebug) %%printf("kohzuCtl:updateRdbk: done\n");
	} state thetaMotStopped


	when (thetaHls || thetaLls) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:updateRdbk: Th limit switch\n");
		sprintf(seqMsg1, "Theta Motor hit a limit switch!");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Setting to Manual Mode");
		pvPut(seqMsg2);
	} state stopKohzu

	/* Ignore Y limit switches in "channel-cut" and "freeze-y" modes */
	when ((ccMode != 1) && (ccMode != 3) && (yHls || yLls)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:updateRdbk: Y limit switch\n");
		sprintf(seqMsg1, "Y Motor hit a limit switch!");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Setting to Manual Mode");
		pvPut(seqMsg2);
	} state stopKohzu

	/* Ignore Z limit switches in "channel-cut" and "freeze-z" modes */
	when ((ccMode != 1) && (ccMode != 2) && (zHls || zLls)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:updateRdbk: Z limit switch\n");
		sprintf(seqMsg1, "Z Motor hit a limit switch!");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Setting to Manual Mode");
		pvPut(seqMsg2);
	} state stopKohzu

	when ((!thetaDmov || !yDmov || !zDmov) && delay(.1)) {
		if (kohzuCtlDebug>1) %%printf("kohzuCtl:updateRdbk:moving\n");
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = D * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);
	} state updateRdbk
}

state thetaMotStopped {
	when (causedMove) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:thetaMotStopped:causedMove (done=%d,moving=%d)\n", kohzuDone, kohzuMoving);
		if (speedControl) {
			thSpeedCmd = oldThSpeed;
			ySpeedCmd = oldYSpeed;
			zSpeedCmd = oldZSpeed;
			pvPut(thSpeedCmd);
			pvPut(ySpeedCmd);
			pvPut(zSpeedCmd);
		}
		if (kohzuCtlDebug) %%printf("kohzuCtl:thetaMotStopped: ySpeed = %f\n", ySpeed);
		causedMove = 0;
	} state waitForCmndEnter

	/* Someone moved the Theta motor directly */
	when (!causedMove) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:thetaMotStopped:!causedMove (done=%d,moving=%d)\n", kohzuDone, kohzuMoving);
		if (initSeqDone) {
			opAlert = 1;
			pvPut(opAlert);
			sprintf(seqMsg1, "Only Theta Motor Moved");
			pvPut(seqMsg1);
			sprintf(seqMsg2, "Hit MOVE to Adjust Y & Z");
			pvPut(seqMsg2);
			autoMode = 0;
			pvPut(autoMode);
		}
		theta = thetaMotRdbk;
		pvPut(theta);
		lambda = D*sin(theta/radConv);
		pvPut(lambda);
		E = hc/lambda;
		pvPut(E);  
	} state thChanged
}

state chkMotorLimits {
	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:chkMotorLimits\n");
		opAlert = 0;
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = D * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);

		if (ccMode != 1 && ccMode != 3) {
			if (yMotRdbk == 0) {
				sprintf(seqMsg1, "Y Readback of 0 not reasonable");
				pvPut(seqMsg1);
				opAlert = 1;
			}
			if ((yMotRdbk < yMotLoLim) || (yMotRdbk > yMotHiLim)) {
				sprintf(seqMsg1, "Y Readback outside Motor Limits");
				pvPut(seqMsg1);
				opAlert = 1;
			}
		}
		if (ccMode != 1 && ccMode != 2) {
			if (zMotRdbk == 0) {
				sprintf(seqMsg1, "Z Readback of 0 not reasonable");
				pvPut(seqMsg1);
				opAlert = 1;
			}
			if ((zMotRdbk < zMotLoLim)||(zMotRdbk > zMotHiLim)) {
				sprintf(seqMsg1, "Z Readback outside Motor Limits");
				pvPut(seqMsg1);
				opAlert = 1;
			}
		}
		if (thetaMotRdbk == 0) {
			sprintf(seqMsg1, "Theta Readback of 0 not reasonable");
			pvPut(seqMsg1);
			opAlert = 1;
		}
		if ((thetaMotRdbk<thetaMotLoLim)||(thetaMotRdbk>thetaMotHiLim)) {
			sprintf(seqMsg1, "Theta Readback outside Motor Limits");
			pvPut(seqMsg1);
			opAlert = 1;
		}
		if (opAlert) {
			pvPut(opAlert);
			autoMode = 0;
			pvPut(autoMode);
			sprintf(seqMsg2, "Set to Manual Mode");
			pvPut(seqMsg2);
		}
	} state chkMotorLimitsExit
}

state chkMotorLimitsExit {
	when (!opAlert) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:chkMotorLimitsExit: OK\n");
		pvPut(opAlert);
		sprintf(seqMsg1, "All motors within limits");
		pvPut(seqMsg1);
	} state waitForCmndEnter

	when (opAlert && delay(2.)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:chkMotorLimitsExit: opAlert\n");
		pvPut(opAlert);
		sprintf(seqMsg2, "Modify Motor Positions/Limits");
		pvPut(seqMsg2);          
	} state chkMotorLimits
}

state stopKohzu {
	when () {
		if (kohzuCtlDebug) %%printf("kohzuCtl:stopKohzu\n");
		pvPut(thetaMotStop);
		pvPut(yStop);
		pvPut(zStop);
	} state stoppedKohzuWait
}

state stoppedKohzuWait {
	when(delay(1.)) {
		if (kohzuCtlDebug) %%printf("kohzuCtl:stoppedKohzuWait\n");
	} state updateRdbk
}

} /* end of state set kohzuCtl  */


/**************************************************************************
 *
 * This State Set keeps the motor independent fields updated 
 *
 **************************************************************************/
ss updatePsuedo_soft {
state init {
	when () {
	} state update
}

state update {
	when (efTestAndClear(thetaMotRdbkPseudo_mon)) {
		thetaMotRdbkEcho = thetaMotRdbk;
		pvPut(thetaMotRdbkEcho);
	} state update

	when (efTestAndClear(yMotRdbk_mon)) {
		yMotRdbkEcho = yMotRdbk;
		pvPut(yMotRdbkEcho);
	} state update

	when (efTestAndClear(zMotRdbk_mon)) {
		zMotRdbkEcho = zMotRdbk;
		pvPut(zMotRdbkEcho);
	} state update

	when (efTestAndClear(thetaMotVel_mon)) {
		thetaMotVelEcho = thSpeed;
		pvPut(thetaMotVelEcho);
	} state update

	when (efTestAndClear(yMotVel_mon)) {
		yMotVelEcho = ySpeed;
		pvPut(yMotVelEcho);
	} state update

	when (efTestAndClear(zMotVel_mon)) {
		zMotVelEcho = zSpeed;
		pvPut(zMotVelEcho);
	} state update

	when (efTestAndClear(thetaMotCmdMon_mon)) {
		thetaMotCmdEcho = thetaMotCmdMon;
		pvPut(thetaMotCmdEcho);
	} state update

	when (efTestAndClear(yMotCmdMon_mon)) {
		yMotCmdEcho = yMotCmdMon;
		pvPut(yMotCmdEcho);
	} state update

	when (efTestAndClear(zMotCmdMon_mon)) {
		zMotCmdEcho = zMotCmdMon;
		pvPut(zMotCmdEcho);
	} state update

	when (efTestAndClear(thetaDmov_mon)) {
		thetaDmovEcho = thetaDmov;
		pvPut(thetaDmovEcho);
	} state update

	when (efTestAndClear(yDmov_mon)) {
		yDmovEcho = yDmov;
		pvPut(yDmovEcho);
	} state update

	when (efTestAndClear(zDmov_mon)) {
		zDmovEcho = zDmov;
		pvPut(zDmovEcho);
	} state update
}
}

/**************************************************************************
 *
 * This State Set keeps the (calibration) set/use flag consistent
 * with the individual motor set/use flags.
 *
 **************************************************************************/
ss updateSet {
state init {
	when () {
	} state update
}

state update {
	when (efTestAndClear(useSetMode_mon)) {
		if (thetaMotSet != useSetMode) {
			thetaMotSetCmd = useSetMode;
			thetaMotSet = thetaMotSetCmd;
			pvPut(thetaMotSetCmd);
		}
		if (yMotSet != useSetMode) {
			yMotSetCmd = useSetMode;
			yMotSet = yMotSetCmd;
			pvPut(yMotSetCmd);
		}
		if (zMotSet != useSetMode) {
			zMotSetCmd = useSetMode;
			zMotSet = zMotSetCmd;
			pvPut(zMotSetCmd);
		}
	} state update

	when (efTestAndClear(thetaMotSet_mon)) {
		if (useSetMode != thetaMotSet) {
			useSetModeCmd = thetaMotSet;
			useSetMode = useSetModeCmd;
			pvPut(useSetModeCmd);
		}
	} state update

	when (efTestAndClear(yMotSet_mon)) {
		if (useSetMode != yMotSet) {
			useSetModeCmd = yMotSet;
			useSetMode = useSetModeCmd;
			pvPut(useSetModeCmd);
		}
	} state update

	when (efTestAndClear(zMotSet_mon)) {
		if (useSetMode != zMotSet) {
			useSetModeCmd = zMotSet;
			useSetMode = useSetModeCmd;
			pvPut(useSetModeCmd);
		}
	} state update
}
}

/***************************************************************************/
/***************************************************************************/
/*   C subroutines */

%{
double kohzuCtl_soft_calc2dSpacing()
{

	double result;

	result = (2*A)/sqrt(H*H + K*K + L*L);

	/* determine validity of (H, K, L) */
	opAlert = 0;
	if (((fmod(H,2)) != (fmod(K,2))) || ((fmod(H,2)) != (fmod(L,2))))
		opAlert = 1;
	avg = (H+K+L)/2.0;
	/* C defect: nint was an int set by NINT(avg), a cast undefined once
	 * |avg| passes INT_MAX; a double from round() is the same value below
	 * that. */
	nint = round(avg);
	if ((fabs(avg-nint) <= .25) && (fmod(nint,2)))
		opAlert = 1;
	if (opAlert) {
		sprintf(seqMsg1, "(H,K,L) is 'forbidden' reflection");
	} else {
		sprintf(seqMsg1, "New d spacing");
	}
	return(result);
}


}%
