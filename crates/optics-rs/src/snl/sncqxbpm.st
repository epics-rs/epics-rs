program sncqxbpm ("name=qxbpm, P=iad:xbpm:, S=lax:serialOI:a4")

option +d;  /* turn ON run-time debugging messages */

#include "seqPVmacros.h"

/*
 * usage:
 seq sncqxbpm, "P=iad:xbpm:, S=lax:serialOI:a4"

 * to kill the running sequence program:
 seqStop "sncqxbpm"

       WISH LIST for qxbpm
       ====================
       * convert the support to "device support"
          per Eric Norum's instructions
       * 2007-03-01,PRJ: raw signals now are unsigned longs
           but there is no such record available in EPICS.
	   When a diode channel overflows (at 32 bits),
	   EPICS truncates this to 2^31 in the longout record.
	   Is this really a problem?  
	   Leave it alone for now but note it.

 */

/*------------------------------------------------
  ------------------------------------------------   macros
  ------------------------------------------------ */

#define MIN(A,B,C) {if ((A)<(B)) {C=(A);} else {C=(B);}}
#define MAX(A,B,C) {if ((A)>(B)) {C=(A);} else {C=(B);}}
#define taskDelay(A) {epicsThreadSleep(A/60.);}

#define SCAN_PASSIVE    0
#define SCAN_RAPID      7
#define TMOD_WRITE_READ 0
#define TMOD_WRITE      1
#define TMOD_READ       2
#define LINE_FEED       "\n"
#define CARRIAGE_RETURN "\r"
#define RECORD_PROCESS  1
#define STRING_ROOM     39   /* a string's 40 bytes less the terminator */

   /* try to reconnect every 30 seconds */
#define ERROR_RECONNECT_INTERVAL   (0.5 * 60.0)

#define CHAN_A 1
#define CHAN_B 2
#define CHAN_C 3
#define CHAN_D 4
#define ARR_INDEX(_G_,_C_) ((_G_)*4 + (_C_) - 1)
  /* circumvent cygwin problem when RAW < offset */
  /* C defect: subtracted the int offset from the unsigned raw count in
   * unsigned arithmetic, so raw < offset wrapped to about 4.29e9. */

#define DIODE_CURRENT(_RAW_,_G_,_C_)                  \
  gainTrim[ARR_INDEX(_G_,_C_)]*( (double) _RAW_ - offset[ARR_INDEX(_G_,_C_)] )

#define NUM_GAINS         6
#define NUM_CHANNELS      4
  /* C defect: gainStr, gainTrim and offset were indexed with {P}gain
   * unchecked, and a numeric put gives the mbbo any 0..65535.  A use reads
   * gain once into g so a monitor cannot move it past the check. */
#define GAIN_OK(_G_)      ((_G_) >= 0 && (_G_) < NUM_GAINS)
#define NEW_GAIN_DELAY    3
  /* 
   * BASE_SAMPLE_TIME is the measured absolute 
   * fastest time data can be acquired.
   * Measured 0.050003 +/- 0.001082 seconds
   * Use this to set period_s_LOPR
   */
#define BASE_SAMPLE_TIME  0.05

    /*
     * error messages
     */
#define NO_ERROR                 0
#define ERROR_UNKNOWN            1
#define ERROR_COMM_ERROR         2


  /*  */
    /*
     * I/O to the BPM through the serial record
     */

  /* C defect: was 0.25, the seconds, so the loop below counting 1/60 s
   * ticks waited one. */
#define  BPM_TIMEOUT_TICKS 15     /* wait up to 0.25 seconds */
#define  NUM_RETRIES       3

  /* C defect: BPM_COMM copied from s_ainp+1, past the terminator, when
   * the reply was empty. */
  /* C defect: s_ainp_mon was cleared only in init, so after the first reply
   * every command took s_ainp as its reply whether or not one had come. */
