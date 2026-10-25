/*  xia_slit - XIA HSC-1 Sequence Application

 
 Calling seqeunce:
    seq &xia_slit,"name=hsc1,P=pfx:,HSC=hsc1:S,pfx:serial9"
 

 Where:
    P   = Prefix of database and sequencer
    HSC = Hardware controller ID (i.e. hsc1:,hsc2:)
    S   = Asyn record for communication


 Developer notes:
    1) Includes Pete's modification for new ID string format
       (XIA dropped the leading zero)
    2) Wish list for XIAHSC:
        - motor velocity control
        - gear backlash setting
        - motor record interface
        - progress indication (not provided on device)
        - support for INQUIRY command
        - user coordinates
        - user can change which serial port to use
        - motor record interface
    3) VxWorks usage:
        - ld < xia_slit.o
          seq &xia_slit "name=hsc1, S=ito:serial:b0, P=ito:, HSC=hsc1:"
        - type td "hsc1" to kill the running sequence program.
    4) Alias support:
        - In order to support aliases, some controllers need to have
          bit 6 of the control set to TRUE. When the bit is set, the
          module responds with the alias; otherwise it reponds with
          the module ID. See page 11 in the user's manual for a
          description of the control word.


 Source control info:
    Modified by:    dkline
                    2008/07/03 19:19:34
                    1.11

 =============================================================================
 History:
 Author: Pete Jemian / Tim Mooney
 -----------------------------------------------------------------------------
 YYYY.MM.DD P/T - Originally developed by Pete Jemain, then modified by
                  Tim Mooney to make it scannable by incorporating the BUSY
                  record.
 2009.08.11 DMK - Corrected a problem where the horizontal axis BUSY record
                  would never change to done.
                - Incorporated the "seqPVmacros.h" include file. (macros that
                  connect to PVs.)
                - Reformmated source code for clarity and consistency. Replaced
                  all TABs with 4 spaces.
                - Increased FIFO size.
                - Added check if asynOption is supported. If not, there is no
                  need to initialize the serial port parameters (i.e. baud).
                  This may indicate that communications is through a device
                  server of some kind (i.e. Moxa or Cyclades).
                - Added INIT_FIFO() to clear the FIFO pointers before reading
                  data from the HSC.
                - Alias are now supported (see note #4). Supported formats are
                  either XIAHSC-C-NNNN, C-NNNN, or CNNNN.
                - Tested communication using a Moxa NPort-5410 device server.
                - Added the following query commands for both horizontal and
                  vertical slits.The PVs associated with the readbacks are
                  updated as well. Refer to the user's manual (page 10) for
                  additional information regarding the HSC1 memory parameters.
                  - Read outer motion limit.
                  - Read origin position.
                  - Read motor step delay.
                  - Read gear backlash. 
                  - Read control word.
 -----------------------------------------------------------------------------

 */

program xia_slit("name=hsc1,P=pfx:,HSC=hsc1:,S=pfx:serial9")


option +d;  /* Turn on run-time debug messages */
option -c;  /* Begin execution before PV connections complete */
option +r;  /* Make the run-time code reentrant */
option -w;  /* Suppress SNC warnings */


%%#include <stdio.h>
%%#include <math.h>
%%#include <string.h>
%% #include <epicsThread.h>
#include "seqPVmacros.h"


/*
 * Assignments
 */
#define SCAN_PASSIVE        (0)
#define SCAN_RAPID          (7)
#define TMOD_WRITE_READ     (0)
#define TMOD_WRITE          (1)
#define TMOD_READ           (2)
#define IEOS                ("\r\n")    /* was 10 for serial record */
#define OEOS                ("\r")      /* was 13 for serial record */
#define ORIENT_LEFT_RIGHT   (0)
#define ORIENT_RIGHT_LEFT   (1)
#define ORIENT_TOP_BOTTOM   (0)
#define ORIENT_BOTTOM_TOP   (1)
#define MOTOR_IDLE          (0)
#define MOTOR_BUSY          (1)
#define STOP_IDLE           (0)
#define STOP_STOP           (1)
#define CALIBRATE_DONE      (0)
#define CALIBRATE_START     (1)

#define BAUD_9600           (6)     /* was 5 for serial record */
#define DBIT_8              (4)     /* was 3 for serial record */
#define SBIT_1              (1)     /* was 0 for serial record */
#define PRTY_NONE           (1)     /* was 0 for serial record */
#define FCTL_NONE           (1)     /* was 0 for serial record */

#define DONE                (0)
#define BUSY                (1)
#define SUPPORTED           (1)
#define STEPS_PER_mm        (400.0)
#define FIFO_SIZE           40      /* this requires *no* brackets */

#define MOVE_TIMEOUT                (5*60.0)
#define IDLE_POLL_INTERVAL          (30)
#define MOVING_POLL_INTERVAL        (0.2)
#define ERROR_RECONNECT_INTERVAL    (10*60.0)

/*
 * there are 14 error messages from the HSC
 * (numbered in HSC from 0-13, I move them to 1-14)
 * error 0 means no error
 */
#define NO_ERROR            (0)
#define ERROR_SOFT_LIMITS   (15)
#define ERROR_UNKNOWN       (16)
#define ERROR_BAD_ID        (17)
#define ERROR_COMM_ERROR    (18)
#define COMM_ERROR_LOCATES  (3)     /* locates in a row without a reply */
#define TIMEOUT_TICKS       (30)    /*15*/   /* wait up to 0.5 (was 0.25) seconds */

/* HSC memory write commands (WR) */
#define HSC1_WR_OUTER_MOTION_LIMIT  ("W 1 %d")
#define HSC1_WR_ORIGIN_POSITION     ("W 2 %d")
#define HSC1_WR_MOTOR_STEP_DELAY    ("W 5 %d")
#define HSC1_WR_GEAR_BACKLASH       ("W 6 %d")
#define HSC1_WR_CONTROL_WORD        ("W 7 %d")

/* HSC memory read commands (RD) */
#define HSC1_RD_OUTER_MOTION_LIMIT  ("R 1")
#define HSC1_RD_ORIGIN_POSITION     ("R 2")
#define HSC1_RD_MOTOR_A_POSITION    ("R 3")
#define HSC1_RD_MOTOR_B_POSITION    ("R 4")
#define HSC1_RD_MOTOR_STEP_DELAY    ("R 5")
#define HSC1_RD_GEAR_BACKLASH       ("R 6")
#define HSC1_RD_CONTROL_WORD        ("R 7")

/* HSC commands */
#define HSC1_KILL_MOVEMENT          ("K")
#define HSC1_MODULE_INQUERY         ("I")
#define HSC1_POSITION_INQUIRY       ("P")
#define HSC1_CALIBRATE_IMMEDIATE    ("0 I")

/* HSC control/status word (CSW) */
#define HSC1_CSW_PWRLVL             (0x03)      /* Power level 0=lo,1=med,2=hi */
#define HSC1_CSW_LIMITS             (0x04)      /* Limits enabled */
#define HSC1_CSW_BANNER             (0x08)      /* Print intro banner */
#define HSC1_CSW_ECHO               (0x10)      /* Command echo */
#define HSC1_CSW_LOCK               (0x20)      /* Lock buttons */
#define HSC1_CSW_ALIAS              (0x40)      /* Use alias as ID */
#define HSC1_CSW_TEXT               (0x80)      /* Print error text */


/*
 * Asyn record (was "generic serial record")
 */
PV(string,s_ainp,"{S}.AINP",EvFlag);        /* ASCII input */
PV(string,s_tinp,"{S}.TINP",Monitor);       /* Translated input */
PV(string,s_aout,"{S}.AOUT",NoMon);         /* ASCII output */
PV(string,s_ieos,"{S}.IEOS",NoMon);         /* Input EOS */
PV(string,s_oeos,"{S}.OEOS",NoMon);         /* Output EOS */

PV(double,s_tmot,"{S}.TMOT",Monitor);       /* Timeout */

