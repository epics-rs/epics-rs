/*================================================================
 *
 * seqPVmacros.h -- PV-related macros for EPICS State Notation
 *     Language (SNL) code development
 *
 * 2003-Mar-20	[jemian] changed pvSet to PVPUT (and pvSetStr to PVPUTSTR)
 * 2003-Mar-19	[jemian] changed printf to errlogPrintf
 * 2002-Sep-24	[tischler] added pvSetStr and DEBUG_PRINT (a la Jemian)
 * 2002-Jun-04	[lazmo] renamed to seqPVmacros.h
 * 2002-Mar-08	[lazmo] added pvSet()
 * 2002-Feb-21	[lazmo] fixed comments
 * 2001-Nov-24	[lazmo] added PVA & PVAA
 * 2001-Oct-18  [lazmo] original
 *
 * Author:
 *   M. Laznovsky -- lazmo_at_slac.stanford.edu
 *
 *================================================================
 */

/*----------------------------------------------------------------
 * PV() -- declare a variable and the EPICS PV it's assigned to, with
 * optional monitor & event flag.  One line takes the place of
 * the usual:
 *
 *    int blah;
 *    assign blah ...
 *    monitor blah;
 *    evflag blahFLag;
 *    sync blah ...
 *
 * Format:
 *
 *    PV( varType, varName, pvName, other )
 *
 * Where:
 *
 *    varType   is int, short, float, etc.
 *    varName   is the variable name, e.g. foo
 *    pvName    is the associated PV, e.g. "XYZ:ETC:FOO"
 *    other     is one of the following:
 *                  NoMon
 *                  Monitor
 *                  EvFlag
 *
 * Examples:
 *
 *    PV (int,   gui_goo,   "{STN}:GUI:GOO",   NoMon);     <-- no monitor
 *    PV (float, gui_fudge, "{STN}:GUI:FUDGE", Monitor);   <-- w/monitor
 *    PV (short, gui_run,   "{STN}:GUI:RUN",   EvFlag);    <-- w/monitor & event flag
 *
 *----------------------------------------------------------------
 */

#define PV(_TYPE_,_VAR_,_PV_,_OTHER_)	\
  _TYPE_   _VAR_;			\
  assign   _VAR_ to _PV_		\
  _OTHER_ (_VAR_)

/*----------------------------------------------------------------
 * PVA(), for single waveform rec or array of PVs
 *
 *   "varName" becomes array of <varType>; 3rd arg is number of elements
 *
 * examples:
 *
 *   single waveform record:
 *
 *     PVA (short, plot_x0,  32, "{STN}:DATA:PLOT:X0", NoMon);
 *  
 *   array of PVs:
 *
 *     #define PVA_zap {	\
 *         "{STN}:GUI:ZAP1",	\
 *         "{STN}:GUI:ZAP2",	\
 *         "{STN}:GUI:ZAP3"	\
 *       }
 *     PVA (int, zap, 3, PVA_zap, EvFlag);
 *
 *----------------------------------------------------------------
 */

#define PVA(_TYPE_,_VAR_,_NELEM_,_PV_,_OTHER_)	\
  _TYPE_   _VAR_ [ _NELEM_ ];			\
  assign   _VAR_ to _PV_			\
  _OTHER_ (_VAR_)

/*----------------------------------------------------------------
 * PVAA(), for arrays of waveform records
 *
 *   "varName" becomes double-dimensioned array of <varType>
 *   3rd arg is number of waveform records
 *   4th arg is number of elements per record
 *   Need to define another macro first for "_PV_"
 *
 * example:
 *
 *	#define PVAA_plotx {		\
 *	    "{STN}:DATA:PLOT:X1",	\
 *	    "{STN}:DATA:PLOT:X2",	\
 *	    "{STN}:DATA:PLOT:X3"	\
 *	  }
 *	PVAA (short, plotx, 3, 500, PVAA_plotx, NoMon);
 *
 *----------------------------------------------------------------
 */

