program flexCombinedMotion("name=flexCombinedMotion,P=xxx:,M=m1,C=cap1,FM=pi:c0:m1,CM=nf:c0:m1")

/* Turn on run-time debug messages */
option +d;
/* Make the generated code reentrant */
option +r;
/* Suppress SNC warnings */
/*option -w;*/

#include "seqPVmacros.h"

%%#include <math.h>

char* SNLtaskName;
double last_set_point;
double new_set_point;
double dist_calc;
double coarse_disp;
double fine_pos;
double fine_home;
double pos_error;
double act_db;
int num_retries;

/* */
PV(int,     debug_flag,      "{P}{M}:debug.VAL",         Monitor);
PV(double,  set_point,       "{P}{M}:setPoint.VAL",      EvFlag);
PV(double,  mode,            "{P}{M}:mode.VAL",          Monitor);
PV(double,  deadband,        "{P}{M}:deadband.VAL",      Monitor);
PV(double,  retries,         "{P}{M}:retries.VAL",       NoMon);
PV(double,  max_retries,     "{P}{M}:maxRetries.VAL",    Monitor);
PV(double,  pos_monitor,     "{P}{C}:pos",               Monitor);
PV(double,  fine_val_put,    "{P}{FM}.VAL",              NoMon);
PV(double,  fine_rbv,        "{P}{FM}.RBV",              Monitor);
PV(double,  coarse_rel_put,  "{P}{CM}.RLV",              NoMon);
PV(double,  coarse_rbv,      "{P}{CM}.RBV",              Monitor);
PV(double,  coarse_mres,     "{P}{CM}.MRES",             Monitor);
PV(short,   coarse_dmov,     "{P}{CM}.DMOV",             EvFlag);
PV(short,   coarse_stop_put, "{P}{CM}.STOP",             NoMon);
PV(double,  upper_limit,     "{P}{M}:upperLimit.VAL",    Monitor);
PV(double,  home_pos,        "{P}{M}:homePos.VAL",       Monitor);
PV(double,  lower_limit,     "{P}{M}:lowerLimit.VAL",    Monitor);
PV(short,   stop,            "{P}{M}:stop.VAL",          EvFlag);
PV(short,   stop_put,        "{P}{M}:stop.VAL",          NoMon);
/*PV(short,   reset_busy_put,  "{P}{M}:resetBusy.PROC",    NoMon);*/
PV(short,   busy_put,        "{P}{M}:busy.VAL",          NoMon);

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

