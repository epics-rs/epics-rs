program scanProgress ("S=34ide:, P=34ide:scanProgress:")
#define PROG_BAR_VERSION 2.3
/*
 *	program to provide a progress bar information for a scan record
 *	by Jon Tischler, ORNL
 *	Oct, 2012
 *
 */
/* 
 *	v1.7, added a separate pause for each of the scans, pause1, pause2, pause3, pause4
 *			so if you pause a single scan it will behave as if it is paused
 *
 *	March 2017, added hungSec and hungTimeStr tho show if scan appears to be hung
 */
/*
		usage (VxWorks command):
 ld < ../../lasApp/src/O.mv162/scanProgress.o
 seq &scanProgress
		(or to over-ride the defaults)
 seq &scanProgress, "S=34ide:"

		to kill the running sequence program (VxWorks):
 td "scanProgress"

#  P  = prefix for all of the PV's associated with the progress Bar
#  S  = prefix for all of the PV's associated with the VME containing scan records
 */


/*------------------------------------------------ defines & includes ------------------------------------------------ */
#include "seqPVmacros.h"

%% #include <string.h>
%% #include <epicsThread.h>
%% #include <epicsTime.h>
%% #include <string.h>
%% #include <time.h>
%% #include <limits.h>

#include "seqPVmacros.h"

#define debug_flag scanProgressDebug
#define UPDATE_DELAY 2.				/* time to wait between updates of progress (second) */
#define UPDATE_DELAY_LONG 20.		/* update paused PV this often regardless of anything (second) */

#ifndef NULL
#define NULL 0L
#endif

#define TodayFMT "%H:%M:%S Today (%A)"				/* for today use:		"22:23:44 Today (Wednesday)" */
#define TomorrowFMT "%H:%M:%S Tomorrow (%A)"			/* for tomorrow use:	"22:23:44 Tomorrow (Thursday)" */
#define YesterdayFMT "%H:%M:%S Yesterday (%A)"		/* for yesterday use:	"22:23:44 Yesterday (Tuesday)" */
#define GeneralDateFMT "%H:%M:%S  (%a) %b %d, %Y"		/* for all others use:	"22:23:44  (Thu) Nov  8, 2011" */

/*   ------------------------   assignments   ------------------------ */

PV (short,	scanProgressDebug,	"{P}debug", EvFlag);			/* debug flag */
PV (double,	fractionDone,		"{P}fractionDone", NoMon);		/* fraction of scan finished */
PV (double,	percentDone,		"{P}percentDone", NoMon);		/* percent of scan finished */
PV (string,	startingTimeStr,	"{P}startingTimeStr",	NoMon);	/* string with starting time & date */
PV (string,	endingTimeStr,		"{P}endingTimeStr",	NoMon);		/* string with predicted end time & date */
PV (string,	remainingTimeStr,	"{P}remainingTimeStr", NoMon);	/* string with remaining time */
PV (string,	pauseTimeStr,		"{P}pauseTimeStr", NoMon);		/* string with time spent paused */
PV (string,	totalElapsedTimeStr,"{P}totalElapsedTimeStr", NoMon);/* string with total clock time */
PV (string,	totalActiveTimeStr,	"{P}totalActiveTimeStr", NoMon);/* string time that scan was actively scanning */
PV (int,	pauseSec,			"{P}pauseSec", NoMon);			/* total seconds spent paused */
PV (int,	remainingSec,		"{P}remainingSec", NoMon);		/* seconds remaining in scan */
PV (int,	npts,				"{P}Ntotal", NoMon);			/* total number of scan points in all dimensions */
PV (int,	cpt,				"{P}Nfinished", NoMon);			/* number of scan points completed */
PV (short,	running,			"{P}running", NoMon);			/* flag, scan is running, an echo of (busy1||busy2||busy3||busy4) */
PV (short,	paused,				"{P}paused", NoMon);			/* flag, scan is paused, is true when the scan is actually paused */
PV (int,	Npauses,			"{P}Npauses", NoMon);			/* number of times scan was paused */
PV (int,	hangWaitSec,		"{P}hangWaitSec", EvFlag);		/* must wait this long before declaring a hang, probalby ~10min = 600sec  */
PV (int,	hungSec,			"{P}hungSec", NoMon);			/* seconds spent hung, reset if scan un-hangs, either 0 or >hangWaitSec */
PV (string,	hungTimeStr,		"{P}hungTimeStr", NoMon);		/* string version of hungSec */
PV (double,	version,			"{P}version", NoMon);			/* version number */

