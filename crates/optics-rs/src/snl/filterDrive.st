program filterDrive("NAME=filterDrive, P=TEST:, R=filter:, NUM_FILTERS=8")

/*============================================================================
**  filterDrive.st
**  ==============
**
**  SNL program to drive a set of beamline filters.
**
**  The code works for a variable number of filter blades, each of which may
**  be enabled/disabled and locked/freed individually. Disabling a filter
**  blade removes it completely from the transmission calculation, while
**  locking a blade keeps it in its current position (in our out) and does
**  not allow it to change when a different transmission is required, but
**  always includes this blade in the calculation of the total transmission.
**
**  The $(P)$(R):TransmissionSetpoint, (P)$(R):TransmissionStepUp and 
**  (P)$(R):TransmissionStepDown records are monitored and, every time one of
**  them changes, the filters are moved in and out accordingly.
**
**  The following macro substitutions are required:
**    NAME:        The name of the filterDrive program.
**                 e.g.: NAME="Sector 33-ID-D filters"
**    P:           The IOC prefix for the filterDriver records.
**                 e.g.: P="ioc33idd:"
**    R:           The common record name for the filterDriver records.
**                 e.g.: R="filter:"
**    NUM_FILTERS: The number of filter blades used in the calculations.
**                 e.g.: NUM_FILTERS = 8
**
**  The filter blade records must be named with the following naming
**  scheme, and numbered (N) consecutively starting from 1:
**    $(P)$(R)Fi$(N)
**
**  The filter transmission calculations are based on the filter material
**  and thickness, using the Chantler Tables to interpolate the material's
**  absorption coefficient for the requested energy:
**    C.T. Chantler, Journal of Physical and Chemical Reference Data 24,
**    71 (1995).
**
**  Note: This code is based both on the filter code from the X04SA beamline
**        at the Swiss Light Source, originally written by D. Maden, and the
**        pf4 filter code from synApps with major contributions from
**        J. Tischler, T. Mooney, and P. Jemian.
**
**  Created: 2013-01-16 by C.M. Schlepuetz @ APS
** 
**  $Revision: 5064 $
**  $Date: 2013-02-08 12:06:55 -0600 (Fri, 08 Feb 2013) $
**  $Author: cschlep $
**  $URL: https://subversion.xray.aps.anl.gov/bcdaioc/33bmTEST/33bmTESTApp/src/filterDrive.st $
**==========================================================================*/

/* Ensure we are translated reentrantly */
option +r;

%%#include <string.h>
%%#include <math.h>
%%#include "chantler.h"

/* Maximum number of filters supported.  Note MAX_PERMS = 1<<MAX_ENABLED */
#define MAX_FILTERS 32
#define MAX_ENABLED 16
#define MAX_PERMS 65536
#define SMALL_FRAC 1e-8


/*----------------------------------------------------------------------------
**  Declare variables and assign them to channels
*/

/* The global busy record */
int STATUS_BUSY;
assign STATUS_BUSY to "{P}{R}Status";
monitor STATUS_BUSY;

/* The number of filters */
int NFILTERS;
assign NFILTERS to "{P}{R}NumFilters";

/* Debug Level:
** If non-zero, it causes debug printout:
**     Bit 0 (0x1) --> general debug info.
**     Bit 1 (0x2) --> global transmission calculation and sorting.
**     Bit 2 (0x4) --> filter blade transmission calculation.
**     Bit 3 (0x8) --> filter blade interpolation steps.
*/
int DEBUG;
assign DEBUG to "{P}{R}Debug";
monitor DEBUG;

/* The transmission readback value */
double TRANSMISSION;
assign TRANSMISSION to "{P}{R}Transmission";

/* Next higher transmission readback value */
double TRANSMISSION_UP;
assign TRANSMISSION_UP to "{P}{R}TransmissionUp";

/* Next higher transmission readback value */
double TRANSMISSION_DOWN;
assign TRANSMISSION_DOWN to "{P}{R}TransmissionDown";

/* The FilterMask readback value */
int FILTER_MASK;
assign FILTER_MASK to "{P}{R}FilterMask";