PV(short,s_ofmt,"{S}.OFMT",NoMon);          /* Output EOS format */
PV(short,s_ifmt,"{S}.IFMT",NoMon);          /* Input EOS format */
PV(short,s_port,"{S}.PORT",NoMon);          /* Asyn port name */
PV(short,s_tmod,"{S}.TMOD",NoMon);          /* Transfer mode */
PV(short,s_scan,"{S}.SCAN",NoMon);          /* Scan */
PV(short,s_nord,"{S}.NORD",Monitor);        /* Actual length returned */
PV(short,s_nrrd,"{S}.NRRD",NoMon);          /* Requested return length */

PV(short,s_proc,"{S}.PROC",NoMon);          /* Processing */

PV(int,s_sevr,"{S}.SEVR",Monitor);          /* Severity */
PV(int,s_optioniv,"{S}.OPTIONIV",NoMon);    /* asynOption is valid */

PV(short,s_baud,"{S}.BAUD",NoMon);          /* Baud rate */
PV(short,s_dbit,"{S}.DBIT",NoMon);          /* Data bits */
PV(short,s_sbit,"{S}.SBIT",NoMon);          /* Stop bits */
PV(short,s_prty,"{S}.PRTY",NoMon);          /* Parity bit */
PV(short,s_mctl,"{S}.MCTL",NoMon);          /* Modem control */
PV(short,s_fctl,"{S}.FCTL",NoMon);          /* Flow control */

PV(int,debug_flag,"{P}{HSC}debug",Monitor); /* Debug level */


/*
 * XIAHSC database 
 */
PV(string, port,    "{P}{HSC}port",    NoMon);
PV(string, hID,     "{P}{HSC}hID",     EvFlag);
PV(string, vID,     "{P}{HSC}vID",     EvFlag);
PV(short,  hOrient, "{P}{HSC}hOrient", EvFlag);
PV(short,  vOrient, "{P}{HSC}vOrient", EvFlag);

PV(short,  hBusy,         "{P}{HSC}hBusy",          NoMon);
PV(short,  h_Busy,        "{P}{HSC}h_Busy",         Monitor);
PV(short,  h_BusyOff,     "{P}{HSC}h_BusyOff.PROC", NoMon);
PV(short,  h_BusyDisable, "{P}{HSC}h_BusyDisable",  NoMon);

PV(short,  vBusy,         "{P}{HSC}vBusy",          NoMon);
PV(short,  v_Busy,        "{P}{HSC}v_Busy",         Monitor);
PV(short,  v_BusyOff,     "{P}{HSC}v_BusyOff.PROC", NoMon);
PV(short,  v_BusyDisable, "{P}{HSC}v_BusyDisable",  NoMon);

PV(short,  stop,      "{P}{HSC}stop",   EvFlag);
PV(short,  locate,    "{P}{HSC}locate", Monitor);
PV(short,  init,      "{P}{HSC}init",   Monitor);
PV(short,  enable,    "{P}{HSC}enable", Monitor);
PV(short,  calibrate, "{P}{HSC}calib",  Monitor);

/* HSC1 outer limit */
PV(int,    hOuterLimit,     "{P}{HSC}hOuterLimitLI",    Monitor);
PV(int,    vOuterLimit,     "{P}{HSC}vOuterLimitLI",    Monitor);

/* HSC1 origin position */
PV(int,    hOrigin,         "{P}{HSC}hOriginLI",        Monitor);
PV(int,    vOrigin,         "{P}{HSC}vOriginLI",        Monitor);

/* HSC1 motor step delay */
PV(int,    hStepDelay,      "{P}{HSC}hStepDelayLI",     Monitor);
PV(int,    vStepDelay,      "{P}{HSC}vStepDelayLI",     Monitor);

/* HSC1 gear backlash */
PV(int,    hGearBacklash,   "{P}{HSC}hGearBacklashLI",  Monitor);
PV(int,    vGearBacklash,   "{P}{HSC}vGearBacklashLI",  Monitor);

/* HSC1 status word */
PV(int,    hStPwr,      "{P}{HSC}hStatPwrMI",   Monitor);
PV(int,    hStLmts,     "{P}{HSC}hStatLmtsBI",  Monitor);
PV(int,    hStBan,      "{P}{HSC}hStatBanBI",   Monitor);
PV(int,    hStEcho,     "{P}{HSC}hStatEchoBI",  Monitor);
PV(int,    hStLock,     "{P}{HSC}hStatLockBI",  Monitor);
PV(int,    hStAlias,    "{P}{HSC}hStatAliasBI", Monitor);
PV(int,    hStText,     "{P}{HSC}hStatTextBI",  Monitor);

PV(int,    vStPwr,      "{P}{HSC}vStatPwrMI",   Monitor);
PV(int,    vStLmts,     "{P}{HSC}vStatLmtsBI",  Monitor);
PV(int,    vStBan,      "{P}{HSC}vStatBanBI",   Monitor);
PV(int,    vStEcho,     "{P}{HSC}vStatEchoBI",  Monitor);
PV(int,    vStLock,     "{P}{HSC}vStatLockBI",  Monitor);
PV(int,    vStAlias,    "{P}{HSC}vStatAliasBI", Monitor);
PV(int,    vStText,     "{P}{HSC}vStatTextBI",  Monitor);


/* slit positions
 * Note - we want to be notified when a client writes to these positions,
 * so we define an event associated with them.  Each slit position forward
 * links to a <position>_event record, which posts a monitor every time it
 * processes (MDEL=-1).  But sometimes we will want to write to the positions,
 * and we don't want to get events from these writes, because we would then
 * have to distinguish client events from our write-back events.  The database
 * is constructed so we can write to, e.g., load_l, which will disable l_event,
 * write to l, and then re-enable l_event. 
 */ 
PV(double, t,            "{P}{HSC}t",        Monitor);
PV(double, t_event,      "{P}{HSC}t_event",  EvFlag);
PV(double, t_hi,         "{P}{HSC}t.HOPR",   Monitor);
PV(double, t_lo,         "{P}{HSC}t.LOPR",   Monitor);
PV(double, t_RB,         "{P}{HSC}tRB",      Monitor);
PV(double, load_t,       "{P}{HSC}load_t.B", NoMon);

PV(double, l,            "{P}{HSC}l",        Monitor);
PV(double, l_event,      "{P}{HSC}l_event",  EvFlag);
PV(double, l_hi,         "{P}{HSC}l.HOPR",   Monitor);
PV(double, l_lo,         "{P}{HSC}l.LOPR",   Monitor);
PV(double, l_RB,         "{P}{HSC}lRB",      Monitor);
PV(double, load_l,       "{P}{HSC}load_l.B", NoMon);

PV(double, b,            "{P}{HSC}b",        Monitor);
PV(double, b_event,      "{P}{HSC}b_event",  EvFlag);
PV(double, b_hi,         "{P}{HSC}b.HOPR",   Monitor);
PV(double, b_lo,         "{P}{HSC}b.LOPR",   Monitor);
PV(double, b_RB,         "{P}{HSC}bRB",      Monitor);
PV(double, load_b,       "{P}{HSC}load_b.B", NoMon);

PV(double, r,            "{P}{HSC}r",        Monitor);
PV(double, r_event,      "{P}{HSC}r_event",  EvFlag);
PV(double, r_hi,         "{P}{HSC}r.HOPR",   Monitor);
PV(double, r_lo,         "{P}{HSC}r.LOPR",   Monitor);
PV(double, r_RB,         "{P}{HSC}rRB",      Monitor);
PV(double, load_r,       "{P}{HSC}load_r.B", NoMon);

PV(double, h0,           "{P}{HSC}h0",        Monitor);
PV(double, h0_event,     "{P}{HSC}h0_event",  EvFlag);
PV(double, h0_hi,        "{P}{HSC}h0.HOPR",   Monitor);
PV(double, h0_lo,        "{P}{HSC}h0.LOPR",   Monitor);
PV(double, h0_RB,        "{P}{HSC}h0RB",      Monitor);
PV(double, load_h0,      "{P}{HSC}load_h0.B", NoMon);