PV (int,	npts1,				"{S}scan1.NPTS", NoMon);		/* # of points in scan 1 */
PV (int,	npts2,				"{S}scan2.NPTS", NoMon);		/* # of points in scan 2 */
PV (int,	npts3,				"{S}scan3.NPTS", NoMon);		/* # of points in scan 3 */
PV (int,	npts4,				"{S}scan4.NPTS", NoMon);		/* # of points in scan 4 */
PV (int,	cpt1,				"{S}scan1.CPT", NoMon);			/* current point # in scan 1 */
PV (int,	cpt2,				"{S}scan2.CPT", NoMon);			/* current point # in scan 2 */
PV (int,	cpt3,				"{S}scan3.CPT", NoMon);			/* current point # in scan 3 */
PV (int,	cpt4,				"{S}scan4.CPT", NoMon);			/* current point # in scan 4 */
PV (short,	pause1,				"{S}scan1.PAUS", EvFlag);		/* individual pause for scan1 */
PV (short,	pause2,				"{S}scan2.PAUS", EvFlag);		/* individual pause for scan2 */
PV (short,	pause3,				"{S}scan3.PAUS", EvFlag);		/* individual pause for scan3 */
PV (short,	pause4,				"{S}scan4.PAUS", EvFlag);		/* individual pause for scan4 */
PV (short,	busy1,				"{S}scan1.BUSY", EvFlag);		/* scan 1 is busy */
PV (short,	busy2,				"{S}scan2.BUSY", EvFlag);		/* scan 2 is busy */
PV (short,	busy3,				"{S}scan3.BUSY", EvFlag);		/* scan 3 is busy */
PV (short,	busy4,				"{S}scan4.BUSY", EvFlag);		/* scan 4 is busy */

/*------------------------------------------------ Globals ------------------------------------------------ */
char	*SNLtaskName;
char	new_msg[256];

/* using long instead of time_t for the following 6 variables */
long	predictEnd;		/* predicted epoch of end */
long	startTime;		/* starting time of scan (seconds),  startTime = time(NULL); */
long	pauseStart;		/* epoch of the start of a pause */
long	now;
long	cptAtLastOK;	/* number of points completed at last check, used to check for hang */
long	timeOfLastOK;	/* epoch of when cptAtLastOK was last updated, used to check for hang */ 
char	tStr[100];
char	tStr1[100];
int		beginPause;		/* flags the start of a pause in duringScanPause */
int		almostPaused;
int		stopped;
int		aPause;
int		scanDim;		/* dimension of scan, {1,2,3,4} */

%% static void clocks2str(long isec, char *str);
%% static void formatTimeStr(char *tStr, int tStrLen, time_t final, int full);
%% static long calc_cpt(void);
%% static int allStopped(void);

/*
 * state set:  scanProgress
 */
