program Oxford_CS800 ("name=cs0, CID=0, P=xxx:, R=CS0:, SP=xxx:CS:status_packet")

/* option -c;  /* Begin execution before PV connections complete */
option +d;  /* turn ON run-time debugging messages */
option +r;  /* re-usable code, can load more than once */
option -w;  /* Suppress SNC warnings */

/*
 * usage:
   
   VxWorks-only: (re)load the compiled sequence program:
    ld < /usr/local/epics/synApps_6_1__base_7_0_3_1/support/xxx-R6-1/xxxApp/src/O.linux-arm/cs800.o

 seq &cs800, "name=cs113, CID=113, P=xxx:, R=CS0:, SP=xxx:CS:status_packet"
 seq &cs800, "name=cs144, CID=144, P=xxx:, R=CS1:, SP=xxx:CS:status_packet"
 
 * to kill the running sequence program:
 td "cs113"
 
 Parameters
 
 name | this instance of running SNL program
 ---- | ------------------------------------
 CID  | controller ID
 P    | IOC prefix
 R    | controller's database prefix
 SP   | PV of status packet (asyn record)

 */

/*------------------------------------------------ 
  ------------------------------------------------   general purpose
  ------------------------------------------------ */

%%#include <math.h>
%%#include <stdio.h>
%%#include <stdlib.h>
%%#include <string.h>
%%#include <epicsThread.h>
#include "seqPVmacros.h"


/*------------------------------------------------ 
  ------------------------------------------------   macros
  ------------------------------------------------ */

#define STATUS_PACKET_VALID_LENGTH    928
#define NUMBER_OF_PARAMETERS          230
#define IDLE_POLL_INTERVAL            (30)
#define DEBUGLEVEL_OFF                (0)
#define DEBUGLEVEL_ERROR              (1)
#define DEBUGLEVEL_WARNING            (2)
#define DEBUGLEVEL_INFO               (3)
#define DEBUGLEVEL_DEBUG              (4)
#define NOT_FOUND                     (-1)

#define CHANGE_STATE(_STATE_)    \
      DEBUG_PRINT2(DEBUGLEVEL_DEBUG, "state change: %s -> %s", current_state, _STATE_); \
      sprintf(current_state, _STATE_);

#define CHECK_PVPUT(_PV_, _PARAM_ID_)     \
      value = getParam(_PARAM_ID_, param_id, param_value);       \
      if (value != NOT_FOUND) PVPUT(_PV_, value);

#define GETBYTE(_ARR_, _IDX_)  (_ARR_[_IDX_] & 0xff)
#define GETUINT16(_ARR_, _IDX_)  (GETBYTE(_ARR_, _IDX_)*256 + GETBYTE(_ARR_, _IDX_+1))