#define  BPM_COMM(MODE, FMT, V1, V2, V3) {                              \
  sprintf(bpm_command, FMT, bpm_address, V1, V2, V3);                   \
  sprintf(debug_str, "BPM_COMM: <%s> (%d) <-- <%s>", s_aout, MODE, bpm_command);  \
  DEBUG_PRINT(15, debug_str);						\
  if (s_tmod != MODE) PVPUT(s_tmod,  MODE);   taskDelay (1);            \
  if (MODE == TMOD_WRITE) {                                             \
    if (strcmp(s_aout,  bpm_command)) {                                 \
      PVPUTSTR(s_aout,  bpm_command);  taskDelay (1);                   \
    } else {                                                            \
      PVPUT(s_proc,  1);  taskDelay (1);                                \
    }                                                                   \
  } else {                                                              \
    strcpy (bpm_response, "");                                          \
    for (retry = 0; retry < NUM_RETRIES; retry++) {                     \
      efClear(s_ainp_mon);                                              \
      if (strcmp(s_aout,  bpm_command)) {                               \
        PVPUTSTR(s_aout,  bpm_command);  taskDelay (1);                 \
      } else {                                                          \
        PVPUT(s_proc,  1);  taskDelay (1);                              \
      }                                                                 \
      for (i = 0; i <  BPM_TIMEOUT_TICKS; i++) {                        \
        taskDelay (1);                                                  \
        if (efTest(s_ainp_mon)) break;                                  \
      }                                                                 \
      if (!s_sevr && efTest(s_ainp_mon)) {                              \
        sprintf(debug_str, "BPM_COMM: <%s> ==> <%s>", s_aout,s_ainp);   \
	DEBUG_PRINT(10, debug_str);					\
        if (s_ainp[0]) strcpy (bpm_response, s_ainp+1);                 \
        break;                                                          \
      }                                                                 \
    }                                                                   \
  }                                                                     \
}

#define  VARTYPE_INT    0
#define  VARTYPE_FLOAT  1

#define  BPM_READ(TYPE, BPM_VAR, LOCAL_VAR, PUT_FLAG) {                 \
  BPM_COMM(TMOD_WRITE_READ, BPM_FMT_READ_VARIABLE, BPM_VAR, 0,0);       \
  if (!s_sevr) {                                                        \
    if (TYPE == VARTYPE_FLOAT) {                                        \
      sscanf(bpm_response, "%lg", &LOCAL_VAR);                          \
      sprintf(debug_str, "BPM_READ: <%s> ==> %s = %g <%s>"              \
         bpm_command, BPM_VAR, LOCAL_VAR, bpm_response);                \
      DEBUG_PRINT(10, debug_str);					\
    } else {                                                            \
      sscanf(bpm_response, "%d", &LOCAL_VAR);                           \
      sprintf(debug_str, "BPM_READ: <%s> ==> %s = %d <%s>",             \
         bpm_command, BPM_VAR, LOCAL_VAR, bpm_response);                \
      DEBUG_PRINT(10, debug_str);					\
    }                                                                   \
    if (PUT_FLAG) pvPut (LOCAL_VAR);                                    \
  }                                                                     \
}

/*
 * macros to command the Oxford 4-channel X-ray Beam Position Monitor
 */
#define  BPM_FMT_RESET             "*RST%d"
#define  BPM_FMT_SET_RANGE         ":CONF%d:CURR:RANG %d"
#define  BPM_FMT_READ_RANGE        ":CONF%d:CURR:RANG?"
#define  BPM_FMT_INPUT_AVG         ":CONF%d:AVGCURR %d"
#define  BPM_FMT_INPUT_WINDOW      ":CONF%d:WDWCURR %d"
#define  BPM_FMT_INPUT_SINGLE      ":CONF%d:SINGLE"
#define  BPM_FMT_READ_CURRENT      ":READ%d:CURR%d?"
#define  BPM_FMT_READ_ALL_CURRENT  ":READ%d:CURRALL?"
#define  BPM_FMT_READ_POSITION     ":READ%d:POS%s?"
#define  BPM_FMT_SET_VARIABLE      ":CONF%d:%s %s"
#define  BPM_FMT_READ_VARIABLE     ":CONF%d:%s?"
#define  BPM_FMT_READ_USER_INPUT   ":SENS%d:STAT%d?"
#define  BPM_FMT_READ_USER_OUTPUT  ":SOUR%d:STAT%d?"
#define  BPM_FMT_SET_USER_OUTPUT   ":SOUR%d:STAT%d %d"

