program xiahsc ("name=hsc1, S=ito:serial:b0, P=ito:, HSC=hsc1:")

option +r;  /* re-usable code, can load more than once */
option +d;  /* turn ON run-time debugging messages */

/*
 * usage:
 ld < /home/disk2/ioc/share/synApps/R4_0/uniApp/src/O.ppc604/xiahsc.o
 seq &xiahsc, "name=hsc4, P=jfk:, HSC=hsc4:, S=jfk:serialOI:b3"
 seq &xiahsc, "name=hsc1, P=sfo:, HSC=hsc1:, S=sfo:serialOI:b0"
 
 * to kill the running sequence program:
 td "hsc4"

       WISH LIST for XIAHSC
       ====================
   * support device aliases
   * motor velocity control
   * gear backlash setting
   * status bits
   * progress indication (not provided on device)
   * support for INQUIRY command
   * user coordinates
   * user can change the serial port to be used

 */

/*------------------------------------------------ 
  ------------------------------------------------   general purpose
  ------------------------------------------------ */

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
#define PVPUTSTR(MSG,TEXT)  {strcpy(MSG, TEXT); pvPut(MSG);}
#define MIN(A,B,C) {if ((A)<(B)) {C=(A);} else {C=(B);}}
#define MAX(A,B,C) {if ((A)>(B)) {C=(A);} else {C=(B);}}

#define DEBUG_PRINT(DEBUG_LEVEL,FMT)		\
  if (xiahscDebug >= DEBUG_LEVEL) {		\
    printf("(%s,%d): ", __FILE__, __LINE__);	\
    printf(FMT);				\
    printf("\n");				\
  }

#define DEBUG_PRINT1(DEBUG_LEVEL,FMT,V)	\
  if (xiahscDebug >= DEBUG_LEVEL) {		\
    printf("(%s,%d): ", __FILE__, __LINE__);	\
    printf(FMT,V);				\
    printf("\n");				\
  }

#define DEBUG_PRINT2(DEBUG_LEVEL,FMT,V1,V2)	\
  if (xiahscDebug >= DEBUG_LEVEL) {		\
    printf("(%s,%d): ", __FILE__, __LINE__);	\
    printf(FMT,V1,V2);				\
    printf("\n");				\
  }

#define DEBUG_PRINT3(DEBUG_LEVEL,FMT,V1,V2,V3)	\
  if (xiahscDebug >= DEBUG_LEVEL) {		\
    printf("(%s,%d): ", __FILE__, __LINE__);	\
    printf(FMT,V1,V2,V3);				\
    printf("\n");				\
  }

/*------------------------------------------------ 
  ------------------------------------------------   assignments
  ------------------------------------------------ */


    /**** 
     **** asyn record (was generic serial record)
     ****/
ASSIGN_MON_FLAG(string, s_ainp, "{S}.AINP", s_ainp_mon)
ASSIGN        (string, s_aout,  "{S}.AOUT")
ASSIGN        (short,  s_baud,  "{S}.BAUD")
ASSIGN        (short,  s_dbit,  "{S}.DBIT")
ASSIGN        (short,  s_fctl,  "{S}.FCTL")
/* ASSIGN        (short,  s_idel,  "{S}.IDEL") */
ASSIGN        (string,  s_ieos,  "{S}.IEOS") /* replaces idel */
ASSIGN        (short,  s_ifmt,  "{S}.IFMT")
ASSIGN_MONITOR(short,  s_nord,  "{S}.NORD")
ASSIGN        (short,  s_nrrd,  "{S}.NRRD")
/* ASSIGN        (short,  s_odel,  "{S}.ODEL") */
ASSIGN        (string,  s_oeos,  "{S}.OEOS") /* replaces odel */
ASSIGN        (short,  s_ofmt,  "{S}.OFMT")
ASSIGN_MONITOR(short,  s_proc,  "{S}.PROC")
ASSIGN        (short,  s_prty,  "{S}.PRTY")
ASSIGN        (short,  s_sbit,  "{S}.SBIT")
ASSIGN        (short,  s_scan,  "{S}.SCAN")
ASSIGN_MONITOR(int,    s_sevr,  "{S}.SEVR")
ASSIGN        (short,  s_tmod,  "{S}.TMOD")
/* ASSIGN_MONITOR(long,   s_tmot,  "{S}.TMOT") */
ASSIGN_MONITOR(double,   s_tmot,  "{S}.TMOT") /* was long milliseconds */


    /**** 
     **** XIAHSC database 
     ****/