#define PARAM_DEVICETYPE      (1000)
#define PARAM_DEVICESUBTYPE   (1001)
#define PARAM_DEVICEMINTEMP   (1002)
#define PARAM_DEVICEMAXTEMP   (1003)
#define PARAM_VER             (1004)
#define PARAM_CTRLNUM         (1028)
#define PARAM_SETPOINT        (1050)
#define PARAM_TEMP            (1051)
#define PARAM_ERROR           (1052)
#define PARAM_RUNMODE         (1053)
#define PARAM_PHASE           (1054)
#define PARAM_RAMPRATE        (1055)
#define PARAM_TARGETTEMP      (1056)
#define PARAM_EVAPTEMP        (1057)
#define PARAM_SUCTTEMP        (1058)
#define PARAM_REMAINING       (1059)
#define PARAM_GASFLOW         (1060)
#define PARAM_GASHEAT         (1061)
#define PARAM_EVAPHEAT        (1062)
#define PARAM_SUCTHEAT        (1063)
#define PARAM_ALARMI          (1065)
#define PARAM_RUNTIME         (1066)
#define PARAM_EVAPADJUST      (1067)
#define PARAM_TURBOMODE       (1068)
#define PARAM_BACKPRESS       (1101)
#define PARAM_VALVEOPENING    (1103)
#define PARAM_LEVEL           (1203)
#define PARAM_COLDHEADTEMP    (2010)
#define PARAM_SHIELDTEMP      (2011)
#define PARAM_VACGAUGE        (2012)
#define PARAM_NOZZLETEMP      (2013)
#define PARAM_SAMPLEHEAT      (2014)
#define PARAM_COLDHEADHEAT    (2015)
#define PARAM_SHIELDHEAT      (2016)
#define PARAM_NOZZLEHEAT      (2017)
#define PARAM_AVGSAMPLEHEAT   (2019)
#define PARAM_AVGNOZZLEHEAT   (2020)
#define PARAM_SAMHOLDTEMP     (2030)
#define PARAM_CRYOSTATTEMP    (2031)
#define PARAM_SAMHOLDPRES     (2032)
#define PARAM_ELAPSED         (2034)
#define PARAM_SUCTSETTEMP     (2035)
#define PARAM_NOZZLESETTEMP   (2036)
#define PARAM_COLLARTEMP      (2041)
#define PARAM_VACSENSOR       (2042)

/****
 **** local variables 
 ****/
char current_state[256];
char scratch[256];
int i;
unsigned int cid;
unsigned int s_cid;
unsigned int value;
unsigned int s_checksum;
int param_id[NUMBER_OF_PARAMETERS];
unsigned int param_value[NUMBER_OF_PARAMETERS];
char*   SNLtaskName;

%% int getParam(int id, int *pid, unsigned int *pval);

/*------------------------------------------------ 
  ------------------------------------------------   assignments
  ------------------------------------------------ */

/**** 
 **** asyn record
 ****/
PV(unsigned int, s_nrrd, "{SP}.NRRD", Monitor);       /* number bytes received */
PVA(unsigned short, s_binp, STATUS_PACKET_VALID_LENGTH, "{SP}.BINP", EvFlag);    /* Binary input */


/**** 
 **** CS800 controller database 
 ****/
PV(unsigned int, disable,      "{P}{R}DISABLE",          Monitor);
PV(unsigned int, setpoint,     "{P}{R}SETPOINT.RVAL",    NoMon);
PV(unsigned int, temperature,  "{P}{R}TEMP.RVAL",        NoMon);
PV(unsigned int, run_mode,     "{P}{R}RUNMODE.RVAL",     NoMon);
PV(unsigned int, phase,        "{P}{R}PHASE.RVAL",       NoMon);
PV(string, alarm,     "{P}{R}ALARM",            NoMon);

PV(unsigned int, debug_flag, "{P}{R}DEBUG", Monitor);    /* SNL diagnostics level */