ss scanProgress {

	state init {
		when () {
			SNLtaskName = macValueGet("name");
			/* C defect: without a name macro, as every upstream st.cmd runs
			 * it, DEBUG_PRINT passes this NULL to %s */
			if (!SNLtaskName) SNLtaskName = "scanProgress";
			scanDim = 1;
			pvGet(scanProgressDebug);
			PVPUT(version,PROG_BAR_VERSION)
			PVPUT(fractionDone,0.0)					/* reset the progress bar PVs */
			PVPUT(percentDone,0.0)
			PVPUTSTR(endingTimeStr,"")
			PVPUTSTR(remainingTimeStr,"")
			PVPUTSTR(pauseTimeStr,"")
			PVPUTSTR(startingTimeStr,"")
			PVPUTSTR(totalElapsedTimeStr,"")
			PVPUTSTR(totalActiveTimeStr,"")
			PVPUTSTR(hungTimeStr,"")
			PVPUT(hungSec,0)
			PVPUT(pauseSec,0)
			PVPUT(remainingSec,0)
			PVPUT(npts,0)
			PVPUT(cpt,0)
			PVPUT(Npauses,0)
			PVPUT(running,0)
			pvGet(hangWaitSec);
			pvGet(busy1);
			pvGet(busy2);
			pvGet(busy3);
			pvGet(busy4);
			beginPause = 0;
			cptAtLastOK = 0;
			efClear(scanProgressDebug_mon);			/* clear all event flags */
			efClear(hangWaitSec_mon);
			DEBUG_PRINT(1, "init complete in state init");
		} state idle
	}


	state idle {									/* main program loop */
		when (efTest(scanProgressDebug_mon)) {		/* debug level changed */
			sprintf(new_msg, "changed debug flag to %d", scanProgressDebug);
			DEBUG_PRINT(1, new_msg);
			efClear(scanProgressDebug_mon);
		} state idle

		when (efTest(hangWaitSec_mon)) {			/* length of time do declare a hang changed */
			sprintf(new_msg, "changed requred time for a hang to %d (sec)", hangWaitSec);
			DEBUG_PRINT(1, new_msg);
			efClear(hangWaitSec_mon);
		} state idle

		when (busy1||busy2||busy3||busy4) {			/* a scan has started */
			DEBUG_PRINT(1, "scan started");
			if (busy4) scanDim=4;					/* largest dimension of this scan */
			else if (busy3) scanDim=3;
			else if (busy2) scanDim=2;
			else scanDim=1;
			sprintf(new_msg, "got scanDim = %d", scanDim);
			DEBUG_PRINT(2, new_msg);
%%			startTime = time(NULL);					/* set startTime to now */
%%			formatTimeStr(startingTimeStr,40,startTime,1);
			pvPut(startingTimeStr);
			PVPUT(Npauses,0)
			PVPUT(pauseSec,0)
			PVPUT(fractionDone,0.0)
			PVPUT(percentDone,0.0)
			PVPUT(running,1)
			PVPUTSTR(totalElapsedTimeStr,"00:00:00")
			PVPUTSTR(totalActiveTimeStr,"00:00:00")
			PVPUTSTR(pauseTimeStr,"00:00:00")
			pvGet(npts1);
			npts = npts1;							/* npts will be the total number of points in all active scans */
			if (scanDim>=2) {						/* running the second dimension */
				pvGet(npts2);
				npts *= npts2;
				if (scanDim>=3) {					/* running the third dimension */
					pvGet(npts3);
					npts *= npts3;
					if (scanDim>=4) {				/* running the fourth dimension */
						pvGet(npts4);
						npts *= npts4;
					}
				}
			}
			pvPut(npts);
/*			efSet(pause1_mon);						// force processing of the scan pause calculation */
			cptAtLastOK = 0;						/* initialize hang parameters */
			timeOfLastOK = startTime;				/* epoch of when cptAtLastOK was last updated, used to check for hang */ 
			PVPUT(hungSec,0)
			PVPUTSTR(hungTimeStr,"")
		} state duringScan							/* go to loop during a scan */
	}


	state duringScan {								/* the scan is running, regularly update the progress */
		when (!(busy1||busy2||busy3||busy4)) {		/* none are busy, done with scan, goto scanFinish, and then back to idle */
			DEBUG_PRINT(2, "in duringScan, scan is done, goto scanFinish");
		} state scanFinish

		when (paused) {								/* scan was paused */
			beginPause = 1;
			DEBUG_PRINT(2, "in duringScan, scan is paused, goto duringScanPause");
		} state duringScanPause

		when (delay(UPDATE_DELAY)) {				/* scan progressing normally, update progress values */
			DEBUG_PRINT(3, "in duringScan, do a regular update to progress");
			pvGet(cpt1);							/* get number of completed points in each scan */
			if (scanDim>=2) {
				pvGet(cpt2);
				if (scanDim>=3) {
					pvGet(cpt3);
					if (scanDim>=4) {
						pvGet(cpt4);
					}
				}
			}
			cpt = calc_cpt();
			pvPut(cpt);
%%			fractionDone = (double)cpt/(double)npts;
			sprintf(new_msg, "scanDim = %d,  completed = {%d/%d,  %d/%d,  %d/%d,  %d/%d}, fractionDone = %g", scanDim,cpt1,npts1,cpt2,npts2,cpt3,npts3,cpt4,npts4,fractionDone);
			DEBUG_PRINT(4, new_msg);
			if (fractionDone>1.0) fractionDone = 1.0;/* limit fractionDone to range [0,1] */
			else if (fractionDone<=0.0) fractionDone = 0.0;	/* no negative fractions */
%%			now = time(NULL);
			if (fractionDone>0.0) {
				/* C defect: (int) of a double past INT_MAX is undefined; a 1000x1000
				 * scan with one point done after 36 minutes passes it */
%%				{ double r = (1.0-fractionDone)/fractionDone * (double)(now-startTime-pauseSec); remainingSec = r > INT_MAX ? INT_MAX : r < INT_MIN ? INT_MIN : (int)r; }
				pvPut(remainingSec);
				predictEnd = now + remainingSec;	/* add remaining seconds to now */
%%				clocks2str(remainingSec,tStr);		/* time remaining */
				PVPUTSTR(remainingTimeStr,tStr)
%%				clocks2str(now-startTime,tStr);		/* total execution time on the clock */
				PVPUTSTR(totalElapsedTimeStr,tStr)	/* includes scanning time and pause time */
%%				clocks2str(now-startTime-pauseSec,tStr);
				PVPUTSTR(totalActiveTimeStr,tStr)	/* time spent actively scanning */
%%				formatTimeStr(endingTimeStr,40,predictEnd,0);
				pvPut(endingTimeStr);
				sprintf(new_msg, "   now = %ld sec,   remaining = %d,  predictEnd = %ld",now,remainingSec,predictEnd);
				DEBUG_PRINT(5, new_msg);
				sprintf(new_msg, "   remainingTimeStr = '%s',   endingTimeStr = '%s'",remainingTimeStr,endingTimeStr);
				DEBUG_PRINT(5, new_msg);
			}
			else {
				PVPUTSTR(remainingTimeStr,"")		/* fractionDone is still zero, cannot yet predict anything */
				PVPUTSTR(totalActiveTimeStr,"")
				PVPUTSTR(endingTimeStr,"")
				PVPUTSTR(totalElapsedTimeStr,"")
				PVPUT(remainingSec,0)
			}
			pvPut(fractionDone);
			PVPUT(percentDone,100.0*fractionDone)

			if (cpt != cptAtLastOK) {				/* NOT hung, cpt is changing */
				cptAtLastOK = cpt;					/* reset all hang parameters to now */
				timeOfLastOK = now;					/* set timeOfLastOK to now */
				PVPUT(hungSec,0)
				PVPUTSTR(hungTimeStr,"")
			}
			else if ( (now - timeOfLastOK) > hangWaitSec ) {	/* HUNG, cptAtLastOK unchanged in hangWaitSec */
				hungSec = now - timeOfLastOK;		/* number of seonds hung */
				pvPut(hungSec);
%%				clocks2str(hungSec,tStr);
				PVPUTSTR(hungTimeStr,tStr)			/* time spent hung */
			}

		} state duringScan
	}


	state duringScanPause {							/* the scan is running, but it is paused */
		when (!(busy1||busy2||busy3||busy4)) {		/* This scan is done, don't hang around in the paused state */
			DEBUG_PRINT(2, "in duringScanPause, scan is done, goto scanFinish");
		} state scanFinish

		when (beginPause) {							/* scan just began pause, first loop in this state */
			beginPause = 0;
%%			pauseStart = time(NULL);				/* time at start of pause */
			PVPUT(Npauses,Npauses+1)				/* at start of a pause, increment Npauses */
			DEBUG_PRINT(2, "in duringScanPause, started a scan pause");
			timeOfLastOK = pauseStart;				/* at a pause, you cannot be hanging */
			PVPUT(hungSec,0)
			PVPUTSTR(hungTimeStr,"")
		} state duringScanPause

		when (!paused) {							/* scan just resumed from a pause */
%%			now = time(NULL);
			pauseSec += now - pauseStart;			/* add to time spent paused */
			pvPut(pauseSec);
%%			clocks2str(pauseSec,tStr);				/* total elapsed time spent paused */
			PVPUTSTR(pauseTimeStr,tStr)				/* includes scanning time and pause time */
			DEBUG_PRINT(2, "in duringScanPause, scan pause rescinded");
			timeOfLastOK = now;						/* restart from a pause with no hang time accumulated */
		} state duringScan							/* done with this pause state, got back to the regular scan state */

		when (delay(UPDATE_DELAY)) {				/* regular updates during a paused scan */
%%			now = time(NULL);
%%			clocks2str(now-pauseStart,tStr);		/* time spent in current pause */
%%			clocks2str(now-pauseStart+pauseSec,tStr1);/* total elapsed seconds spent paused, includes previous pauses */
			sprintf(pauseTimeStr,"current %s,  total %s",tStr,tStr1);
			pvPut(pauseTimeStr);
%%			clocks2str(now-startTime,tStr);			/* total execution time on the clock */
			PVPUTSTR(totalElapsedTimeStr,tStr)		/* includes scanning time and pause time */
			if (fractionDone>0.0) {					/* uses the prevously computed fractionDone to update end time */
				/* clamped as in duringScan */
%%				{ double r = (1.0-fractionDone)/fractionDone * (double)(now-startTime-pauseSec); remainingSec = r > INT_MAX ? INT_MAX : r < INT_MIN ? INT_MIN : (int)r; }
				pvPut(remainingSec);
				predictEnd = now + remainingSec;	/* add remaining seconds to now */
%%				formatTimeStr(endingTimeStr,40,predictEnd,0);
				pvPut(endingTimeStr);
			}
			sprintf(new_msg, "   periodic check in duringScanPause, time paused; '%s', ",pauseTimeStr);
			DEBUG_PRINT(5, new_msg);
		} state duringScanPause
	}


	state scanFinish {								/* the scan is running, regularly update the progress */
		when (1) {									/* scan is done, reset and branch back to idle */
%%			now = time(NULL);						/* reset the progress bar PVs to final values */
			PVPUTSTR(remainingTimeStr,"00:00:00")	/* no more time remaining, it is one */
%%			clocks2str(now-startTime-pauseSec,tStr);/* total execution time - pause time */
			PVPUTSTR(totalActiveTimeStr,tStr)		/* set to total scan time (excludes pause time) */
%%			clocks2str(now-startTime,tStr);			/* total execution time on the clock */
			PVPUTSTR(totalElapsedTimeStr,tStr)		/* includes scanning time and pause time */
%%			formatTimeStr(endingTimeStr,40,now,1);	/* actual end time & date */
			pvPut(endingTimeStr);
			PVPUT(running,0)
			PVPUT(remainingSec,0)

			/* do a final update on what got scanned */
			pvGet(cpt1);							/* get number of completed points in each scan */
			if (scanDim>=2) {
				pvGet(cpt2);
				if (scanDim>=3) {
					pvGet(cpt3);
					if (scanDim>=4) {
						pvGet(cpt4);
					}
				}
			}
			cpt = calc_cpt();
			pvPut(cpt);
%%			fractionDone = (double)cpt/(double)npts;
			if (fractionDone>1.0) fractionDone = 1.0;/* limit fractionDone to range [0,1] */
			pvPut(fractionDone);
			PVPUT(percentDone,100.0*fractionDone)
			PVPUT(hungSec,0)
			PVPUTSTR(hungTimeStr,"")
			DEBUG_PRINT(2, "no scan is busy in duringScan, back to idle");
		} state idle
	}
}



