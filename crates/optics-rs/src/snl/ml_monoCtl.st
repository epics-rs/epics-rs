program ml_monoCtl  ("P=xxx:, M_THETA=m9, M_THETA2=m12, M_Y=m10, M_Z=m11, Y_OFF=35, GEOM=1")

/* arguments on the command line:
 * 
 * ex.  seq &ml_monoCtl, "P=xxx:, M_THETA=m9, M_THETA2=m12, M_Y=m10, M_Z=m11, Y_OFF=35, GEOM=1"
 *
 *      P        - prefix     
 *      M_THETA  - motor record for Theta    
 *      M_THETA2 - motor record for Theta2  
 *      M_Z      - motor record for Z   
 *      M_Y      - motor record for Y   
 *      Y_OFF    - Y offset
 *      GEOM     - monochromator geometry:
 *                 1: 7bma1
 *                 >1: not implemented   
 */


/*   EDITS & MODIFICATIONS ...
 *

 * 03-12-09 tmm   Adapted from kohzuCtl.st for multilayer monochromator
 */

/* A NaN is not == itself. */
#define SAME(a,b) ((a)==(b) || ((a)!=(a) && (b)!=(b)))

/* General Purpose PV's used by sequence */
short ml_monoCtlDebug;
assign  ml_monoCtlDebug to "{P}ml_monoCtlDebug"; 
monitor ml_monoCtlDebug;

string  seqMsg1;
assign  seqMsg1 to "{P}ml_monoSeqMsg1"; 

string  seqMsg2;
assign  seqMsg2 to "{P}ml_monoSeqMsg2"; 

short   opAlert;
assign  opAlert to "{P}ml_monoAlert";

short   opAck;
assign  opAck to "{P}ml_monoOperAck";
monitor opAck;

/* {P}ml_monoPut is the user's "Move" button.  If {P}ml_monoMode==0, user
 * can write whatever values he wants to whatever fields, and set
 * {P}ml_monoPut=1 to say "make it so".
 */
short   putVals; 
assign  putVals to "{P}ml_monoPut";
monitor putVals;

/* {P}ml_monoMode is the user's "Manual/Auto" menu.  If {P}ml_monoMode==0,
 * nothing happens to motors until the user sets {P}ml_monoPut=1.  If
 * {P}ml_monoMode==1, we respond directly to changes in {P}ml_monoE, etc. 
 */
short   autoMode;
assign  autoMode to "{P}ml_monoMode";
monitor autoMode;

/* Menu: Normal, Channel Cut */
#define MODE_NORMAL 0
#define MODE_FREEZE_Z 1
short   ccMode;
assign  ccMode to "{P}ml_monoMode2";
monitor ccMode;

short   ml_monoMoving; 
assign  ml_monoMoving to "{P}ml_monoMoving";
monitor ml_monoMoving;

short   ml_monoDone; 

/* crystal configuration */
double  Order;
assign  Order to "{P}ml_monoOrder";
monitor Order;
evflag  Order_mon;
sync    Order Order_mon;

double  D;
assign  D to "{P}ml_monoD";
monitor D;
evflag  D_mon;
sync    D D_mon;

double  TwoD;

/* E, Lambda, and Theta */
/* C defect: monitored E, lambda and theta themselves, so an event landing
 * between a pass's assign and its put, such as the clamp dInputChanged's
 * DRVH/DRVL puts post, was put as the pass's value, and a theta request
 * landing before calcMovements replaced the theta it moved to. Requests
 * land in EReq, lambdaReq and thetaReq, which the request arms take. */
double  E;
assign  E to "{P}ml_monoE";

double  EReq;
assign  EReq to "{P}ml_monoE";
monitor EReq;
evflag  E_mon;
sync    EReq E_mon;

double  EHi;
assign  EHi to "{P}ml_monoE.DRVH";

double  ELo;
assign  ELo to "{P}ml_monoE.DRVL";

double  ERdbk;
assign  ERdbk to "{P}ml_monoERdbk";

double  lambda;
assign  lambda to "{P}ml_monoLambda";

double  lambdaReq;
assign  lambdaReq to "{P}ml_monoLambda";
monitor lambdaReq;
evflag  lambda_mon;
sync    lambdaReq lambda_mon;

double  lambdaHi;
assign  lambdaHi to "{P}ml_monoLambda.DRVH";

double  lambdaLo;
assign  lambdaLo to "{P}ml_monoLambda.DRVL";

double  lambdaRdbk;
assign  lambdaRdbk to "{P}ml_monoLambdaRdbk";

double  theta;
assign  theta to "{P}ml_monoTheta";

double  thetaReq;
assign  thetaReq to "{P}ml_monoTheta";
monitor thetaReq;
evflag  theta_mon;
sync    thetaReq theta_mon;

double  thetaHi;
assign  thetaHi to "{P}ml_monoTheta.DRVH";

double  thetaLo;
assign  thetaLo to "{P}ml_monoTheta.DRVL";

double  thetaRdbk;
assign  thetaRdbk to "{P}ml_monoThetaRdbk";

/* C defect: waitForCmndEnter cleared E_mon, lambda_mon and theta_mon,
 * which dropped a request landing in a pass or in the enter's delay, and
 * then asserted done for it. A request is now told from this program's
 * own echo by value: *Last is what the PV held after this program's last
 * put to it or take from it, read back so the AO's DRVH/DRVL clamp is in
 * it, and done waits until each PV still holds it. */
double  ELast;
assign  ELast to "{P}ml_monoE";
double  ENow;
assign  ENow to "{P}ml_monoE";
double  lambdaLast;
assign  lambdaLast to "{P}ml_monoLambda";
double  lambdaNow;
assign  lambdaNow to "{P}ml_monoLambda";
double  thetaLast;
assign  thetaLast to "{P}ml_monoTheta";
double  thetaNow;
assign  thetaNow to "{P}ml_monoTheta";

/* The following are "soft" PV's that contain motor information that is
 * either echoed to or echoed from the actual motor records. The reason
 * for these PV's is so the medm display can be independent from the
 * motor PV names
 */