#define  BPM_SIGNAL_MODE_SINGLE        0
#define  BPM_SIGNAL_MODE_AVERAGE       1
#define  BPM_SIGNAL_MODE_WINDOW        2

  /*  */
/*------------------------------------------------
  ------------------------------------------------   generic serial record
  ------------------------------------------------ */

PV (string, s_ainp, "{S}.AINP", EvFlag);
PV (string, s_aout, "{S}.AOUT", NoMon);
PV (short,  s_baud, "{S}.BAUD", NoMon);
PV (short,  s_dbit, "{S}.DBIT", NoMon);
PV (short,  s_fctl, "{S}.FCTL", NoMon);
PV (string, s_idel, "{S}.IEOS", NoMon);
PV (short,  s_ifmt, "{S}.IFMT", NoMon);
PV (short,  s_nord, "{S}.NORD", Monitor);
PV (short,  s_nrrd, "{S}.NRRD", NoMon);
PV (string, s_odel, "{S}.OEOS", NoMon);
PV (short,  s_ofmt, "{S}.OFMT", NoMon);
PV (short,  s_proc, "{S}.PROC", Monitor);
PV (short,  s_prty, "{S}.PRTY", NoMon);
PV (short,  s_sbit, "{S}.SBIT", NoMon);
PV (short,  s_scan, "{S}.SCAN", NoMon);
PV (int,    s_sevr, "{S}.SEVR", Monitor);
PV (short,  s_tmod, "{S}.TMOD", NoMon);
PV (double, s_tmot, "{S}.TMOT", Monitor);


  /*  */
/*------------------------------------------------
  ------------------------------------------------   oxford_bpm database
  ------------------------------------------------ */

PV (string,   port,            "{P}port",            NoMon);
PV (int,      bpm_address,     "{P}address",         Monitor);
PV (short,    init,            "{P}init",            Monitor);
PV (short,    enable,          "{P}enable",          Monitor);
PV (int,      error,           "{P}error",           NoMon);
PV (string,   errMsg,          "{P}errMsg",          NoMon);
PV (int,      debug_flag,      "{P}debug",           Monitor);
PV (double,   pos_x,           "{P}pos:x",           NoMon);
PV (double,   pos_y,           "{P}pos:y",           NoMon);
PV (double,   current_total,   "{P}current:total",   NoMon);
PV (double,   current_a,       "{P}current:a",       NoMon);
PV (double,   current_b,       "{P}current:b",       NoMon);
PV (double,   current_c,       "{P}current:c",       NoMon);
PV (double,   current_d,       "{P}current:d",       NoMon);
/*
PV (unsigned long,     raw_a,	       "{P}current:a:raw",   NoMon);
PV (unsigned long,     raw_b,	       "{P}current:b:raw",   NoMon);
PV (unsigned long,     raw_c,	       "{P}current:c:raw",   NoMon);
PV (unsigned long,     raw_d,	       "{P}current:d:raw",   NoMon);
*/
PV (unsigned int,     raw_a,	       "{P}current:a:raw",   NoMon);
PV (unsigned int,     raw_b,	       "{P}current:b:raw",   NoMon);
PV (unsigned int,     raw_c,	       "{P}current:c:raw",   NoMon);
PV (unsigned int,     raw_d,	       "{P}current:d:raw",   NoMon);
/*PV (long,     current_low_raw, "{P}current:low:raw", Monitor);*/
PV (int,     current_low_raw, "{P}current:low:raw", Monitor);
PV (short,    current_low,     "{P}current:low",     NoMon);
PV (short,    current_ok,      "{P}current:ok",      NoMon);
PV (short,    signal_mode,     "{P}mode",            EvFlag);
PV (short,    buflen,          "{P}buflen",          EvFlag);
PV (short,    buflen_lo,       "{P}buflen.LOPR",     Monitor);
PV (short,    buflen_hi,       "{P}buflen.HOPR",     Monitor);
PV (int,      gain,            "{P}gain",            EvFlag);
PV (double,   period_s,        "{P}period",          EvFlag);
PV (double,   period_s_LOPR,   "{P}period.LOPR",     Monitor);
PV (double,   period_s_HOPR,   "{P}period.HOPR",     Monitor);