#define PVAA(_TYPE_,_VAR_,_NREC_,_NELEM_,_PV_,_OTHER_)	\
  _TYPE_   _VAR_ [ _NREC_ ] [ _NELEM_ ];		\
  assign   _VAR_ to _PV_				\
  _OTHER_ (_VAR_)

/*----------------------------------------------------------------
 * macros for last arg of PV* ("_OTHER_")
 *----------------------------------------------------------------
 */

/*
 * no monitor
 */
#define NoMon(_VAR_)	/* this macro intentionally left blank :P */

/*
 * monitor
 */
#define Monitor(_VAR_) ; monitor _VAR_

/* 
 * monitor & event flag; flag var will be named "<var>_EvFlag"
 */
#define EvFlag(_VAR_)		\
  Monitor   (_VAR_);		\
  evflag     _VAR_##_mon;	\
  sync _VAR_ _VAR_##_mon

/*================================================================*/
/*================================================================*/

/*----------------------------------------------------------------
 * PVPUT() -- variable assign and pvPut() in one
 *
 * Format:
 *
 *    PVPUT( varName, expr )
 *
 * Where:
 *
 *    varName   is the variable name, e.g. foo
 *    expr      is any C expression
 *
 * Examples:
 *
 *    PVPUT (foo, 3);
 *      expands to:
 *        foo = 3;
 *        pvPut(foo,SYNC);
 *
 *    PVPUT (bar, xyz + 2);
 *      expands to:
 *        bar = xyz + 2;
 *        pvPut(bar,SYNC);
 *
 *----------------------------------------------------------------
 */

#define PVPUT(_VAR_,_EXPR_)	\
  {				\
    _VAR_ = ( _EXPR_ );		\
    pvPut(_VAR_,SYNC);		\
  }

/*----------------------------------------------------------------
 * PVPUTSTR() -- string assign and pvPut() in one
 *
 * Format:
 *
 *    PVPUTSTR( strName, string expr )
 *
 * Where:
 *
 *    strName      is the variable name, e.g. foo
 *    string expr  is any C expression that points to a string
 *
 * Example:
 *
 *    PVPUTSTR (foo, "error message");
 *      expands to:
 *        strcpy(foo,"error message");
 *        pvPut(foo,SYNC);
 *
 *----------------------------------------------------------------
 */

#define PVPUTSTR(_STR_,_EXPR_)	\
  {				\
    strcpy(_STR_,_EXPR_);	\
    pvPut(_STR_,SYNC);		\
  }

/*================================================================*/
/*================================================================*/

/*----------------------------------------------------------------
 * DEBUG_PRINT() -- print a debug string for a particular debug level
 *
 * Format:
 *
 *    DEBUG_PRINT( debug_level, string expr )
 *
 * Where:
 *
 *    debug_level  is the debugging level, ie 3
 *    string expr  is any C expression that points to a string
 *
 * Example:
 *
 *    DEBUG_PRINT (1, "ERROR, how did I get here?");
 *
 *----------------------------------------------------------------
 */

#if defined(NO_DEBUGGING_OUTPUT)
#define DEBUG_PRINT(DEBUG_LEVEL, MSG) ;
#else

#define DEBUG_PRINT_HEADER(DEBUG_LEVEL)		\
   printf("<%s,%d,%s,%d> ",			\
   	__FILE__, __LINE__, 			\
	SNLtaskName, DEBUG_LEVEL);

#define DEBUG_PRINT(DEBUG_LEVEL, MSG)		\
    if (debug_flag >= DEBUG_LEVEL) {		\
        DEBUG_PRINT_HEADER (DEBUG_LEVEL);	\
        printf("%s\n", MSG);			\
        epicsThreadSleep(0.01);			\
    }

#define DEBUG_PRINT1(DEBUG_LEVEL, MSG, _V1_)	\
    if (debug_flag >= DEBUG_LEVEL) {		\
        DEBUG_PRINT_HEADER (DEBUG_LEVEL);	\
        printf(MSG, _V1_);			\
        printf("\n");				\
        epicsThreadSleep(0.01);			\
    }

#endif

/*================================================================*/
/*================================================================*/
/*================================================================*/

/* end */