/* The transmission setpoint*/
evflag  EvFlgTRANSM_SETPT;
double  TRANSM_SETPT;                        
assign  TRANSM_SETPT to "{P}{R}TransmissionSetpoint";
monitor TRANSM_SETPT;
sync    TRANSM_SETPT EvFlgTRANSM_SETPT;

/* The FilterMask setpoint*/
evflag  EvFlgFILTERMASK_SETPT;
int     FILTERMASK_SETPT;                        
assign  FILTERMASK_SETPT to "{P}{R}FilterMaskSetpoint";
monitor FILTERMASK_SETPT;
sync    FILTERMASK_SETPT EvFlgFILTERMASK_SETPT;

/* Transmission step up */
int     TRANSM_UP;                          
assign  TRANSM_UP to "{P}{R}TransmissionStepUp";
monitor TRANSM_UP;

/*Transmission  step down */
int     TRANSM_DN;                          
assign  TRANSM_DN to "{P}{R}TransmissionStepDown";
monitor TRANSM_DN;

evflag  needRecalc;
evflag  newEnergy;

/* The beam energy */
double ENERGY;
assign ENERGY to "{P}{R}Energy";
monitor ENERGY;
sync ENERGY newEnergy;

/* Diagnostic message */
string  MSGE;
assign  MSGE to "{P}{R}Message";

/* Wait time for filters to be inserted/removed */
double FILTER_WAIT_TIME;
assign FILTER_WAIT_TIME to "{P}{R}WaitTime";

/* Define array of transmissions */
double  TRANSM[MAX_FILTERS];
assign  TRANSM to {};

/* Define array of thicknesses */
double  THICK[MAX_FILTERS];
assign  THICK to {};
monitor THICK;
sync THICK needRecalc;

/* Define array of filter materials */
string  MATER[MAX_FILTERS];
assign  MATER to {};
monitor MATER;
sync MATER needRecalc;

/* Define array for LOCK status of filters */
int     LOCK[MAX_FILTERS];
assign  LOCK to {};
monitor LOCK;
sync LOCK needRecalc;

/* Define array of filters activators */
int     SET[MAX_FILTERS];
assign  SET to {};
monitor SET;

/* Define array for the enable/disable status of the filters */
int     ENBL[MAX_FILTERS];
assign  ENBL to {};
monitor ENBL;
sync ENBL needRecalc;

/* Define array for the output channels, UNUSED */
/*
**int     OUT[MAX_FILTERS];
**assign  OUT to {};
*/

/* Define array for the readback of the output channels */
int     OUTGET[MAX_FILTERS];
assign  OUTGET to {};
                
evflag  externalIO;
monitor OUTGET;
sync OUTGET externalIO;

char     *name;
char     *prefix;
char     *record;
int      changeFlag;
int      doWait;
int      nFltrs;
int      nEnabled;
int      nPerms;
int      ind;
int      indNow;
int      fltrsInNow;
double   transNow;
int      indUp;
int      fltrsInUp;
double   transUp;
int      indDown;
int      fltrsInDown;
double   transDown;
double   trans;
int      indMin;
double   transMin;
int      i;
int      j;
int      k;
int      myAction;
int      fltrsReq;
int      fltrsSetpt;
int      maxMask;
char     str[64];
double   transMis[MAX_PERMS];
int      fltrsIn[MAX_PERMS];
string   buff0;
string   buff1;
  