PV(double, v0,           "{P}{HSC}v0",        Monitor);
PV(double, v0_event,     "{P}{HSC}v0_event",  EvFlag);
PV(double, v0_hi,        "{P}{HSC}v0.HOPR",   Monitor);
PV(double, v0_lo,        "{P}{HSC}v0.LOPR",   Monitor);
PV(double, v0_RB,        "{P}{HSC}v0RB",      Monitor);
PV(double, load_v0,      "{P}{HSC}load_v0.B", NoMon);

PV(double, width,        "{P}{HSC}width",        Monitor);
PV(double, width_event,  "{P}{HSC}width_event",  EvFlag);
PV(double, width_hi,     "{P}{HSC}width.HOPR",   Monitor);
PV(double, width_lo,     "{P}{HSC}width.LOPR",   Monitor);
PV(double, width_RB,     "{P}{HSC}widthRB",      Monitor);
PV(double, load_width,   "{P}{HSC}load_width.B", NoMon);

PV(double, height,       "{P}{HSC}height",        Monitor);
PV(double, height_event, "{P}{HSC}height_event",  EvFlag);
PV(double, height_hi,    "{P}{HSC}height.HOPR",   Monitor);
PV(double, height_lo,    "{P}{HSC}height.LOPR",   Monitor);
PV(double, height_RB,    "{P}{HSC}heightRB",      Monitor);
PV(double, load_height,  "{P}{HSC}load_height.B", NoMon);

PV(int,    error,  "{P}{HSC}error",  NoMon);
PV(string, errMsg, "{P}{HSC}errMsg", NoMon);


/*  
 * local variables 
 */
char    myChar;
char*   SNLtaskName;
double  dScratch;
/*long    numWords;*/
int    numWords;
string  word1;
string  word2;
string  word3; 
string  word4;
string  word5;
string  word6;
int     hCSW;
int     vCSW;
int     h_isMoving;
int     v_isMoving;
/*long    pos_a;*/
/*long    pos_b;*/
int    pos_a;
int    pos_b;
double  pos_ag;
double  pos_bg;
/*
long    l_raw;
long    r_raw;
long    t_raw;
long    b_raw;
*/
int    l_raw;
int    r_raw;
int    t_raw;
int    b_raw;
string  id;
short   hasID;
string  hscErrors[14];
evflag  start_H_move;
evflag  start_V_move;
evflag  wait_H_input;
evflag  wait_V_input;
evflag  new_H_RB;
evflag  new_V_RB;
evflag  move_h;
evflag  move_v;
double  t_old;
double  l_old;
double  b_old;
double  r_old;
double  width_old;
double  height_old;
double  h0_old;
double  v0_old;
int     i;
int     delta;
int     init_delay;
int     ainp_fifo_write;
int     ainp_fifo_read;
int     ainp_replies;       /* replies put into the FIFO */
int     silent_locates;     /* locates in a row without a reply */
int     hsc_silent_reads;   /* WRITE_READ_HSC reads without a reply */
string  ainp_fifo[FIFO_SIZE];
string  scratch;

/* for single response commands <40 chars */
int     hsc_timeout;
string  hsc_command;
string  hsc_response;


/*
 * Macros
 */
#define DEBUG_PRINT2(DEBUG_LEVEL,FMT,V1,V2)     \
    if (debug_flag >= DEBUG_LEVEL)              \
    {                                           \
        printf("(%s,%d): ",__FILE__,__LINE__);  \
        printf(FMT,V1,V2);                      \
        printf("\n");                           \
    }
#define DEBUG_PRINT3(DEBUG_LEVEL,FMT,V1,V2,V3)  \
    if (debug_flag >= DEBUG_LEVEL)              \
    {                                           \
        printf("(%s,%d): ",__FILE__,__LINE__);  \
        printf(FMT,V1,V2,V3);                   \
        printf("\n");                           \
    }