PV (double,   gx,              "{P}GX",              Monitor);
PV (double,   gy,              "{P}GY",              Monitor);

PV (short,    set_offsets,     "{P}set_offsets",     Monitor);
PV (short,    set_defaults,    "{P}set_defaults",    Monitor);
PV (double,   settling,        "{P}settling",        Monitor);

  /* read the choice values from the mbbo (multiple-choice) PVs */
#define PVA_gainStr { \
  "{P}gain.ZRST",  "{P}gain.ONST",  "{P}gain.TWST",     \
  "{P}gain.THST",  "{P}gain.FRST",  "{P}gain.FVST"      \
}
#define PVA_signal_modeStr { \
  "{P}mode.ZRST",  "{P}mode.ONST",  "{P}mode.TWST"      \
}
PVA (string, gainStr,        6, PVA_gainStr,        Monitor);
PVA (string, signal_modeStr, 3, PVA_signal_modeStr, Monitor);


#define PVA_gainTrim {                                \
  "{P}r1:A1",  "{P}r1:B1",  "{P}r1:C1",  "{P}r1:D1",  \
  "{P}r2:A1",  "{P}r2:B1",  "{P}r2:C1",  "{P}r2:D1",  \
  "{P}r3:A1",  "{P}r3:B1",  "{P}r3:C1",  "{P}r3:D1",  \
  "{P}r4:A1",  "{P}r4:B1",  "{P}r4:C1",  "{P}r4:D1",  \
  "{P}r5:A1",  "{P}r5:B1",  "{P}r5:C1",  "{P}r5:D1",  \
  "{P}r6:A1",  "{P}r6:B1",  "{P}r6:C1",  "{P}r6:D1"   \
}

#define PVA_offset {                                  \
  "{P}r1:A2",  "{P}r1:B2",  "{P}r1:C2",  "{P}r1:D2",  \
  "{P}r2:A2",  "{P}r2:B2",  "{P}r2:C2",  "{P}r2:D2",  \
  "{P}r3:A2",  "{P}r3:B2",  "{P}r3:C2",  "{P}r3:D2",  \
  "{P}r4:A2",  "{P}r4:B2",  "{P}r4:C2",  "{P}r4:D2",  \
  "{P}r5:A2",  "{P}r5:B2",  "{P}r5:C2",  "{P}r5:D2",  \
  "{P}r6:A2",  "{P}r6:B2",  "{P}r6:C2",  "{P}r6:D2"   \
}

PVA (double, gainTrim, 24, PVA_gainTrim, Monitor);
/*PVA (long,   offset,   24, PVA_offset,   Monitor);*/
PVA (int,   offset,   24, PVA_offset,   Monitor);

  /*  */
/*------------------------------------------------
  ------------------------------------------------   local variables
  ------------------------------------------------ */

char	 scratch[256];
int	 i;
string	 bpm_command;
string	 bpm_response;
string	 SNLtaskName;
int	 retry;
double	 updateDelay;
evflag	 started;
int	 oldGain;
int	 newGain;
int	 channel;
double   factor;
/* C defect: debug_str was a string (40 chars), and the sprintfs into it
 * write up to 110 (bpm_command=<%s>  bpm_response=<%s>), past its end. */
char	 debug_str[256];
/* C defect: both state sets, each on its own thread, wrote and printed
 * debug_str; sncqxbpm_tools now formats into tools_str. */
char	 tools_str[256];

%% #include <math.h>
%% #include <string.h>
%% #include <epicsThread.h>
/*
%% static int i;
*/



  /*  */
/*
 * state set:  sncqxbpm
 */