/*============================================================================
** C-code (enclosed in "%{" ... "}%")
**==========================================================================*/
%{

/*----------------------------------------------------------------------------
**  cvtToBin - convert an integer to a binary representation.
**             The converted string has least sig bit on left
**             (to match the order of filters in beam).
**--------------------------------------------------------------------------*/
static char *cvtToBin(char *buff,   /* Buffer for result */
                      int   val,    /* Value to convert */
                      int   nBits)  /* Number of bits to convert */
{  
  int     i, nxtBit, nxtChar;
  
  nxtChar = 0;
  for (i = 0; i < nBits; i++) {
    nxtBit = 1 << (nBits - i - 1);
    if ((i != 0) && ((i & 0x0003) == 0)) buff[nxtChar++] = '.';

    if (val & nxtBit) {
      buff[nxtChar++] = '1';
      val &= ~nxtBit;
    }else {
      buff[nxtChar++] = '0';
    }
  }
  buff[nxtChar] = '\0';
  if (val != 0) {
    for (i = 0; i < nxtChar; i++) buff[i] = '*';
  }

  return buff;
}

/*--------------------------------------------------------------------------*/
static double calcTrans(double keV,
                        double thickness,
                        char *species,
                        int debug)
{
  int i, j;
  double mu, frac, absLen, transm;

  for(i=0; i<NUM_SPECIES; i++) {
    if (strcmp(filtermat[i].name, species) == 0) break; 
  }
  if(i >= NUM_SPECIES) {
    if(debug) printf("Filter material '%s' not found\n", species);
    return(0.);
  }
  if(debug & 4) printf("  species = '%s', index = %d\n", species, i);
  if(debug & 4) printf("  density = %.4e gm/cm3, thickness = %.4e um\n", \
      matdensity[i], thickness);
  if(debug & 4) printf("  energy = %.4e\n", keV);

  for(j=0; j<filtermat[i].numEntries; j++) {
    if (keV < filtermat[i].keV[j]) break; 
  }
  if((j < 1) | (j >= filtermat[i].numEntries)) {
    if(debug) printf("Energy '%f' not found in filter data\n", keV);
    return(0.);
  }

  /* linear interpolation of mu */
  /* mu is in cm^2/g, density is g/cm^3, want absorption length in microns,
  ** then calculate transmission */
  frac = (keV - filtermat[i].keV[j-1]) / (filtermat[i].keV[j] - filtermat[i].keV[j-1]);
  mu = filtermat[i].mu[j-1] + frac * (filtermat[i].mu[j] - filtermat[i].mu[j-1]);
  absLen = 1.0/(matdensity[i]*mu);
  absLen *= 1.e4; /* cm to microns */
  transm = exp(-(thickness/absLen));

  if(debug & 8){
    printf("  E1 = %.4e keV, mu1 = %.4e\n", \
        filtermat[i].keV[j-1], filtermat[i].mu[j-1]);
    printf("  E2 = %.4e keV, mu2 = %.4e\n", \
        filtermat[i].keV[j], filtermat[i].mu[j]);
    printf("  frac = %.4f, mu = %.4e\n", frac, mu);
  }

  if(debug & 4){
    printf("  attenuation length ='%.4e' um, transmission=%.4e\n", absLen, transm);
  }

  return(transm);
}

}%