PV(unsigned int, devicetype, "{P}{R}DEVICETYPE.RVAL", NoMon);
PV(unsigned int, devicesubtype, "{P}{R}DEVICESUBTYPE.RVAL", NoMon);
PV(unsigned int, devicemintemp, "{P}{R}DEVICEMINTEMP.RVAL", NoMon);
PV(unsigned int, devicemaxtemp, "{P}{R}DEVICEMAXTEMP.RVAL", NoMon);
PV(unsigned int, ver, "{P}{R}VER.RVAL", NoMon);
PV(unsigned int, ramprate, "{P}{R}RAMPRATE.RVAL", NoMon);
PV(unsigned int, targettemp, "{P}{R}TARGETTEMP.RVAL", NoMon);
PV(unsigned int, evaptemp, "{P}{R}EVAPTEMP.RVAL", NoMon);
PV(unsigned int, sucttemp, "{P}{R}SUCTTEMP.RVAL", NoMon);
PV(unsigned int, remaining, "{P}{R}REMAINING.RVAL", NoMon);
PV(unsigned int, gasflow, "{P}{R}GASFLOW.RVAL", NoMon);
PV(unsigned int, gasheat, "{P}{R}GASHEAT.RVAL", NoMon);
PV(unsigned int, evapheat, "{P}{R}EVAPHEAT.RVAL", NoMon);
PV(unsigned int, suctheat, "{P}{R}SUCTHEAT.RVAL", NoMon);
PV(unsigned int, backpress, "{P}{R}BACKPRESS.RVAL", NoMon);
PV(unsigned int, runtime, "{P}{R}RUNTIME.RVAL", NoMon);
PV(unsigned int, alarmi, "{P}{R}ALARMI.RVAL", NoMon);
PV(unsigned int, ctrlnum, "{P}{R}CTRLNUM.RVAL", NoMon);
PV(unsigned int, evapadjust, "{P}{R}EVAPADJUST.RVAL", NoMon);
PV(unsigned int, turbomode, "{P}{R}TURBOMODE.RVAL", NoMon);
PV(unsigned int, valveopening, "{P}{R}VALVEOPENING.RVAL", NoMon);
PV(unsigned int, level, "{P}{R}LEVEL.RVAL", NoMon);
PV(unsigned int, error, "{P}{R}ERROR.RVAL", NoMon);
PV(unsigned int, coldheadtemp, "{P}{R}COLDHEADTEMP.RVAL", NoMon);
PV(unsigned int, shieldtemp, "{P}{R}SHIELDTEMP.RVAL", NoMon);
PV(unsigned int, vacgauge, "{P}{R}VACGAUGE.RVAL", NoMon);
PV(unsigned int, nozzletemp, "{P}{R}NOZZLETEMP.RVAL", NoMon);
PV(unsigned int, sampleheat, "{P}{R}SAMPLEHEAT.RVAL", NoMon);
PV(unsigned int, coldheadheat, "{P}{R}COLDHEADHEAT.RVAL", NoMon);
PV(unsigned int, shieldheat, "{P}{R}SHIELDHEAT.RVAL", NoMon);
PV(unsigned int, nozzleheat, "{P}{R}NOZZLEHEAT.RVAL", NoMon);
PV(unsigned int, avgsampleheat, "{P}{R}AVGSAMPLEHEAT.RVAL", NoMon);
PV(unsigned int, avgnozzleheat, "{P}{R}AVGNOZZLEHEAT.RVAL", NoMon);
PV(unsigned int, samholdtemp, "{P}{R}SAMHOLDTEMP.RVAL", NoMon);
PV(unsigned int, cryostattemp, "{P}{R}CRYOSTATTEMP.RVAL", NoMon);
PV(unsigned int, samholdpres, "{P}{R}SAMHOLDPRES.RVAL", NoMon);
PV(unsigned int, elapsed, "{P}{R}ELAPSED.RVAL", NoMon);
PV(unsigned int, suctsettemp, "{P}{R}SUCTSETTEMP.RVAL", NoMon);
PV(unsigned int, nozzlesettemp, "{P}{R}NOZZLESETTEMP.RVAL", NoMon);
PV(unsigned int, collartemp, "{P}{R}COLLARTEMP.RVAL", NoMon);
PV(unsigned int, vacsensor, "{P}{R}VACSENSOR.RVAL", NoMon);


/* 
 * state set:  cs800
 */