ss sncqxbpm {

  state startup {
    when () {
      efClear(started);
      /* debug_flag = 0;*/
      pvGet (s_ainp);
      pvGet (s_aout);
      PVPUT (init, 1);          /* need to initialize */
      PVPUT (period_s_LOPR, BASE_SAMPLE_TIME);
      /*
       * users have requested some mention of the associated serial port name
       * for the future:  Can use change the serial port via this PV?
       */
      /* C defect: strcpy'd the macros unbounded into these 40-byte
       * strings; their last byte stays the zero they start with. */
      strncpy (SNLtaskName, macValueGet("name"), STRING_ROOM);
      strncpy (port, macValueGet("S"), STRING_ROOM);
      pvPut (port);
      PVPUT (current_low, 1);          /* not sure yet */
      PVPUT (current_ok,  0);          /* not sure yet */
      PVPUT (set_offsets, 0);          /* do not set the dark currents on startup */
      updateDelay = NEW_GAIN_DELAY;
      efSet(started);
    } state idle   /* go to idle first, no need to init if disabled */
  }

  /*  */
  state init {
    when (!enable) {
      DEBUG_PRINT (0, "software disabled");
    } state disable
    when (init)  {
      /*
       * initialization complete
       */
      PVPUT (init, 0);
      /*
       * generic serial record configuration
       */
      PVPUT (s_baud, 7);	     taskDelay(2);  /* 19200 */
      PVPUT (s_dbit, 4);	     taskDelay(2);  /* 8 data bits */
      PVPUT (s_prty, 1);	     taskDelay(2);  /* no parity */
      PVPUT (s_sbit, 1);	     taskDelay(2);  /* 1 stop bit */
      PVPUT (s_fctl, 1);	     taskDelay(2);  /* no flow control */
      PVPUT (s_tmot, 0.250);			    /* timeout in 1/4 second */
      PVPUT (s_ofmt, 0);			    /* ASCIIoutput format */
      PVPUT (s_ifmt, 0);			    /* ASCIIoutput format */
      PVPUTSTR (s_odel, LINE_FEED);  taskDelay(2);  /* output delimiter */
      PVPUTSTR (s_idel, LINE_FEED);  taskDelay(2);  /* input delimiter */
      PVPUT (s_nrrd, 0);			    /* number of characters to read */
      PVPUT (s_scan, SCAN_PASSIVE);
      efClear (s_ainp_mon);			    /* watch serial record process */
      /*
       * RS232 cable configuration 
       *   (This is a good place to write it down)
       *   IOC DB-9	  XBPM DB9
       *   2 (RX)	  3 (TX)
       *   3 (TX)	  2 (RX)
       *   5 (GND)	  5 (GND)
       *   if the IOC is a PC, then add
       *   4 - 6 - 8	  (none)
       */
      /*
       * Oxford BPM configuration
       */
      BPM_COMM(TMOD_WRITE, BPM_FMT_RESET,     0,0,0);
      taskDelay(6);
      efSet(gain_mon);
      efSet(signal_mode_mon);
      efSet(period_s_mon);
      /*
       * Ignore the internal variables stored on the BPM.
       * (GX, GY, A1, A2, ...)
       * Readout of them over RS232 is flaky 
       * and EPICS can provide better support anyway
       */
    } state idle
  } /* state init */

  /*  */
  /*
   * wait for the user to re-enable this software
   * then reinitialize everything
   */
  state disable {
    when (enable) {
      PVPUT (init, 1);
      DEBUG_PRINT (0, "software re-enabled");
    } state idle
  }

  /*
   * _any_ communications errors will land us here
   */
  state comm_error {
    when (init || !s_sevr || delay(ERROR_RECONNECT_INTERVAL)) {
      sprintf (debug_str, "comm_error: init=%d  s_sevr=%d", init, s_sevr);
      DEBUG_PRINT(20, debug_str);
      s_sevr = 0;  /* try this, just to see if communications are working now */
      PVPUT (init, 1);
    } state init
  }

  /*  */
  state idle {
    /* user can disable this software */

    when (!enable) {} state disable

    when (s_sevr)  {
      PVPUT(error, ERROR_COMM_ERROR);
      sprintf (scratch, "communications error: %d", s_sevr);
      PVPUTSTR (errMsg, scratch);
    } state comm_error

    when (init)  {
      DEBUG_PRINT (20, "user re-initialized");
    } state init

    /* amplifier gain was changed */
    when (efTestAndClear(gain_mon))  {
      int g;
      g = gain;
      if (GAIN_OK(g)) {
        sprintf (debug_str, "gain changed to %s (%d)", gainStr[g],g);
        DEBUG_PRINT (5, debug_str);
        BPM_COMM(TMOD_WRITE, BPM_FMT_SET_RANGE, g+1, 0,0);
      } else {
        sprintf (debug_str, "gain %d is out of range", g);
        DEBUG_PRINT (5, debug_str);
      }
      /*
       * allow device to settle in after a gain (range) change
       * before processing currents again
       */
      updateDelay = NEW_GAIN_DELAY; /* time to fill the buffer */
      updateDelay = 0.01; /* ignore for now */
    } state idle

    /* signal mode or average/window buffer length was changed */
    when (efTestAndClear(signal_mode_mon) || efTestAndClear(buflen_mon))  {
      /* check to make sure that buflen is within range */
      if (buflen < buflen_lo) PVPUT(buflen, buflen_lo);
      if (buflen > buflen_hi) PVPUT(buflen, buflen_hi);
      taskDelay (2);
      efClear(buflen_mon);
      if (signal_mode == BPM_SIGNAL_MODE_SINGLE) {
        sprintf (debug_str, "gain changed to %s (%d)",
          signal_modeStr[signal_mode], signal_mode);
        DEBUG_PRINT (5, debug_str);
        BPM_COMM(TMOD_WRITE, BPM_FMT_INPUT_SINGLE, 0,0,0);
      }
      if (signal_mode == BPM_SIGNAL_MODE_AVERAGE) {
        sprintf (debug_str, "gain changed to %s (%d) with buffer length = %d",
          signal_modeStr[signal_mode], signal_mode, buflen);
        DEBUG_PRINT (5, debug_str);
        BPM_COMM(TMOD_WRITE, BPM_FMT_INPUT_AVG, buflen, 0,0);
      }
      if (signal_mode == BPM_SIGNAL_MODE_WINDOW) {
        sprintf (debug_str, "gain changed to %s (%d) with buffer length = %d",
          signal_modeStr[signal_mode], signal_mode, buflen);
        DEBUG_PRINT (5, debug_str);
        BPM_COMM(TMOD_WRITE, BPM_FMT_INPUT_WINDOW, buflen, 0,0);
      }
    } state idle

    when (efTestAndClear(period_s_mon))  {
      if (period_s < period_s_LOPR)  PVPUT(period_s, period_s_LOPR);
      if (period_s > period_s_HOPR)  PVPUT(period_s, period_s_HOPR);
      sprintf (debug_str, "checking update period: %g s", period_s);
      DEBUG_PRINT (5, debug_str);
    } state idle

    /* update the diode current and beam position when idle */
    when (delay(period_s - BASE_SAMPLE_TIME + updateDelay))  {
      int g;
      unsigned int ra, rb, rc, rd;
      BPM_COMM(TMOD_WRITE_READ, BPM_FMT_READ_ALL_CURRENT, 0,0,0);
      sprintf (debug_str, "serial.SEVR: %d", s_sevr);
      DEBUG_PRINT (5, debug_str);
      /* C defect: scanned into raw_a..d unchecked, so a reply short of four
       * counts posted the previous counts (or a mix) as a new reading. */
      if (!s_sevr
          && sscanf(bpm_response, "%u%u%u%u", &ra, &rb, &rc, &rd) == 4) {
        raw_a = ra;  raw_b = rb;  raw_c = rc;  raw_d = rd;
        /* C defect: never wrote NO_ERROR, so error and errMsg kept the
         * comm error after the device answered again. */
        if (error != NO_ERROR) {
          PVPUT (error, NO_ERROR);
          PVPUTSTR (errMsg, "");
        }
	pvPut (raw_a);
	pvPut (raw_b);
	pvPut (raw_c);
	pvPut (raw_d);
        sprintf (debug_str, "bpm_command=<%s>  bpm_response=<%s>", 
          bpm_command, bpm_response);
 	DEBUG_PRINT (12, debug_str);
        /* C defect: printed the unsigned raw_a..d with %d. */
        sprintf (debug_str, "raw: a=%u  b=%u  c=%u  d=%u", 
          raw_a, raw_b, raw_c, raw_d);
 	DEBUG_PRINT (12, debug_str);
        g = gain;
        if (GAIN_OK(g)) {
          PVPUT(current_a, DIODE_CURRENT(raw_a, g, CHAN_A));
          PVPUT(current_b, DIODE_CURRENT(raw_b, g, CHAN_B));
          PVPUT(current_c, DIODE_CURRENT(raw_c, g, CHAN_C));
          PVPUT(current_d, DIODE_CURRENT(raw_d, g, CHAN_D));
        }
        PVPUT(current_total, current_a+current_b+current_c+current_d);
        /*
         * observed that manual is incorrect as to which channels
         *    Discover that
         * X position is (B-D)/(B+D)
         * Y position is (A-C)/(A+C)
         */
        PVPUT(pos_x,     gx*(current_b - current_d)/(current_b + current_d));
        PVPUT(pos_y,     gy*(current_a - current_c)/(current_a + current_c));
        /* C defect: compared the unsigned raw counts with the int
         * current_low_raw in unsigned, so a negative threshold read as about
         * 4.29e9 and every count was low. */
        PVPUT (
          current_low, 
             ((double) raw_a<current_low_raw)
          && ((double) raw_b<current_low_raw)
          && ((double) raw_c<current_low_raw)
          && ((double) raw_d<current_low_raw)
        );
        PVPUT (
          current_ok, 
             ((double) raw_a>=current_low_raw)
          && ((double) raw_b>=current_low_raw)
          && ((double) raw_c>=current_low_raw)
          && ((double) raw_d>=current_low_raw)
        );

      }
    } state idle

  } /* state idle */

}

  /*  */
