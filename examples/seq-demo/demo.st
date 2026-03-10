/* demo.st - Simple sequencer demo for testing runtime.
 *
 * Expects two PVs:
 *   {P}counter  - numeric PV (ai/ao) for counting
 *   {P}light    - numeric PV (bi/bo) toggled by ss_light
 *
 * ss_counter: increments {P}counter every 1 second
 * ss_light:   monitors counter and toggles light when counter changes
 */
program demo

option +s;

double counter;
assign counter to "{P}counter";
monitor counter;

int light;
assign light to "{P}light";

evflag ef_counter;
sync counter to ef_counter;

ss counter_ss {
    state init {
        when (delay(1.0)) {
            counter = 0.0;
            pvPut(counter);
        } state counting
    }
    state counting {
        when (counter >= 10.0) {
        } state done
        when (delay(1.0)) {
            counter += 1.0;
            pvPut(counter);
        } state counting
    }
    state done {
        when (delay(0.1)) {
        } exit
    }
}

ss light_ss {
    state idle {
        when (efTestAndClear(ef_counter)) {
            if (counter > 0.0) {
                light = 1;
            } else {
                light = 0;
            }
            pvPut(light);
        } state idle
        when (delay(15.0)) {
        } exit
    }
}