ASSIGN         (string, port,      "{P}{HSC}port")
ASSIGN_MON_FLAG(string, hID,       "{P}{HSC}hID",     hID_mon)
ASSIGN_MON_FLAG(string, vID,       "{P}{HSC}vID",     vID_mon)
ASSIGN_MON_FLAG(short,  hOrient,   "{P}{HSC}hOrient", hOrient_mon)
ASSIGN_MON_FLAG(short,  vOrient,   "{P}{HSC}vOrient", vOrient_mon)
ASSIGN         (short,  hBusy,     "{P}{HSC}hBusy")
ASSIGN         (short,  vBusy,     "{P}{HSC}vBusy")
ASSIGN_MON_FLAG(short,  stop,      "{P}{HSC}stop",    stop_mon)
ASSIGN_MONITOR (short,  locate,    "{P}{HSC}locate")
ASSIGN_MONITOR (short,  init,      "{P}{HSC}init")
ASSIGN_MONITOR (short,  enable,    "{P}{HSC}enable")
ASSIGN_MONITOR (short,  calibrate, "{P}{HSC}calib")
ASSIGN_MON_FLAG(double, t,         "{P}{HSC}t",       t_mon)
ASSIGN_MONITOR (double, t_hi,      "{P}{HSC}t.HOPR")
ASSIGN_MONITOR (double, t_lo,      "{P}{HSC}t.LOPR")
ASSIGN_MONITOR (double, t_RB,      "{P}{HSC}tRB")
ASSIGN_MON_FLAG(double, l,         "{P}{HSC}l",       l_mon)
ASSIGN_MONITOR (double, l_hi,      "{P}{HSC}l.HOPR")
ASSIGN_MONITOR (double, l_lo,      "{P}{HSC}l.LOPR")
ASSIGN_MONITOR (double, l_RB,      "{P}{HSC}lRB")
ASSIGN_MON_FLAG(double, b,         "{P}{HSC}b",       b_mon)
ASSIGN_MONITOR (double, b_hi,      "{P}{HSC}b.HOPR")
ASSIGN_MONITOR (double, b_lo,      "{P}{HSC}b.LOPR")
ASSIGN_MONITOR (double, b_RB,      "{P}{HSC}bRB")
ASSIGN_MON_FLAG(double, r,         "{P}{HSC}r",       r_mon)
ASSIGN_MONITOR (double, r_hi,      "{P}{HSC}r.HOPR")
ASSIGN_MONITOR (double, r_lo,      "{P}{HSC}r.LOPR")
ASSIGN_MONITOR (double, r_RB,      "{P}{HSC}rRB")
ASSIGN_MON_FLAG(double, h0,        "{P}{HSC}h0",      h0_mon)
ASSIGN_MONITOR (double, h0_hi,     "{P}{HSC}h0.HOPR")
ASSIGN_MONITOR (double, h0_lo,     "{P}{HSC}h0.LOPR")
ASSIGN_MONITOR (double, h0_RB,     "{P}{HSC}h0RB")
ASSIGN_MON_FLAG(double, v0,        "{P}{HSC}v0",      v0_mon)
ASSIGN_MONITOR (double, v0_hi,     "{P}{HSC}v0.HOPR")
ASSIGN_MONITOR (double, v0_lo,     "{P}{HSC}v0.LOPR")
ASSIGN_MONITOR (double, v0_RB,     "{P}{HSC}v0RB")
ASSIGN_MON_FLAG(double, width,     "{P}{HSC}width",   width_mon)
ASSIGN_MONITOR (double, width_hi,  "{P}{HSC}width.HOPR")
ASSIGN_MONITOR (double, width_lo,  "{P}{HSC}width.LOPR")
ASSIGN_MONITOR (double, width_RB,  "{P}{HSC}widthRB")
ASSIGN_MON_FLAG(double, height,    "{P}{HSC}height",  height_mon)
ASSIGN_MONITOR (double, height_hi, "{P}{HSC}height.HOPR")
ASSIGN_MONITOR (double, height_lo, "{P}{HSC}height.LOPR")
ASSIGN_MONITOR (double, height_RB, "{P}{HSC}heightRB")
ASSIGN         (int,    error,     "{P}{HSC}error")
ASSIGN         (string, errMsg,    "{P}{HSC}errMsg")

ASSIGN_MONITOR (int,  xiahscDebug, "{P}{HSC}debug")

    /****  
     **** local variables 
     ****/
char   scratch[256];
char   myChar;
double dScratch;
/*long   numWords; tmm changed to int for 64-bit linux */
int   numWords;
string word1;         string word2; string word3; 
string word4;         string word5; string word6;
/*long   pos_a;         long   pos_b; tmm changed to int for 64-bit linux */
int   pos_a;         int   pos_b;
double pos_ag;        double pos_bg;
string id;            short  hasID;
string hscErrors[14];
evflag start_H_move;  evflag start_V_move;
evflag wait_H_input;  evflag wait_V_input;
evflag new_H_RB;      evflag new_V_RB;
evflag move_h;        evflag move_v;
double t_old;         double l_old;
double b_old;         double r_old;
double width_old;     double height_old;
double h0_old;        double v0_old;
int    init_delay;
int    i;
int    count;
int    hOrigin;      int    vOrigin;
int    hOuterLimit;  int    vOuterLimit;
string hsc_command;
string hsc_response;
int    hsc_timeout;
int    h_valid;
int    v_valid;


%% #include <math.h>
%% #include <string.h>
%% #include <epicsThread.h>
/*
%% static int i;
*/

%% int validateResponse(char *out, char *in);

/*------------------------------------------------ 
  ------------------------------------------------   macros
  ------------------------------------------------ */

#define SCAN_PASSIVE    0
#define SCAN_RAPID      7
#define TMOD_WRITE_READ 0
#define TMOD_WRITE      1
#define TMOD_READ       2
#define LINE_FEED       "\n"	/* was 10 for generic serial record */
#define CARRIAGE_RETURN "\r"	/* was 13 for generic serial record */
#define ORIENT_LEFT_RIGHT      0
#define ORIENT_RIGHT_LEFT      1
#define ORIENT_TOP_BOTTOM      0
#define ORIENT_BOTTOM_TOP      1
#define MOTOR_IDLE             0
#define MOTOR_BUSY             1
#define STOP_IDLE              0
#define STOP_STOP              1
#define CALIBRATE_DONE         0
#define CALIBRATE_START        1

