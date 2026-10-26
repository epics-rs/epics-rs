/* motor XPS_trajectoryScan.st cut to its xpsTrajectoryAbort and
 * xpsTrajectoryRun state sets: Run blocks in trajectoryExecute for the
 * whole trajectory while Abort waits on the abort monitor. */
program foreignCall("P=fc:")

option +r;

%%extern void trajectoryExecute(void);

int execState;
assign execState to "{P}execState";

int abort;
assign abort to "{P}abort";
monitor abort;
evflag abortMon;
sync abort to abortMon;

ss xpsTrajectoryAbort {
    state monitorAbort {
        when (efTestAndClear(abortMon) && abort == 1 && execState == 1) {
            abort = 0;
            pvPut(abort);
        } state monitorAbort
    }
}

ss xpsTrajectoryRun {
    state asyncExecute {
        when (execState == 0) {
            execState = 1;
            pvPut(execState);
            %%trajectoryExecute();
        } state done
    }
    state done {
        when (execState == 2) {
        } state done
    }
}
