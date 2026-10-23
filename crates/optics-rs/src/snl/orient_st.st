/* set your tab width to 4 */

program orient  ("P=xxx:orient1:,PM=xxx:,mTTH=m9,mTH=m10,mCHI=m11,mPHI=m12")

/******************************************************************************

This code is part of the implementation of synApps' orientation-matrix support,
which allows a user to specify a geometrical plane (actually, a set of
parallel, equally spaced planes) in a crystalline sample, and arranges sample
and detector orientation so that incident x-rays of a specified wavelength will
diffract from those planes into the detector.  To do this, the software must
know the x-ray wavelength, the crystal structure, and the crystal's orientation
in space.

The wavelength (or energy) and crystal structure are given by the user via
EPICS PV's.  From this information the software constructs a matrix named "A0".

The crystal's orientation in space can be inferred from the angles at which it
diffracts x rays: two diffraction conditions, and the identities of the crystal
planes involved in those diffractions, normally are sufficient to determine the
crystal's orientation. The user is responsible for finding those diffraction
conditions (x-ray wavelength, and the crystal angles TTH, TH, PHI, CHI), and
naming the planes that were involved (H,K,L).  This is done by specifying two
sets of numbers:

	H1, K1, L1, TTH1, TH1, CHI1, PHI1 - "the primary reflection"

	H2, K2, L2, TTH2, TH2, CHI2, PHI2 - "the secondary reflection"

From the primary and secondary reflections, and using the matrix A0, the
software constructs the orientation matrix, "OMTX".  Using this matrix, a
vector (H, K, L) can be transformed to a family of angle vectors (TTH, TH, PHI,
CHI).  A mathematical constraint (e.g., TH = TTH/2) selects a single member of
this family for use.  It is also possible for the user directly to set matrix
elements of the A0 and/or OMTX matrices.

This code monitors PV's by which the user changes and uses the orientation
matrix; it arranges for the calculation of special-purpose vectors and matrices
as they are required, calling functions in orient.c to perform those
calculations; it writes to and monitors motor PV's that move sample and
detector, and detects the completion of those moves; it writes readback
information derived from those motor PV's.

This code also allows the motor PV's to be moved by an external agent, and
manually or automatically synchronizes (H, K, L) drive values to agree with the
new motor positions.

This code is invoked as follows:

seq &orient, "P=xxx:orient1:,PM=yyy:,mTTH=m9,mTH=m10,mCHI=m11,mPHI=m12"

 where

 P    - non-motor PV names start with this string
 PM   - motor PV names start with this string
 mTTH - name of PV to which TTH is written
 mTH  - name of PV to which TH is written
 mCHI - name of PV to which CHI is written
 mPHI - name of PV to which PHI is written
 
*******************************************************************************/

option +r;	/* allow more than one copy of the program */

/* Some help to simplify SNL's wordy syntax */

#define ASSIGN(TYPE, LOCAL_VAR, PV) \
        TYPE LOCAL_VAR;             \
        assign LOCAL_VAR to PV;

#define ASSIGN_MONITOR(TYPE, LOCAL_VAR, PV) \
        ASSIGN(TYPE, LOCAL_VAR, PV)         \
        monitor LOCAL_VAR;

#define ASSIGN_MONITOR_FLAG(TYPE, LOCAL_VAR, PV, LOCAL_FLAG) \
        ASSIGN_MONITOR(TYPE, LOCAL_VAR, PV)              \
        evflag LOCAL_FLAG;                               \
        sync   LOCAL_VAR LOCAL_FLAG;

/* As abovem but using already defined event flag */
#define ASSIGN_MONITOR_eFLAG(TYPE, LOCAL_VAR, PV, LOCAL_FLAG) \
        ASSIGN_MONITOR(TYPE, LOCAL_VAR, PV)              \
        sync   LOCAL_VAR LOCAL_FLAG;

#define MSG_SIZE 39
#define PVPUT(PV,VAL)  {(PV) = (VAL);  pvPut(PV);}
#define PVPUTSTR(MSG,TEXT)  {strcpy((MSG), (TEXT)); pvPut(MSG);}
#define PUTMSG(LINE1)  {strncpy(Msg, (LINE1), MSG_SIZE); pvPut(Msg); if (orientDebug) printf("orient: %s\n", Msg);}
#define PUTALERT(LINE1)  {strncpy(Msg, (LINE1), MSG_SIZE); pvPut(Msg); PVPUT(Alert, 1); printf("orient: %s\n", Msg);}

char tmpstr[100];
char *cp;

#define SMALL 1.0e-9
#define DIFFERENT(a,b) (fabs((a)-(b)) > SMALL)

/*
 * The maximum time we're willing to wait to wait for all elements of a vector write to
 * arrive.  (If user writes (H,K,L), we don't want to launch motors three times,
 * but would rather wait for all three elements to be written, and launch once.)  I don't
 * yet know how to determine an optimal time interval, so I'm going with something that
 * just feels right, and seems to work well on vxWorks.
 */
#define SYNC_WAIT_SECS 0.02

/* General Purpose PV's */

ASSIGN_MONITOR(short, orientDebug, "{P}Debug")
ASSIGN(string, Msg, "{P}Msg")
/*
 * C defect: ss stop's PUTMSG wrote Msg, the buffer ss orient writes from
 * its own thread, so a message from ss orient landing between ss stop's
 * strncpy and pvPut replaced "stopping motors".  ss stop writes stopMsg.
 */
ASSIGN(string, stopMsg, "{P}Msg")
ASSIGN(short, Alert, "{P}Alert")
ASSIGN_MONITOR(short, opAck, "{P}OperAck")

/* Mode is the constraint according to which HKL gets translated into TTH, TH, CHI, PHI. */
ASSIGN_MONITOR_FLAG(short, Mode, "{P}Mode", Mode_mon)

/* {P}motPut is the user's "Move" button.  If {P}motPut_Auto==0, user
 * can write whatever values he wants to whatever fields without anything
 * propagating to actual motors, and then set {P}motPut=1 to say "make it so".
 * If {P}motPut_Auto==1, this happens whenever the local angle values change.
 * Note that local angle values are themselves driven by (H,K,L) changes,
 * which, in turn, are driven by OMTX and A0 matrix changes, and A0 is driven
 * by changes in crystal parameters and energy/wavelength.  This means that
 * {P}motPut_Auto is the only gatekeeper between user input and motors.
 */
ASSIGN_MONITOR_FLAG(short, motPut, "{P}motPut", motPut_mon)
ASSIGN_MONITOR(short, motPut_Auto, "{P}motPut_Auto")
ASSIGN(short, load_motPut, "{P}load_motPut")

/* {P}motGet causes motor positions to be read from actual motors to local copies
 * [TTH, TH, CHI, PHI] (and from there automatically back transformd to [H, K, L]
 * values).  If {P}motGet_Auto==1, this will happen whenever the actual motor
 * drive values change.  Note that this switch applies only to drive values;
 * readback values are always back transformd, if the A0 and OMTX matrices exist.
 */
ASSIGN_MONITOR_FLAG(short, motGet, "{P}motGet", motGet_mon)
ASSIGN_MONITOR(short, motGet_Auto, "{P}motGet_Auto")

/* Motor PV's */
ASSIGN_MONITOR(string, motTTH_name, "{P}motTTH_name")
ASSIGN_MONITOR(string, motTH_name, "{P}motTH_name")
ASSIGN_MONITOR(string, motCHI_name, "{P}motCHI_name")
ASSIGN_MONITOR(string, motPHI_name, "{P}motPHI_name")

ASSIGN_MONITOR_FLAG(double, motTTH, "{PM}{mTTH}", motTTH_mon)
ASSIGN_MONITOR_FLAG(double, motTH, "{PM}{mTH}", motTH_mon)
ASSIGN_MONITOR_FLAG(double, motCHI, "{PM}{mCHI}", motCHI_mon)
ASSIGN_MONITOR_FLAG(double, motPHI, "{PM}{mPHI}", motPHI_mon)