#define WRITE_HSC(MODE, HSC_ID,HSC_COMMAND)             \
{                                                       \
    sprintf(hsc_command, "!%s %s",HSC_ID,HSC_COMMAND);  \
    DEBUG_PRINT1(10,"WRITE_HSC: <%s>", hsc_command);    \
    PVPUT(s_scan,  SCAN_PASSIVE);                       \
    PVPUT(s_tmod,  MODE);                               \
    PVPUTSTR(s_ieos,  IEOS);                            \
    PVPUTSTR(s_oeos,  OEOS);                            \
    PVPUTSTR(s_aout,  hsc_command);                     \
}
#define WAIT_HSC_RESPONSE(TICKS)                                                \
{                                                                               \
    for (i = TICKS; i; i--)                                                     \
    {                                                                           \
        if (efTest(s_ainp_mon))                                                 \
        {                                                                       \
            hsc_timeout = 0;    /* Aha! Got a response. Thus, no timeout */     \
            break;              /* input is ready */                            \
        }                                                                       \
        epicsThreadSleep(0.02);                                                 \
    }                                                                           \
    if (hsc_timeout == 1)                                                       \
    {                                                                           \
        strcpy(hsc_response, "");                                               \
        DEBUG_PRINT1(10,"\nWAIT_HSC_RESPONSE: <%s> -> <<< TIMEOUT >>>",s_tinp); \
    }                                                                           \
    else                                                                        \
    {                                                                           \
        strcpy(hsc_response, s_ainp);                                           \
        DEBUG_PRINT3(10,"\nWAIT_HSC_RESPONSE: <%s> -> NORD=%d  TINP=%s",hsc_command,s_nord,s_tinp); \
    }                                                                           \
    /* a read timeout posts an emptied AINP, so hsc_timeout misses it */       \
    if (!hsc_response[0]) ++hsc_silent_reads;                                   \
}
#define WRITE_READ_HSC(HSC_ID, HSC_COMMAND)                             \
{                                                                       \
    efClear(s_ainp_mon);    /* will wait for the record to process */   \
    hsc_timeout = 1;        /* assume a timeout */                      \
    WRITE_HSC(TMOD_WRITE_READ, HSC_ID, HSC_COMMAND);                    \
    WAIT_HSC_RESPONSE(TIMEOUT_TICKS);                                   \
}
#define READ_HSC()                                      \
{                                                       \
    PVPUT(s_tmod,TMOD_READ);                            \
    efClear(s_ainp_mon);                                \
    PVPUT(s_proc, 1);                                   \
    for (i = 0; i <= TIMEOUT_TICKS; i++)                \
    {                                                   \
        if (efTest(s_ainp_mon))                         \
        {                                               \
            DEBUG_PRINT(10,"READ_HSC: got a message."); \
            break;                                      \
        }                                               \
        epicsThreadSleep(0.02);                         \
    }                                                   \
}
#define FIFO_PTR_CHECK(PTR)             \
{                                       \
    if ((PTR) >= FIFO_SIZE) (PTR) = 0;  \
}
#define INPUT_TO_FIFO()                                                     \
{                                                                           \
    if (efTest(s_ainp_mon))                                                 \
    {                                                                       \
        ++ainp_replies;                                                     \
        strcpy(ainp_fifo[ainp_fifo_read++], s_ainp);                        \
        FIFO_PTR_CHECK(ainp_fifo_read);                                     \
        delta = ainp_fifo_read-ainp_fifo_write;                             \
        if (delta < 0) delta += FIFO_SIZE;                                  \
        DEBUG_PRINT2(10,"FIFO_IN: adding msg %d:\n ->'%s'",delta,s_ainp);   \
    }                                                                       \
}
#define FIFO_TO_OUTPUT(OUTPUT)                                          \
{                                                                       \
    delta = ainp_fifo_read-ainp_fifo_write;                             \
    if (delta < 0) delta += FIFO_SIZE;                                  \
    strcpy(OUTPUT, ainp_fifo[ainp_fifo_write++]);                       \
    DEBUG_PRINT2(10,"FIFO_OUT: got message %d:\n ->'%s'",delta,OUTPUT); \
    FIFO_PTR_CHECK(ainp_fifo_write);                                    \
}
/* C defect fixed: C never reported a silent controller (comm_error is
** unreachable), so error stayed 0 and a move stayed busy. The next reply
** clears the error through its "if (error)" path. */
#define REPORT_COMM_ERROR()                             \
{                                                       \
    if (hBusy || h_Busy)                                \
    {                                                   \
        PVPUT(hBusy, MOTOR_IDLE);                       \
        PVPUT(h_BusyOff, 0);                            \
    }                                                   \
    if (vBusy || v_Busy)                                \
    {                                                   \
        PVPUT(vBusy, MOTOR_IDLE);                       \
        PVPUT(v_BusyOff, 0);                            \
    }                                                   \
    if (error != ERROR_COMM_ERROR)                      \
    {                                                   \
        PVPUT(error, ERROR_COMM_ERROR);                 \
        PVPUTSTR(errMsg, "communications error");       \
    }                                                   \
}
#define S_PORT_CONFIG(BAUD,DBIT,SBIT,PRTY,FCTL) \
{                                               \
    pvGet(s_optioniv);                          \
    if( s_optioniv==SUPPORTED )                 \
    {                                           \
        PVPUT(s_baud,  BAUD);                   \
        PVPUT(s_dbit,  DBIT);                   \
        PVPUT(s_sbit,  SBIT);                   \
        PVPUT(s_prty,  PRTY);                   \
        PVPUT(s_fctl,  FCTL);                   \
    }                                           \
}
#define READ_HSC_CSW(ID,CSW,SP,SL,SB,SE,SK,SA,ST)           \
{                                                           \
    WRITE_READ_HSC(ID,HSC1_RD_CONTROL_WORD);                \
    if( sscanf(hsc_response,"%*s %*s %d",&CSW) == 1 )       \
    {                                                       \
        DEBUG_PRINT2(10,"%s cntrl word data %d",ID,CSW);    \
        PVPUT(SP, (HSC1_CSW_PWRLVL & CSW) );                \
        PVPUT(SL,((HSC1_CSW_LIMITS & CSW)?1:0) );           \
        PVPUT(SB,((HSC1_CSW_BANNER & CSW)?1:0) );           \
        PVPUT(SE,((HSC1_CSW_ECHO   & CSW)?1:0) );           \
        PVPUT(SK,((HSC1_CSW_LOCK   & CSW)?1:0) );           \
        PVPUT(SA,((HSC1_CSW_ALIAS  & CSW)?1:0) );           \
        PVPUT(ST,((HSC1_CSW_TEXT   & CSW)?1:0) );           \
    }                                                       \
    else                                                    \
    {                                                       \
        ++error;                                            \
        DEBUG_PRINT1(1,"%s cntrl word format error",ID);    \
    }                                                       \
}
#define READ_HSC_BACKLASH(ID,BL)                         \
{                                                        \
    WRITE_READ_HSC(ID,HSC1_RD_GEAR_BACKLASH);            \
    if (sscanf(hsc_response,"%*s %*s %d",&BL) == 1)      \
    {                                                    \
        DEBUG_PRINT2(10,"%s backlash %d",ID,BL);         \
        pvPut(BL,SYNC);                                  \
    }                                                    \
    else                                                 \
    {                                                    \
        ++error;                                         \
        DEBUG_PRINT1(1,"%s backlash format error",ID);   \
    }                                                    \
}
#define READ_HSC_STEPDELAY(ID,SD)                           \
{                                                           \
    WRITE_READ_HSC(ID,HSC1_RD_MOTOR_STEP_DELAY);            \
    if (sscanf(hsc_response,"%*s %*s %d",&SD) == 1)         \
    {                                                       \
        DEBUG_PRINT2(10,"%s step delay %d",ID,SD);          \
        pvPut(SD,SYNC);                                     \
    }                                                       \
    else                                                    \
    {                                                       \
        ++error;                                            \
        DEBUG_PRINT1(1,"%s step delay format error",ID);    \
    }                                                       \
}

#define SMALL                   (1.e-9)
#define DIFFERENT(A,B)          (fabs((A)-(B))>(SMALL))
#define MIN(A,B,C)              {if ((A)<(B)) {(C)=(A);} else {(C)=(B);}}
#define MAX(A,B,C)              {if ((A)>(B)) {(C)=(A);} else {(C)=(B);}}

#define RAW_TO_DIAL(A,ORIGIN)   (((A)-(ORIGIN))/(STEPS_PER_mm) )
#define DIAL_TO_RAW(A,ORIGIN)   ((A)*(STEPS_PER_mm)+(0.5)+(ORIGIN))

#define LIMIT_TEST(LO,VAL,HI)   (((LO)<=(VAL))&&((VAL)<=(HI)))


/* 
 * slit state:  xiahsc
 */
