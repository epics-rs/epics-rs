program Federal

/*  
 *
 *  Author :  Kurt Goetze
 *  Created:  02/27/02
 *  Version:  0
 *  
 *  Modification log:  
 *  
 *
 */

%%#include <stdlib.h>
%%#include <stdio.h>
%%#include <string.h>
%%#include <math.h>
%%#include <epicsThread.h>

int i;

int FederalDebug;
assign FederalDebug to "{P}Fed:{S}:debug";
monitor FederalDebug;

int run;
assign run to "{P}Fed:{S}:run";
monitor run;

int num_probes;
assign num_probes to "{P}Fed:{S}:num_probes";
monitor num_probes;

int probe_num;
assign probe_num to "{P}Fed:{S}:probe";

float position;
assign position to "{P}Fed:{S}:position";

int proc = 1;
assign proc to "{P}Fed:{S}:serial.PROC";

string readback;
assign readback to "{P}Fed:{S}:serial.AINP";

double probe_[10];
assign probe_[0] to "{P}Fed:{S}:probe_1";
assign probe_[1] to "{P}Fed:{S}:probe_2";
assign probe_[2] to "{P}Fed:{S}:probe_3";
assign probe_[3] to "{P}Fed:{S}:probe_4";
assign probe_[4] to "{P}Fed:{S}:probe_5";
assign probe_[5] to "{P}Fed:{S}:probe_6";
assign probe_[6] to "{P}Fed:{S}:probe_7";
assign probe_[7] to "{P}Fed:{S}:probe_8";
assign probe_[8] to "{P}Fed:{S}:probe_9";
assign probe_[9] to "{P}Fed:{S}:probe_10";

ss Federal_scan {
    state get_readings {
        when(run) {
            for (i = 0; i < num_probes; i++)
            {
               if (!run) break;
               pvPut(proc);         /* process the serial record */
               epicsThreadSleep(.33);    /* wait for database to parse strings */
               pvGet(readback);     /* get the whole readback string */
               pvGet(position);     /* get the position as a float */
               pvGet(probe_num);    /* get the number of the probe (int) */
               if (FederalDebug)
               {
                  printf("\nFederal.st:  ascii input: %s\n",readback);
                  printf("Federal.st:  probe number: %d\n",probe_num);
                  printf("Federal.st:  position: %3.4f\n",position);
               }
               if (probe_num > num_probes) break;
               probe_num--;
               probe_[probe_num] = position;
               pvPut(probe_[probe_num]);
            }
        } state wait
    }
    state wait {
        when(delay(0.5)) {
        } state get_readings
    }
}