ASSIGN_MONITOR_FLAG(double, motTTH_RBV, "", motRBV_mon)
ASSIGN_MONITOR_eFLAG(double, motTH_RBV, "", motRBV_mon)
ASSIGN_MONITOR_eFLAG(double, motCHI_RBV, "", motRBV_mon)
ASSIGN_MONITOR_eFLAG(double, motPHI_RBV, "", motRBV_mon)
double rbv_angles[4];

ASSIGN(int, motTTH_stop, "")
ASSIGN(int, motTH_stop, "")
ASSIGN(int, motCHI_stop, "")
ASSIGN(int, motPHI_stop, "")
ASSIGN_MONITOR(int, stopMotors, "{P}stopMotors")

ASSIGN(int, motTTH_Connected, "{P}motTTH_Connected")
ASSIGN(int, motTH_Connected, "{P}motTH_Connected")
ASSIGN(int, motCHI_Connected, "{P}motCHI_Connected")
ASSIGN(int, motPHI_Connected, "{P}motPHI_Connected")
int motorsConnected;
int motorStopsConnected;

/* User's command to calculate the orientaion matrix */
ASSIGN_MONITOR(short, OMTX_Calc, "{P}OMTX_Calc")

/* PV to implement completion detection.  The database sets this 
 * PV, we clear it when we've determined that everything's done.
 */
ASSIGN_MONITOR(short, Busy, "{P}Busy")

/* HKL */
ASSIGN_MONITOR(double, H, "{P}H")
ASSIGN_MONITOR_FLAG(double, H_event, "{P}H_event", H_mon)
ASSIGN_MONITOR(short, H_busy, "{P}H_busy")
ASSIGN(double, loadH, "{P}loadH")
ASSIGN(double, H_RBV, "{P}H_RBV")

ASSIGN_MONITOR(double, K, "{P}K")
ASSIGN_MONITOR_FLAG(double, K_event, "{P}K_event", K_mon)
ASSIGN_MONITOR(short, K_busy, "{P}K_busy")
ASSIGN(double, loadK, "{P}loadK")
ASSIGN(double, K_RBV, "{P}K_RBV")

ASSIGN_MONITOR(double, L, "{P}L")
ASSIGN_MONITOR_FLAG(double, L_event, "{P}L_event", L_mon)
ASSIGN_MONITOR(short, L_busy, "{P}L_busy")
ASSIGN(double, loadL, "{P}loadL")
ASSIGN(double, L_RBV, "{P}L_RBV")

double hkl[3];
double rbv_hkl[3];

/* angles (trial motor positions) */
ASSIGN_MONITOR(double, TTH, "{P}TTH")
ASSIGN_MONITOR_FLAG(double, TTH_event, "{P}TTH_event", TTH_mon)
ASSIGN_MONITOR(short, TTH_busy, "{P}TTH_busy")
ASSIGN(double, loadTTH, "{P}loadTTH")

ASSIGN_MONITOR(double, TH, "{P}TH")
ASSIGN_MONITOR_FLAG(double, TH_event, "{P}TH_event", TH_mon)
ASSIGN_MONITOR(short, TH_busy, "{P}TH_busy")
ASSIGN(double, loadTH, "{P}loadTH")

ASSIGN_MONITOR(double, CHI, "{P}CHI")
ASSIGN_MONITOR_FLAG(double, CHI_event, "{P}CHI_event", CHI_mon)
ASSIGN_MONITOR(short, CHI_busy, "{P}CHI_busy")
ASSIGN(double, loadCHI, "{P}loadCHI")

ASSIGN_MONITOR(double, PHI, "{P}PHI")
ASSIGN_MONITOR_FLAG(double, PHI_event, "{P}PHI_event", PHI_mon)
ASSIGN_MONITOR(short, PHI_busy, "{P}PHI_busy")
ASSIGN(double, loadPHI, "{P}loadPHI")

double angles[4];

/* primary and secondary reflections */
ASSIGN_MONITOR(double, H1, "{P}H1")
ASSIGN_MONITOR(double, K1, "{P}K1")
ASSIGN_MONITOR(double, L1, "{P}L1")
ASSIGN_MONITOR(double, TTH1, "{P}TTH1")
ASSIGN_MONITOR(double, TH1, "{P}TH1")
ASSIGN_MONITOR(double, CHI1, "{P}CHI1")
ASSIGN_MONITOR(double, PHI1, "{P}PHI1")
ASSIGN_MONITOR(short, refGet1, "{P}refGet1")
ASSIGN_MONITOR(short, refPut1, "{P}refPut1")

ASSIGN_MONITOR(double, H2, "{P}H2")
ASSIGN_MONITOR(double, K2, "{P}K2")
ASSIGN_MONITOR(double, L2, "{P}L2")
ASSIGN_MONITOR(double, TTH2, "{P}TTH2")
ASSIGN_MONITOR(double, TH2, "{P}TH2")
ASSIGN_MONITOR(double, CHI2, "{P}CHI2")
ASSIGN_MONITOR(double, PHI2, "{P}PHI2")
ASSIGN_MONITOR(short, refGet2, "{P}refGet2")
ASSIGN_MONITOR(short, refPut2, "{P}refPut2")

/* crystal parameters */
ASSIGN_MONITOR_FLAG(double, a, "{P}a", xtalParm_mon)
ASSIGN_MONITOR_eFLAG(double, b, "{P}b", xtalParm_mon)
ASSIGN_MONITOR_eFLAG(double, c, "{P}c", xtalParm_mon)
ASSIGN_MONITOR_eFLAG(double, alpha, "{P}alpha", xtalParm_mon)
ASSIGN_MONITOR_eFLAG(double, beta, "{P}beta", xtalParm_mon)
ASSIGN_MONITOR_eFLAG(double, Gamma, "{P}gamma", xtalParm_mon)

/* energy and wavelength */
ASSIGN_MONITOR(double, lambda, "{P}lambda")
ASSIGN_MONITOR_FLAG(double, lambda_event, "{P}lambda_event", lambda_mon)
ASSIGN(double, load_lambda, "{P}load_lambda")

ASSIGN_MONITOR(double, energy, "{P}energy")
ASSIGN_MONITOR_FLAG(double, energy_event, "{P}energy_event", energy_mon)
ASSIGN(double, load_energy, "{P}load_energy")

double hc; /* energy (keV) = hc / lambda (Angstroms) */

/* A0 matrix and its elements */
double A0[3][3];
double A0_i[3][3];

ASSIGN_MONITOR_FLAG(double, A0_11, "{P}A0_11", A0_mon)
ASSIGN_MONITOR_eFLAG(double, A0_12, "{P}A0_12", A0_mon)
ASSIGN_MONITOR_eFLAG(double, A0_13, "{P}A0_13", A0_mon)

ASSIGN_MONITOR_eFLAG(double, A0_21, "{P}A0_21", A0_mon)
ASSIGN_MONITOR_eFLAG(double, A0_22, "{P}A0_22", A0_mon)
ASSIGN_MONITOR_eFLAG(double, A0_23, "{P}A0_23", A0_mon)

ASSIGN_MONITOR_eFLAG(double, A0_31, "{P}A0_31", A0_mon)
ASSIGN_MONITOR_eFLAG(double, A0_32, "{P}A0_32", A0_mon)
ASSIGN_MONITOR_eFLAG(double, A0_33, "{P}A0_33", A0_mon)

/* OMTX matrix and its elements */
double OMTX[3][3];
double OMTX_i[3][3];

ASSIGN_MONITOR_FLAG(double, OMTX_11, "{P}OMTX_11", OMTX_mon)
ASSIGN_MONITOR_eFLAG(double, OMTX_12, "{P}OMTX_12", OMTX_mon)
ASSIGN_MONITOR_eFLAG(double, OMTX_13, "{P}OMTX_13", OMTX_mon)

ASSIGN_MONITOR_eFLAG(double, OMTX_21, "{P}OMTX_21", OMTX_mon)
ASSIGN_MONITOR_eFLAG(double, OMTX_22, "{P}OMTX_22", OMTX_mon)
ASSIGN_MONITOR_eFLAG(double, OMTX_23, "{P}OMTX_23", OMTX_mon)