string  thetaMotName;
assign  thetaMotName to "{P}ml_monoThetaPv"; 

string  theta2MotName;
assign  theta2MotName to "{P}ml_monoTheta2Pv"; 

string  zMotName;
assign  zMotName to "{P}ml_monoZPv"; 

string  yMotName;
assign  yMotName to "{P}ml_monoYPv"; 

double  thetaMotCmdEcho;
assign  thetaMotCmdEcho to "{P}ml_monoThetaCmd";

double  theta2MotCmdEcho;
assign  theta2MotCmdEcho to "{P}ml_monoTheta2Cmd";

double  zMotCmdEcho;
assign  zMotCmdEcho to "{P}ml_monoZCmd";

double  thetaMotRdbkEcho;
assign  thetaMotRdbkEcho to "{P}ml_monoThetaRdbkEcho";

double  theta2MotRdbkEcho;
assign  theta2MotRdbkEcho to "{P}ml_monoTheta2RdbkEcho";

double  zMotRdbkEcho;
assign  zMotRdbkEcho to "{P}ml_monoZRdbk";

double  thetaMotVelEcho;
assign  thetaMotVelEcho to "{P}ml_monoThetaVel";

double  theta2MotVelEcho;
assign  theta2MotVelEcho to "{P}ml_monoTheta2Vel";

double  zMotVelEcho;
assign  zMotVelEcho to "{P}ml_monoZVel";

short   thetaDmovEcho;
assign  thetaDmovEcho to "{P}ml_monoThetaDmov";

short   theta2DmovEcho;
assign  theta2DmovEcho to "{P}ml_monoTheta2Dmov";

short   zDmovEcho;
assign  zDmovEcho to "{P}ml_monoZDmov"; 

/* Confusion may result between the theta motor variables and the
 * user-entered theta. To eliminate confusion, variables associated
 * with the theta motor are named thetaMotXXXXX  unless it is clear they
 * refer to a motor (e.g. thSpeed, etc)
 */

short   thetaMotStop;
assign  thetaMotStop to "{P}{M_THETA}.STOP";

short   theta2MotStop;
assign  theta2MotStop to "{P}{M_THETA2}.STOP";

short   zStop;
assign  zStop to "{P}{M_Z}.STOP";

short   thetaDmov;
assign  thetaDmov to "{P}{M_THETA}.DMOV";
monitor thetaDmov;
evflag  thetaDmov_mon;
sync    thetaDmov thetaDmov_mon;

short   theta2Dmov;
assign  theta2Dmov to "{P}{M_THETA2}.DMOV";
monitor theta2Dmov;
evflag  theta2Dmov_mon;
sync    theta2Dmov theta2Dmov_mon;

short   zDmov;
assign  zDmov to "{P}{M_Z}.DMOV";
monitor zDmov;
evflag  zDmov_mon;
sync    zDmov zDmov_mon;

/* LIMIT SWITCHES */
short   thetaHls;
assign  thetaHls to "{P}{M_THETA}.HLS";
monitor thetaHls;

short   thetaLls;
assign  thetaLls to "{P}{M_THETA}.LLS";
monitor thetaLls;

short   theta2Hls;
assign  theta2Hls to "{P}{M_THETA2}.HLS";
monitor theta2Hls;

short   theta2Lls;
assign  theta2Lls to "{P}{M_THETA2}.LLS";
monitor theta2Lls;

short   zHls;
assign  zHls to "{P}{M_Z}.HLS";
monitor zHls;

short   zLls;
assign  zLls to "{P}{M_Z}.LLS";
monitor zLls;

/* C defect: monitored the values calcMovements puts here, so the move
 * took what the PV held by then: the late echo of an earlier pass's put,
 * or a user's edit, driven at the speeds computed for the calculated
 * value. The move goes where calcMovements put; an edit before it is
 * dropped. */
double  thetaMotDesired;
assign  thetaMotDesired to "{P}ml_monoThetaSet";

double  theta2MotDesired;
assign  theta2MotDesired to "{P}ml_monoTheta2Set";

double  zMotDesired;
assign  zMotDesired to "{P}ml_monoZSet";

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

double  theta2MotHiLim;
assign  theta2MotHiLim to "{P}{M_THETA2}.HLM";
monitor theta2MotHiLim;
evflag  theta2MotHiLim_mon;
sync    theta2MotHiLim theta2MotHiLim_mon;

double  theta2MotLoLim;
assign  theta2MotLoLim to "{P}{M_THETA2}.LLM";
monitor theta2MotLoLim;
evflag  theta2MotLoLim_mon;
sync    theta2MotLoLim theta2MotLoLim_mon;

double  zMotHiLim;
assign  zMotHiLim to "{P}{M_Z}.HLM";
monitor zMotHiLim;

double  zMotLoLim;
assign  zMotLoLim to "{P}{M_Z}.LLM";
monitor zMotLoLim;

double  thetaMotCmd;
assign  thetaMotCmd to "{P}{M_THETA}";

double  theta2MotCmd;
assign  theta2MotCmd to "{P}{M_THETA2}";

double  zMotCmd;
assign  zMotCmd to "{P}{M_Z}";

double  thetaMotCmdMon;
assign  thetaMotCmdMon to "{P}{M_THETA}";
monitor thetaMotCmdMon;
evflag  thetaMotCmdMon_mon;
sync    thetaMotCmdMon thetaMotCmdMon_mon;

double  theta2MotCmdMon;
assign  theta2MotCmdMon to "{P}{M_THETA2}";
monitor theta2MotCmdMon;
evflag  theta2MotCmdMon_mon;
sync    theta2MotCmdMon theta2MotCmdMon_mon;

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

/* double  th2Time; assume same as thTime */
double  oldTh2Speed;
assign  oldTh2Speed to "{P}{M_THETA2}.VELO";
double  newTh2Speed;
double  th2Speed;
assign  th2Speed to "{P}{M_THETA2}.VELO";
monitor th2Speed;
evflag  theta2MotVel_mon;
sync    th2Speed theta2MotVel_mon;