/*
 * state set:  scanPauseUpdate, this maintains the PV "{P}paused" = paused
 */
ss scanPauseUpdate {

	state initPause {
		when () {
			PVPUT(paused,0)
			almostPaused = 0;
			epicsThreadSleep(1.0);					/* sleep for 1 sec, give the main state set a chance to process */
			efSet(pause1_mon);						/* insure that the when() in idlePause processes the first time */
			DEBUG_PRINT(1, "completed state initPause");
		} state idlePause
	}


	state idlePause {								/* loop for maintaining paused */
		when (efTest(pause1_mon) || efTest(pause2_mon) || efTest(pause3_mon) || efTest(pause4_mon)) {
			/* C defect: %p took the event flags, which are unsigned EF_IDs */
			sprintf(new_msg, "at top of state idlePause, monitors are : %u,  %u,  %u,  %u",pause1_mon,pause2_mon,pause3_mon,pause4_mon);
			DEBUG_PRINT(7, new_msg);
			/* one of the pause states has changed, so reset paused */
			efClear(pause1_mon);
			efClear(pause2_mon);
			efClear(pause3_mon);
			efClear(pause4_mon);
		} state checkAndSetPause

		/* a pause has been set on an outer scan, but an inner scan is still running, recheck whenever a busy flag changes */
		when (almostPaused && (efTest(busy1_mon) || efTest(busy2_mon) || efTest(busy3_mon) || efTest(busy4_mon))) {
			sprintf(new_msg, "paused but waiting for busy, busy flags are:  busy1=%d, busy2=%d, busy3=%d, busy4=%d", busy1,busy2,busy3,busy4);
			DEBUG_PRINT(6, new_msg);
			efClear(busy1_mon);
			efClear(busy2_mon);
			efClear(busy3_mon);
			efClear(busy4_mon);
		} state checkAndSetPause

		when (delay(UPDATE_DELAY_LONG)) {			/* do a check and reset just in case I missed something */
			DEBUG_PRINT(7, "delay(UPDATE_DELAY_LONG) in scanPauseUpdate");
		} state checkAndSetPause
	}


	state checkAndSetPause {						/* check status and set the paused PV */
		when (1) {									/* something happened, check status and set the paused PV */
			sprintf(new_msg, "paused flags are pause1=%d, pause2=%d, pause3=%d, pause4=%d", pause1,pause2,pause3,pause4);
			DEBUG_PRINT(8, new_msg);
			aPause = pause1;						/* aPause is true if some pause flag is set, even if an inner scan still going */
			if (scanDim>1) {
				aPause = aPause || pause2;
				if (scanDim>2) {
					aPause = aPause || pause3;
					if (scanDim>3) {
						aPause = aPause || pause4;
					}
				}
			}
			stopped = allStopped();					/* true when all scans either !busy or paused */
			almostPaused = aPause && !stopped;		/* 0 means really paused, 1 means a pause is set on an outer scan, but waiting for inner scan to complete */
			paused = aPause && stopped;				/* yes, all scans either !busy or paused */
			pvPut(paused);							/* set my pause flag */
		} state idlePause
	}
}