ss xiahsc {

  state startup {
    when () {
      /* C defect fixed: C never assigned SNLtaskName, which the
      ** DEBUG_PRINT/DEBUG_PRINT1 header prints with %s. */
      SNLtaskName = macValueGet("name");
      pvGet (s_ainp);
      pvGet (s_aout);
      PVPUT(init, 1);		/* need to initialize */
      init_delay = 1.0;		/* time to delay before re-init */
      /*
       * users have requested some mention of the associated serial port name
       * for the future:  Can use change the serial port via this PV?
       */
      strcpy (port, macValueGet("S"));
      pvPut(port);

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

      pvGet (hID,SYNC);
      pvGet (vID,SYNC);

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

      PVPUT(enable,       1);
      PVPUT(s_tmot,    .250); /* seconds (generic serial used milliseconds) */

    } state init
  }

  /* 
   */
  state disable { 
    when (enable) { 
      init_delay = 1.0;
      PVPUT(init, 1);
    } state init
  }

  /*
   * _any_ communications errors will land us here
   */
  /* C defect: nothing enters comm_error (dead since optics b414555), so C
  ** left communication faults silent. Every read here ends in a timeout, so
  ** rs reports faults from idle with REPORT_COMM_ERROR(); this stays dead. */
  state comm_error {
    when (init || !s_sevr || delay(ERROR_RECONNECT_INTERVAL)) { 
      DEBUG_PRINT2 (20, "comm_error: init=%d  s_sevr=%d", init, s_sevr)
      init_delay = 1.0;
      s_sevr = 0;  /* try this, just to see if communications are working now */
      PVPUT(init, 1);
    } state init
  }

 /*  */
  state init {
    when (!enable) {} state disable
    when ((efTest (hID_mon) || efTest (vID_mon))) {
      /*
       * configure the serial port parameters
       * users have requested some mention of the associated serial port name
       * reset it here in case some curious user changed it
       */
      strcpy (port, macValueGet("S"));
      pvPut(port);
      /*
       * make a quick check after user has changed an ID
       */
      init_delay = 0;
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
        PVPUTSTR(errMsg, "H & V IDs must be different");
        error++;
      }

      if (sscanf (hID, "XIAHSC-%c-%d%s", &myChar, &numWords, scratch) != 2) { 
        if (sscanf (hID, "%c-%d%s", &myChar, &numWords, scratch) != 2) { 
          if (sscanf (hID, "%c%d%s", &myChar, &numWords, scratch) != 2) { 
            PVPUTSTR(errMsg, "H ID not a valid HSC ID");
            error++;
          }
        }
      }
      if (sscanf (vID, "XIAHSC-%c-%d%s", &myChar, &numWords, scratch) != 2) { 
        if (sscanf (vID, "%c-%d%s", &myChar, &numWords, scratch) != 2) { 
          if (sscanf (vID, "%c%d%s", &myChar, &numWords, scratch) != 2) {
            PVPUTSTR(errMsg, "V ID not a valid HSC ID");
            error++;
          }
        }
      }
      if (!strlen (hID)) { 
        PVPUTSTR(errMsg, "define H ID string");
        error++;
      }
      if (!strlen (vID)) { 
        PVPUTSTR(errMsg, "define V ID string");
        error++;
      }
      /*
       * could also try to use each ID string
       */
      if (error) { 
        PVPUT(error, ERROR_BAD_ID);
      } else { 
        PVPUT(init, 0);
        PVPUTSTR(errMsg, "no error");
      }
      pvPut(error);
      efClear (hID_mon); 
      efClear (vID_mon); 
      /* recheck every 30 seconds for appropriate ID strings */
      init_delay = 30;
    } state init
    when (!init && delay(0.1)) {
      PVPUT(hBusy,   MOTOR_IDLE);
      PVPUT(vBusy,   MOTOR_IDLE);
      PVPUT(error,   NO_ERROR);
      PVPUT(h_BusyOff,   0);
      PVPUT(v_BusyOff,   0);
      /*
       * serial port configuration
       */
      S_PORT_CONFIG(BAUD_9600, DBIT_8, SBIT_1, PRTY_NONE, FCTL_NONE);
      /*
       * asyn record configuration
       */
      PVPUTSTR(s_ieos, IEOS);	/* input delimiter is <LF> */
      PVPUT(s_ifmt,  0);	/* ASCII input format */
      PVPUT(s_nrrd,  0);	/* number of characters to read */
      PVPUTSTR(s_oeos, OEOS);	/* output delim is <CR> */
      PVPUT(s_ofmt, 0);	/* ASCII output format */
      PVPUT(s_tmod,  0);	/* write/read transfer mode */
      PVPUT(s_scan,  SCAN_PASSIVE);	/* serial port is idle */
      PVPUT(s_tmot, .250);	/* timeout in 1/4 second */

      efClear (s_ainp_mon);	/* watch serial record process */

      efClear (wait_H_input);   /* flag to watch for H axis input */
      efClear (wait_V_input);   /* flag to watch for V axis input */

      efClear (move_h);		/* flag H axis to be moved */
      efClear (move_v);		/* flag V axis to be moved */
      efClear (new_H_RB);	/* new readback data available */
      efClear (new_V_RB);	/* new readback data available */

      PVPUT(stop, STOP_IDLE);	/* presume the motors are stopped */
      epicsThreadSleep(.02);	/* allow the command to process */
      efClear (stop_mon);

      efClear (t_event_mon);  efClear (l_event_mon);
      efClear (b_event_mon);  efClear (r_event_mon);
      efClear (h0_event_mon); efClear (width_event_mon);
      efClear (v0_event_mon); efClear (height_event_mon);
      PVPUT(calibrate, CALIBRATE_DONE);	/* don't reset pos. to zero */

      efClear (hID_mon);
      efClear (vID_mon);
      efClear (hOrient_mon);
      efClear (vOrient_mon);

      /*
       * Now, everything is initialized
       * stop all the motors and ask where are we
       */
      WRITE_HSC (TMOD_WRITE, "ALL", HSC1_KILL_MOVEMENT);
      epicsThreadSleep(.1);	/* wait for record to process */
      WRITE_HSC (TMOD_WRITE, "ALL", HSC1_POSITION_INQUIRY);
      epicsThreadSleep(.1);	/* wait for record to process */
      ainp_fifo_read = 0;	/* next FIFO position to store input */
      ainp_fifo_write = 0;	/* next FIFO position to process */
      while (!s_sevr) {		/* read until no more input data */
        READ_HSC();		/* process generic serial port */
        INPUT_TO_FIFO();	/* put input string into FIFO */
      }
    } state init_limits
  }

  state init_limits {
    when () {
      /* should check the response for (in order):
         	timeout
         	proper controller ID
         	proper response length
         What to do when each of these fails?
       */
      hsc_silent_reads = 0;
      error = 0;
      READ_HSC_STEPDELAY(hID,hStepDelay);
      READ_HSC_BACKLASH(hID,hGearBacklash);
      READ_HSC_CSW(hID,hCSW,hStPwr,hStLmts,hStBan,hStEcho,hStLock,hStAlias,hStText);

      WRITE_READ_HSC(hID, HSC1_RD_OUTER_MOTION_LIMIT);
      if (sscanf (hsc_response, "%*s %*s %d", &hOuterLimit) == 1)
      {
        pvPut(hOuterLimit,SYNC);
      }
      else
      {
        error++;
        DEBUG_PRINT(5, "Could not read horizontal outer limit");
      }
      WRITE_READ_HSC(hID, HSC1_RD_ORIGIN_POSITION);
      if (sscanf (hsc_response, "%*s %*s %d", &hOrigin) == 1)
      {
        pvPut(hOrigin,SYNC);
      }
      else
      {
        error++;
        DEBUG_PRINT(5, "Could not read horizontal origin position");
      }
      if (!error) {
        pos_ag = RAW_TO_DIAL(0,           hOrigin);
        pos_bg = RAW_TO_DIAL(hOuterLimit, hOrigin);
        PVPUT(l_lo, pos_ag);  PVPUT(l_hi, pos_bg);
        PVPUT(r_lo, pos_ag);  PVPUT(r_hi, pos_bg);
          /*
           * Calculate the absolute limits of gap and center.
           * (Note the actual limits are also dependent on
           *  current values of l and r!  However, these
           *  situations will be trapped by the limits
           *  on the corresponding axes.)
           */
        MAX(l_lo, r_lo, dScratch);
        MAX(dScratch, 0.0, width_lo);  pvPut(width_lo);
        PVPUT(width_hi, l_hi + r_hi);
        PVPUT(h0_hi, (r_hi - l_lo)/2);
        PVPUT(h0_lo, (r_lo - l_hi)/2);
      }

      error = 0;
      READ_HSC_CSW(vID,vCSW,vStPwr,vStLmts,vStBan,vStEcho,vStLock,vStAlias,vStText);
      READ_HSC_BACKLASH(vID,vGearBacklash);
      READ_HSC_STEPDELAY(vID,vStepDelay);

      WRITE_READ_HSC(vID, HSC1_RD_OUTER_MOTION_LIMIT);
      if (sscanf (hsc_response, "%*s %*s %d", &vOuterLimit) == 1)
      {
        pvPut(vOuterLimit,SYNC);
      }
      else
      {
        error++;
        DEBUG_PRINT(5, "Could not read vertical outer limit");
      }

      WRITE_READ_HSC(vID, HSC1_RD_ORIGIN_POSITION);
      if (sscanf (hsc_response, "%*s %*s %d", &vOrigin) == 1)
      {
        pvPut(vOrigin,SYNC);
      }
      else
      {
        error++;
        DEBUG_PRINT(5, "Could not read vertical origin position");
      }
      if (!error) {
        pos_ag = RAW_TO_DIAL(0,           vOrigin);
        pos_bg = RAW_TO_DIAL(vOuterLimit, vOrigin);
        PVPUT(t_lo, pos_ag);    PVPUT(t_hi, pos_bg);
        PVPUT(b_lo, pos_ag);    PVPUT(b_hi, pos_bg);
          /*
           * Calculate the absolute limits of gap and center.
           * (Note the actual limits are also dependent on 
           *  current values of t and b!  However, these
           *  situations will be trapped by the limits
           *  on the corresponding axes.)
           */
        MAX(t_lo, b_lo, dScratch);
        MAX(dScratch, 0.0, height_lo);  pvPut(height_lo);
        PVPUT(height_hi, t_hi + b_hi);

        PVPUT(v0_hi, (t_hi - b_lo)/2);
        PVPUT(v0_lo, (t_lo - b_hi)/2);
      }
      if (hsc_silent_reads) REPORT_COMM_ERROR();
      PVPUT(locate, 1);  /* update the positions from the controller */

    } state idle
  }