double  zTime;
double  oldZSpeed;
assign  oldZSpeed to "{P}{M_Z}.VELO";
double  newZSpeed;
double  zSpeed;
assign  zSpeed to "{P}{M_Z}.VELO";
monitor zSpeed;
evflag  zMotVel_mon;
sync    zSpeed zMotVel_mon;

/* C defect: put thSpeed, th2Speed and zSpeed themselves, which are
 * monitored: the echo of the previous speed put could land before a put,
 * which then wrote that speed. The puts go through these. */
double  thSpeedCmd;
assign  thSpeedCmd to "{P}{M_THETA}.VELO";

double  th2SpeedCmd;
assign  th2SpeedCmd to "{P}{M_THETA2}.VELO";

double  zSpeedCmd;
assign  zSpeedCmd to "{P}{M_Z}.VELO";

double  thetaMotRdbk;
assign  thetaMotRdbk to "{P}{M_THETA}.RBV";
monitor thetaMotRdbk;
evflag  thetaMotRdbk_mon;
sync    thetaMotRdbk thetaMotRdbk_mon;
evflag  thetaMotRdbkPseudo_mon;

double  theta2MotRdbk;
assign  theta2MotRdbk to "{P}{M_THETA2}.RBV";
monitor theta2MotRdbk;
evflag  theta2MotRdbk_mon;
sync    theta2MotRdbk theta2MotRdbk_mon;
evflag  theta2MotRdbkPseudo_mon;

double  zMotRdbk;
assign  zMotRdbk to "{P}{M_Z}.RBV";
monitor zMotRdbk;
evflag  zMotRdbk_mon;
sync    zMotRdbk zMotRdbk_mon;

double  yMotRdbk;
assign  yMotRdbk to "{P}{M_Y}.RBV";
monitor yMotRdbk;
evflag  yMotRdbk_mon;
sync    yMotRdbk yMotRdbk_mon;

/* Set/Use support */
short   useSetMode;
assign  useSetMode to "{P}ml_monoUseSet";
monitor useSetMode;
evflag  useSetMode_mon;
sync    useSetMode useSetMode_mon;

short   thetaMotSet;
assign  thetaMotSet to "{P}{M_THETA}.SET";
monitor thetaMotSet;
evflag  thetaMotSet_mon;
sync    thetaMotSet thetaMotSet_mon;

short   theta2MotSet;
assign  theta2MotSet to "{P}{M_THETA2}.SET";
monitor theta2MotSet;
evflag  theta2MotSet_mon;
sync    theta2MotSet theta2MotSet_mon;

short   zMotSet;
assign  zMotSet to "{P}{M_Z}.SET";
monitor zMotSet;
evflag  zMotSet_mon;
sync    zMotSet zMotSet_mon;

/* C defect: put useSetMode and the motor .SET flags themselves, which
 * are monitored: the echo of the previous put could land before a put,
 * which then wrote that flag. The puts, and updateSet's gets, go
 * through these. */
short   useSetModeCmd;
assign  useSetModeCmd to "{P}ml_monoUseSet";

short   thetaMotSetCmd;
assign  thetaMotSetCmd to "{P}{M_THETA}.SET";

short   theta2MotSetCmd;
assign  theta2MotSetCmd to "{P}{M_THETA2}.SET";

short   zMotSetCmd;
assign  zMotSetCmd to "{P}{M_Z}.SET";

/* Each flag as updateSet last put or took it, and the one it mirrors. */
short   useSetModeLast;
short   thetaMotSetLast;
short   theta2MotSetLast;
short   zMotSetLast;
short   mirrorSet;

/* tweaking PV's */
double  ETweakVal;
assign  ETweakVal to "{P}ml_monoETweak";
monitor ETweakVal;

short   ETweakInc;
assign  ETweakInc to "{P}ml_monoEInc";
monitor ETweakInc;

short   ETweakDec;
assign  ETweakDec to "{P}ml_monoEDec";
monitor ETweakDec;

double  LTweakVal;
assign  LTweakVal to "{P}ml_monoLambdaTweak";
monitor LTweakVal;

short   LTweakInc;
assign  LTweakInc to "{P}ml_monoLambdaInc";
monitor LTweakInc;

short   LTweakDec;
assign  LTweakDec to "{P}ml_monoLambdaDec";
monitor LTweakDec;

double  thTweakVal;
assign  thTweakVal to "{P}ml_monoThetaTweak";
monitor thTweakVal;

short   thTweakInc;
assign  thTweakInc to "{P}ml_monoThetaInc";
monitor thTweakInc;

short   thTweakDec;
assign  thTweakDec to "{P}ml_monoThetaDec";
monitor thTweakDec;

/* yOffset (y dist from incident/exit beams to center of rotation) */
double  yOffset;
assign  yOffset to "{P}ml_mono_yOffset";
monitor yOffset;
evflag  yOffset_mon;
sync    yOffset yOffset_mon;

double  yOffsetHi;
assign  yOffsetHi to "{P}ml_mono_yOffset.DRVH";

double  yOffsetLo;
assign  yOffsetLo to "{P}ml_mono_yOffset.DRVL";

/* C defect: put yOffset itself, which is monitored and which both state
 * sets assign, so a yOffset event or the other state set's assignment
 * landing between an assignment and its put was put in its place. The
 * readback mirrors put yMotRdbkEcho; initSequence puts yOffsetInit. */
double  yMotRdbkEcho;
assign  yMotRdbkEcho to "{P}ml_mono_yOffset";

double  yOffsetInit;
assign  yOffsetInit to "{P}ml_mono_yOffset";

%%#include <string.h>
%%#include <math.h>
%%#include <stdlib.h>

%%double ml_monoCtl_calc2dSpacing();
%%char *ml_monoCtl_pmacP;
%%char *ml_monoCtl_pmacM;
%%char *ml_monoCtl_pmacGEOM;
%%char *ml_monoCtl_pmacY_OFF;
short geom;

short  causedMove;
short  initSeq;
short  initSeqDone;

double hc;
double radConv;
double temp;