%{

static long calc_cpt(void)			/* calculates the number of completed points */
{
	long	lcpt;					/* local version of cpt, the value that is returned */
	long	nBelow;

	lcpt = cpt1;					/* cpt will be the TOTAL number of completed points */
	if (scanDim<2) return lcpt;

	nBelow = npts1;
	lcpt += cpt2 * nBelow;			/* each complete scan2 is worth nBelow points */
	if (scanDim<3) return lcpt;

	nBelow *= npts2;				/* at this point nBelow = npts1*npts2 */
	lcpt += cpt3 * nBelow;
	if (scanDim<4) return lcpt;

	nBelow *= npts3;				/* at this point nBelow = npts1*npts2*npts3 */
	lcpt += cpt4*nBelow;			/* each complete scan4 is worth nBelow points */
	return lcpt;
}


/* format clocks to a time string of form hh:mm:ss, e.g. "07:23:05" */
/* static void clocks2str(time_t isec, char *str) */
static void clocks2str(long isec, char *str)
{
	int		hh,mm,ss;
	int		sign=1;
	if (isec>32000000) {		/* 32000000sec is 9 days more than 1 year */
		strcpy(str,"infinite");
		return;
	}
	else if (isec<-32000000) {
		strcpy(str,"-infinite");
		return;
	}
	else if (isec<0) {
		isec *= -1;
		sign = -1;
	}
	ss = isec % 60;
	isec /= 60;
	mm = isec % 60;
	hh = sign * isec / 60;
	sprintf(str,"%02d:%02d:%02d",hh,mm,ss);
	return;
}


/* format clocks to a string containig the date and time" */
static void formatTimeStr(
char *tStr,			/* string to recieve the result */
int tStrLen,		/* maximum length of tStr */
time_t final,			/* final time as a time_t, this is what's given */
int full)			/* when true, always use GeneralDateFMT */
{
	time_t	now;
	struct tm *tm1;
	int	isToday=0;
	int	isTomorrow=0;
	int	isYesterday=0;
	int	hh,mm,ss;
	time_t final_t = (time_t)final;

	if (!full) {
		tm1 = localtime(&final_t);
		hh = tm1->tm_hour;				/* save time of day */
		mm = tm1->tm_min;
		ss = tm1->tm_sec;

		now = time(NULL);
		tm1 = localtime(&now);
		tm1->tm_hour = hh;				/* make tm1 the same time of day as final, but on today */
		tm1->tm_min = mm;
		tm1->tm_sec = ss;
		isToday = (final_t==mktime(tm1));
		if (!isToday) {					/* not today, maybe tomorrow */
			tm1->tm_mday++;				/* increment the day */
			isTomorrow = (final_t==mktime(tm1));
		}
		if (!isToday && !isTomorrow) {	/* not today or tomorrow, maybe yesterday */
			tm1->tm_mday -= 2;			/* decrement the day by 2 */
			isYesterday = (final_t==mktime(tm1));
		}
	}

	tm1 = localtime(&final_t);
	if (isToday)			strftime(tStr,tStrLen, TodayFMT,tm1);
	else if (isTomorrow)	strftime(tStr,tStrLen, TomorrowFMT,tm1);
	else if (isYesterday)	strftime(tStr,tStrLen, YesterdayFMT,tm1);
	else					strftime(tStr,tStrLen, GeneralDateFMT,tm1);
	return;
}


static int allStopped(void)
{
	int stopCount;

	stopCount = (pause1 || !busy1) ? 1 : 0;
	stopCount += (scanDim<2) || (pause2 || !busy2) ? 2 : 0;
	stopCount += (scanDim<3) || (pause3 || !busy3) ? 4 : 0;
	stopCount += (scanDim<4) || (pause4 || !busy4) ? 8 : 0;
	return (stopCount==15);
}

}%