/* 
 */
  state idle {

    /*
     * user can disable this program
     */
    when (!enable) { } state disable

    /*
     * user can re-initialize this program
     */
    when (init) {
      init_delay = 1; PVPUT(init, 1); PVPUT(h_BusyOff, 0);  PVPUT(v_BusyOff, 0);
    } state init

    /*
     * user can stop the motors
     */
    when (efTest (stop_mon) && stop) { 
      WRITE_HSC (TMOD_WRITE, "ALL", HSC1_KILL_MOVEMENT);
      epicsThreadSleep(.1);
      efClear (stop_mon);
    } state idle

    /*
     * target value has changed
     */
    when ( efTest (l_event_mon) ) {
      DEBUG_PRINT1(2,"efTest(l_event_mon) -> premove, l=%f", l);
    } state premove

    when ( efTest (l_event_mon)     || efTest (r_event_mon) 
    ||     efTest (t_event_mon)     || efTest (b_event_mon) 
    ||     efTest (h0_event_mon)    || efTest (height_event_mon) 
    ||     efTest (v0_event_mon)    || efTest (width_event_mon)   ) {
      DEBUG_PRINT(2,"efTest() -> premove");
    } state premove

    /*
     * send a MOVE command to HSC
     */
    when (efTest (start_H_move)) {
      if (hBusy) { 		/* allow an active move to be interrupted */
        DEBUG_PRINT(2,"efTest(start_H_move) and hBusy");
        WRITE_HSC (TMOD_WRITE, hID, HSC1_KILL_MOVEMENT);
        epicsThreadSleep(.5);		/* wait for it to happen */
      } else {
        PVPUT(hBusy, MOTOR_BUSY);	/* tell the world we are busy */
      }
      if (hOrient == ORIENT_LEFT_RIGHT) { 
        pos_ag = l;  pos_bg = r;
      } else { 
        pos_ag = r;  pos_bg = l;
      }
      h_isMoving = 1;
      efSet (wait_H_input);		/* could enable a timeout on the move */
      efClear (start_H_move);		/* OK, we told it to move now */
/*if (ainp_fifo_read != ainp_fifo_write) printf("start_H_move: fifo not empty\n"); */
      while (!s_sevr) {		/* read until no more input data */
        READ_HSC();		/* process generic serial port */
        /* discard possibly outdated messages involving hID */
        sscanf(s_ainp, "%%%s", id);
        if (!strcmp(id, hID)) {
          DEBUG_PRINT2(2,"discarding hID message '%s' id='%s'\n", s_tinp, id);
        } else {
          INPUT_TO_FIFO();
        }
      }
      pos_a = DIAL_TO_RAW (pos_ag, hOrigin);	/* convert to raw (motor) coordinates */
      pos_b = DIAL_TO_RAW (pos_bg, hOrigin);	/* convert to raw (motor) coordinates */
      sprintf (scratch, "M %d %d", pos_a, pos_b);
      DEBUG_PRINT(2,"start_H_move: writing to motor");
      WRITE_HSC(TMOD_WRITE, hID, scratch);
    } state idle

    when (efTest (start_V_move)) {
      if (vBusy) {  		/* allow an active move to be interrupted */
        WRITE_HSC (TMOD_WRITE, vID, HSC1_KILL_MOVEMENT);
        epicsThreadSleep(.5);		/* wait for it to happen */
      } else {
        PVPUT(vBusy, MOTOR_BUSY);	/* tell the world we are busy */
      }
      if (vOrient == ORIENT_TOP_BOTTOM) { 
        pos_ag = t;  pos_bg = b;
      } else { 
        pos_ag = b;  pos_bg = t;
      }
      v_isMoving = 1;
      efSet (wait_V_input);		/* could enable a timeout on the move */
      efClear (start_V_move);
/*if (ainp_fifo_read != ainp_fifo_write) printf("start_V_move: fifo not empty\n"); */
      while (!s_sevr) {		/* read until no more input data */
        READ_HSC();		/* process generic serial port */
        /* discard possibly outdated messages involving vID */
        sscanf(s_ainp, "%%%s", id);
        if (!strcmp(id, vID)) {
          DEBUG_PRINT2(2,"discarding vID message '%s' id='%s'\n", s_tinp, id);
        } else {
          INPUT_TO_FIFO();
        }
      }
      pos_a = DIAL_TO_RAW (pos_ag, vOrigin);	/* convert to raw (motor) coordinates */
      pos_b = DIAL_TO_RAW (pos_bg, vOrigin);	/* convert to raw (motor) coordinates */
      sprintf (scratch, "M %d %d", pos_a, pos_b);
      WRITE_HSC(TMOD_WRITE, vID, scratch);
    } state idle

    /*
     * user can request device to resend coordinates
     */
    when (locate) { 	/* poll the generic serial port for input */
      /*
       * users have requested some mention of the associated serial port name
       * reset here in case some curious user changed it
       */
      strcpy (port, macValueGet("S"));
      pvPut(port);
      PVPUT(locate, 0);
      WRITE_HSC (TMOD_WRITE, "ALL", HSC1_POSITION_INQUIRY);
      epicsThreadSleep(.1);
      ainp_replies = 0;
      while (!s_sevr) {		/* read until no more input data */
        DEBUG_PRINT(10,"locate: calling READ_HSC");
        READ_HSC();		/* process generic serial port */
        if (!s_sevr) INPUT_TO_FIFO();	/* put input string into FIFO */
      }
      DEBUG_PRINT(10,"locate: s_sevr = TRUE");
      /* Every read ends in a timeout, so only a locate with no reply at all
      ** counts; one alone can be a read loop skipped on a stale SEVR. */
      if (ainp_replies) {
        silent_locates = 0;
      } else if (++silent_locates >= COMM_ERROR_LOCATES) {
        REPORT_COMM_ERROR();
      }
    } state idle

    when (calibrate) {
      /*
       * user requested a device calibration
       * ASSUME the user has closed the slits down
       * all the way.
       */
      WRITE_HSC(TMOD_WRITE, "ALL", HSC1_CALIBRATE_IMMEDIATE)  
      epicsThreadSleep(.1);		/* wait for record to process */
      PVPUT(calibrate, CALIBRATE_DONE);	/* OK, we're done here */
      PVPUT(locate, 1);	/* update with new controller positions */
    } state idle


    /*
     * process the input data
     */
    when (ainp_fifo_read != ainp_fifo_write) { 
      FIFO_TO_OUTPUT(scratch);
      id[0] = word1[0] = word2[0] = word3[0] = word4[0] = 
        word5[0] = word6[0] = 0;
      numWords = sscanf (scratch, "%s %s %s %s %s %s", 
          word1, word2, word3, word4, word5, word6);
      hasID = sscanf (word1, "%%%s", id);
      if (debug_flag >= 5 && (numWords>0)) {
        printf("xia_slit:fifo_read: (%d words)...\n'%s'\n", numWords, scratch);
      }
    } state process_response

    /*
     * new H axis readback data is available
     */
    when (efTest (new_H_RB)) { 
      efClear (new_H_RB);
      /*
       * update the target positions
       * but only if we have not interrupted a move-in-progress
       */
      DEBUG_PRINT3(2,"efTest(new_H_RB)==1; h_Busy|hBusy=%d, l=%f,l_RB=%f", h_Busy|hBusy, l, l_RB);
      if (!hBusy && !h_Busy) {
        if (DIFFERENT(l, l_RB)) PVPUT(load_l, l_RB);
        if (DIFFERENT(r, r_RB)) PVPUT(load_r, r_RB);
        if (DIFFERENT(h0, h0_RB)) PVPUT(load_h0, h0_RB);
        if (DIFFERENT(width, width_RB)) PVPUT(load_width, width_RB);
        efClear (wait_H_input);
      }
    } state idle

    /*
     * new V axis readback data is available
     */
    when (efTest (new_V_RB)) { 
      efClear (new_V_RB);
      /*
       * update the target positions
       * but only if we have not interrupted a move-in-progress
       */
      if (!vBusy && !v_Busy) {
        if (DIFFERENT(t, t_RB)) PVPUT(load_t, t_RB);
        if (DIFFERENT(b, b_RB)) PVPUT(load_b, b_RB);
        if (DIFFERENT(v0, v0_RB)) PVPUT(load_v0, v0_RB);
        if (DIFFERENT(height, height_RB)) PVPUT(load_height, height_RB);
        efClear (wait_V_input);
      }
    } state idle

    /*
     * signal that the KILL command has finished
     */
    when (stop && !efTest (wait_H_input) && !efTest (wait_V_input)) { 
      PVPUT(stop, STOP_IDLE);
      efClear (stop_mon);
    } state idle

    /*
     * things to do in the background ...
     */
    when ((hBusy || vBusy) && delay (MOVING_POLL_INTERVAL)) { 
      PVPUT(locate, 1);		/* signal for a position update */
    } state idle
    when (delay (IDLE_POLL_INTERVAL)) { 
      PVPUT(locate, 1);		/* signal for a position update */
      DEBUG_PRINT(2, "IDLE POLL");
    } state idle
  }

 /* 
 */
  state premove { 
    /*
     * ignore any non-movement monitors
     */
    when (efTest (l_event_mon) && (l == l_old)) {
      efClear (l_event_mon);
      PVPUT(h_BusyOff, 0);
      DEBUG_PRINT(2, "l==l_old: ->h_BusyOff");
    } state idle
    when (efTest (r_event_mon) && (r == r_old)) {
      efClear (r_event_mon);
      PVPUT(h_BusyOff, 0);
      DEBUG_PRINT(2, "r==r_old: ->h_BusyOff");
    } state idle
    when (efTest (t_event_mon) && (t == t_old)) {
      efClear (t_event_mon);
      PVPUT(v_BusyOff, 0);
      DEBUG_PRINT(2, "t==t_old: ->v_BusyOff");
    } state idle
    when (efTest (b_event_mon) && (b == b_old)) {
      efClear (b_event_mon);
      PVPUT(v_BusyOff, 0);
      DEBUG_PRINT(2, "b==b_old: ->v_BusyOff");
    } state idle
    when (efTest (h0_event_mon) && (h0 == h0_old)) {
      efClear (h0_event_mon);
      PVPUT(h_BusyOff, 0);
      DEBUG_PRINT(2, "h0==h0_old: ->h_BusyOff");
    } state idle
    when (efTest (v0_event_mon) && (v0 == v0_old)) {
      efClear (v0_event_mon);
      PVPUT(v_BusyOff, 0);
      DEBUG_PRINT(2, "v0==v0_old: ->v_BusyOff");
    } state idle
    when (efTest (width_event_mon) && (width == width_old)) {
      efClear (width_event_mon);
      PVPUT(h_BusyOff, 0);
      DEBUG_PRINT(2, "width==width_old: ->h_BusyOff");
    } state idle
    when (efTest (height_event_mon) && (height == height_old)) {
      efClear (height_event_mon);
      PVPUT(v_BusyOff, 0);
      DEBUG_PRINT(2, "height==height_old: ->v_BusyOff");
    } state idle
    when () { 
      /*
       * horizontal axis
       */
      if (efTest (l_event_mon) || efTest (r_event_mon) ||
          efTest (h0_event_mon) || efTest (width_event_mon)) { 
        /* find out which coordinate scheme changed, calc the other */
        if (efTest (l_event_mon) || efTest (r_event_mon)) {
          width = r + l;
          h0 = (r - l) / 2;
        } else {
          l = width / 2 - h0;
          r = width / 2 + h0;
        }
        /* check the limits */
        error = 0;
        if (!LIMIT_TEST (l_lo, l, l_hi)) { l = l_old; error++; }
        if (!LIMIT_TEST (r_lo, r, r_hi)) { r = r_old; error++; }
        if (!LIMIT_TEST (h0_lo, h0, h0_hi)) { h0 = h0_old; error++; }
        if (!LIMIT_TEST (width_lo, width, width_hi)) { 
           width = width_old; error++; }
        if (error) { 
          /* reset values if there was an error */
          if (efTest (l_event_mon)) PVPUT(load_l, l);
          if (efTest (r_event_mon)) PVPUT(load_r, r);
          if (efTest (h0_event_mon)) PVPUT(load_h0, h0);
          if (efTest (width_event_mon)) PVPUT(load_width, width);
          error = ERROR_SOFT_LIMITS;
          PVPUTSTR(errMsg, "H soft limits exceeded");
          PVPUT(h_BusyOff, 0);
        } else { 
          PVPUTSTR(errMsg, "no error");
          /* update the new coordinates */
          efSet (start_H_move);
          DEBUG_PRINT(2, "premove: efSet(start_H_move)");
          if (efTest (l_event_mon) || efTest (r_event_mon)) {
            PVPUT(load_h0, h0);  PVPUT(load_width, width);
          } else {
            PVPUT(load_l, l);  PVPUT(load_r, r);
          }
        }
        pvPut(error);
        efClear(l_event_mon);   efClear(r_event_mon);
        efClear(h0_event_mon);  efClear(width_event_mon);
      }
      /*
       * vertical axis
       */
      if (efTest (t_event_mon) || efTest (b_event_mon) 
      ||  efTest (v0_event_mon) || efTest (height_event_mon)) { 
        /* find out which coordinate scheme changed, calc the other */
        if (efTest (t_event_mon) || efTest (b_event_mon)) {
          height = t + b;
          v0 = (t - b) / 2;
        } else {
          t = height / 2 + v0;
          b = height / 2 - v0;
        }
        /* check the limits */
        error = 0;
        if (!LIMIT_TEST (t_lo, t, t_hi)) { t = t_old; error++; }
        if (!LIMIT_TEST (b_lo, b, b_hi)) { b = b_old; error++; }
        if (!LIMIT_TEST (v0_lo, v0, v0_hi)) { v0 = v0_old; error++; }
        if (!LIMIT_TEST (height_lo, height, height_hi)) { 
           height = height_old; error++; }
        if (error) { 
          /* reset values if there was an error */
          if (efTest (t_event_mon)) PVPUT(load_t, t);
          if (efTest (b_event_mon)) PVPUT(load_b, b);
          if (efTest (v0_event_mon)) PVPUT(load_v0, v0);
          if (efTest (height_event_mon)) PVPUT(load_height, height);
          error = ERROR_SOFT_LIMITS;
          PVPUTSTR(errMsg, "V soft limits exceeded");
          PVPUT(v_BusyOff, 0);
        } else { 
          PVPUTSTR(errMsg, "no error");
          /* update the new coordinates */
          efSet (start_V_move);
          if (efTest (t_event_mon) || efTest (b_event_mon)) {
            PVPUT(load_v0, v0);  PVPUT(load_height, height);
          } else {
            PVPUT(load_t, t);  PVPUT(load_b, b);
          }
        }
        pvPut(error);
        efClear(t_event_mon);   efClear(b_event_mon);
        efClear(v0_event_mon);  efClear(height_event_mon);
      }
    } state idle
  }

 /* 
 */
  state process_response {
    entry {
      DEBUG_PRINT1(5, "process_response:entry numWords=%d", numWords);
    }
    /*
     * various error messages
     */
    when (numWords == -1) {
      /*
       * The last read of a series times out and returns nothing.
       * That's how we know we got everything there was to get.
       */
    } state idle
    when ( (numWords == 2) && (!strcmp(word2, "OK;")) ) { 
      /* Ignore command acknowledge from controller. */
    } state idle
    when ( (numWords >= 2) && (!strcmp(word2, "ERROR;")) ) {
      if (numWords == 2) {
        PVPUT(error, ERROR_UNKNOWN);
        /* C defect fixed: "%s" ran this message past the 40-byte errMsg
        ** for a reply id of 25 characters or more. */
        sprintf (errMsg, "%.24s: unknown error", id);
        pvPut(errMsg);
      } else {
        sscanf (word3, "%d", &error);
        pvPut(error);
        /* C defect fixed: C indexed the 14 hscErrors with any code, the
        ** reply's or, when it did not scan, a stale one such as 15-17. */
        if (error >= 0 && error < 14) {
          PVPUTSTR(errMsg, hscErrors[error]);
        } else {
          sprintf (errMsg, "%.24s: unknown error", id);
          pvPut(errMsg);
        }
      }
      if (!strcmp(word1, hID)) {
        DEBUG_PRINT1(2, "'%s' reported an error: ->h_BusyOff", hID);
        PVPUT(h_BusyOff, 0);
        h_isMoving = 0;
      } else if (!strcmp(word1, vID)) {
        DEBUG_PRINT1(2, "'%s' reported an error: ->v_BusyOff", vID);
        PVPUT(v_BusyOff, 0);
        v_isMoving = 0;
      }
    } state idle
    /*
     * controller is busy
     */
    when ( (numWords == 2) && (!strcmp(word2, "BUSY;")) ) { 
      /* axes are moving */
      if (!strcmp(id, hID)) { 
        PVPUT(hBusy,   MOTOR_BUSY);
      } else {
        PVPUT(vBusy,   MOTOR_BUSY);
      }
      if (error) { /* don't update unless previous error exists */
        PVPUT(error, 0);
        PVPUTSTR(errMsg, "no error");
      }
    } state idle

    /*
     * position update
     */
    when ( (numWords == 4) && (!strcmp(word4, "DONE;")) ) { 
      /* position info received */
      sscanf (word2, "%d", &pos_a);
      sscanf (word3, "%d", &pos_b);
      if (error) { /* don't update unless previous error exists */
        PVPUT(error, 0);
        PVPUTSTR(errMsg, "no error");
      }
      /* did we get where we want to go? */
      if (!strcmp(id, hID)) {
        DEBUG_PRINT2(5,"hDONE: pos_a = %d, pos_b = %d", pos_a, pos_b);
        pos_ag = RAW_TO_DIAL(pos_a, hOrigin);
        pos_bg = RAW_TO_DIAL(pos_b, hOrigin);
        h_isMoving = 0;
        l_raw = DIAL_TO_RAW(l, hOrigin);
        r_raw = DIAL_TO_RAW(r, hOrigin);
        DEBUG_PRINT2(10,"hRaw; l=%d, r=%d", l_raw, r_raw);
        if (hOrient == ORIENT_LEFT_RIGHT) { 
          if ((pos_a == l_raw) && (pos_b == r_raw)) PVPUT(h_BusyOff, 0);
        } else { 
          if ((pos_a == r_raw) && (pos_b == l_raw)) PVPUT(h_BusyOff, 0);
        }
      } else if (!strcmp(id, vID)){
        DEBUG_PRINT2(5,"vDONE: pos_a = %d, pos_b = %d", pos_a, pos_b);
        pos_ag = RAW_TO_DIAL(pos_a, vOrigin);
        pos_bg = RAW_TO_DIAL(pos_b, vOrigin);
        v_isMoving = 0;
        t_raw = DIAL_TO_RAW(t, vOrigin);
        b_raw = DIAL_TO_RAW(b, vOrigin);
        DEBUG_PRINT2(10,"vRaw; t=%d, b=%d", t_raw, b_raw);
        if (vOrient == ORIENT_TOP_BOTTOM) { 
          if ((pos_a == t_raw) && (pos_b == b_raw)) PVPUT(v_BusyOff, 0);
        } else { 
          if ((pos_a == b_raw) && (pos_b == t_raw)) PVPUT(v_BusyOff, 0);
        }
      }
    } state update_positions

    when ( (numWords == 5) && (!strcmp(word2, "OK")) ) { 
      /* position info received */
      sscanf (word3, "%d", &pos_a);
      sscanf (word4, "%d", &pos_b);
      if (error) { /* don't update unless previous error exists */
        PVPUT(error, 0);
        PVPUTSTR(errMsg, "no error");
      }
      if (!strcmp(id, hID)) {
        pos_ag = RAW_TO_DIAL(pos_a, hOrigin);
        pos_bg = RAW_TO_DIAL(pos_b, hOrigin);
      } else {
        pos_ag = RAW_TO_DIAL(pos_a, vOrigin);
        pos_bg = RAW_TO_DIAL(pos_b, vOrigin);
      }
    } state update_positions

    when () {
      /* keep us from getting stuck here */
      printf("xia_slit: did not understand reply: '%s' '%s' '%s' '%s' '%s' '%s'\n",
          word1, word2, word3, word4, word5, word6);
      epicsThreadSleep(.1);
    } state idle
  }

 /* 
 */
  state update_positions {
    when () {
      /* horizontal */
      if (!strcmp(id, hID)) { 
        if (hOrient == ORIENT_LEFT_RIGHT) { 
          PVPUT(l_RB, pos_ag);
          PVPUT(r_RB, pos_bg);
        } else { 
          PVPUT(r_RB, pos_ag);
          PVPUT(l_RB, pos_bg);
        }
        PVPUT(width_RB, l_RB+r_RB);
        PVPUT(h0_RB, (r_RB-l_RB)/2);
        DEBUG_PRINT(2,"update_positions: efSet(new_H_RB)");
        efSet (new_H_RB);	/* new readback data available */
        PVPUT(hBusy,   MOTOR_IDLE);
        /*
         * keep track of last known good values
         * to impose motor limits
         */
        l_old = l_RB;	r_old = r_RB;
        h0_old = h0_RB;	width_old = width_RB;
      }
      /* vertical */
      if (!strcmp(id, vID)) { 
        if (vOrient == ORIENT_TOP_BOTTOM) { 
          PVPUT(t_RB, pos_ag);
          PVPUT(b_RB, pos_bg);
        } else { 
          PVPUT(b_RB, pos_ag);
          PVPUT(t_RB, pos_bg);
        }
        PVPUT(height_RB, t_RB+b_RB);
        PVPUT(v0_RB, (t_RB-b_RB)/2);
        efSet (new_V_RB);	/* new readback data available */
        PVPUT(vBusy,   MOTOR_IDLE);
        /*
         * keep track of last known good values
         * to impose motor limits
         */
        t_old = t_RB;	b_old = b_RB;
        v0_old = v0_RB;	height_old = height_RB;
      }
    } state idle
  }

}