ss ml_monoCtl {
state init {
	when () {
		initSeq = 0;
		hc = 12.3984244;
		radConv = 57.2958;

	} state initSequence
}

state initSequence {
	when (initSeq == 0) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:initSequence:initSeq=0\n");
		thetaMotStop = 1;
		theta2MotStop = 1;
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
		ml_monoCtl_pmacP = seq_macValueGet(ssId,"P");
		ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_THETA");
		if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) {
			sprintf(thetaMotName,"None");
		} else {
			snprintf(thetaMotName, sizeof(thetaMotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM);
		}

		ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_THETA2");
		if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) {
			sprintf(theta2MotName,"None");
		} else {
			snprintf(theta2MotName, sizeof(theta2MotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM);
		}

		ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_Z");
		if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) {
			sprintf(zMotName,"None");
		} else {
			snprintf(zMotName, sizeof(zMotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM);
		}

		ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_Y");
		if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) {
			sprintf(yMotName,"None");
		} else {
			snprintf(yMotName, sizeof(yMotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM);
		}

		/* We're going to use the y motor readback for yOffset */
		ml_monoCtl_pmacY_OFF = seq_macValueGet(ssId,"Y_OFF");
		yOffsetInit = atof(ml_monoCtl_pmacY_OFF);
		if ((yOffsetInit < 1) ||  (yOffsetInit > 60.))
			yOffsetInit = 35.;

		ml_monoCtl_pmacGEOM = seq_macValueGet(ssId,"GEOM");
		geom = 1;
		/* if (*ml_monoCtl_pmacGEOM == '2') geom = 2; */
}%

		yOffset = yOffsetInit;
		pvPut(yOffsetInit);
		if (geom == 1) {
		}
		pvPut(thetaMotName);
		pvPut(theta2MotName);
		pvPut(zMotName);
		pvPut(yMotName);
		/* C's monitored E, lambda and theta started at the PVs' values. */
		pvGet(ELast);
		pvGet(lambdaLast);
		pvGet(thetaLast);
		theta = thetaLast;
		initSeq = 1;
	} state dInputChanged

	when (initSeq == 1) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:initSequence:initSeq=1\n");
		initSeq = 2;
	} state chkMotorLimits

	when (initSeq == 2) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:initSequence:initSeq=2\n");
		initSeq = 3;
	} state updateRdbk

	when ((initSeq == 3) && delay(1.)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:initSequence:initSeq=3\n");
		sprintf(seqMsg1, "ml_mono Control Ready");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		initSeqDone=1;
		theta = thetaMotRdbk;
		pvPut(theta);
		pvGet(thetaLast);
	} state thChanged
}

state waitForCmndEnter {
	entry {
		/* Between passes a working value is what its PV held after this
		 * program's last put or take, clamp included, as C's monitored
		 * one was. */
		E = ELast;
		lambda = lambdaLast;
		theta = thetaLast;
	}
	when (initSeqDone && delay(.1)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmndEnter:initSeqDone\n");
		thetaMotRdbkEcho = thetaMotRdbk;
		pvPut(thetaMotRdbkEcho);
		theta2MotRdbkEcho = theta2MotRdbk;
		pvPut(theta2MotRdbkEcho);
		zMotRdbkEcho = zMotRdbk;
		pvPut(zMotRdbkEcho);
		yOffset = yMotRdbk;
		yMotRdbkEcho = yMotRdbk;
		pvPut(yMotRdbkEcho);
		thetaMotVelEcho = thSpeed;
		pvPut(thetaMotVelEcho);
		theta2MotVelEcho = th2Speed;
		pvPut(theta2MotVelEcho);
		zMotVelEcho = zSpeed;
		pvPut(zMotVelEcho);
		thetaMotCmdEcho = thetaMotCmdMon;
		pvPut(thetaMotCmdEcho);
		theta2MotCmdEcho = theta2MotCmdMon;
		pvPut(theta2MotCmdEcho);
		zMotCmdEcho = zMotCmdMon;
		pvPut(zMotCmdEcho);
		thetaDmovEcho = thetaDmov;
		pvPut(thetaDmovEcho);
		theta2DmovEcho = theta2Dmov;
		pvPut(theta2DmovEcho);
		zDmovEcho = zDmov;
		pvPut(zDmovEcho);
		if (autoMode || useSetMode) {
			thetaRdbk = thetaMotRdbk;
			pvPut(thetaRdbk);
			lambdaRdbk = TwoD*sin(thetaRdbk/radConv);
			pvPut(lambdaRdbk);
			ERdbk = hc/lambdaRdbk;
			pvPut(ERdbk);
		}
		opAck = 0;
		pvPut(opAck);
		if (ml_monoCtlDebug) {
			%%printf("ml_monoCtl:waitForCmndEnter: ml_monoMoving=%d; ml_monoDone=%d\n",
			%%	ml_monoMoving, ml_monoDone);
		}
		/* A request still in a PV is not done. */
		pvGet(ENow);
		pvGet(lambdaNow);
		pvGet(thetaNow);
		if (ml_monoMoving && ml_monoDone && SAME(ENow, ELast) &&
		    SAME(lambdaNow, lambdaLast) && SAME(thetaNow, thetaLast)) {
			if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmndEnter: asserting 'done'.\n");
			ml_monoMoving=0; ml_monoDone = 0;
			pvPut(ml_monoMoving);
		}
	} state waitForCmnd  

	when (!initSeqDone) {
	} state initSequence
}