/*
 * state set:  sncqxbpm_tools
 */
ss sncqxbpm_tools {

  state startup {
    when (efTest(started)) {
    } state idle
  }

  state idle {

    when (set_offsets) {
      int g;
      oldGain = gain;
      taskDelay(60);
      for (newGain = 0; newGain < NUM_GAINS; newGain++) {
 	if (!set_offsets) break;
	PVPUT(gain, newGain);
 	if (!set_offsets) break;
	epicsThreadSleep(settling);
 	if (!set_offsets) break;
 	sprintf (tools_str, "new gain=%d",  gain);
 	DEBUG_PRINT (18, tools_str);
	g = gain;
	if (!GAIN_OK(g)) continue;
	PVPUT(offset[ARR_INDEX(g,1)], raw_a);
	PVPUT(offset[ARR_INDEX(g,2)], raw_b);
	PVPUT(offset[ARR_INDEX(g,3)], raw_c);
	PVPUT(offset[ARR_INDEX(g,4)], raw_d);
	for (channel=1; channel <= NUM_CHANNELS; channel++) {
 	  sprintf (tools_str, 
	     "  offset[%d]=%d", 
	     channel,offset[ARR_INDEX(g,channel)]);
 	  DEBUG_PRINT (19, tools_str);
	}
      }
      PVPUT(gain, oldGain);
      PVPUT(set_offsets, 0);
      taskDelay(2);
      sprintf (tools_str, "restored to previous gain #%d", gain);
      DEBUG_PRINT (18, tools_str);
    } state idle

    when (set_defaults) {
      DEBUG_PRINT (18, "restore default calibrations");
      PVPUT(signal_mode,     2);
      PVPUT(buflen,	     30);
      PVPUT(current_low_raw, 1000);
      PVPUT(period_s,	     0.1);
      PVPUT(gx,	             4.5);
      PVPUT(gy,	             4.5);
      PVPUT(settling,	     2.5);
      for (newGain = 0; newGain < NUM_GAINS; newGain++) {
	if (newGain == 0) {factor =  350e-9;} /* amps full-scale */
	if (newGain == 1) {factor =  700e-9;}
	if (newGain == 2) {factor = 1400e-9;}
	if (newGain == 3) {factor =    7e-6;}
	if (newGain == 4) {factor =   70e-6;}
	if (newGain == 5) {factor =  700e-6;}
	factor /= 10;			      /* 10 VDC full-scale */
	factor /= 1e5;			      /* 100 kHz / VDC */
	for (channel=1; channel <= NUM_CHANNELS; channel++) {
	  PVPUT(gainTrim[ARR_INDEX(newGain,channel)], factor);
	}
      }
      PVPUT(set_defaults, 0);
      taskDelay(2);
    } state idle

  } /* state: idle */

}