ss combinedMotionMain
{

  state init
  {
    when ( pvConnectCount() == pvAssignCount() )
    {
      /* C defect fixed: C never assigned SNLtaskName, which the
      ** DEBUG_PRINT/DEBUG_PRINT1 header prints with %s. */
      SNLtaskName = macValueGet("name");
      DEBUG_PRINT(3, "init -> idle\n");
      /* Initialize last_set_point to an unlikely-to-be-used value */
      last_set_point = -314159.26;
      /* Only respond to changes in the set point after the program is running.
         This does mean it is possible for the readback and set point to be out of sync. */
      efClear(set_point_mon);
      efClear(stop_mon);
    } state idle
  }

  state idle
  {
    when ( efTestAndClear(set_point_mon) )
    {
      DEBUG_PRINT1(5, "set_point = %f", set_point);
      new_set_point = set_point;
      num_retries = 0;
      PVPUT(retries, num_retries);
      DEBUG_PRINT(3, "idle -> checkSetPoint");
    } state checkSetPoint
  
    when ( efTest(stop_mon) )
    {
      DEBUG_PRINT(3, "idle -> abort");
    } state abort
  }

  state checkSetPoint
  {
    when ( new_set_point == last_set_point )
    {
      DEBUG_PRINT1(5, "new_set_point = last_set_point = %f", new_set_point);
      /* Also calcDistance when the value doesnt change, since this locks-in the feedback.
         Eventually there could be a lock-in button to do this and this block could go to 
         the idle state instead of calcDistance. */
      DEBUG_PRINT(3, "checkSetPoint -> calcDistance");
    } state calcDistance
  
    when ( new_set_point != last_set_point )
    {
      DEBUG_PRINT2(5, "last_set_point = %f ; new_set_point = %f", last_set_point, new_set_point);
      last_set_point = new_set_point;
      DEBUG_PRINT(3, "checkSetPoint -> calcDistance");
    } state calcDistance
  }

  state calcDistance
  {
    entry
    {
      /* requested fine position */
      dist_calc = new_set_point - pos_monitor + fine_rbv;
      DEBUG_PRINT3(5, "dist_calc=%f, pos_monitor=%f, fine_rbv=%f", dist_calc, pos_monitor, fine_rbv);
    }
    
    when ( mode == 3 )
    {
      /* In fine mode the coarse motor never moves, so the requested move needs to be limited to the fine motor range */
      if ( dist_calc > upper_limit )
      {
        dist_calc = upper_limit;
        DEBUG_PRINT1(5, "dist_calc=%f restricted by upper limit", dist_calc);
      }
      else if (dist_calc < lower_limit )
      {
        dist_calc = lower_limit;
        DEBUG_PRINT1(5, "dist_calc=%f restricted by lower limit", dist_calc);
      }
      DEBUG_PRINT(3, "calcDistance -> moveFine: mode = 3\n");
    } state moveFine
    
    when ( mode == 2 )
    {
      /* In setup mode the piezo stays parked at the home position */
      DEBUG_PRINT(3, "calcDistance -> homeFine: mode = 2\n");
    } state homeFine
    
    when ( (dist_calc >= upper_limit) || (dist_calc <= lower_limit) )
    {
      DEBUG_PRINT(3, "calcDistance -> homeFine");
      /* do something */
    } state homeFine
  
    when ( (dist_calc < upper_limit) && (dist_calc > lower_limit) )
    {
      DEBUG_PRINT1(5, "dist=%f", dist_calc);
      DEBUG_PRINT(3, "calcDistance -> moveFine");
      /* requested displacement is within the fine motor range */
    } state moveFine
  }

  state homeFine
  {
    when () 
    {
      /* Do not allow home to be outside of limits */
      if (home_pos > upper_limit)
      {
        fine_home = upper_limit;
        DEBUG_PRINT(5, "restricting fine home to the upper limit");
      }
      else if (home_pos < lower_limit)
      {
        fine_home = lower_limit;
        DEBUG_PRINT(5, "restricting fine home to the lower limit");
      }
      else
      {
        fine_home = home_pos;
      }
      
      DEBUG_PRINT1(5, "fine_home=%f", fine_home);
      PVPUT(fine_val_put, fine_home);
      DEBUG_PRINT(3, "homeFine -> moveCoarse");
    } state moveCoarse
  } 
  
  state moveCoarse
  {
    entry {
      coarse_disp = dist_calc - fine_home;
    }
    
    when ()
    {
      /* What does this do? */
      efClear(coarse_dmov_mon);

      DEBUG_PRINT1(5, "coarse_disp=%f", coarse_disp);

      PVPUT(coarse_rel_put, coarse_disp);

      DEBUG_PRINT(3, "moveCoarse -> waitMove");
    } state waitCoarse
  }
  
  state waitCoarse
  {
    when ( efTestAndClear(coarse_dmov_mon) && coarse_dmov == 1 )
    {
      DEBUG_PRINT(3, "waitMove -> afterMove");
      /* DMOV changed and is currently done */
    } state afterMove
    
    when ( efTestAndClear(stop_mon) )
    {
      DEBUG_PRINT(3, "waitMove -> abort");
    } state abort
  }

  state abort
  {
    when ( delay(0.15) )
    {
      /* stop the coarse motor -- the fine motor should not be moving right now */
      PVPUT(coarse_stop_put, 1);
      DEBUG_PRINT(3, "abort -> resetBusy");
    } state resetStop
  }

  state resetStop
  {
    when ()
    {
      /* reset the stop PV -- this should not be necessary */
      /*PVPUT(stop_put, 0);*/
      stop_put = 0;
      pvPut(stop_put, SYNC);
      efClear(stop_mon);
    } state resetBusy
  }

  state afterMove
  {
    when ( mode == 0 )
    {
      DEBUG_PRINT(3, "afterMove -> calcDistance\n");
      
      /* Go back to calcDistance because New Focus picomotors suck and the motor might still not be within
         the piezo range after one coarse move, if the move is long and the direction is the bad one */
    } state calcDistance
  
    when ( mode == 1 || mode == 2 )
    {
      DEBUG_PRINT(3, "afterMove -> maybeRetry\n");
    } state maybeRetry
  }

  state maybeRetry
  {
    entry {
      pos_error = new_set_point - pos_monitor;
      if (deadband < coarse_mres)
        act_db = coarse_mres;
      else
        act_db = deadband;
      DEBUG_PRINT2(5, "pos_error=%f, deadband=%f", pos_error, act_db);
    }
    
    when ( num_retries >= max_retries )
    {
      DEBUG_PRINT(1, "Max retries reached; Giving up.\n");
      DEBUG_PRINT(3, "maybeRetry -> resetBusy\n");
    } state resetBusy
    
    when ( fabs(pos_error) > act_db )
    {
      dist_calc = pos_error + fine_home;
      DEBUG_PRINT1(5, "dist_calc=%f", dist_calc);
      num_retries += 1;
      PVPUT(retries, num_retries);
      DEBUG_PRINT(3, "maybeRetry -> moveCoarse\n");
    } state moveCoarse
    
    when ( fabs(pos_error) <= act_db && mode == 1)
    {
      DEBUG_PRINT(3, "maybeRetry -> calcDistance\n");
      /* Should this go directly to moveFine instead? */
    } state calcDistance
    
    when ( fabs(pos_error) <= act_db && mode == 2)
    {
      /* In setup mode the piezo stays parked at the home position */
      DEBUG_PRINT(3, "maybeRetry -> resetBusy\n");
    } state resetBusy
  }

  state moveFine
  {
    entry {
      if ( mode != 3)
      {
        /* fine_pos is the same calc as dist_calc, but we recalculate it because pos_mon and fine_rbv have changed */
        fine_pos = new_set_point - pos_monitor + fine_rbv;	  
      }
      else
      {
        /* Do not recalculate the fine position in fine mode; the position may have been restricted by the limits */
        fine_pos = dist_calc;
      }
    }
    
    when ()
    {
      DEBUG_PRINT2(5, "fine_pos=%f, dist=%f", fine_pos, dist_calc);
      /*PVPUT(fine_val_put, fine_pos);*/
      fine_val_put = fine_pos;
      pvPut(fine_val_put, SYNC);
      DEBUG_PRINT(3, "moveFine -> resetBusy");
    } state resetBusy
  }
  
  state resetBusy
  {
    when ()
    {
      /* Process the reset-busy calcout */
      /*PVPUT(reset_busy_put, 1);*/
      /* Reset the busy record */
      PVPUT(busy_put, 0);
      
      DEBUG_PRINT(3, "resetBusy -> idle\n");
    } state idle
  }
}