state waitForCmnd {
	when (efTest(Order_mon) || efTest(D_mon)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:Order/D_mon\n");
		sprintf(seqMsg1, "Layer Spacing Changed");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
	} state dInputChanged

	when (efTest(thetaMotHiLim_mon) || efTest(thetaMotLoLim_mon)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:thetaMotXxLim_mon\n");
		sprintf(seqMsg1, "Theta Motor Soft Limits Changed");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		efClear(thetaMotHiLim_mon);
		efClear(thetaMotLoLim_mon);
	} state thetaLimits

	when (efTest(theta2MotHiLim_mon) || efTest(theta2MotLoLim_mon)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:theta2MotXxLim_mon\n");
		sprintf(seqMsg1, "Theta2 Motor Soft Limits Changed");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		efClear(theta2MotHiLim_mon);
		efClear(theta2MotLoLim_mon);
	} state thetaLimits

	when (efTestAndClear(thetaMotRdbk_mon)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:thetaMotRdbk_mon\n");
		thetaMotRdbkEcho = thetaMotRdbk;
		pvPut(thetaMotRdbkEcho);
		/*efSet(thetaMotRdbkPseudo_mon);*/
	} state updateRdbk

	when (efTestAndClear(theta2MotRdbk_mon)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:theta2MotRdbk_mon\n");
		theta2MotRdbkEcho = theta2MotRdbk;
		pvPut(theta2MotRdbkEcho);
		/*efSet(theta2MotRdbkPseudo_mon);*/
	} state updateRdbk

	/* An event holding *Last is this program's own echo, which the
	 * failing value test consumes. */
	when (efTestAndClear(E_mon) && !SAME(EReq, ELast)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:E_mon\n");
	} state reqTake

	when (efTestAndClear(lambda_mon) && !SAME(lambdaReq, lambdaLast)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:lambda_mon\n");
	} state reqTake

	when (efTestAndClear(theta_mon) && !SAME(thetaReq, thetaLast)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:theta_mon\n");
	} state reqTake

	when (putVals && delay(.1)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:putVals\n");
	} state move_ml_mono

	when (ETweakInc||ETweakDec||LTweakInc||LTweakDec||thTweakInc||thTweakDec) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:Tweak\n");
	} state tweak

	when (efTestAndClear(yOffset_mon) && initSeqDone) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:yOffset_mon\n");
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
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:waitForCmnd:opAck\n");
		opAlert = 0;
		pvPut(opAlert);
		sprintf(seqMsg1, " ");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
	} state waitForCmndEnter
}

/* A request is taken from its PV: an event can hold a value the PV no
 * longer holds, as an own put's echo landing after a later put does. */
state reqTake {
	entry {
		pvGet(ENow);
		pvGet(lambdaNow);
		pvGet(thetaNow);
	}
	when (!SAME(ENow, ELast)) {
		E = ENow;
		ELast = E;
		ml_monoDone = 0;
	} state eChanged
	when (!SAME(lambdaNow, lambdaLast)) {
		lambda = lambdaNow;
		lambdaLast = lambda;
		ml_monoDone = 0;
	} state lChanged
	when (!SAME(thetaNow, thetaLast)) {
		theta = thetaNow;
		thetaLast = theta;
		ml_monoDone = 0;
	} state thChanged
	when () {
	} state waitForCmnd
}

state tweak {
	when (ETweakInc || ETweakDec) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:tweak:E+\n");
		temp = E + ETweakVal * (ETweakInc?1:-1);
		if ((temp>=ELo) && (temp<=EHi)) {
			E = temp;
			pvPut(E);
			pvGet(ELast);
		} else {
			sprintf(seqMsg1, "E tweak exceeds limit");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
		}
		if (ETweakInc) {
			ETweakInc = 0;
			pvPut(ETweakInc);
		} else {
			ETweakDec = 0;
			pvPut(ETweakDec);
		}
	} state eChanged

	when (LTweakInc || LTweakDec) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:tweak:L\n");
		temp = lambda + LTweakVal * (LTweakInc?1:-1);
		if ((temp>=lambdaLo) && (temp<=lambdaHi)) {
			lambda = temp;
			pvPut(lambda);
			pvGet(lambdaLast);
		} else {
			sprintf(seqMsg1, "Lambda tweak exceeds limit");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
		}
		if (LTweakInc) {
			LTweakInc = 0;
			pvPut(LTweakInc);
		} else {
			LTweakDec = 0;
			pvPut(LTweakDec);
		}
	} state lChanged

	when (thTweakInc || thTweakDec) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:tweak:th\n");
		temp = theta + thTweakVal * (thTweakInc?1:-1);
		if ((temp>=thetaLo) && (temp<=thetaHi)) {
			theta = temp;
			pvPut(theta);
			pvGet(thetaLast);
		} else {
			sprintf(seqMsg1, "Theta tweak exceeds limit");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
		}
		if (thTweakInc) {
			thTweakInc = 0;
			pvPut(thTweakInc);
		} else {
			thTweakDec = 0;
			pvPut(thTweakDec);
		}
	} state thChanged
}

state dInputChanged {
	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:dInputChanged\n");
		/* Cleared where Order and D are read, so a change landing after
		 * this runs the pass again. */
		efClear(Order_mon);
		efClear(D_mon);
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Set to Manual Mode");
		pvPut(seqMsg2);
		TwoD = ml_monoCtl_calc2dSpacing();
		pvPut(seqMsg1);
		/* A drive limit put clamps the AO's VAL. *Last takes that clamp
		 * only when no request is in the PV, so a request stays one. */
		pvGet(lambdaNow);
		pvGet(ENow);
		lambdaHi = TwoD*sin(thetaHi/radConv);
		pvPut(lambdaHi);
		lambdaLo = TwoD*sin(thetaLo/radConv);
		pvPut(lambdaLo);
		EHi = hc/lambdaLo;
		pvPut(EHi);
		ELo = hc/lambdaHi;
		pvPut(ELo);
		if (SAME(lambdaNow, lambdaLast)) pvGet(lambdaLast);
		if (SAME(ENow, ELast)) pvGet(ELast);
		pvPut(opAlert);
	} state dInputTake
}

/* C defect: thChanged recomputed E and lambda from theta over a request
 * already in E or lambda, which was then never moved to. A request
 * pending at a layer spacing change is applied under the new 2d. */
state dInputTake {
	entry {
		pvGet(ENow);
		pvGet(lambdaNow);
		pvGet(thetaNow);
	}
	when (!SAME(ENow, ELast)) {
		E = ENow;
		ELast = E;
		ml_monoDone = 0;
	} state eChanged
	when (!SAME(lambdaNow, lambdaLast)) {
		lambda = lambdaNow;
		lambdaLast = lambda;
		ml_monoDone = 0;
	} state lChanged
	when (!SAME(thetaNow, thetaLast)) {
		theta = thetaNow;
		thetaLast = theta;
		ml_monoDone = 0;
	} state thChanged
	when () {
	} state thChanged
}