ss cs800 {

  state startup {
    when () {
      sprintf(current_state, "startup");
      pvGet(s_binp);
      pvGet(s_nrrd);
      
      cid = atoi(macValueGet("CID"));
      efSet(s_binp_mon);

      printf("(%s,#%d) PV prefix %s%s\n", 
             macValueGet("name"), 
             cid,
             macValueGet("P"),
             macValueGet("R"));
      printf("(%s) status packet PV: %s\n", macValueGet("name"), macValueGet("SP"));
    } state idle
  }


  state idle {
    when (!strcmp(current_state, "idle")) {
      CHANGE_STATE("idle")
    } state idle

    when (disable) {} state ignore

    when (efTestAndClear(s_binp_mon) && s_nrrd == STATUS_PACKET_VALID_LENGTH) {
      /* interpret binary input buffer

         ignore ...
           first two bytes: 0xaaab
           second two bytes: length of data packet
           second last two bytes: 16-bit checksum
           last two bytes: 0xabaa
       */
      s_checksum = GETUINT16(s_binp, 924);

      for (i=4; i < STATUS_PACKET_VALID_LENGTH-4; i+=4) {
        param_id[i/4] = GETUINT16(s_binp, i);
        param_value[i/4] = GETUINT16(s_binp, i+2);
      }

      /*
       * If CID was given as 0, then look at the first packet
       * and reassign cid to the controller ID found in that packet.
       * This is likely the default case when only one CS800 controller
       * is on the network.  No need to identify the controller by 
       * number since it is the only one.
       */
      s_cid = getParam(PARAM_CTRLNUM, param_id, param_value);
      if (cid == 0) {
        cid = s_cid;
        printf("(%s) choosing controller ID: %d\n", macValueGet("name"), cid);
      }
    } state validate

    when (delay(IDLE_POLL_INTERVAL)) {} state idle
  }


  /*
   * park here while user has disabled this software
   */
  state ignore {
    when (!strcmp(current_state, "ignore")) {
      CHANGE_STATE("ignore")
    } state ignore

    when (!disable) {} state idle

    when (delay(IDLE_POLL_INTERVAL)) {} state ignore
  }


  state validate { 
    when (!strcmp(current_state, "validate")) {
      CHANGE_STATE("validate")
    } state validate

    when (disable) {} state ignore

    when (cid < 1) {} state idle

    when (cid != s_cid) {
        // DEBUG_PRINT2(DEBUGLEVEL_DEBUG, "controller ID mismatch: expected=%d received=%d", cid, s_cid);
    } state idle

    when () {} state process
  }


  state process { 
    when (!strcmp(current_state, "process")) {
      CHANGE_STATE("process")
    } state process

    when (disable) {} state ignore

    when () {
      CHECK_PVPUT(setpoint,     PARAM_SETPOINT);
      CHECK_PVPUT(temperature,  PARAM_TEMP);
      CHECK_PVPUT(run_mode,     PARAM_RUNMODE);
      CHECK_PVPUT(phase,        PARAM_PHASE);

      CHECK_PVPUT(devicetype, PARAM_DEVICETYPE);
      CHECK_PVPUT(devicesubtype, PARAM_DEVICESUBTYPE);
      CHECK_PVPUT(devicemintemp, PARAM_DEVICEMINTEMP);
      CHECK_PVPUT(devicemaxtemp, PARAM_DEVICEMAXTEMP);
      CHECK_PVPUT(ver, PARAM_VER);
      CHECK_PVPUT(ramprate, PARAM_RAMPRATE);
      CHECK_PVPUT(targettemp, PARAM_TARGETTEMP);
      CHECK_PVPUT(evaptemp, PARAM_EVAPTEMP);
      CHECK_PVPUT(sucttemp, PARAM_SUCTTEMP);
      CHECK_PVPUT(remaining, PARAM_REMAINING);
      CHECK_PVPUT(gasflow, PARAM_GASFLOW);
      CHECK_PVPUT(gasheat, PARAM_GASHEAT);
      CHECK_PVPUT(evapheat, PARAM_EVAPHEAT);
      CHECK_PVPUT(suctheat, PARAM_SUCTHEAT);
      CHECK_PVPUT(backpress, PARAM_BACKPRESS);
      CHECK_PVPUT(runtime, PARAM_RUNTIME);
      CHECK_PVPUT(alarmi, PARAM_ALARMI);
      CHECK_PVPUT(ctrlnum, PARAM_CTRLNUM);
      CHECK_PVPUT(evapadjust, PARAM_EVAPADJUST);
      CHECK_PVPUT(turbomode, PARAM_TURBOMODE);
      CHECK_PVPUT(valveopening, PARAM_VALVEOPENING);
      CHECK_PVPUT(level, PARAM_LEVEL);
      CHECK_PVPUT(error, PARAM_ERROR);
      CHECK_PVPUT(coldheadtemp, PARAM_COLDHEADTEMP);
      CHECK_PVPUT(shieldtemp, PARAM_SHIELDTEMP);
      CHECK_PVPUT(vacgauge, PARAM_VACGAUGE);
      CHECK_PVPUT(nozzletemp, PARAM_NOZZLETEMP);
      CHECK_PVPUT(sampleheat, PARAM_SAMPLEHEAT);
      CHECK_PVPUT(coldheadheat, PARAM_COLDHEADHEAT);
      CHECK_PVPUT(shieldheat, PARAM_SHIELDHEAT);
      CHECK_PVPUT(nozzleheat, PARAM_NOZZLEHEAT);
      CHECK_PVPUT(avgsampleheat, PARAM_AVGSAMPLEHEAT);
      CHECK_PVPUT(avgnozzleheat, PARAM_AVGNOZZLEHEAT);
      CHECK_PVPUT(samholdtemp, PARAM_SAMHOLDTEMP);
      CHECK_PVPUT(cryostattemp, PARAM_CRYOSTATTEMP);
      CHECK_PVPUT(samholdpres, PARAM_SAMHOLDPRES);
      CHECK_PVPUT(elapsed, PARAM_ELAPSED);
      CHECK_PVPUT(suctsettemp, PARAM_SUCTSETTEMP);
      CHECK_PVPUT(nozzlesettemp, PARAM_NOZZLESETTEMP);
      CHECK_PVPUT(collartemp, PARAM_COLLARTEMP);
      CHECK_PVPUT(vacsensor, PARAM_VACSENSOR);

      if (alarmi == 0) {PVPUTSTR(alarm, "No error")}
      else if (alarmi == 1) {PVPUTSTR(alarm, "Stop Pressed")}
      else if (alarmi == 2) {PVPUTSTR(alarm, "Stop Command")}
      else if (alarmi == 3) {PVPUTSTR(alarm, "End Complete")}
      else if (alarmi == 4) {PVPUTSTR(alarm, "Purge Complete")}
      else if (alarmi == 5) {PVPUTSTR(alarm, "Temp Warning")}
      else if (alarmi == 6) {PVPUTSTR(alarm, "Pressure Warning")}
      else if (alarmi == 7) {PVPUTSTR(alarm, "Check Vacuum")}
      else if (alarmi == 8) {PVPUTSTR(alarm, "Self-check Fail")}
      else if (alarmi == 9) {PVPUTSTR(alarm, "Flow Rate Fail")}
      else if (alarmi == 10) {PVPUTSTR(alarm, "Temp Control Error")}
      else if (alarmi == 11) {PVPUTSTR(alarm, "Gas Type Error")}
      else if (alarmi == 12) {PVPUTSTR(alarm, "Temp Reading Error")}
      else if (alarmi == 13) {PVPUTSTR(alarm, "Suct Temp Error")}
      else if (alarmi == 14) {PVPUTSTR(alarm, "Sensor Fail")}
      else if (alarmi == 15) {PVPUTSTR(alarm, "Brownout")}
      else if (alarmi == 16) {PVPUTSTR(alarm, "Sink Overheat")}
      else if (alarmi == 17) {PVPUTSTR(alarm, "PSU Overheat")}
      else if (alarmi == 18) {PVPUTSTR(alarm, "Power Loss")}
      else if (alarmi == 19) {PVPUTSTR(alarm, "Coldhead Too Cold")}
      else if (alarmi == 20) {PVPUTSTR(alarm, "Coldhead Timeout")}
      else if (alarmi == 21) {PVPUTSTR(alarm, "Cryodrive Not Found")}
      else if (alarmi == 22) {PVPUTSTR(alarm, "Cryodrive Error")}
      else if (alarmi == 23) {PVPUTSTR(alarm, "No Nitrogen")}
      else if (alarmi == 24) {PVPUTSTR(alarm, "No Helium")}
      else if (alarmi == 25) {PVPUTSTR(alarm, "Vac Gauge Fail")}
      else if (alarmi == 26) {PVPUTSTR(alarm, "Vac Reading Error")}
      else if (alarmi == 27) {PVPUTSTR(alarm, "RS232 Error")}
      else if (alarmi == 28) {PVPUTSTR(alarm, "Coldhead Temp Warning")}
      else if (alarmi == 29) {PVPUTSTR(alarm, "Coldhead Temp Error")}
      /* TODO: verify next two messages, they look the same */
      else if (alarmi == 30) {PVPUTSTR(alarm, "Do Not Open Cryostate")}
      else if (alarmi == 31) {PVPUTSTR(alarm, "Do Not Open Cryostat")}
      else if (alarmi == 32) {PVPUTSTR(alarm, "Unplug Xtal Sensor")}
      else if (alarmi == 33) {PVPUTSTR(alarm, "Cryostat Open")}
      else if (alarmi == 34) {PVPUTSTR(alarm, "High Temp Warning")}
      else if (alarmi == 35) {PVPUTSTR(alarm, "High Temp Error")}
      else if (alarmi == 36) {PVPUTSTR(alarm, "Cryodrive T Sensor Fault")}
      else if (alarmi == 37) {PVPUTSTR(alarm, "Cryodrive P Sensor Fault")}
      else if (alarmi == 38) {PVPUTSTR(alarm, "Cryodrive Low T Trip")}
      else if (alarmi == 39) {PVPUTSTR(alarm, "Cryodrive High T Trip")}
      else if (alarmi == 40) {PVPUTSTR(alarm, "Cryodrive Low P Trip")}
      else if (alarmi == 41) {PVPUTSTR(alarm, "Cryodrive High T Warning")}
      else if (alarmi == 42) {PVPUTSTR(alarm, "Cryodrive Low P Warning")}
      else if (alarmi == 43) {PVPUTSTR(alarm, "Connect Gas Supply")}
      else if (alarmi == 44) {PVPUTSTR(alarm, "Autofill Fault")}
      else if (alarmi == 45) {PVPUTSTR(alarm, "Autofill About To Fill")}
      else if (alarmi == 46) {PVPUTSTR(alarm, "Autofill Filling")}
      else if (alarmi == 47) {PVPUTSTR(alarm, "Collar Temp Error")}
      else if (alarmi == 48) {PVPUTSTR(alarm, "Coldhead Error")}
      else if (alarmi == 49) {PVPUTSTR(alarm, "Turbo Flow")}
      else if (alarmi == 50) {PVPUTSTR(alarm, "He Selected")}
      else if (alarmi == 51) {PVPUTSTR(alarm, "Cryodrive Not Ready")}
      else if (alarmi == 52) {PVPUTSTR(alarm, "Regen Required")}
      else if (alarmi == 53) {PVPUTSTR(alarm, "Regen Complete")}
      else if (alarmi == 54) {PVPUTSTR(alarm, "Connect Vacuum")}
      else if (alarmi == 55) {PVPUTSTR(alarm, "Disconnect Vacuum")}
      else {PVPUTSTR(alarm, "not recognized")}
    } state idle
  }

}


%{

int getParam(int id, int *param_id, unsigned int *param_value) {
  int i;
  for (i=0; i<NUMBER_OF_PARAMETERS; i++)
    if (param_id[i] == id)
      return param_value[i];

  return NOT_FOUND;
}

}%