/*============================================================================
** The State Notation Language code
**==========================================================================*/

  ss filterDrive {
/*==============*/

/*-------------*/
    state start {
      when() {
        /* Show that we are starting up. */
        changeFlag = 1;
        
        /* Wait a bit to let things settle down */
        epicsThreadSleep(1.0);

        /* Assign the macro substitution values */
        name = macValueGet("NAME");
        prefix = macValueGet("P");
        record = macValueGet("R");
        sscanf(macValueGet("NUM_FILTERS"), "%d", &nFltrs);

        /* C defect fixed: C assigned and indexed THICK..OUTGET, which hold
        ** MAX_FILTERS blades, up to NUM_FILTERS. */
        if(nFltrs > MAX_FILTERS){
          printf("%s: ERROR: Too many filters (%d/%d).\n", name, nFltrs, MAX_FILTERS);
          nFltrs = MAX_FILTERS;
        }

        /* C defect fixed: C took the highest valid mask as (1 << nFltrs) - 1,
        ** which overflows the int from 31 blades and shifts by its width at
        ** MAX_FILTERS. */
        if(nFltrs >= 31) maxMask = 0x7fffffff;
        else if(nFltrs > 0) maxMask = (1 << nFltrs) - 1;
        else maxMask = 0;
        
        /* dynamically assign the PV channels to database variables */
        for(i = 0; i < nFltrs; i++) {
          sprintf(str, "%s%sFi%d:Thickness", prefix, record, i+1);
          pvAssign(THICK[i], str);
          sprintf(str, "%s%sFi%d:Material", prefix, record, i+1);
          pvAssign(MATER[i], str);
          sprintf(str, "%s%sFi%d:Transmission", prefix, record, i+1);
          pvAssign(TRANSM[i], str);
          sprintf(str, "%s%sFi%d:Lock", prefix, record, i+1);
          pvAssign(LOCK[i], str);
          sprintf(str, "%s%sFi%d:Set", prefix, record, i+1);
          pvAssign(SET[i], str);
          sprintf(str, "%s%sFi%d:Enable", prefix, record, i+1);
          pvAssign(ENBL[i], str);
          /*
          **sprintf(str, "%s%sFi%d:Set", prefix, record, i+1);
          **pvAssign(OUT[i], str);
          */
          sprintf(str, "%s%sFi%d:OutGet", prefix, record, i+1);
          pvAssign(OUTGET[i], str);
          
        }

        NFILTERS = nFltrs;
        pvPut(NFILTERS);

        printf("%s: SNL program started for %s%s\n", name, prefix, record);
        printf("%s:   # of filters = %d\n", name, nFltrs);

        /* Wait a bit again to let things settle down */
        epicsThreadSleep(1.0);

      } state init
    }
    

/*------------*/
    state init {
      /* Ensure everything is connected */
      when(pvConnectCount() == pvAssignCount()) {
    
        epicsThreadSleep(2.0);

        sprintf(MSGE, "Initialising ..."); pvPut(MSGE); strcpy(MSGE, "");
        efClear(EvFlgTRANSM_SETPT);
        TRANSM_UP = 0; pvPut(TRANSM_UP);
        TRANSM_DN = 0; pvPut(TRANSM_DN);
        efSet(needRecalc);
      } state waiting
    }


/*---------------*/
    state waiting {

      /* Connection was lost */
      when(pvConnectCount() != pvAssignCount()) {
        printf ("%s: channel connection lost.\n", name);
      } state init
      
      /* Energy changed */
      when(efTestAndClear(newEnergy)) {
        if (DEBUG & 0x1) printf("%s: Energy change detected\n", name);
      } state update_filters

      /* Configuration changed - recalculation required */
      when(efTestAndClear(needRecalc)) {
        if (DEBUG & 0x1) printf("%s: Configuration change detected\n", name);
      } state update_filters

      /* Transmission setpoint changed */
      when(efTestAndClear(EvFlgTRANSM_SETPT)) {
        if(DEBUG & 0x1) printf("%s: transmission setpoint changed.\n", name);
        myAction = 0;
      } state action

      /* Transmission step up */
      when(TRANSM_UP) {
        TRANSM_UP = 0;
        pvPut(TRANSM_UP);
        if(DEBUG & 0x1) printf("%s: step up.\n", name);
        myAction = 1;
      } state action

      /* Transmission step down */
      when(TRANSM_DN) {
        TRANSM_DN = 0;
        pvPut(TRANSM_DN);
        if(DEBUG & 0x1) printf("%s: step down.\n", name);
        myAction = 2;
      } state action

      /* Filter Mask Setpoint changed */
      when(efTestAndClear(EvFlgFILTERMASK_SETPT)) {
        if(DEBUG & 0x1) printf("%s: filter mask setpoint changed.\n", name);
        myAction = 3;
      } state action      

      /* Something triggered the status to be busy */
      when(STATUS_BUSY){
        if (DEBUG & 0x1) printf("%s: Busy record was set\n", name);
        changeFlag = 1;
      } state update_filters

      /* Filters were changed by external agent */
      when (efTestAndClear(externalIO)) {
        if (DEBUG & 0x1) printf("%s: External IO event flag was set\n", name);
      } state update_filters

    }


/*----------------------*/
    state update_filters {
      entry {
        if (DEBUG & 0x1) printf("%s: entering state: update_filters\n", name);
        pvPut(MSGE); strcpy(MSGE, "OK");
      }

      when(){
        /* Make sure the physical binary PVs of the actuators
        ** are in sync with the SET bo PVs */
        for(i = 0; i < nFltrs; i++) {
          pvGet(OUTGET[i]);
          efClear(externalIO);  /* Is this needed?  It probably should not be. */
          pvGet(SET[i]);
          if (SET[i] != OUTGET[i]) {
            /* We mustn't write to SET[i] unless OUTGET[i] is different, because
	     * writing processes, which sets STATUS_UPDATE, which brings us back
             * here, causing an infinite loop.
             */
            SET[i] = OUTGET[i];
            pvPut(SET[i]);
            sprintf(MSGE, "Filter state set by external agent.");
            pvPut(MSGE);
          }
        }

        /* Update the readback values */
        transNow = 1.0;
        fltrsInNow = 0;
        nEnabled = 0;
        for(i = 0; i < nFltrs; i++) {
          /* calculate the individual filter blade transmissions */
          if(DEBUG & 0x4){
            printf("%s: Calculating transmission for blade %d.\n", name, i);
          }
          TRANSM[i] = calcTrans(ENERGY, THICK[i], MATER[i], DEBUG);
          pvPut(TRANSM[i]);

          /*  update combined transmission, enable count, ... */
          if(SET[i] && ENBL[i]){            
            fltrsInNow |= (1 << i);
            transNow *= TRANSM[i];
          }
          if((LOCK[i] == 0) && ENBL[i]) {
            nEnabled += 1;
          }
        }

        TRANSMISSION = transNow; pvPut(TRANSMISSION);
        FILTER_MASK = fltrsInNow; pvPut(FILTER_MASK);

        /* Can only deal with up to MAX_ENABLED unlocked filters */
        if(nEnabled > MAX_ENABLED){
          if(DEBUG & 0x1){
            printf("%s: ERROR: Too many filters unlocked (%d/%d).\n",
                name, nEnabled, MAX_ENABLED);
          }
          sprintf(MSGE, "ERROR: Too many unlocked (%d/%d)", nEnabled, MAX_ENABLED);
          pvPut(MSGE);
          /* C defect fixed: C went on to fill 1 << nEnabled patterns into
          ** transMis/fltrsIn, which hold MAX_PERMS. The table is left empty,
          ** so no action selects a pattern. */
          nPerms = 0;
        } else {
          nPerms = 1 << nEnabled;
        }

        /* Calculate array of all possible transmissions
        ** and the corresponding pattern of filters.
        */
        for(i = 0; i < nPerms; i++){
          transMis[i] = 1.0;
          fltrsIn[i]  = 0;
          k           = 0;
          for(j = 0; j < nFltrs; j++){
            if(ENBL[j]){
              if(LOCK[j] == 1){
                if(SET[j]){
                  /* Take transmission into account for locked inserted filters */
                  transMis[i] *= TRANSM[j];
                  fltrsIn[i] |= (1 << j);
                }
              } else {
                if(i & (1 << k)){
                  transMis[i] *= TRANSM[j];
                  fltrsIn[i] |= (1 << j);
                }
                k++;
              }
            }
          }
        }
        if(DEBUG & 0x2){
          printf("%s: <#-enabled>=%d, <#-permutations>=%d, transmission=%e\n", 
              name, nEnabled, nPerms, transNow);
          for(i = 0; ((i < nPerms) && (i < 100)); i++){
            printf("  %s: %s    %e\n",
                cvtToBin(buff0, i, nEnabled),
                cvtToBin(buff1, fltrsIn[i], nFltrs),
                transMis[i]);
          }
        }
        
        /* Find the current filter combination index */
        indNow = -1;
        for(ind = 0; ind < nPerms; ind++) {
          if(transMis[ind] == transNow) indNow = ind;
        }
        if(indNow < 0){
          printf("ERROR: Could not find current filter index\n");
        }
        if(DEBUG & 0x2){
          printf("%s: Current index = %d\n", name, indNow);
        }
    
        /* Update the transmission up readback values and index */
        indUp = -1;
        transUp = 10.0;
        for(ind = 0; ind < nPerms; ind++) {
          if( (transMis[ind] > (transNow * (1 + SMALL_FRAC))) &
              (transMis[ind] < transUp) ){
            transUp = transMis[ind];
            indUp = ind; 
          }
        }
        if(transUp > 1.0){
          transUp = transNow;
          indUp = indNow;
        }
        /* C defect fixed: indUp is -1 when the current pattern was not
        ** found, and C read fltrsIn[-1]. */
        if(indUp >= 0) fltrsInUp = fltrsIn[indUp];
        TRANSMISSION_UP = transUp;
        pvPut(TRANSMISSION_UP);
        if(DEBUG & 0x2){
          printf("%s: Filter up: index = %d, transm = %e\n",
	          name, indUp, transUp);
        }

        /* Update the transmission down readback values and index */
        indDown = -1;
        transDown = -1.0;
        for(ind = 0; ind < nPerms; ind++) {
          if( (transMis[ind] < (transNow * (1 - SMALL_FRAC))) &	
              (transMis[ind] > transDown) ){
            transDown = transMis[ind];
            indDown = ind; 
          }
        }
        if(transDown < 0.0){
          transDown = transNow;
          indDown = indNow;
        }
        /* C defect fixed: indDown is -1 when the current pattern was not
        ** found, and C read fltrsIn[-1]. */
        if(indDown >= 0) fltrsInDown = fltrsIn[indDown];
        TRANSMISSION_DOWN = transDown;
        pvPut(TRANSMISSION_DOWN);        
        if(DEBUG & 0x2){
          printf("%s: Filter down: index = %d, transm = %e\n",
              name, indDown, transDown);
        }

        /* Clear the busy record */
        if(changeFlag){
          if(DEBUG & 0x1) printf("%s: clearing busy record.\n", name);
          changeFlag = 0;

          STATUS_BUSY = 0;
          pvPut(STATUS_BUSY);
        }

      } state waiting
    }


/*--------------*/
    state action {

      when() {

        strcpy(MSGE, ""); pvPut(MSGE);

        /* signal that we are changing something */
        changeFlag = 1;

        if(DEBUG & 0x1){
          sprintf(MSGE, "Action = %d", myAction); pvPut(MSGE);
          printf("Action = %d\n", myAction);
          /*epicsThreadSleep(1.0);*/
        }
        strcpy(MSGE, "");      

        if(myAction == 0) {

          /* Set the transmission to the next lower or equal transmission as
          ** requested in the setpoint
          */
          if(DEBUG & 0x1){
            printf("%s: Required transmission = %e\n", name, TRANSM_SETPT);
          }

          trans = -1.0;
          transMin = 1.0;
          /* C defect fixed: with no pattern below 1.0, C took indMin from an
          ** earlier table, which can be past this one's end; none is taken. */
          indMin = -1;
          for(k = 0; k < nPerms; k++) {
            if( (transMis[k] <= (TRANSM_SETPT)) &
                (transMis[k] > trans) ){
              ind = k;
              trans = transMis[k];
            }
            if(transMis[k] < transMin){
              transMin = transMis[k];
              indMin = k;
            }
          }
          if(trans < transMin){
            ind = indMin;
          }

          if(DEBUG & 0x1) {
            printf("%s: New filter index: %d\n", name, ind);
            if(ind >= 0) printf("%s: Selected filters are: %s   Transmission = %e\n",
              name, cvtToBin(buff1, fltrsIn[ind], nFltrs), transMis[ind]);
          }

          if((ind < 0) || (transMis[ind] == transNow)){
            if(DEBUG & 0x1) printf("NO CHANGE! Current settings are best.\n");
            strcpy(MSGE, "OK - No change");
            pvPut(MSGE);
          } else {
            strcpy(MSGE, "OK"); pvPut(MSGE);
          }
		  
        } else if(myAction == 1){

          /* Set the transmission to the next available step up */

          ind = indUp;
          if(transUp == transNow) {
            strcpy(MSGE, "NO CHANGE! Step unavailable.");
            pvPut(MSGE);
            if(DEBUG & 0x1) printf("%s: No more steps up available!\n", name);
          } else {
            strcpy(MSGE, "OK"); pvPut(MSGE);
          }
          /* C defect fixed: ind is -1 when the current pattern was not
          ** found, and C printed fltrsIn[-1]. */
          if((DEBUG & 0x1) && (ind >= 0)){
            printf("%s: Next filter pattern is %s,  Transmission = %e\n",
            name, cvtToBin(buff1, fltrsIn[ind], nFltrs), transMis[ind]);
          }

        } else if(myAction == 2) {

          /* Set the transmission to the next available step down */
 
          ind = indDown;
          if(transDown == transNow) {
            strcpy(MSGE, "NO CHANGE! Step unavailable.");
            pvPut(MSGE);
            if(DEBUG & 0x1) printf("%s: No more steps down available!\n", name);
          } else {
            strcpy(MSGE, "OK"); pvPut(MSGE);
          }
          /* C defect fixed: ind is -1 when the current pattern was not
          ** found, and C printed fltrsIn[-1]. */
          if((DEBUG & 0x1) && (ind >= 0)){
            printf("%s: Next filter pattern is %s,  Transmission = %e\n",
            name, cvtToBin(buff1, fltrsIn[ind], nFltrs), transMis[ind]);
          }

        } else if(myAction == 3){

          /* Set filter according to the requested filtermask */
          fltrsSetpt = FILTERMASK_SETPT;

          /* Check validity of filterMask */
          if( (fltrsSetpt < 0) | (fltrsSetpt > maxMask) ){
            ind = indNow;
	    strcpy(MSGE, "ERROR! Invalid mask.");
            pvPut(MSGE);
            if(DEBUG & 0x1){
	      printf("%s: ERROR! Invalid filter mask: %d\n", name, fltrsSetpt);
              printf("%s:   Valid masks are in the range (0 - %d)\n",
		 name, maxMask);
            }
	  } else {

            /* Adjust the requested filter mask to ignore locked filters */
            fltrsReq = 0;
            for(j = 0; j < nFltrs; j++){
              if(ENBL[j]){
                if(LOCK[j] == 1){
                  if(SET[j]){
                    /* Take locked and inserted filters into account */
                    fltrsReq |= (1 << j);
                  }
                } else {
                  fltrsReq |= (1 << j) & fltrsSetpt;
                }
              }
            }
            if(DEBUG & 0x1){
              printf("%s: Required filter mask: %x\n", name, fltrsSetpt);
              printf("%s: Adjusted filter mask: %x\n", name, fltrsReq);
            }
	    
            ind = -1;
            for(i = 0; i < nPerms; i++) {
              if(fltrsIn[i] == fltrsReq) ind = i;
            }
            if(ind < 0) {
              printf("%s: Cannot find the requested filter pattern!\n", name);
              strcpy(MSGE, "ERROR! Filter Mask not found.");
              pvPut(MSGE);
            } else {
              strcpy(MSGE, "OK"); pvPut(MSGE);
            }
	    
            /* C defect fixed: C printed fltrsIn[-1] for a mask not found. */
            if((DEBUG & 0x1) && (ind >= 0)){
              printf("%s: Next filter pattern is %s,  Transmission = %e\n",
              name, cvtToBin (buff1, fltrsIn[ind], nFltrs), transMis[ind]);
            }
          }
        }

        /* clear the filter up/down push buttons */
        TRANSM_UP = 0; pvPut(TRANSM_UP);
        TRANSM_DN = 0; pvPut(TRANSM_DN);

      } state change_filters
    }

/*----------------------*/
    state change_filters {
      when() {
        if(ind >=0){

          /* Start by putting in enabled filters */
          doWait = 0;
          for(j = 0; j < nFltrs; j++) {
            if(ENBL[j]){
              if(fltrsIn[ind] & (1 << j)) {
                if(SET[j] == 0) doWait = 1;
                SET[j] = 1; pvPut(SET[j]);
              }
            }
          }

          /* Give them time to get in if something changed */
          if(doWait){
            pvGet(FILTER_WAIT_TIME);
            epicsThreadSleep(FILTER_WAIT_TIME);
          }

          /* Then take out the other enabled filters */
          doWait = 0;
          for(j = 0; j < nFltrs; j++) {
            if(ENBL[j]){
              if((fltrsIn[ind] & (1 << j)) == 0) {
                if(SET[j] == 1) doWait = 1;
                SET[j] = 0; pvPut(SET[j]);
              }
            }
          }

          /* Give them time to get out if something changed*/
          if(doWait){
            pvGet(FILTER_WAIT_TIME);
            epicsThreadSleep(FILTER_WAIT_TIME);
          }
        }
      } state waiting
    }
  }

/*==============================================================================
** End of $ Id: $
==============================================================================*/