state thetaLimits {
	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:thetaLimits\n");
		thetaHi = thetaMotHiLim;
		thetaLo = thetaMotLoLim;
		if (thetaHi > 89.0) {
			thetaHi = 89.0;
			sprintf(seqMsg2, "ThetaHiLim > 89 deg. (Using 89 deg.)");
			pvPut(seqMsg2);
		}
		if (thetaLo < 0.1) {
			thetaLo = 0.1;
			sprintf(seqMsg2, "ThetaLoLim < 0.1 deg. (Using 0.1 deg.)");
			pvPut(seqMsg2);
		}
		/* Read back as in dInputChanged. */
		pvGet(thetaNow);
		pvGet(lambdaNow);
		pvGet(ENow);
		pvPut(thetaHi);
		pvPut(thetaLo);
		lambdaHi = TwoD*sin(thetaHi/radConv);
		pvPut(lambdaHi);
		lambdaLo = TwoD*sin(thetaLo/radConv);
		pvPut(lambdaLo);
		EHi = hc/lambdaLo;
		pvPut(EHi);
		ELo = hc/lambdaHi;
		pvPut(ELo);
		if (SAME(thetaNow, thetaLast)) pvGet(thetaLast);
		if (SAME(lambdaNow, lambdaLast)) pvGet(lambdaLast);
		if (SAME(ENow, ELast)) pvGet(ELast);
	} state checkDone
}

state eChanged {
	when () { 
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:eChanged\n");
		lambda = hc/E;
		pvPut(lambda);
		pvGet(lambdaLast);
	} state lChanged
}

state lChanged {
	when (lambda > TwoD) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:lChanged: lambda>2*D\n");
		/* Don't change Theta, notify operator */
		sprintf(seqMsg1, "Wavelength > 2d spacing.");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
	} state thChanged 

	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:lChanged\n");
		theta = radConv * asin(lambda/TwoD);
		pvPut(theta);
		pvGet(thetaLast);
	} state thChanged
}

state thChanged {
	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:thChanged\n");
		if ((theta <= thetaLo) || (theta >= thetaHi)) {
			sprintf(seqMsg1, "Theta constrained to LIMIT");
			pvPut(seqMsg1);
			opAlert = 1;
			pvPut(opAlert);
			autoMode = 0;
			pvPut(autoMode);
			sprintf(seqMsg2, "Set to Manual Mode");
			pvPut(seqMsg2);
		}
		lambda = TwoD*sin(theta/radConv);
		pvPut(lambda);
		pvGet(lambdaLast);
		E = hc/lambda;
		pvPut(E);            
		pvGet(ELast);

		/* Update the readback values as well */
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = TwoD * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);
	} state calcMovements
}

state calcMovements {
	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:calcMovements\n");

		/* Calculate new Z, check against limits */
		thetaMotDesired = theta;
		theta2MotDesired = theta;
		if (geom == 1) {
			zMotDesired = yOffset/tan(2*theta/radConv);
		} else {
			printf("ml_mono:calcMovements: unimplemented geom=%d\n", geom);
		}
		if (ccMode != MODE_FREEZE_Z &&
		   ((zMotDesired < zMotLoLim)||(zMotDesired > zMotHiLim))) {
			sprintf(seqMsg1, "Z will exceed soft limits");
			pvPut(seqMsg1);
			sprintf(seqMsg2, "Setting to Manual Mode");
			pvPut(seqMsg2);
			opAlert = 1;
			pvPut(opAlert);
			autoMode = 0;
			pvPut(autoMode);
		}
		pvPut(thetaMotDesired);
		pvPut(theta2MotDesired);
		pvPut(zMotDesired);

		/***** Calculate required speeds ******/
		/* C defect: took the old speeds from thSpeed, th2Speed and zSpeed,
		 * whose monitors can still hold the previous move's speed after its
		 * restore, which this move's restore then put back. They are read
		 * from VELO, and the monitored ones only feed the VelEcho mirrors. */
		pvGet(oldThSpeed);
		pvGet(oldTh2Speed);
		pvGet(oldZSpeed);
		/* C defect: divided by each motor's speed, which speed control
		 * itself sets to 0 for a motor with no travel. A zero speed gave
		 * an inf time, which made every speed 0, or 0/0, a NaN, which
		 * failed the comparison below and sent theta's speed through a
		 * division by it. A motor with no speed takes zero time. */
		zTime = 0.0;
		if (oldZSpeed > 0) zTime = fabs(zMotDesired-zMotRdbk)/oldZSpeed;
		thTime = 0.0;
		if (oldThSpeed > 0) thTime = fabs(theta-thetaMotRdbk)/oldThSpeed;
		/* Frozen motors take zero time to move. (tmm) */
		if (ccMode == MODE_FREEZE_Z) {
			/* channel-cut mode: Z frozen */
			zTime = 0.0;
		}
		/*** Test for longest time ***/
		if (thTime >= zTime) {
			newThSpeed = oldThSpeed;
			newTh2Speed = oldTh2Speed;
			/* C defect: divided by thTime when it was 0, putting 0/0, a NaN,
			 * into Z's VELO when neither motor had to move. */
			if (thTime > 0) {
				newZSpeed = fabs(zMotDesired-zMotRdbk)/thTime;
			} else {
				newZSpeed = oldZSpeed;
			}
		} else {
			newThSpeed = fabs(theta-thetaMotRdbk)/zTime;
			newTh2Speed = newThSpeed;
			newZSpeed = oldZSpeed;
		}

	} state move_ml_mono
}