/*
#############################################################################
#  Illinois Open Source License
#  University of Illinois
#  Open Source License
#
#
#  Copyright (c) 2004,  UNICAT.  All rights reserved.
#
#
#  Developed by:
#
#  UNICAT, Advanced Photon Source, Argonne National Laboratory
#
#  Frederick Seitz Materials Research Laboratory,
#  University of Illinois at Urbana-Champaign
#
#  http://www.uni.aps.anl.gov
#
#  Permission is hereby granted, free of charge, to any person obtaining a
#  copy of this software and associated documentation files (the
#  "Software"), to deal with the Software without restriction, including
#  without limitation the rights to use, copy, modify, merge, publish,
#  distribute, sublicense, and/or sell copies of the Software, and to
#  permit persons to whom the Software is furnished to do so, subject to
#  the following conditions:
#
#
#  Redistributions of source code must retain the above copyright notice,
#  this list of conditions and the following disclaimers.
#
#
#  Redistributions in binary form must reproduce the above copyright
#  notice, this list of conditions and the following disclaimers in the
#  documentation and/or other materials provided with the distribution.
#
#
#  Neither the names of UNICAT, Frederick Seitz Materials Research
#  Laboratory, University of Illinois at Urbana-Champaign,
#  nor the names of its contributors may be used to endorse or promote
#  products derived from this Software without specific prior written
#  permission.
#
#
#  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
#  OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
#  MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
#  IN NO EVENT SHALL THE CONTRIBUTORS OR COPYRIGHT HOLDERS BE LIABLE FOR
#  ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
#  TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
#  SOFTWARE OR THE USE OR OTHER DEALINGS WITH THE SOFTWARE.
#############################################################################
 */