#define BAUD_9600	6 /* was 5 for serial record */
#define DBIT_8		4 /* was 3 for serial record */
#define SBIT_1		1 /* was 0 for serial record */
#define PRTY_NONE	1 /* was 0 for generic serial */
#define FCTL_NONE	1 /* was 0 for serial record */

#define MOVING_POLL_INTERVAL         0.25
#define IDLE_POLL_INTERVAL           5.0
#define MOVE_TIMEOUT               (5 * 60.0)
#define MOVE_HESITATION              0.1
#define ERROR_RECONNECT_INTERVAL   (10 * 60.0)

#define STEPS_PER_mm    400

/*
 * there are 14 error messages from the HSC
 * (numbered in HSC from 0-13, I move them to 1-14)
 * error 0 means no error
 */
#define NO_ERROR                 0
#define ERROR_SOFT_LIMITS       15
#define ERROR_UNKNOWN		16
#define ERROR_BAD_ID		17
#define ERROR_COMM_ERROR	18



#define WAIT_AINP()   {							\
   efClear (s_ainp_mon); /* will wait for the record to process */	\
   for (i = 0; i <= 15; i++) { /* wait no more than 0.25 seconds */	\
     if (efTest (s_ainp_mon)) break;	/* input is ready */		\
     epicsThreadSleep(.02); /* wait 1 clock tick before retrying */	\
   } }

#define WRITE_HSC(MODE, HSC_ID, HSC_COMMAND)  {				\
  if (!s_sevr) {							\
   sprintf (hsc_command, "!%s %s", HSC_ID, HSC_COMMAND);		\
   DEBUG_PRINT1(10,"WRITE_HSC: <%s>", hsc_command);			\
   PVPUT(s_scan,  SCAN_PASSIVE);					\
   PVPUT(s_tmod,  MODE);						\
   PVPUTSTR(s_ieos,  LINE_FEED);					\
   PVPUTSTR(s_oeos,  CARRIAGE_RETURN);					\
   PVPUTSTR(s_aout,  hsc_command);					\
  }									\
}

#define WAIT_HSC_RESPONSE(TICKS) {					\
  if (!s_sevr) {							\
    for (i = TICKS; i; i--) {	 					\
      if (efTest (s_ainp_mon)) {					\
        hsc_timeout = 0;  /* Aha! Got a response. Thus, no timeout */	\
        break;	/* input is ready */					\
      }									\
      epicsThreadSleep(.02); /* wait 1 clock tick before retrying */	\
    }; 									\
    if (hsc_timeout == 1) {						\
      strcpy(hsc_response, ""); 					\
      DEBUG_PRINT1(10,"WAIT_HSC_RESPONSE: <%s> -> <<< TIMEOUT >>>", 	\
      			hsc_command);					\
    } else {								\
      strcpy(hsc_response, s_ainp); 					\
      DEBUG_PRINT3(10,"WAIT_HSC_RESPONSE: <%s> -> NORD=%d  AINP=%s", 	\
	      		hsc_command, s_nord, hsc_response);		\
    };  								\
  }									\
}

#define TIMEOUT_TICKS 15   /* wait up to 0.25 seconds */

#define WRITE_READ_HSC(HSC_ID, HSC_COMMAND)  {				\
    efClear (s_ainp_mon); /* will wait for the record to process */	\
    hsc_timeout = 1;	  /* assume a timeout */			\
    WRITE_HSC(TMOD_WRITE_READ, HSC_ID, HSC_COMMAND);			\
    WAIT_HSC_RESPONSE(TIMEOUT_TICKS);					\
  }

#define S_PORT_CONFIG(BAUD, DBIT, SBIT, PRTY, FCTL)   { \
      PVPUT (s_baud,  BAUD);				\
      PVPUT (s_dbit,  DBIT);				\
      PVPUT (s_sbit,  SBIT);				\
      PVPUT (s_prty,  PRTY);				\
      PVPUT (s_fctl,  FCTL); }

#define RAW_TO_DIAL(A, ORIGIN) ( 1.0*(A-ORIGIN)/STEPS_PER_mm )
#define DIAL_TO_RAW(A, ORIGIN) ( (A) * STEPS_PER_mm + 0.5 + ORIGIN )

#define LIMIT_TEST(LO,VAL,HI)	( (LO <= VAL) && (VAL <= HI) )


/* 
 * macros to command the HSC-1
 */
#define HSC1_OUTER_MOTION_LIMIT "R 1"
#define HSC1_ORIGIN_POSITION    "R 2"
#define HSC1_MOTOR_A_POSITION   "R 3"
#define HSC1_MOTOR_B_POSITION   "R 4"
#define HSC1_MOTOR_DELAY_CODE   "R 5"
#define HSC1_GEAR_BACKLASH      "R 6"
#define HSC1_CONTROL_WORD       "R 7"

#define HSC1_KILL_MOVEMENT       "K"
#define HSC1_POSITION_INQUIRY    "P"
#define HSC1_CALIBRATE_IMMEDIATE "0 I"


/* 
 * slit state:  xiahsc
 */