state move_ml_mono {
	when (autoMode || putVals || useSetMode) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:move_ml_mono:auto|put\n");
		thSpeedCmd = newThSpeed;
		th2SpeedCmd = newThSpeed;
		zSpeedCmd = newZSpeed;
		pvPut(thSpeedCmd);
		pvPut(th2SpeedCmd);
		if (ccMode == MODE_NORMAL) {
			pvPut(zSpeedCmd);
		}
		thetaMotCmd = thetaMotDesired;
		theta2MotCmd = theta2MotDesired;
		zMotCmd = zMotDesired;
		pvPut(thetaMotCmd);
		pvPut(theta2MotCmd);
		if (ccMode == MODE_NORMAL) {
			pvPut(zMotCmd);
		}
		putVals = 0;
		pvPut(putVals);
		causedMove = 1;
	} state updateRdbk

	when (1) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:move_ml_mono\n");
	} state waitForCmndEnter
}

state checkDone {
	when (thetaDmov && theta2Dmov && zDmov) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:checkDone: done\n");
		ml_monoDone = 1;
	} state waitForCmndEnter

	when (!thetaDmov || !theta2Dmov ||!zDmov) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:checkDone: not done\n");
	} state updateRdbk

}

state updateRdbk {
	when (thetaMotRdbk == 0 || theta2MotRdbk == 0) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:updateRdbk:thetaMotRdbk==0\n");
		sprintf(seqMsg1, "Theta Motor Readback is 0 !");
		pvPut(seqMsg1);
		sprintf(seqMsg2, " ");
		pvPut(seqMsg2);
		opAlert = 1;
		pvPut(opAlert);
	} state waitForCmndEnter

	when (thetaDmov && theta2Dmov && zDmov) {
		if (ml_monoCtlDebug) {
			%%printf("ml_monoCtl:updateRdbk:*Dmov (done=%d,moving=%d)\n",
			%%	ml_monoDone, ml_monoMoving);
		}
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = TwoD * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);
		ml_monoDone = 1;
		efSet(thetaMotRdbkPseudo_mon);
		efClear(thetaMotRdbk_mon);
		efSet(theta2MotRdbkPseudo_mon);
		efClear(theta2MotRdbk_mon);
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:updateRdbk: done\n");
	} state thetaMotStopped


	when (thetaHls || thetaLls || theta2Hls || theta2Lls) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:updateRdbk: Th limit switch\n");
		sprintf(seqMsg1, "Theta Motor hit a limit switch!");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Setting to Manual Mode");
		pvPut(seqMsg2);
	} state stop_ml_mono

	/* Ignore Z limit switches in "channel-cut" mode */
	when ((ccMode != MODE_FREEZE_Z) && (zHls || zLls)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:updateRdbk: Z limit switch\n");
		sprintf(seqMsg1, "Z Motor hit a limit switch!");
		pvPut(seqMsg1);
		opAlert = 1;
		pvPut(opAlert);
		autoMode = 0;
		pvPut(autoMode);
		sprintf(seqMsg2, "Setting to Manual Mode");
		pvPut(seqMsg2);
	} state stop_ml_mono

	when ((!thetaDmov || !theta2Dmov ||!zDmov) && delay(.1)) {
		if (ml_monoCtlDebug>1) %%printf("ml_monoCtl:updateRdbk:moving\n");
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = TwoD * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);

		thetaMotRdbkEcho = thetaMotRdbk;
		pvPut(thetaMotRdbkEcho);
		theta2MotRdbkEcho = theta2MotRdbk;
		pvPut(theta2MotRdbkEcho);

	} state updateRdbk
}

state thetaMotStopped {
	when (causedMove) {
		if (ml_monoCtlDebug) {
			%%printf("ml_monoCtl:thetaMotStopped:causedMove (done=%d,moving=%d)\n",
			%%	ml_monoDone, ml_monoMoving);
		}
		thSpeedCmd = oldThSpeed;
		th2SpeedCmd = oldTh2Speed;
		zSpeedCmd = oldZSpeed;
		pvPut(thSpeedCmd);
		pvPut(th2SpeedCmd);
		pvPut(zSpeedCmd);
		causedMove = 0;
	} state waitForCmndEnter

	/* Someone moved the Theta motor directly */
	when (!causedMove) {
		if (ml_monoCtlDebug) {
			%%printf("ml_monoCtl:thetaMotStopped:!causedMove (done=%d,moving=%d)\n",
			%%	ml_monoDone, ml_monoMoving);
		}
		if (initSeqDone) {
			opAlert = 1;
			pvPut(opAlert);
			sprintf(seqMsg1, "Only Theta Motor Moved");
			pvPut(seqMsg1);
			sprintf(seqMsg2, "Hit MOVE to Adjust Z");
			pvPut(seqMsg2);
			autoMode = 0;
			pvPut(autoMode);
		}
		theta = thetaMotRdbk;
		pvPut(theta);
		pvGet(thetaLast);
		lambda = TwoD*sin(theta/radConv);
		pvPut(lambda);
		pvGet(lambdaLast);
		E = hc/lambda;
		pvPut(E);  
		pvGet(ELast);
	} state thChanged
}

