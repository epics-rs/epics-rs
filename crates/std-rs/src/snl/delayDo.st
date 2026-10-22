program delayDo("name=delayDo,P=xxx:,R=delayDo1")

/* 
    TODO: replace scanning state with something generic
          for s26, a calcout can be used to watch the scan record states
    a string seq could be used for output to make this very general

*/

/* Turn on run-time debug messages */
option +d;
/* Make the generated code reentrant */
option +r;
/* Suppress SNC warnings */
/*option -w;*/

#include "seqPVmacros.h"

char* SNLtaskName;
int resumeWaiting;

PV(int, debug_flag,   "{P}{R}:debug.VAL", Monitor);
PV(short, enable, "{P}{R}:enable.VAL", EvFlag);
PV(int, standby, "{P}{R}:standbyCalc", EvFlag);
PV(int, active, "{P}{R}:activeCalc", EvFlag);
PV(double, delayPeriod, "{P}{R}:delay", Monitor);
PV(int, doSeq, "{P}{R}:doSeq.PROC", NoMon);
PV(string, seqState, "{P}{R}:state", NoMon);

ss delayDoMain
{

  state init
  {
    when ( pvConnectCount() == pvAssignCount() )
    {
      resumeWaiting = 0; SNLtaskName = macValueGet("name"); /* C: never set, so DEBUG_PRINT passed NULL to %s */
      DEBUG_PRINT(3, "init -> idle\n");
      PVPUTSTR(seqState, "idle");
    } state idle
    
  }

  state disable
  {
    when ( (efTestAndClear(enable_mon) && enable) )
    {
      /* Clear active monitors. Only events that occur after reenabling should result in action */
      efClear(active_mon);
      DEBUG_PRINT(3, "disable -> maybeStandby\n");
      /* Don't put the maybeStandby string to the state PV; the state doesn't last long enough */
      /*PVPUTSTR(seqState, "maybeStandby");*/
    } state maybeStandby
  }
  
  state maybeStandby
  {
    when ( standby )
    {
      DEBUG_PRINT(3, "maybeStandby -> standby\n");
      PVPUTSTR(seqState, "standby");
    } state standby

    when ( active )
    {
      DEBUG_PRINT(3, "maybeStandby -> active\n");
      PVPUTSTR(seqState, "active");
    } state active

    when ( ! standby )
    {
      DEBUG_PRINT(3, "maybeStandby -> idle\n");
      PVPUTSTR(seqState, "idle");
    } state idle
  
  }

  state idle 
  {
    when ( efTestAndClear(enable_mon) && !enable )
    {
      DEBUG_PRINT(3, "idle -> disable\n");
      PVPUTSTR(seqState, "disable");
    } state disable
  
    when ( (efTestAndClear(standby_mon) && standby) )
    {
      DEBUG_PRINT(3, "idle -> standby\n");
      PVPUTSTR(seqState, "standby");
    } state standby
  
    when ( (efTestAndClear(active_mon) && active) )
    {
      DEBUG_PRINT(3, "idle -> active\n");
      PVPUTSTR(seqState, "active");
    } state active
  
  }

  state standby
  {
    when ( (efTestAndClear(enable_mon) && !enable) )
    {
      resumeWaiting = 0;
      DEBUG_PRINT(3, "standby -> disable\n");
      PVPUTSTR(seqState, "disable");
    } state disable
  
    when ( (efTestAndClear(standby_mon) && !standby) )
    {
      DEBUG_PRINT(3, "standby -> maybeWait\n");
      /* Don't put the maybeWait string to the state PV; the state doesn't last long enough */
      /*PVPUTSTR(seqState, "maybeWait");*/
    } state maybeWait
  
  }

  state maybeWait
  {
    /* active calc is currently high */
    when ( active )
    {
      DEBUG_PRINT(3, "maybeWait -> active\n");
      PVPUTSTR(seqState, "active");
    } state active

    /* active calc was high at some point during standby, but it isn't now 
       -OR- 
       standby was enabled while waiting was in progress */
    when ( efTest(active_mon) || resumeWaiting )
    {
      efClear(active_mon);
      DEBUG_PRINT(3, "maybeWait -> waiting\n");
      PVPUTSTR(seqState, "waiting");
    } state waiting

    /* active calc wasn't high during standby */
    when ( ! efTest(active_mon) )
    {
      DEBUG_PRINT(3, "maybeWait -> idle\n");
      PVPUTSTR(seqState, "idle");
    } state idle

  }

  state active
  {
    when ( (efTestAndClear(enable_mon) && !enable) )
    {
      DEBUG_PRINT(3, "active -> disable\n");
      PVPUTSTR(seqState, "disable");
    } state disable

    when ( (efTestAndClear(standby_mon) && standby) )
    {
      DEBUG_PRINT(3, "active -> standby\n");
      PVPUTSTR(seqState, "standby");
    } state standby
  
    when ( (efTestAndClear(active_mon) && !active) )
    {
      DEBUG_PRINT(3, "active -> waiting\n");
      PVPUTSTR(seqState, "waiting");
    } state waiting
  
  }

  state waiting
  {
    when ( (efTestAndClear(enable_mon) && !enable) )
    {
      DEBUG_PRINT(3, "waiting -> disable\n");
      PVPUTSTR(seqState, "disable");
    } state disable

    when ( (efTestAndClear(standby_mon) && standby) )
    {
      resumeWaiting = 1;
      DEBUG_PRINT(3, "waiting -> standby\n");
      PVPUTSTR(seqState, "standby");
    } state standby

    when ( (efTestAndClear(active_mon) && active) )
    {
      DEBUG_PRINT(3, "waiting -> active\n");
      PVPUTSTR(seqState, "active");
    } state active

    when ( delay( delayPeriod ) )
    {
      resumeWaiting = 0;
      DEBUG_PRINT(3, "waiting -> action\n");
      PVPUTSTR(seqState, "action");
    } state action

  }

  state action
  {
  
    when ()
    {
      DEBUG_PRINT(3, "Processing doSeq\n");
      PVPUT(doSeq, 1);
      DEBUG_PRINT(3, "action -> idle\n");
      PVPUTSTR(seqState, "idle");
    } state idle
  }

}