ss xiahsc {

  state startup {
    when () {
      xiahscDebug = 0;
      pvGet (s_ainp);
      pvGet (s_aout);
      PVPUT (init, 1);		/* need to initialize */
      init_delay = 1.0;		/* time to delay before re-init */
      /*
       * users have requested some mention of the associated serial port name
       * for the future:  Can use change the serial port via this PV?
       */
      strcpy (port, macValueGet("S"));
      pvPut (port);

      strcpy (hscErrors[0],  "Missing Command");
      strcpy (hscErrors[1],  "Unrecognized Command");
      strcpy (hscErrors[2],  "Input Buffer Overflow");
      strcpy (hscErrors[3],  "No new Alias Given");
      strcpy (hscErrors[4],  "Alias too long");
      strcpy (hscErrors[5],  "Invalid Field Parameter");
      strcpy (hscErrors[6],  "Value Out of Range");
      strcpy (hscErrors[7],  "Parameter is read-only");
      strcpy (hscErrors[8],  "Invalid/Missing Argument");
      strcpy (hscErrors[9],  "No Movement Required");
      strcpy (hscErrors[10], "Uncalibrated: no motion allowed");
      strcpy (hscErrors[11], "Motion out of range");
      strcpy (hscErrors[12], "Invalid/missing direction character");
      strcpy (hscErrors[13], "Invalid Motor Specified");

      /*
       * keep track of last known good value
       * to impose motor limits
       */
      t_old = t;	b_old = b;
      l_old = l;	r_old = r;
      h0_old = h0;	width_old = width;
      v0_old = v0;	height_old = height;

      /* controller default settings */
      hOrigin = vOrigin = 400;  		/* origin position */
      hOuterLimit = vOuterLimit = 4400;		/* outer limit (inner limit always 0) */

      PVPUT (enable,       1);
      /* PVPUT (s_tmot,    250); */
      PVPUT (s_tmot,    .250); /* seconds (generic serial used milliseconds) */

    } state init
  }

  /* 
   */
  state disable { 
    when (enable) { 
      init_delay = 1.0;
      PVPUT (init, 1);
    } state init
  }

  /*
   * _any_ communications errors will land us here
   */
  state comm_error {
    when (init || !s_sevr || delay(ERROR_RECONNECT_INTERVAL)) { 
      DEBUG_PRINT2 (20, "comm_error: init=%d  s_sevr=%d", init, s_sevr)
      init_delay = 1.0;
      s_sevr = 0;  /* try this, just to see if communications are working now */
      PVPUT (init, 1);
    } state init
  }

/*  */
  state init {
    when (!enable) {} state disable
    when ( efTest (hID_mon) || efTest (vID_mon) ) {
      /*
         test was:
	 	init && (efTest (hID_mon) || efTest (vID_mon))
	  Why was this test for "init" needed?
       */
      /*
       * configure the serial port parameters
       */
      /*
       * users have requested some mention of the associated serial port name
       * reset it here in case some curious user changed it
       */
      strcpy (port, macValueGet("S"));
      pvPut (port);
      /*
       * make a quick check after user has changed an ID
       */
      init_delay = 0;
      init = 1;
      efClear (hID_mon);
      efClear (vID_mon);
      efClear (hOrient_mon);
      efClear (vOrient_mon);
    } state init
    when (init && delay(init_delay)) { 
      /*
       * verify that the ID strings look OK
       */
      error = 0;
      if (!strcmp (hID, vID)) { 
        PVPUTSTR (errMsg, "H & V IDs must be different");
        error++;
      }
      if (sscanf (hID, "XIAHSC-%c-%d%s", &myChar, &numWords, scratch) != 2) { 
        if (sscanf (hID, "%c-%d%s", &myChar, &numWords, scratch) != 2) { 
	  PVPUTSTR (errMsg, "H ID not a valid HSC ID");
          error++;
        }
      }
      if (sscanf (vID, "XIAHSC-%c-%d%s", &myChar, &numWords, scratch) != 2) { 
        if (sscanf (vID, "%c-%d%s", &myChar, &numWords, scratch) != 2) { 
          PVPUTSTR (errMsg, "V ID not a valid HSC ID");
          error++;
        }
      }
      if (!strlen (hID)) { 
        PVPUTSTR (errMsg, "define H ID string");
        error++;
      }
      if (!strlen (vID)) { 
        PVPUTSTR (errMsg, "define V ID string");
        error++;
      }
      /*
       * could also try to use each ID string
       */
      if (error) { 
        PVPUT (error, ERROR_BAD_ID);
      } else { 
        PVPUT (init, 0);
        PVPUTSTR (errMsg, "no error");
      }
      pvPut (error);
      efClear (hID_mon); 
      efClear (vID_mon); 
      /* recheck every 30 seconds for appropriate ID strings */
      init_delay = 30;
    } state init
    when (!init && delay(0.1)) {   /* start slit initialization */
      PVPUT(hBusy,   MOTOR_IDLE);
      PVPUT(vBusy,   MOTOR_IDLE);
      PVPUT(error,   NO_ERROR);
      /*
       * serial port configuration
       */
      /* S_PORT_CONFIG(BAUD_9600, DBIT_7, SBIT_2, PRTY_EVEN, FCTL_NONE); */
      S_PORT_CONFIG(BAUD_9600, DBIT_8, SBIT_1, PRTY_NONE, FCTL_NONE);
      /*
       * asyn record (was generic serial record) configuration
       */
      /* PVPUT (s_idel, LINE_FEED); */
      PVPUTSTR (s_ieos, LINE_FEED);	/* input delimiter is <LF> */
      PVPUT (s_ifmt,  0);	/* ASCII input format */
      PVPUT (s_nrrd,  0);	/* number of characters to read */
      /* PVPUT (s_odel, CARRIAGE_RETURN); */
      PVPUTSTR (s_oeos, CARRIAGE_RETURN);	/* output delim is <CR> */
      PVPUT (s_ofmt, 0);	/* ASCII output format */
      PVPUT (s_tmod,  0);	/* write/read transfer mode */
      PVPUT (s_scan,  SCAN_PASSIVE);	/* serial port is idle */
      /* PVPUT (s_tmot, 250); */
      PVPUT (s_tmot, .250);	/* timeout in 1/4 second */

      efClear (s_ainp_mon);	/* watch serial record process */

      efClear (wait_H_input);   /* flag to watch for H axis input */
      efClear (wait_V_input);   /* flag to watch for V axis input */

      efClear (move_h);		/* flag H axis to be moved */
      efClear (move_v);		/* flag V axis to be moved */
      efClear (new_H_RB);	/* new readback data available */
      efClear (new_V_RB);	/* new readback data available */

      PVPUT (stop, STOP_IDLE);	/* presume the motors are stopped */
      epicsThreadSleep(.02);	/* allow the command to process */
      efClear (stop_mon);

      efClear (t_mon);  efClear (l_mon);  
      efClear (b_mon);  efClear (r_mon);
      efClear (h0_mon); efClear (width_mon);
      efClear (v0_mon); efClear (height_mon);
      PVPUT (calibrate, CALIBRATE_DONE);	/* don't reset pos. to zero */

      efClear (hID_mon);
      efClear (vID_mon);
      efClear (hOrient_mon);
      efClear (vOrient_mon);

      /*
       * Now, everything is initialized
       * stop all the motors and ask where are we
       */
      WRITE_HSC (TMOD_WRITE, "ALL", HSC1_KILL_MOVEMENT);
      epicsThreadSleep(.1);	/* wait for action to complete */

      /* should check the response for (in order):
         	timeout
         	proper controller ID
         	proper response length
         What to do when each of these fails?
       */

      error = 0;
      WRITE_READ_HSC(hID, HSC1_OUTER_MOTION_LIMIT);
      if (sscanf (hsc_response, "%*s %*s %d", &hOuterLimit) != 1) {
 	error++;
	DEBUG_PRINT(5, "Could not read horizontal outer limit");
      }
      WRITE_READ_HSC(hID, HSC1_ORIGIN_POSITION);
      if (sscanf (hsc_response, "%*s %*s %d", &hOrigin) != 1) {
 	error++;
	DEBUG_PRINT(5, "Could not read horizontal origin position");
      }

      if (!error) {
 	pos_ag = RAW_TO_DIAL(0,           hOrigin);
 	pos_bg = RAW_TO_DIAL(hOuterLimit, hOrigin);
 	PVPUT (l_lo, pos_ag);  PVPUT (l_hi, pos_bg);
 	PVPUT (r_lo, pos_ag);  PVPUT (r_hi, pos_bg);
	  /*
	   * Calculate the absolute limits of gap and center.
	   * (Note the actual limits are also dependent on
	   *  current values of l and r!  However, these
	   *  situations will be trapped by the limits
	   *  on the corresponding axes.)
	   */
	MAX(l_lo, r_lo, dScratch);
	MAX(dScratch, 0.0, width_lo);  pvPut (width_lo);
	PVPUT (width_hi, l_hi + r_hi);
	/*
	MIN( l_hi, -r_lo, h0_hi);   pvPut (h0_hi);
	MAX( l_lo, -r_hi, h0_lo);   pvPut (h0_lo);
	*/
	PVPUT (h0_hi, (r_hi - l_lo)/2);
	PVPUT (h0_lo, (r_lo - l_hi)/2);
      }

      error = 0;
      WRITE_READ_HSC(vID, HSC1_OUTER_MOTION_LIMIT);
      if (sscanf (hsc_response, "%*s %*s %d", &vOuterLimit)!= 1) {
        error++;
	DEBUG_PRINT(5, "Could not read vertical outer limit");
      }
      WRITE_READ_HSC(vID, HSC1_ORIGIN_POSITION);
      if (sscanf (hsc_response, "%*s %*s %d", &vOrigin)!= 1) {
        error++;
	DEBUG_PRINT(5, "Could not read vertical origin position");
      }
      if (!error) {
        pos_ag = RAW_TO_DIAL(0,           vOrigin);
        pos_bg = RAW_TO_DIAL(vOuterLimit, vOrigin);
        PVPUT (t_lo, pos_ag);    PVPUT (t_hi, pos_bg);
        PVPUT (b_lo, pos_ag);    PVPUT (b_hi, pos_bg);
	  /*
	   * Calculate the absolute limits of gap and center.
	   * (Note the actual limits are also dependent on 
	   *  current values of t and b!  However, these
	   *  situations will be trapped by the limits
	   *  on the corresponding axes.)
	   */
	MAX(t_lo, b_lo, dScratch);
	MAX(dScratch, 0.0, height_lo);  pvPut (height_lo);
	PVPUT (height_hi, t_hi + b_hi);
	/*
	MIN( t_hi, -b_lo, v0_hi);   pvPut (v0_hi);
	MAX( t_lo, -b_hi, v0_lo);   pvPut (v0_lo);
	*/
	PVPUT (v0_hi, (t_hi - b_lo)/2);
	PVPUT (v0_lo, (t_lo - b_hi)/2);
      }
      PVPUT(locate, 1);  /* update the positions from the controller */

    } state idle
  }

/*  */
  state idle {
    /* user can disable this software */
    when (!enable) {} state disable
    when (s_sevr)  {
      PVPUT(error, ERROR_COMM_ERROR);
      %%sprintf (pVar->scratch, "communications error: %d", pVar->s_sevr);
      PVPUTSTR (errMsg, scratch);
    } state comm_error
    when (efTest (hID_mon)     || efTest (vID_mon))     {} state init
    when (efTest (hOrient_mon) || efTest (vOrient_mon)) {} state init

    /* user can re-initialize this program */
    when (init) { init_delay = 1; PVPUT (init, 1); } state init

    when (efTest (stop_mon) && stop) { 
      /*
       * user can stop the motors
       */
      /* What if the record is still processing? 
         Not likely in this version.*/
      WRITE_HSC(TMOD_WRITE, "ALL", HSC1_KILL_MOVEMENT)  
      epicsThreadSleep(.1); /* wait for it to happen */
      efClear (stop_mon);
      PVPUT(locate, 1);  /* update the positions from the controller */
    } state idle

    /*
     * target value has changed
     */
    when ( efTest (l_mon)     || efTest (r_mon) 
      ||   efTest (t_mon)     || efTest (b_mon) 
      ||   efTest (h0_mon)    || efTest (height_mon) 
      ||   efTest (v0_mon)    || efTest (width_mon)   ) {
    } state premove

    /*
     * send a MOVE command to HSC
     *  wait just a skosh before processing
     */
    when ( (efTest (start_H_move) || efTest (start_V_move)) 
    &&     delay (MOVE_HESITATION)) {
      if (efTest (start_H_move)) { 
        if (hBusy) { 		/* allow an active move to be interrupted */
          WRITE_HSC(TMOD_WRITE, hID, HSC1_KILL_MOVEMENT); /* stop this axis only */
          epicsThreadSleep(.1);	/* wait for it to happen */
        } else {
          PVPUT (hBusy, MOTOR_BUSY);	/* tell the world we are busy */
        }
        if (hOrient == ORIENT_LEFT_RIGHT) { 
          pos_ag = l;  pos_bg = r;
        } else { 
          pos_ag = r;  pos_bg = l;
        }
        strcpy (word1, hID);		/* horizontal axis */
        efSet (wait_H_input);		/* could enable a timeout on the move */
        efClear (start_H_move);		/* OK, we told it to move now */
	/*
	 * convert to raw (motor) coordinates
	 */
        pos_a = DIAL_TO_RAW (pos_ag, hOrigin);
        pos_b = DIAL_TO_RAW (pos_bg, hOrigin);
      }
      if (efTest (start_V_move)) { 
        if (vBusy) {  		/* allow an active move to be interrupted */
          WRITE_HSC(TMOD_WRITE, vID, HSC1_KILL_MOVEMENT); /* stop this axis only */
          epicsThreadSleep(.1); /* wait for it to happen */
        } else {
          PVPUT (vBusy, MOTOR_BUSY);	/* tell the world we are busy */
        }
        if (vOrient == ORIENT_TOP_BOTTOM) { 
          pos_ag = t;  pos_bg = b;
        } else { 
          pos_ag = b;  pos_bg = t;
        }
        strcpy (word1, vID);		/* vertical axis */
        efSet (wait_V_input);		/* could enable a timeout on the move */
        efClear (start_V_move);
	/*
	 * convert to raw (motor) coordinates
	 */
        pos_a = DIAL_TO_RAW (pos_ag, vOrigin);
        pos_b = DIAL_TO_RAW (pos_bg, vOrigin);
      }
      sprintf (scratch, "M %d %d", pos_a, pos_b);
      WRITE_HSC(TMOD_WRITE, word1, scratch);
      epicsThreadSleep(.1); /* wait for move to begin */
      PVPUT (locate, 1);		/* expect a readback */
    } state idle

    when (locate) {
      /*
       * user can request device to resend coordinates
       */
      /***
       *** additional feature requested by users:
       ***  reset the name of the serial port here
       ***  in case some curious user changed it
       ***/
      strcpy (port, macValueGet("S"));
      pvPut (port);
      PVPUT (locate, 0);
    } state get_readback_positions

    when ( calibrate ) {
      /*
       * user requested a device calibration
       * ASSUME the user has closed the slits down
       * all the way.
       */
      WRITE_HSC(TMOD_WRITE, "ALL", HSC1_CALIBRATE_IMMEDIATE)  
      epicsThreadSleep(.1);  /* wait for it to happen */
      PVPUT (calibrate, CALIBRATE_DONE);	/* OK, we're done here */
      PVPUT(locate, 1);  /* update the positions from the controller */
    } state idle

    when (efTest (new_H_RB)) { 
      efClear (new_H_RB);  /* new H axis readback data is available */
      /*
       * update the target positions
       * but only if we have not interrupted a move-in-progress
       */
      if (!hBusy) {
        if (l     != l_RB)     PVPUT (l, l_RB);
        if (r     != r_RB)     PVPUT (r, r_RB);
        if (h0    != h0_RB)    PVPUT (h0, h0_RB);
        if (width != width_RB) PVPUT (width, width_RB);
        epicsThreadSleep(.02);
        efClear (l_mon);    efClear (r_mon);
        efClear (h0_mon);   efClear (width_mon);
        efClear (wait_H_input);
      }
    } state idle

    when (efTest (new_V_RB)) { 
      efClear (new_V_RB);  /* new V axis readback data is available */
      /*
       * update the target positions
       * but only if we have not interrupted a move-in-progress
       */
      if (!vBusy) {
        if (t      != t_RB)      PVPUT (t, t_RB);
        if (b      != b_RB)      PVPUT (b, b_RB);
        if (v0     != v0_RB)     PVPUT (v0, v0_RB);
        if (height != height_RB) PVPUT (height, height_RB);
        epicsThreadSleep(.02);
        efClear (t_mon);    efClear (b_mon);
        efClear (v0_mon);   efClear (height_mon);
        efClear (wait_V_input);
      }
    } state idle

    when ((hBusy || vBusy) && delay (MOVING_POLL_INTERVAL)) { 
      PVPUT (locate, 1);		/* signal for a position update */
    } state idle
    when (delay (IDLE_POLL_INTERVAL)) { 
      PVPUT (locate, 1);		/* signal for a position update */
    } state idle

  }

/*  */
  state get_readback_positions {
    when (!enable) {} state disable
    when ( ) {

      /*
       * horizontal axis
       */
      WRITE_READ_HSC(hID, HSC1_POSITION_INQUIRY);
      h_valid = validateResponse(hsc_command, hsc_response);
      DEBUG_PRINT1(2, "validateResponse (H): %i", h_valid);
      if (h_valid) {
      	if (sscanf (hsc_response, "%*s %*s %d %d", &pos_a, &pos_b) == 2) {
      	  pos_ag = RAW_TO_DIAL(pos_a, hOrigin);
      	  pos_bg = RAW_TO_DIAL(pos_b, hOrigin);
      	  if (hOrient == ORIENT_LEFT_RIGHT) {
      	    if (pos_ag != l_RB) { PVPUT (l_RB, pos_ag) };
      	    if (pos_bg != r_RB) { PVPUT (r_RB, pos_bg) };
      	  } else {
      	    if (pos_ag != r_RB) { PVPUT (r_RB, pos_ag) };
      	    if (pos_bg != l_RB) { PVPUT (l_RB, pos_bg) };
      	  }
	  if (hBusy) PVPUT (hBusy, MOTOR_IDLE);
      	  /*
	   * also, update width and center readbacks
	   */
      	  PVPUT (width_RB, l_RB+r_RB);
      	  PVPUT (h0_RB, (r_RB-l_RB)/2);
      	  efSet (new_H_RB);	  /* new readback data available */
      	  /*
      	   * keep track of last known good values
      	   * to impose motor limits
      	   */
      	  l_old = l_RB;   r_old = r_RB;
      	  h0_old = h0_RB; width_old = width_RB;
      	} else
      	  if (sscanf (hsc_response, "%*s %*s %s", scratch) == 1) {
      	    DEBUG_PRINT3(5, "axis=%s, command=<%s>, response=%s", "H", hsc_command, hsc_response);
	    if (!strcmp (scratch, "BUSY;")) {
	      if (!hBusy) PVPUT (hBusy, MOTOR_BUSY);
	    } else {
      	      DEBUG_PRINT1(20, "could not understand %s", hsc_response);
	    }
	  }
      } else {
        DEBUG_PRINT1(2, "incorrect response:  %s", hsc_response);
      }

      /*
       * vertical axis
       */
      WRITE_READ_HSC(vID, HSC1_POSITION_INQUIRY);
      v_valid = validateResponse(hsc_command, hsc_response);
      DEBUG_PRINT1(2, "validateResponse (V): %i", v_valid);
      if (v_valid) {      
      	if (sscanf (hsc_response, "%*s %*s %d %d", &pos_a, &pos_b) == 2) {
      	  pos_ag = RAW_TO_DIAL(pos_a, vOrigin);
      	  pos_bg = RAW_TO_DIAL(pos_b, vOrigin);
      	  if (vOrient == ORIENT_TOP_BOTTOM) {
      	    if (pos_ag != t_RB) { PVPUT (t_RB, pos_ag) };
      	    if (pos_bg != b_RB) { PVPUT (b_RB, pos_bg) };
      	  } else {
      	    if (pos_ag != b_RB) { PVPUT (b_RB, pos_ag) };
      	    if (pos_bg != t_RB) { PVPUT (t_RB, pos_bg) };
      	  }
	  if (vBusy) PVPUT (vBusy, MOTOR_IDLE);
      	  /*
	   * also, update height and center readbacks
	   */
      	  PVPUT (height_RB, t_RB+b_RB);
      	  PVPUT (v0_RB, (t_RB-b_RB)/2);
      	  efSet (new_V_RB);	  /* new readback data available */
      	  /*
      	   * keep track of last known good values
      	   * to impose motor limits
      	   */
      	  t_old = t_RB;   b_old = b_RB;
      	  v0_old = v0_RB; height_old = height_RB;
      	} else
      	  if (sscanf (hsc_response, "%*s %*s %s", scratch) == 1) {
      	    DEBUG_PRINT3(5, "axis=%s, command=<%s>, response=%s", "V", hsc_command, hsc_response);
	    if (!strcmp (scratch, "BUSY;")) {
	      if (!vBusy) PVPUT (vBusy, MOTOR_BUSY);
	    } else {
      	      DEBUG_PRINT1(20, "could not understand %s", hsc_response);
	    }
	  }
      } else {
          DEBUG_PRINT1(2, "incorrect response:  %s", hsc_response);
      }

    } state idle
  }

/*  */
  state premove { 
    /*
     * ignore any monitors which do not cause a movement
     */
    when (efTest (l_mon) && (l == l_old)) { efClear (l_mon); } state idle
    when (efTest (r_mon) && (r == r_old)) { efClear (r_mon); } state idle
    when (efTest (t_mon) && (t == t_old)) { efClear (t_mon); } state idle
    when (efTest (b_mon) && (b == b_old)) { efClear (b_mon); } state idle
    when (efTest (h0_mon) && (h0 == h0_old)) { efClear (h0_mon); } state idle
    when (efTest (v0_mon) && (v0 == v0_old)) { efClear (v0_mon); } state idle
    when (efTest (width_mon) && (width == width_old)) { 
       efClear (width_mon); } state idle
    when (efTest (height_mon) && (height == height_old)) { 
       efClear (height_mon); } state idle
    when () { 
      /*
       * horizontal axis
       */
      if (efTest (l_mon) || efTest (r_mon) 
      ||  efTest (h0_mon) || efTest (width_mon)) { 
        /* find out which coordinate scheme changed, calc the other */
        if (efTest (l_mon) || efTest (r_mon)) {
          width = r + l;
          h0 = (r - l) / 2;
        } else {
          l = width / 2 - h0;
          r = width / 2 + h0;
        }
        /* check the limits */
        error = 0;
        if (!LIMIT_TEST (l_lo, l, l_hi))    { l = l_old; error++; }
        if (!LIMIT_TEST (r_lo, r, r_hi))    { r = r_old; error++; }
        if (!LIMIT_TEST (h0_lo, h0, h0_hi)) { h0 = h0_old; error++; }
        if (!LIMIT_TEST (width_lo, width, width_hi)) { 
           width = width_old; error++; }
        if (error) { 
          /* reset values if there was an error */
          if (efTest (l_mon))     pvPut (l);
          if (efTest (r_mon))     pvPut (r);
          if (efTest (h0_mon))    pvPut (h0);
          if (efTest (width_mon)) pvPut (width);
          error = ERROR_SOFT_LIMITS;
          PVPUTSTR (errMsg, "H soft limits exceeded");
        } else { 
          PVPUTSTR (errMsg, "no error");
          /* update the new coordinates */
          efSet (start_H_move);
          if (efTest (l_mon) || efTest (r_mon)) {
            pvPut (h0);  pvPut (width);
          } else {
            pvPut (l);  pvPut (r);
          }
        }
        epicsThreadSleep(.02);
        efClear(l_mon);   efClear(r_mon);
        efClear(h0_mon);  efClear(width_mon);
        pvPut (error);
      }
      /*
       * vertical axis
       */
      if (efTest (t_mon) || efTest (b_mon) 
      ||  efTest (v0_mon) || efTest (height_mon)) { 
        /* find out which coordinate scheme changed, calc the other */
        if (efTest (t_mon) || efTest (b_mon)) {
          height = t + b;
          v0 = (t - b) / 2;
        } else {
          t = height / 2 + v0;
          b = height / 2 - v0;
        }
        /* check the limits */
        error = 0;
        if (!LIMIT_TEST (t_lo, t, t_hi))    { t = t_old; error++; }
        if (!LIMIT_TEST (b_lo, b, b_hi))    { b = b_old; error++; }
        if (!LIMIT_TEST (v0_lo, v0, v0_hi)) { v0 = v0_old; error++; }
        if (!LIMIT_TEST (height_lo, height, height_hi)) { 
           height = height_old; error++; }
        if (error) { 
          /* reset values if there was an error */
          if (efTest (t_mon))      pvPut (t);
          if (efTest (b_mon))      pvPut (b);
          if (efTest (v0_mon))     pvPut (v0);
          if (efTest (height_mon)) pvPut (height);
          error = ERROR_SOFT_LIMITS;
          PVPUTSTR (errMsg, "V soft limits exceeded");
        } else { 
          /* update the new coordinates */
          efSet (start_V_move);
          if (efTest (t_mon) || efTest (b_mon)) {
            pvPut (v0);  pvPut (height);
          } else {
            pvPut (t);  pvPut (b);
          }
        }
        epicsThreadSleep(.02);
        efClear(t_mon);   efClear(b_mon);
        efClear(v0_mon);  efClear(height_mon);
        pvPut (error);
      }
    } state idle
  }

}

%{

int validateResponse(char *out, char *in)
{
	int return_val;
	/* start with second char, since 1st isn't important */
	int k=1;

	if (strlen(in) == 0)
	{
		return_val = 0;
	}
	else
	{
		while (out[k] != ' ')
		{
			if (out[k] != in[k])
			{
				return_val = 0;
				break;
			}
			else
			{
				return_val = 1;
				k += 1;
			}
		}
	}

	return return_val;
}

}%