state chkMotorLimits {
	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:chkMotorLimits\n");
		opAlert = 0;
		thetaRdbk = thetaMotRdbk;
		lambdaRdbk = TwoD * sin(thetaRdbk/radConv);
		ERdbk = hc/lambdaRdbk;
		pvPut(thetaRdbk);
		pvPut(lambdaRdbk);
		pvPut(ERdbk);

		if (ccMode != MODE_FREEZE_Z) {
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
		if (yMotRdbk == 0) {
			sprintf(seqMsg1, "Y Readback of 0 not reasonable");
			pvPut(seqMsg1);
			opAlert = 1;
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

		if (theta2MotRdbk == 0) {
			sprintf(seqMsg1, "Theta2 Readback of 0 not reasonable");
			pvPut(seqMsg1);
			opAlert = 1;
		}
		if ((theta2MotRdbk<theta2MotLoLim)||(theta2MotRdbk>theta2MotHiLim)) {
			sprintf(seqMsg1, "Theta2 Readback outside Motor Limits");
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
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:chkMotorLimitsExit: OK\n");
		pvPut(opAlert);
		sprintf(seqMsg1, "All motors within limits");
		pvPut(seqMsg1);
	} state waitForCmndEnter

	when (opAlert && delay(2.)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:chkMotorLimitsExit: opAlert\n");
		pvPut(opAlert);
		sprintf(seqMsg2, "Modify Motor Positions/Limits");
		pvPut(seqMsg2);          
	} state chkMotorLimits
}

state stop_ml_mono {
	when () {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:stop_ml_mono\n");
		pvPut(thetaMotStop);
		pvPut(theta2MotStop);
		pvPut(zStop);
	} state stopped_ml_monoWait
}

state stopped_ml_monoWait {
	when(delay(1.)) {
		if (ml_monoCtlDebug) %%printf("ml_monoCtl:stopped_ml_monoWait\n");
	} state updateRdbk
}

} /* end of state set ml_monoCtl  */


/**************************************************************************
 *
 * This State Set keeps the motor independent fields updated 
 *
 **************************************************************************/
ss updatePsuedo {
state init {
	when () {
	} state update
}

state update {
	when (efTestAndClear(thetaMotRdbkPseudo_mon)) {
		thetaMotRdbkEcho = thetaMotRdbk;
		pvPut(thetaMotRdbkEcho);
	} state update

	when (efTestAndClear(theta2MotRdbkPseudo_mon)) {
		theta2MotRdbkEcho = theta2MotRdbk;
		pvPut(theta2MotRdbkEcho);
	} state update

	when (efTestAndClear(zMotRdbk_mon)) {
		zMotRdbkEcho = zMotRdbk;
		pvPut(zMotRdbkEcho);
	} state update

	when (efTestAndClear(yMotRdbk_mon)) {
		yOffset = yMotRdbk;
		yMotRdbkEcho = yMotRdbk;
		pvPut(yMotRdbkEcho);
	} state update

	when (efTestAndClear(thetaMotVel_mon)) {
		thetaMotVelEcho = thSpeed;
		pvPut(thetaMotVelEcho);
	} state update

	when (efTestAndClear(theta2MotVel_mon)) {
		theta2MotVelEcho = th2Speed;
		pvPut(theta2MotVelEcho);
	} state update

	when (efTestAndClear(zMotVel_mon)) {
		zMotVelEcho = zSpeed;
		pvPut(zMotVelEcho);
	} state update

	when (efTestAndClear(thetaMotCmdMon_mon)) {
		thetaMotCmdEcho = thetaMotCmdMon;
		pvPut(thetaMotCmdEcho);
	} state update

	when (efTestAndClear(theta2MotCmdMon_mon)) {
		theta2MotCmdEcho = theta2MotCmdMon;
		pvPut(theta2MotCmdEcho);
	} state update

	when (efTestAndClear(zMotCmdMon_mon)) {
		zMotCmdEcho = zMotCmdMon;
		pvPut(zMotCmdEcho);
	} state update

	when (efTestAndClear(thetaDmov_mon)) {
		thetaDmovEcho = thetaDmov;
		pvPut(thetaDmovEcho);
	} state update

	when (efTestAndClear(theta2Dmov_mon)) {
		theta2DmovEcho = theta2Dmov;
		pvPut(theta2DmovEcho);
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
		useSetModeLast = useSetMode;
		thetaMotSetLast = thetaMotSet;
		theta2MotSetLast = theta2MotSet;
		zMotSetLast = zMotSet;
	} state update
}

/* C defect: compared a flag's monitored value with the others', which
 * can hold the late echo of an earlier mirror put, and mirrored that
 * echo back as a new setting. An arm takes its flag only while the PV
 * still holds it, and mirror compares with the values updateSet last
 * put or took, never with a monitored one. */
state update {
	when (efTestAndClear(useSetMode_mon)) {
		pvGet(useSetModeCmd);
		if (useSetModeCmd == useSetMode) useSetModeLast = useSetMode;
		mirrorSet = useSetModeLast;
	} state mirror

	when (efTestAndClear(thetaMotSet_mon)) {
		pvGet(thetaMotSetCmd);
		if (thetaMotSetCmd == thetaMotSet) thetaMotSetLast = thetaMotSet;
		mirrorSet = thetaMotSetLast;
	} state mirror

	when (efTestAndClear(theta2MotSet_mon)) {
		pvGet(theta2MotSetCmd);
		if (theta2MotSetCmd == theta2MotSet) theta2MotSetLast = theta2MotSet;
		mirrorSet = theta2MotSetLast;
	} state mirror

	when (efTestAndClear(zMotSet_mon)) {
		pvGet(zMotSetCmd);
		if (zMotSetCmd == zMotSet) zMotSetLast = zMotSet;
		mirrorSet = zMotSetLast;
	} state mirror
}

state mirror {
	when () {
		if (useSetModeLast != mirrorSet) {
			useSetModeLast = mirrorSet;
			useSetModeCmd = mirrorSet;
			pvPut(useSetModeCmd);
		}
		if (thetaMotSetLast != mirrorSet) {
			thetaMotSetLast = mirrorSet;
			thetaMotSetCmd = mirrorSet;
			pvPut(thetaMotSetCmd);
		}
		if (theta2MotSetLast != mirrorSet) {
			theta2MotSetLast = mirrorSet;
			theta2MotSetCmd = mirrorSet;
			pvPut(theta2MotSetCmd);
		}
		if (zMotSetLast != mirrorSet) {
			zMotSetLast = mirrorSet;
			zMotSetCmd = mirrorSet;
			pvPut(zMotSetCmd);
		}
	} state update
}
}

/***************************************************************************/
/***************************************************************************/
/*   C subroutines */

%{
#define NINT(f) (int)((f)>0 ? (f)+0.5 : (f)-0.5)

double ml_monoCtl_calc2dSpacing()
{

	double result;

	result = (2*D)/Order;

	opAlert = 0;
	if (Order < 1) opAlert = 1;
	if (opAlert) {
		/* C defect: sprintf wrote this unbounded into the 40-byte seqMsg1,
		 * past it for an Order of -1e21 and below. */
		snprintf(seqMsg1, sizeof(seqMsg1), "Order == %f?", Order);
	} else {
		sprintf(seqMsg1, "New effective d spacing");
	}
	return(result);
}


}%