ASSIGN_MONITOR_eFLAG(double, OMTX_31, "{P}OMTX_31", OMTX_mon)
ASSIGN_MONITOR_eFLAG(double, OMTX_32, "{P}OMTX_32", OMTX_mon)
ASSIGN_MONITOR_eFLAG(double, OMTX_33, "{P}OMTX_33", OMTX_mon)

/* check for OMTX calculation */
ASSIGN(double, errAngle, "{P}errAngle")
ASSIGN_MONITOR(double, errAngleThresh, "{P}errAngleThresh")

%%#include <string.h>
%%#include <math.h>
%%#include <epicsExport.h>
%%#include "matrix3.h"
%%#include "orient.h"


double v1_hkl[3];
double v1_angles[4];
double v2_hkl[3];
double v2_angles[4];
int i;

/* state PV's */ 
ASSIGN(short, newHKL, "{P}newHKL")
ASSIGN(short, newAngles, "{P}newAngles")
ASSIGN(short, newMotors, "{P}newMotors")
ASSIGN(short, waiting4Mot, "{P}waiting4Mot")
ASSIGN_MONITOR(short, waiting4motTTH, "{P}waiting4motTTH")
ASSIGN_MONITOR(short, waiting4motTH, "{P}waiting4motTH")
ASSIGN_MONITOR(short, waiting4motCHI, "{P}waiting4motCHI")
ASSIGN_MONITOR(short, waiting4motPHI, "{P}waiting4motPHI")
int old_connect_state;

/*
ASSIGN_MONITOR(long, A0_state, "{P}A0_state")
ASSIGN_MONITOR(long, OMTX_state, "{P}OMTX_state")
*/
ASSIGN_MONITOR(int, A0_state, "{P}A0_state")
ASSIGN_MONITOR(int, OMTX_state, "{P}OMTX_state")

ASSIGN_MONITOR(int, weWrote_motTTH, "{P}weWrote_motTTH")
ASSIGN_MONITOR(int, weWrote_motTH , "{P}weWrote_motTH")
ASSIGN_MONITOR(int, weWrote_motCHI, "{P}weWrote_motCHI")
ASSIGN_MONITOR(int, weWrote_motPHI, "{P}weWrote_motPHI")

#define CALC_NEEDED    0
#define CALC_SUCCEEDED 1
#define CALC_FAILED    2

#define BEEN_THERE 12345
int startup;

/* for macro-substitution */
char *orient_pmac_PM;
char *orient_pmac_mTTH;
char *orient_pmac_mTH;
char *orient_pmac_mCHI;
char *orient_pmac_mPHI;


/*
 * We have four state sets
 * orient      - initializes and does most of the work
 * connections - periodically checks PV connections and reports to user
 * readback    - monitors motor readback values, back transforms HKL,
 *               and shows user
 * stop        - monitors stop button and stops all motors
 */

ss orient {

	/******************/
	/* Initialization */
	/******************/

	/* If A0 or OMTX calculation failed, wait before retrying */ 
	state init_wait {
		when (delay(1)) {
			if (A0_state == CALC_FAILED) PVPUT(A0_state, CALC_NEEDED);
			if (OMTX_state == CALC_FAILED) PVPUT(OMTX_state, CALC_NEEDED);
		} state init
	}

	/*
	 * Make motor-PV names and CA connections
	 * Do initial A0 and OMTX matrix calculations (we don't leave
	 * until both have succeeded)
	 * Keep energy and wavelength synchronized
	 */
	state init {
		when (startup != BEEN_THERE) {
			hc = 12.3984244; /* energy (keV) = hc / lambda (Angstroms) */
			motorsConnected = 0;
			motorStopsConnected = 0;
			/* Construct motor PV names from macro arguments */
			%%pVar->orient_pmac_PM = seq_macValueGet(ssId, "PM");
			%%pVar->orient_pmac_mTTH = seq_macValueGet(ssId, "mTTH");
			%%pVar->orient_pmac_mTH = seq_macValueGet(ssId, "mTH");
			%%pVar->orient_pmac_mCHI = seq_macValueGet(ssId, "mCHI");
			%%pVar->orient_pmac_mPHI = seq_macValueGet(ssId, "mPHI");
			if (orient_pmac_PM == 0) {
				PUTALERT("motor prefix (PM) not specified");
			} else {
				/*
				 * C defect: built each motor name in its 40-byte motXXX_name, so
				 * PM+mXXX of 40 chars or more ran past it.  The names are built in
				 * tmpstr, whose 100 bytes hold any name the static {PM}{mXXX}
				 * assign can connect to, and motXXX_name gets a display copy.
				 */
				if (orient_pmac_mTTH == 0) {
					PUTALERT("orient: TTH motor name (mTTH) not specified");
				} else {
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mTTH);
					strncpy(motTTH_name, tmpstr, MSG_SIZE);
					motTTH_name[MSG_SIZE] = '\0';
					pvPut(motTTH_name);
					/* Construct name of motor-stop PV and assign. */
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".STOP");
					pvAssign(motTTH_stop, tmpstr);
					/* Construct name of motor-rbv PV and assign. */
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mTTH);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".RBV");
					pvAssign(motTTH_RBV, tmpstr);
				}
				if (orient_pmac_mTH == 0) {
					PUTALERT("orient: TH motor name (mTH) not specified");
				} else {
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mTH);
					strncpy(motTH_name, tmpstr, MSG_SIZE);
					motTH_name[MSG_SIZE] = '\0';
					pvPut(motTH_name);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".STOP");
					pvAssign(motTH_stop, tmpstr);
					/* Construct name of motor-rbv PV and assign. */
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mTH);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".RBV");
					pvAssign(motTH_RBV, tmpstr);
				}
				if (orient_pmac_mCHI == 0) {
					PUTALERT("orient: CHI motor name (mCHI) not specified");
				} else {
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mCHI);
					strncpy(motCHI_name, tmpstr, MSG_SIZE);
					motCHI_name[MSG_SIZE] = '\0';
					pvPut(motCHI_name);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".STOP");
					pvAssign(motCHI_stop, tmpstr);
					/* Construct name of motor-rbv PV and assign. */
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mCHI);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".RBV");
					pvAssign(motCHI_RBV, tmpstr);
				}
				if (orient_pmac_mPHI == 0) {
					PUTALERT("orient: PHI motor name (mPHI) not specified");
				} else {
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mPHI);
					strncpy(motPHI_name, tmpstr, MSG_SIZE);
					motPHI_name[MSG_SIZE] = '\0';
					pvPut(motPHI_name);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".STOP");
					pvAssign(motPHI_stop, tmpstr);
					/* Construct name of motor-rbv PV and assign. */
					%%strcpy(pVar->tmpstr, pVar->orient_pmac_PM);
					%%strcat(pVar->tmpstr, pVar->orient_pmac_mPHI);
					%%if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';
					%%strcat(pVar->tmpstr, ".RBV");
					pvAssign(motPHI_RBV, tmpstr);
				}
			}
			startup = BEEN_THERE;
		} state init

		/* We always have to keep energy and wavelength synchronized */
		when (efTest(lambda_mon)||efTest(energy_mon)) {
			if (efTest(lambda_mon)) {
				energy = hc / lambda;
				/* Write to energy without triggering event flag or setting Busy */
				PVPUT(load_energy, energy);
			} else if (efTest(energy_mon)) {
				lambda = hc / energy;
				/* Write to wavelength without triggering event flag or setting Busy */
				PVPUT(load_lambda, lambda);
			}
			efClear(lambda_mon); efClear(energy_mon);
			PVPUT(A0_state, CALC_NEEDED);
		} state init

		when (A0_state == CALC_NEEDED) {
			/*
			 * Calculate A0 matrix (depends on crystal structure and x-ray wavelength).
			 * If successful, A0_state -> CALC_SUCCEEDED; else A0_state -> CALC_FAILED,
			 * which should result in a retry.
			 */
			if (lambda > SMALL) {
				energy = hc/lambda;
				/* Write to energy without triggering event flag or setting Busy */
				PVPUT(load_energy, energy);
				if (orientDebug) PUTMSG("calculated energy");
			} else if (energy > SMALL) {
				lambda = hc/energy;
				/* Write to wavelength without triggering event flag or setting Busy */
				PVPUT(load_lambda, lambda);
			}
			if ((a!=0) && (b!=0) && (c!=0) && (alpha!=0) && (beta!=0) && (Gamma!=0) && (lambda!=0)) {
				efClear(xtalParm_mon); efClear(lambda_mon); efClear(energy_mon);
				if (orientDebug) PUTMSG("calling calc_A0");
				%%pVar->i = calc_A0(pVar->a, pVar->b, pVar->c, pVar->alpha, pVar->beta, pVar->Gamma,
				%%		pVar->lambda, pVar->A0, pVar->A0_i);
				/* Copy from array to individual matrix-element PV's, and show them to user */
				%%pVar->A0_11 = pVar->A0[0][0]; pVar->A0_12 = pVar->A0[0][1]; pVar->A0_13 = pVar->A0[0][2];
				%%pVar->A0_21 = pVar->A0[1][0]; pVar->A0_22 = pVar->A0[1][1]; pVar->A0_23 = pVar->A0[1][2];
				%%pVar->A0_31 = pVar->A0[2][0]; pVar->A0_32 = pVar->A0[2][1]; pVar->A0_33 = pVar->A0[2][2];
				pvPut(A0_11); pvPut(A0_12); pvPut(A0_13);
				pvPut(A0_21); pvPut(A0_22); pvPut(A0_23);
				pvPut(A0_31); pvPut(A0_32); pvPut(A0_33);
				PVPUT(A0_state, (i==0 ? CALC_SUCCEEDED : CALC_FAILED));
				if (A0_state == CALC_SUCCEEDED) PUTMSG("Successful A0 calc");
			} else {
				PVPUT(A0_state, CALC_FAILED);
			}
		} state init

		when (A0_state == CALC_FAILED) {
			/* A0 calculation failed.  Wait and retry. */
		} state init_wait

		when ((A0_state == CALC_SUCCEEDED) && (OMTX_state == CALC_NEEDED)) {
			/*
			 * Attempt to calculate orientation matrix OMTX -- this defines the orientation of the crystal,
			 * which is determined by user-entered locations (TTH, TH, CHI, PHI) of primary and secondary
			 * diffraction spots, and user-entered identification of the reciprocal-lattice vectors (HKL's)
			 * that correspond with those spots.  Check that calculations of A0 and OMTX actually do
			 * reproduce secondary reflection at its user-entered location.
			 * Don't bother to attempt calculation if primary- or secondary-reflection vectors are zero,
			 * because this cannot possibly work. 
			 */
			if ((H1!=0 || K1!=0 || L1!=0) && (H2!=0 || K2!=0 || L2!=0)) {
				if (orientDebug) PUTMSG("calling calc_OMTX");
				%%pVar->v1_hkl[H_INDEX] = pVar->H1; pVar->v1_hkl[K_INDEX] = pVar->K1; pVar->v1_hkl[L_INDEX] = pVar->L1;
				%%pVar->v1_angles[TTH_INDEX] = pVar->TTH1; pVar->v1_angles[TH_INDEX] = pVar->TH1;
				%%pVar->v1_angles[CHI_INDEX] = pVar->CHI1; pVar->v1_angles[PHI_INDEX] = pVar->PHI1;
				%%pVar->v2_hkl[H_INDEX] = pVar->H2; pVar->v2_hkl[K_INDEX] = pVar->K2; pVar->v2_hkl[L_INDEX] = pVar->L2;
				%%pVar->v2_angles[TTH_INDEX] = pVar->TTH2; pVar->v2_angles[TH_INDEX] = pVar->TH2;
				%%pVar->v2_angles[CHI_INDEX] = pVar->CHI2; pVar->v2_angles[PHI_INDEX] = pVar->PHI2;
				%%pVar->i = calc_OMTX(pVar->v1_hkl, pVar->v1_angles, pVar->v2_hkl, pVar->v2_angles,
				%%	pVar->A0, pVar->A0_i, pVar->OMTX, pVar->OMTX_i);
				%%pVar->OMTX_11 = pVar->OMTX[0][0]; pVar->OMTX_12 = pVar->OMTX[0][1]; pVar->OMTX_13 = pVar->OMTX[0][2];
				%%pVar->OMTX_21 = pVar->OMTX[1][0]; pVar->OMTX_22 = pVar->OMTX[1][1]; pVar->OMTX_23 = pVar->OMTX[1][2];
				%%pVar->OMTX_31 = pVar->OMTX[2][0]; pVar->OMTX_32 = pVar->OMTX[2][1]; pVar->OMTX_33 = pVar->OMTX[2][2];
				pvPut(OMTX_11); pvPut(OMTX_12); pvPut(OMTX_13);
				pvPut(OMTX_21); pvPut(OMTX_22); pvPut(OMTX_23);
				pvPut(OMTX_31); pvPut(OMTX_32); pvPut(OMTX_33);
				if (OMTX_Calc)  PVPUT(OMTX_Calc, 0);
				PVPUT(errAngle, checkOMTX(v2_hkl, v2_angles, A0, A0_i, OMTX_i));
			} else {
				i = -1;
			}
			/*
			 * If calculation returned an error, or secondary reflection was not accurately
			 * reproduced, declare failure (OMTX_state -> CALC_FAILED), which should result
			 * in a retry, and set motor-drive switch to manual, so we don't automatically
			 * drive into a ditch.
			 */ 
			if ((i==0) && (fabs(errAngle) < errAngleThresh)) {
				/* good calculation */
				PVPUT(OMTX_state, CALC_SUCCEEDED);
				PUTMSG("Successful OMTX calc");
			} else {
				PVPUT(OMTX_state, CALC_FAILED);
				if (motPut_Auto != 0) {
					PVPUT(motPut_Auto, 0);
					PUTALERT("Bad OMTX calc; motPut_Auto set to Manual");
				}
			}
		} state init

		when (OMTX_state == CALC_FAILED) {
			/* OMTX calculation failed.  Wait and retry. */
		} state init_wait

		when (A0_state == CALC_SUCCEEDED && (OMTX_state == CALC_SUCCEEDED)) {
			/* Success.  We graduate from the sandbox to the real world. */
		} state enter_monitor_all

	}

	/*********************/
	/* Monitor & respond */
	/*********************/

	state ref_show {
		/*
		 * Implement part of the user interface: user pushed a button to get or put a
		 * primary or secondary reflection from or to active HKL/Angle PV's.  Hold the
		 * value for a little while so MEDM display can show user arrows, or something,
		 * describing what the button did.
		 * (It would be nicer if we could show user what the button is going to do before
		 * he commits to doing it, but I can't figure out how to do this with MEDM.)
		 */
		when (delay(.5)) {
			PVPUT(refGet1, 0); PVPUT(refGet2, 0);
			PVPUT(refPut1, 0); PVPUT(refPut2, 0);
		} state monitor_all
	}

	state enter_monitor_all {
		when (delay(10)) {
			/* clear any event flags resulting from pvPut's we did earlier */
			efClear(A0_mon); efClear(OMTX_mon);
			efClear(H_mon); efClear(K_mon); efClear(L_mon);
			efClear(TTH_mon); efClear(TH_mon); efClear(CHI_mon); efClear(PHI_mon);
			efClear(Mode_mon);
			PUTMSG("Initialization complete; going live");
		} state monitor_all
	}

	state monitor_all_delay {
		/* Wait a little bit before going (back?) to live action. */
		when (delay(1)) {
		} state monitor_all
	}

	/* Watch everything and direct responses to a variety of specific conditions. */
	state monitor_all { 

		/*
		 * Check connections to motor PV's.  These are allowed to be in a different IOC from
		 * the rest of our stuff.
		 */
		when (motorsConnected == 0) {
			PUTMSG("Waiting for motor connection(s)");
		} state monitor_all_delay

		when (motorStopsConnected == 0) {
			PUTMSG("Waiting for motor-stop connection(s)");
		} state monitor_all_delay

		/* Detect user command to recalculate OMTX matrix */
		when ((A0_state == CALC_SUCCEEDED) && OMTX_Calc) {
			/* A0 is ok, but either OMTX is broken or user wants it to be recalculated */
			if (orientDebug) PUTMSG("Recalculating OMTX");
			%%pVar->v1_hkl[H_INDEX] = pVar->H1; pVar->v1_hkl[K_INDEX] = pVar->K1; pVar->v1_hkl[L_INDEX] = pVar->L1;
			%%pVar->v1_angles[TTH_INDEX] = pVar->TTH1; pVar->v1_angles[TH_INDEX] = pVar->TH1;
			%%pVar->v1_angles[CHI_INDEX] = pVar->CHI1; pVar->v1_angles[PHI_INDEX] = pVar->PHI1;
			%%pVar->v2_hkl[H_INDEX] = pVar->H2; pVar->v2_hkl[K_INDEX] = pVar->K2; pVar->v2_hkl[L_INDEX] = pVar->L2;
			%%pVar->v2_angles[TTH_INDEX] = pVar->TTH2; pVar->v2_angles[TH_INDEX] = pVar->TH2;
			%%pVar->v2_angles[CHI_INDEX] = pVar->CHI2; pVar->v2_angles[PHI_INDEX] = pVar->PHI2;
			%%pVar->i = calc_OMTX(pVar->v1_hkl, pVar->v1_angles, pVar->v2_hkl, pVar->v2_angles,
			%%	pVar->A0, pVar->A0_i, pVar->OMTX, pVar->OMTX_i);
			%%pVar->OMTX_11 = pVar->OMTX[0][0]; pVar->OMTX_12 = pVar->OMTX[0][1]; pVar->OMTX_13 = pVar->OMTX[0][2];
			%%pVar->OMTX_21 = pVar->OMTX[1][0]; pVar->OMTX_22 = pVar->OMTX[1][1]; pVar->OMTX_23 = pVar->OMTX[1][2];
			%%pVar->OMTX_31 = pVar->OMTX[2][0]; pVar->OMTX_32 = pVar->OMTX[2][1]; pVar->OMTX_33 = pVar->OMTX[2][2];
			pvPut(OMTX_11); pvPut(OMTX_12); pvPut(OMTX_13);
			pvPut(OMTX_21); pvPut(OMTX_22); pvPut(OMTX_23);
			pvPut(OMTX_31); pvPut(OMTX_32); pvPut(OMTX_33);
			PVPUT(OMTX_Calc, 0);
			errAngle = checkOMTX(v2_hkl, v2_angles, A0, A0_i, OMTX_i);
			pvPut(errAngle);
			if ((i==0) && (fabs(errAngle) < errAngleThresh)) {
				/* good calculation */
				PVPUT(OMTX_state, CALC_SUCCEEDED);
			} else {
				PVPUT(OMTX_state, CALC_FAILED);
				PVPUT(motPut_Auto, 0);
				PUTALERT("Bad OMTX calc; motPut_Auto set to Manual");
			}
		} state newOMTX

		/* Detect any change that will require us to recalculate A0 */
		when (efTest(xtalParm_mon) || efTest(lambda_mon) || efTest(energy_mon)) {
			if (efTest(lambda_mon)) {
				energy = hc / lambda;
				/* Write to energy without triggering event flag or setting Busy */
				PVPUT(load_energy, energy);
			} else if (efTest(energy_mon)) {
				lambda = hc / energy;
				/* Write to wavelength without triggering event flag or setting Busy */
				PVPUT(load_lambda, lambda);
			}
			efClear(xtalParm_mon); efClear(lambda_mon); efClear(energy_mon);
			if (orientDebug) PUTMSG("new XTAL parameters");
			%%pVar->i = calc_A0(pVar->a, pVar->b, pVar->c, pVar->alpha, pVar->beta,
			%%	pVar->Gamma, pVar->lambda, pVar->A0, pVar->A0_i);
			%%pVar->A0_11 = pVar->A0[0][0]; pVar->A0_12 = pVar->A0[0][1]; pVar->A0_13 = pVar->A0[0][2];
			%%pVar->A0_21 = pVar->A0[1][0]; pVar->A0_22 = pVar->A0[1][1]; pVar->A0_23 = pVar->A0[1][2];
			%%pVar->A0_31 = pVar->A0[2][0]; pVar->A0_32 = pVar->A0[2][1]; pVar->A0_33 = pVar->A0[2][2];
			pvPut(A0_11); pvPut(A0_12); pvPut(A0_13);
			pvPut(A0_21); pvPut(A0_22); pvPut(A0_23);
			pvPut(A0_31); pvPut(A0_32); pvPut(A0_33);
			PVPUT(A0_state, (i==0 ? CALC_SUCCEEDED : CALC_FAILED));
			if (A0_state == CALC_SUCCEEDED) {
				PUTMSG("Successful A0 calc");
			} else {
				PUTALERT("A0 calc failed");
			}
		} state newHKL

		/* Detect and handle direct user input of A0 matrix element */
		when (efTest(A0_mon)) {
			efClear(A0_mon);
			if (orientDebug) PUTMSG("new A0 precursor(s)");
			/* Copy to internal array and calculate matrix inverse */
			%%pVar->A0[0][0] = pVar->A0_11; pVar->A0[0][1] = pVar->A0_12; pVar->A0[0][2] = pVar->A0_13;
			%%pVar->A0[1][0] = pVar->A0_21; pVar->A0[1][1] = pVar->A0_22; pVar->A0[1][2] = pVar->A0_23;
			%%pVar->A0[2][0] = pVar->A0_31; pVar->A0[2][1] = pVar->A0_32; pVar->A0[2][2] = pVar->A0_33;
			i = invertArray(A0, A0_i);
			if (i == -1) {
				PUTMSG("Could not invert A0 array");
				PVPUT(A0_state, CALC_FAILED);
			}
		} state newA0

		/* Detect and handle direct user input of OMTX matrix element */
		when (efTest(OMTX_mon)) {
			efClear(OMTX_mon);
			if (orientDebug) PUTMSG("new OMTX element(s)");
			/* Copy to internal array; invert and check OMTX */
			%%pVar->OMTX[0][0] = pVar->OMTX_11; pVar->OMTX[0][1] = pVar->OMTX_12; pVar->OMTX[0][2] = pVar->OMTX_13;
			%%pVar->OMTX[1][0] = pVar->OMTX_21; pVar->OMTX[1][1] = pVar->OMTX_22; pVar->OMTX[1][2] = pVar->OMTX_23;
			%%pVar->OMTX[2][0] = pVar->OMTX_31; pVar->OMTX[2][1] = pVar->OMTX_32; pVar->OMTX[2][2] = pVar->OMTX_33;
			i = invertArray(OMTX, OMTX_i);
			if (i == -1) {
				PUTMSG("Could not invert OMTX array");
				PVPUT(OMTX_state, CALC_FAILED);
			} else if (A0_state != CALC_SUCCEEDED) {
				PUTMSG("Can't check or use OMTX array (No A0)");
				PVPUT(OMTX_state, CALC_FAILED);
			} else {
				errAngle = checkOMTX(v2_hkl, v2_angles, A0, A0_i, OMTX_i);
				pvPut(errAngle);
				if (fabs(errAngle) < errAngleThresh) {
					/* good calculation */
					PVPUT(OMTX_state, CALC_SUCCEEDED);
					if (orientDebug) PUTMSG("User's OMTX passes test");
				} else {
					PVPUT(OMTX_state, CALC_FAILED);
					if (orientDebug) PUTMSG("User's OMTX fails test");
				}
			}
		} state newOMTX

		/* Detect user entry of H, K, or L */
		when (efTest(H_mon)) {
			if (orientDebug) {printf("\norient: client wrote H\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(H_mon);
		} state newHKL

		when (efTest(K_mon)) {
			if (orientDebug) {printf("\norient: client wrote K\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(K_mon);
		} state newHKL

		when (efTest(L_mon)) {
			if (orientDebug) {printf("\norient: client wrote L\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(L_mon);
		} state newHKL


		/* Detect user entry of diffractometer angles. */
		when (efTest(TTH_mon)) {
			if (orientDebug) {printf("\norient: client wrote TTH\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(TTH_mon);
		} state newAngles

		when (efTest(TH_mon)) {
			if (orientDebug) {printf("\norient: client wrote TH\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(TH_mon);
		} state newAngles

		when (efTest(CHI_mon)) {
			if (orientDebug) {printf("\norient: client wrote CHI\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(CHI_mon);
		} state newAngles
		
		when (efTest(PHI_mon)) {
			if (orientDebug) {printf("\norient: client wrote PHI\n");}
			if (!Busy) PVPUT(Busy, 1);
			efClear(PHI_mon);
		} state newAngles

		/*
		 * Detect motor events.  There are two possible event types: a new motor value, and
		 * the completion message resulting from a pvPut(<motor>, ASYNC) call that we made.
		 * We try to distinguish these event types, using three pieces of information:
		 * A) weWrote_motXXX        - a flag we set when we write to the motor
		 * B) waiting4motXXX        - a flag we set when we write to the motor
		 * C) pvPutComplete(motXXX) - returns false after pvPut(<motor>, ASYNC), and continues
		 *                            to return false until the EPICS calls SNL back to say the
		 *                            motor (and any processing it may have triggered) is done.
		 *
		 * If the event is a new value, there are three possibilities:
		 * 1) We caused the event directly by writing to the motor, which resulted in
		 *    an immediate posting of the new value.  In this case, we simply clear the flag
		 *    (weWrote_motXXX) that we set at the time we wrote to the motor.
		 * 2) We caused the event indirectly by causing the motor to move, which resulted
		 *    in a delayed posting of the motor value when the motor stopped.  In this case
		 *    we expect the event will coincide with the done event, and will be masked by it.
		 * 3) We had nothing to do with it.  Some outside agent wrote to the motor.
		 *    In this case we start the process of reconciling HKL and angles with the new
		 *    motor position.
		 * Note that if the motor motion is short enough, we expect (1) and (2) to coincide with
		 * each other, and with the done event, which we expect to mask them both.
		 *
		 * If the event indicates completion, we clear both flags and start the process of
		 * reconciling HKL and angles with the new motor position (in case the motors did not
		 * get where we told them to go).
		 */
		when (efTest(motTTH_mon) && waiting4motTTH && pvPutComplete(motTTH)) {
			if (orientDebug) {printf("orient: motTTH done moving\n");}
			efClear(motTTH_mon); PVPUT(waiting4motTTH, 0);
			/*
			 * If motor completes immediately, the monitor and done events will
			 * set the (shared) event flag twice.  There's no way we can tell if
			 * this happened, except to assume the done event must come after the
			 * monitor event.
			 */ 
			if (weWrote_motTTH) PVPUT(weWrote_motTTH, 0);
		} state checkDone
		when (efTest(motTTH_mon) && weWrote_motTTH) {
			if (orientDebug) {printf("orient: We wrote motTTH\n");}
			efClear(motTTH_mon); PVPUT(weWrote_motTTH, 0);
		} state monitor_all 
		when (efTest(motTTH_mon) && !weWrote_motTTH) {
			if (orientDebug) {printf("orient: client wrote motTTH\n");}
			efClear(motTTH_mon);
		} state newMotors

		when (efTest(motTH_mon) && waiting4motTH && pvPutComplete(motTH)) {
			if (orientDebug) {printf("orient: motTH done moving\n");}
			efClear(motTH_mon); PVPUT(waiting4motTH, 0);
			if (weWrote_motTH) PVPUT(weWrote_motTH, 0);
		} state checkDone
		when (efTest(motTH_mon) && weWrote_motTH) {
			if (orientDebug) {printf("orient: We wrote motTH\n");}
			efClear(motTH_mon); PVPUT(weWrote_motTH, 0);
		} state monitor_all 
		when (efTest(motTH_mon) && !weWrote_motTH) {
			if (orientDebug) {printf("orient: client wrote motTH\n");}
			efClear(motTH_mon);
		} state newMotors

		when (efTest(motCHI_mon) && waiting4motCHI && pvPutComplete(motCHI)) {
			if (orientDebug) {printf("orient: motCHI done moving\n");}
			efClear(motCHI_mon); PVPUT(waiting4motCHI, 0);
			if (weWrote_motCHI) PVPUT(weWrote_motCHI, 0);
		} state checkDone
		when (efTest(motCHI_mon) && weWrote_motCHI) {
			if (orientDebug) {printf("orient: We wrote motCHI\n");}
			efClear(motCHI_mon); PVPUT(weWrote_motCHI, 0);
		} state monitor_all 
		when (efTest(motCHI_mon) && !weWrote_motCHI) {
			if (orientDebug) {printf("orient: client wrote motCHI\n");}
			efClear(motCHI_mon);
		} state newMotors

		when (efTest(motPHI_mon) && waiting4motPHI && pvPutComplete(motPHI)) {
			if (orientDebug) {printf("orient: motPHI done moving\n");}
			efClear(motPHI_mon); PVPUT(waiting4motPHI, 0);
			if (weWrote_motPHI) PVPUT(weWrote_motPHI, 0);
		} state checkDone
		when (efTest(motPHI_mon) && weWrote_motPHI) {
			if (orientDebug) {printf("orient: We wrote motPHI\n");}
			efClear(motPHI_mon); PVPUT(weWrote_motPHI, 0);
		} state monitor_all 
		when (efTest(motPHI_mon) && !weWrote_motPHI) {
			if (orientDebug) {printf("orient: client wrote motPHI\n");}
			efClear(motPHI_mon);
		} state newMotors

		/* Detect completion of all motors involved in a move */
		when (Busy && waiting4Mot && (waiting4motTTH==0) && (waiting4motTH==0) &&
				(waiting4motCHI==0) && (waiting4motPHI==0)) {
			PVPUT(waiting4Mot, 0);
			PVPUT(Busy, 0);
			if (H_busy) PVPUT(H_busy, 0);
			if (K_busy) PVPUT(K_busy, 0);
			if (L_busy) PVPUT(L_busy, 0);
			if (TTH_busy) PVPUT(TTH_busy, 0);
			if (TH_busy) PVPUT(TH_busy, 0);
			if (CHI_busy) PVPUT(CHI_busy, 0);
			if (PHI_busy) PVPUT(PHI_busy, 0);
			if (orientDebug) {printf("motors done\n");}
		} state monitor_all

		/* New constraint governing (H,K,L) <--> (TTH,TH,CHI,PHI) transform */
		when (efTest(Mode_mon)) {
			efClear(Mode_mon);
			if (orientDebug) {
				PUTMSG("new HKL-to-Angles Mode");
				printf("...Mode=%d\n", Mode);
			}
		} state newHKL

		/* User says "Move" -- i.e., send trial-angle values to the motors */
		when (motPut) {
			if (orientDebug) {printf("orient: motPut==%d\n", motPut);}
			PVPUT(weWrote_motTTH, 1); motTTH = TTH; PVPUT(waiting4motTTH, 1); pvPut(motTTH, ASYNC);
			PVPUT(weWrote_motTH,  1); motTH  = TH;  PVPUT(waiting4motTH,  1); pvPut(motTH, ASYNC);
			PVPUT(weWrote_motCHI, 1); motCHI = CHI; PVPUT(waiting4motCHI, 1); pvPut(motCHI, ASYNC);
			PVPUT(weWrote_motPHI, 1); motPHI = PHI; PVPUT(waiting4motPHI, 1); pvPut(motPHI, ASYNC);
			/* Write to motPut without triggering event flag or setting Busy */
			motPut = 0; PVPUT(load_motPut, motPut);
			PVPUT(waiting4Mot, 1);
		} state monitor_all

		/*
		 * User says "Read" -- copy motor positions to trial-angle values, and back transform
		 * to (H,K,L).
		 */
		when (motGet) {
		} state newMotors

		/* User says copy current (H,K,L) and (TTH,TH,CHI,PHI) to primary/secondary reflection */
		when (refGet1) {
			PVPUT(H1, H); PVPUT(K1, K); PVPUT(L1, L);
			PVPUT(TTH1, TTH); PVPUT(TH1, TH); PVPUT(CHI1, CHI); PVPUT(PHI1, PHI);
		} state ref_show

		when (refGet2) {
			PVPUT(H2, H); PVPUT(K2, K); PVPUT(L2, L);
			PVPUT(TTH2, TTH); PVPUT(TH2, TH); PVPUT(CHI2, CHI); PVPUT(PHI2, PHI);
		} state ref_show

		/* User says copy primary/secondary reflection to current (H,K,L) and (TTH,TH,CHI,PHI) */
		when (refPut1) {
			PVPUT(H, H1); PVPUT(K, K1); PVPUT(L, L1);
			PVPUT(TTH, TTH1); PVPUT(TH, TH1); PVPUT(CHI, CHI1); PVPUT(PHI, PHI1);
		} state ref_show

		when (refPut2) {
			PVPUT(H, H2); PVPUT(K, K2); PVPUT(L, L2);
			PVPUT(TTH, TTH2); PVPUT(TH, TH2); PVPUT(CHI, CHI2); PVPUT(PHI, PHI2);
		} state ref_show

		/* User wants to erase message area and clear alert */
		when (opAck) {
			PUTMSG("");
			PVPUT(opAck, 0);
			PVPUT(Alert, 0);
		} state monitor_all
	}


	/********************/
	/* Specific actions */
	/********************/


	/* A0 matrix was just recalculated.  Recalculate everything that depends on it. */
	state newA0 {
		when (A0_state != CALC_SUCCEEDED) {
		} state monitor_all
		when (A0_state == CALC_SUCCEEDED) {
		} state newHKL
	}

	/*
	 * OMTX matrix was just recalculated.  Recalculate everything that depends on it.
	 * If calculation failed, disconnect motors from automatic drive.
	 */
	state newOMTX {
		when (OMTX_state == CALC_SUCCEEDED) {
		} state newHKL
		when (OMTX_state != CALC_SUCCEEDED) {
			PVPUT(motPut_Auto, 0);
			PUTALERT("Bad OMTX calc; motPut_Auto set to Manual");
		} state monitor_all
	}

	/* 
	 * (H,K,L) vector was written to, or its meaning has changed because something in
	 * the calculation path between (H,K,L) and trial angles has changed.  If H, K, or L
	 * was written, wait a bit to see if other elements of the vector are also written,
	 * so we don't launch motors separately on each element of what was intended to be
	 * a vector write.
	 */
	state newHKL {
		when ((A0_state != CALC_SUCCEEDED) || (OMTX_state != CALC_SUCCEEDED)) {
			if (A0_state != CALC_SUCCEEDED) {
				PUTALERT("We don't have a good A0 matrix");
			} else {
				PUTALERT("We don't have a good OMTX matrix");
			}
		} state monitor_all
		when (delay(SYNC_WAIT_SECS)) {
			if (efTest(H_mon)) {efClear(H_mon); if (orientDebug) printf("orient:newHKL: client also wrote H\n");}
			if (efTest(K_mon)) {efClear(K_mon); if (orientDebug) printf("orient:newHKL: client also wrote K\n");}
			if (efTest(L_mon)) {efClear(L_mon); if (orientDebug) printf("orient:newHKL: client also wrote L\n");}
			/* calc angles */
			%%pVar->hkl[H_INDEX] = pVar->H; pVar->hkl[K_INDEX] = pVar->K; pVar->hkl[L_INDEX] = pVar->L;
			%%pVar->angles[TTH_INDEX] = pVar->TTH; pVar->angles[TH_INDEX] = pVar->TH;
			%%pVar->angles[CHI_INDEX] = pVar->CHI; pVar->angles[PHI_INDEX] = pVar->PHI;
			%%pVar->i = HKL_to_angles(pVar->hkl, pVar->A0, pVar->OMTX, pVar->angles, (int)pVar->Mode);
			if (i == 0) {
				%%pVar->TTH = pVar->angles[TTH_INDEX];
				%%pVar->TH = pVar->angles[TH_INDEX];
				%%pVar->CHI = pVar->angles[CHI_INDEX];
				%%pVar->PHI = pVar->angles[PHI_INDEX];
				PVPUT(newAngles, 1);
				if (motPut_Auto) PVPUT(newMotors, 1);
			} else if (i) {
				PUTALERT("Bad HKL2angles calc.");
			}
		} state putAll
	}

	/*
	 * A trial-angle was written to.  Wait a bit to see if other elements of the trial-angle
	 * vector (TTH, TH, CHI, PHI) are also written, so we don't launch motors separately on
	 * each element of what was intended to be a vector write.
	 */
	state newAngles {
		when ((A0_state != CALC_SUCCEEDED) || (OMTX_state != CALC_SUCCEEDED)) {
			if (A0_state != CALC_SUCCEEDED) {
				PUTALERT("We don't have a good A0 matrix");
			} else {
				PUTALERT("We don't have a good OMTX matrix");
			}
		} state monitor_all
		when (delay(SYNC_WAIT_SECS)) {
			if (efTest(TTH_mon)) {efClear(TTH_mon); if (orientDebug) printf("orient:newAngles: client also wrote TTH\n");}
			if (efTest(TH_mon)) {efClear(TH_mon); if (orientDebug) printf("orient:newAngles: client also wrote TH\n");}
			if (efTest(CHI_mon)) {efClear(CHI_mon); if (orientDebug) printf("orient:newAngles: client also wrote CHI\n");}
			if (efTest(PHI_mon)) {efClear(PHI_mon); if (orientDebug) printf("orient:newAngles: client also wrote PHI\n");}
			/* calc HKL */
			%%pVar->angles[TTH_INDEX] = pVar->TTH; pVar->angles[TH_INDEX] = pVar->TH;
			%%pVar->angles[CHI_INDEX] = pVar->CHI; pVar->angles[PHI_INDEX] = pVar->PHI;
			%%angles_to_HKL(pVar->angles, pVar->OMTX_i, pVar->A0_i, pVar->hkl);
			%%pVar->H = pVar->hkl[H_INDEX]; pVar->K = pVar->hkl[K_INDEX]; pVar->L = pVar->hkl[L_INDEX];
			PVPUT(newHKL, 1);
			if (motPut_Auto) {
				PVPUT(newMotors, 1);
			}
		} state putAll
	}

	state checkDone {
		/* Detect completion of all motors involved in a move */
		when (Busy && waiting4Mot && (waiting4motTTH==0) && (waiting4motTH==0) &&
				(waiting4motCHI==0) && (waiting4motPHI==0)) {
			PVPUT(waiting4Mot, 0);
			PVPUT(Busy, 0);
			if (H_busy) PVPUT(H_busy, 0);
			if (K_busy) PVPUT(K_busy, 0);
			if (L_busy) PVPUT(L_busy, 0);
			if (TTH_busy) PVPUT(TTH_busy, 0);
			if (TH_busy) PVPUT(TH_busy, 0);
			if (CHI_busy) PVPUT(CHI_busy, 0);
			if (PHI_busy) PVPUT(PHI_busy, 0);
			if (orientDebug) {printf("motors done\n");}
		} state newMotors
		when () {
		} state monitor_all
	}

	/* A motor drive value has changed.  Update trial-angle values and back transform to (H,K,L). */
	state newMotors {
		when () {
			if (orientDebug) {printf("orient:newMotors: motGet=%d\n", motGet);}
			if (motGet || (motGet_Auto && !waiting4Mot)) {
				/* get angles */
				if (orientDebug) {printf("orient:newMotors: getting values\n");}
				TTH = motTTH; TH = motTH; CHI = motCHI; PHI = motPHI;
				PVPUT(newAngles, 1);
				/* calc HKL */
				%%pVar->angles[TTH_INDEX] = pVar->TTH; pVar->angles[TH_INDEX] = pVar->TH;
				%%pVar->angles[CHI_INDEX] = pVar->CHI; pVar->angles[PHI_INDEX] = pVar->PHI;
				%%angles_to_HKL(pVar->angles, pVar->OMTX_i, pVar->A0_i, pVar->hkl);
				%%pVar->H = pVar->hkl[H_INDEX]; pVar->K = pVar->hkl[K_INDEX]; pVar->L = pVar->hkl[L_INDEX];
				PVPUT(newHKL, 1);
				if (motGet) PVPUT(motGet, 0);
			}
		} state putAll
	}

	/***************************************************/
	/* Write to drive PV's; handle trivial completion  */
	/***************************************************/

/*
 * What if motor isn't allowed to go where we want it to go?  What notification
 * do we get?  Maybe we need to monitor motor limits and back transform to (H,K,L)
 * limits. 
 */
	state putAll {
		when () {
			if (newHKL) {
				if (orientDebug) {printf("orient:putAll: writing HKL\n");}
				/* Write to (H,K,L) without triggering event flag or setting Busy */
				PVPUT(loadH, H);
				PVPUT(loadK, K);
				PVPUT(loadL, L);
				PVPUT(newHKL, 0);
			}
			if (newAngles) {
				if (orientDebug) {printf("orient:putAll: writing Angles\n");}
				/* Write to (TTH,TH,CHI,PHI) without triggering event flag or setting Busy */
				PVPUT(loadTTH, TTH);
				PVPUT(loadTH,  TH);
				PVPUT(loadCHI, CHI);
				PVPUT(loadPHI, PHI);
				PVPUT(newAngles, 0);
			}
			if (newMotors) {
				if (orientDebug) {printf("orient:putAll: comparing new/old Motor values\n");}
				if (motTTH_Connected && motTH_Connected && motCHI_Connected && motPHI_Connected) {
					if (orientDebug) printf("putAll:newMotors: H=%8.4f, K=%8.4f, L=%8.4f\n", H, K, L);
					if (DIFFERENT(motTTH, TTH)) {
						if (orientDebug) printf("putAll:newMotors: TTH=%8.4f, motTTH=%8.4f\n", TTH, motTTH);
						PVPUT(weWrote_motTTH, 1); PVPUT(waiting4motTTH, 1);
						motTTH = TTH; pvPut(motTTH, ASYNC);
					}
					if (DIFFERENT(motTH, TH)) {
						if (orientDebug) printf("putAll:newMotors:  TH=%8.4f,  motTH=%8.4f\n", TH, motTH);
						PVPUT(weWrote_motTH,  1); PVPUT(waiting4motTH,  1);
						motTH = TH; pvPut(motTH, ASYNC);
					}
					if (DIFFERENT(motCHI, CHI)) {
						if (orientDebug) printf("putAll:newMotors: CHI=%8.4f, motCHI=%8.4f\n", CHI, motCHI);
						PVPUT(weWrote_motCHI , 1); PVPUT(waiting4motCHI, 1);
						motCHI = CHI; pvPut(motCHI, ASYNC);
					}
					if (DIFFERENT(motPHI, PHI)) {
						if (orientDebug) printf("putAll:newMotors: PHI=%8.4f, motPHI=%8.4f\n", PHI, motPHI);
						PVPUT(weWrote_motPHI, 1); PVPUT(waiting4motPHI, 1);
						motPHI = PHI; pvPut(motPHI, ASYNC);
					}
					if (waiting4motTTH || waiting4motTH || waiting4motCHI || waiting4motPHI) {
						PVPUT(waiting4Mot, 1);
						if (!Busy) PVPUT(Busy, 1);
					}
					PVPUT(newMotors, 0);
				} else {
					PUTALERT("Motors not connected");
					PVPUT(newMotors, 0);
				}
			}

			if (!waiting4Mot) {
				if (Busy) PVPUT(Busy, 0);
				if (H_busy) PVPUT(H_busy, 0);
				if (K_busy) PVPUT(K_busy, 0);
				if (L_busy) PVPUT(L_busy, 0);
				if (TTH_busy) PVPUT(TTH_busy, 0);
				if (TH_busy) PVPUT(TH_busy, 0);
				if (CHI_busy) PVPUT(CHI_busy, 0);
				if (PHI_busy) PVPUT(PHI_busy, 0);
				if (orientDebug) {printf("orient:putAll: all complete (no motors)\n");}
			}
		} state monitor_all
	}
}


/* Monitor connections to motor-drive values; show user */
ss connections {
	state check {
		/* Report connection state of motor PV's */
		when (delay(1)) {
			old_connect_state = motTTH_Connected;
			motTTH_Connected = pvAssigned(motTTH) ? pvConnected(motTTH) : 0;
			if (old_connect_state != motTTH_Connected) pvPut(motTTH_Connected);

			old_connect_state = motTH_Connected;
			motTH_Connected = pvAssigned(motTH) ? pvConnected(motTH) : 0;
			if (old_connect_state != motTH_Connected) pvPut(motTH_Connected);

			old_connect_state = motCHI_Connected;
			motCHI_Connected = pvAssigned(motCHI) ? pvConnected(motCHI) : 0;
			if (old_connect_state != motCHI_Connected) pvPut(motCHI_Connected);

			old_connect_state = motPHI_Connected;
			motPHI_Connected = pvAssigned(motPHI) ? pvConnected(motPHI) : 0;
			if (old_connect_state != motPHI_Connected) pvPut(motPHI_Connected);
			
			motorsConnected = motTTH_Connected && motTH_Connected && motCHI_Connected &&
				motPHI_Connected;
			
			motorStopsConnected = pvAssigned(motTTH_stop) && pvConnected(motTTH_stop) &&
				pvAssigned(motTH_stop) && pvConnected(motTH_stop) && 
				pvAssigned(motCHI_stop) && pvConnected(motCHI_stop) &&
				pvAssigned(motPHI_stop) && pvConnected(motPHI_stop);
		} state check
	}
}

/* Handle motor-readback values */
ss readback {
	state rbv_monitor_throttle {
		when (delay(.1)) {
		} state rbv_monitor
	}

	state rbv_monitor_wait {
		when ((A0_state == CALC_SUCCEEDED) && (OMTX_state == CALC_SUCCEEDED)) {
		} state rbv_monitor

		when (delay(5)) {
		} state rbv_monitor
	}

	state rbv_monitor {
		when ((A0_state != CALC_SUCCEEDED) || (OMTX_state != CALC_SUCCEEDED)) {
			if (H_RBV != 0) PVPUT(H_RBV, 0);
			if (K_RBV != 0) PVPUT(K_RBV, 0);
			if (L_RBV != 0) PVPUT(L_RBV, 0);
		} state rbv_monitor_wait

		when (efTest(motRBV_mon)) {
			efClear(motRBV_mon);
			/* calc HKL */
			%%pVar->rbv_angles[TTH_INDEX] = pVar->motTTH_RBV; pVar->rbv_angles[TH_INDEX] = pVar->motTH_RBV;
			%%pVar->rbv_angles[CHI_INDEX] = pVar->motCHI_RBV; pVar->rbv_angles[PHI_INDEX] = pVar->motPHI_RBV;
			%%angles_to_HKL(pVar->rbv_angles, pVar->OMTX_i, pVar->A0_i, pVar->rbv_hkl);
			%%pVar->H_RBV = pVar->rbv_hkl[H_INDEX]; pVar->K_RBV = pVar->rbv_hkl[K_INDEX]; pVar->L_RBV = pVar->rbv_hkl[L_INDEX];
			pvPut(H_RBV); pvPut(K_RBV); pvPut(L_RBV);
		} state rbv_monitor_throttle
	}
}

/* Make sure we send stop promptly */
ss stop {
	state stop_all {
		when (stopMotors) {
			strncpy(stopMsg, "stopping motors", MSG_SIZE);
			pvPut(stopMsg);
			if (orientDebug) printf("orient: %s\n", stopMsg);
			if (pvAssigned(motTTH_stop) && pvConnected(motTTH_stop)) PVPUT(motTTH_stop, 1);
			if (pvAssigned(motTH_stop) && pvConnected(motTH_stop)) PVPUT(motTH_stop, 1);
			if (pvAssigned(motCHI_stop) && pvConnected(motCHI_stop)) PVPUT(motCHI_stop, 1);
			if (pvAssigned(motPHI_stop) && pvConnected(motPHI_stop)) PVPUT(motPHI_stop, 1);
			PVPUT(stopMotors, 0);
		} state stop_all
	}
}
