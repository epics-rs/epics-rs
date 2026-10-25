program Keithley65xxEM ("P=13LAB:, Dmm=EM1, channels=10, model=6517A")

/*  This sequencer program works with Keithley65xxEM.db.  This version of the
 *  Keithley65xx Electrometer support has been arranged so that the database
 *  contains largely unlinked records and the flow control is done by this
 *  sequence program.  The support has been significantly enhanced to permit
 *  each channel to have a different mode (V DC, Ohms, etc.).
 *
 *  Orinal Author: Mark Rivers
 *  March 17, 1998
 *  Ported to 6517A by Joe Sullivan 
 *  Sept. 7, 2004
 *
 *  Based on original database by Kurt Goetze
 *  Based on original database(Keithley2KDMM.db) and sequence(Keithley2KDMM.st) by 
 *      Mark Rivers
 *  Modifications:
 * 07-SEPT-2004 JPS   Original port to 65xx Series of Electrometer from 2000 DMM code
*/

/* This program must be compiled with the recursive option so that multiple copies
 * can run at once */
option +r;

#define MAX_CHANNELS 10
#define NUM_FRONT_MODES 4
%% #include "stdlib.h"
%% #include "string.h"

int i;
int scan_chan;
int previous_mode;
int previous_chan;

int    ch_mode[MAX_CHANNELS];
assign  ch_mode to 
           {"{P}{Dmm}ch1_mode_sel.VAL",
            "{P}{Dmm}ch2_mode_sel.VAL",
            "{P}{Dmm}ch3_mode_sel.VAL",
            "{P}{Dmm}ch4_mode_sel.VAL",
            "{P}{Dmm}ch5_mode_sel.VAL",
            "{P}{Dmm}ch6_mode_sel.VAL",
            "{P}{Dmm}ch7_mode_sel.VAL",
            "{P}{Dmm}ch8_mode_sel.VAL",
            "{P}{Dmm}ch9_mode_sel.VAL",
            "{P}{Dmm}ch10_mode_sel.VAL"};

monitor ch_mode;
evflag  ch_mode_mon;
sync    ch_mode ch_mode_mon;

int    ch_on_off[MAX_CHANNELS];
assign  ch_on_off to 
           {"{P}{Dmm}Ch1_on_off.VAL",
            "{P}{Dmm}Ch2_on_off.VAL",
            "{P}{Dmm}Ch3_on_off.VAL",
            "{P}{Dmm}Ch4_on_off.VAL",
            "{P}{Dmm}Ch5_on_off.VAL",
            "{P}{Dmm}Ch6_on_off.VAL",
            "{P}{Dmm}Ch7_on_off.VAL",
            "{P}{Dmm}Ch8_on_off.VAL",
            "{P}{Dmm}Ch9_on_off.VAL",
            "{P}{Dmm}Ch10_on_off.VAL"};
monitor ch_on_off;

int    ch_raw[MAX_CHANNELS];
assign  ch_raw to 
           {"{P}{Dmm}Ch1_raw.PROC",
            "{P}{Dmm}Ch2_raw.PROC",
            "{P}{Dmm}Ch3_raw.PROC",
            "{P}{Dmm}Ch4_raw.PROC",
            "{P}{Dmm}Ch5_raw.PROC",
            "{P}{Dmm}Ch6_raw.PROC",
            "{P}{Dmm}Ch7_raw.PROC",
            "{P}{Dmm}Ch8_raw.PROC",
            "{P}{Dmm}Ch9_raw.PROC",
            "{P}{Dmm}Ch10_raw.PROC"};

/* Note:  dci and aci must be the last 2 modes.  These are defined for
   front panel use only. */
int dmm_modes[NUM_FRONT_MODES];
assign dmm_modes to 
           {"{P}{Dmm}conf_dcv.PROC",
            "{P}{Dmm}conf_dci.PROC",
            "{P}{Dmm}conf_ohm.PROC",
            "{P}{Dmm}conf_coul.PROC"};
/*
 *          "{P}{Dmm}conf_tempk.PROC",
 *          "{P}{Dmm}conf_humid.PROC" */


int ch_mode_sel;
assign ch_mode_sel to "{P}{Dmm}ch_mode_sel.VAL";
monitor ch_mode_sel;
evflag  ch_mode_sel_mon;
sync    ch_mode_sel ch_mode_sel_mon;


int done_read;
assign  done_read to "{P}{Dmm}done_read.VAL";
monitor done_read;

int dmm_chan;
assign  dmm_chan to "{P}{Dmm}dmm_chan.VAL";
monitor dmm_chan;
evflag  dmm_chan_mon;
sync    dmm_chan dmm_chan_mon;

int dmm_delay_read;
assign  dmm_delay_read to "{P}{Dmm}dmm_delay_read.PROC";

int dmm_read;
assign  dmm_read to "{P}{Dmm}dmm_read.PROC";

int Dmm_raw;
assign  Dmm_raw to "{P}{Dmm}Dmm_raw.PROC";

int read_complete;
assign  read_complete to "{P}{Dmm}read_complete.VAL";
monitor read_complete;
evflag  read_complete_mon;
sync    read_complete read_complete_mon;

int single_multi;
assign  single_multi to "{P}{Dmm}single_multi.VAL";
monitor single_multi;

int init_dmm;
assign init_dmm to "{P}{Dmm}init_dmm.VAL";
monitor init_dmm;
evflag  init_dmm_mon;
sync    init_dmm init_dmm_mon;

int do_init;
assign do_init to "{P}{Dmm}init_string.PROC";

string ch_close;  assign  ch_close to "{P}{Dmm}ch_close.VAL";
string model; assign model to "{P}{Dmm}model.VAL";
int channels; assign channels to "{P}{Dmm}channels.VAL";
string dmm_units; assign dmm_units to "{P}{Dmm}units.VAL";
string ch_units;  assign ch_units to "";

char *unit_strings[NUM_FRONT_MODES];
char *P;
char *Dmm;
string pvname;
string close_format;
int mode;
int chan;

ss mode_change {
    state init {
        when() {
            /* We set the following event flags to force these modes and
               labels to be set correctly initially */
            efSet(ch_mode_mon);
            efSet(ch_mode_sel_mon);
            efSet(dmm_chan_mon);
            unit_strings[0] = "VOLT";
            unit_strings[1] = "AMP";
            unit_strings[2] = "OHM";
            unit_strings[3] = "COULOMB";
            P = macValueGet("P");
            Dmm = macValueGet("Dmm");
            strncpy(model, macValueGet("model"), sizeof(model));
            model[sizeof(model)-1] = 0;
            pvPut(model);
            channels = atoi(macValueGet("channels"));
            pvPut(channels);
            if (strcmp(model, "6517A") == 0) {
               strncpy(close_format, "rout:clos (@%d)", sizeof(close_format));
            } else {
               strncpy(close_format, "rout:clos (@%d)", sizeof(close_format));
            }
            close_format[sizeof(close_format)-1] = 0;
        } state monitor_mode_changes
    }
    state monitor_mode_changes {
        when(efTestAndClear(ch_mode_mon)) {
            for (i=0; i<channels; i++) {
                /* Note: We have detected that one of the channel modes has
                   changed.  For now we update all the channel labels.
                   Although inefficient, it is done infrequently and the lack
                   of string arrays in SNC makes any other approach a real
                   mess. 
                */
                mode = ch_mode[i];
                sprintf(ch_units, "%s", unit_strings[mode]);
                sprintf(pvname, "%s%sch%d_units.VAL", P, Dmm, i+1);
                pvAssign(ch_units, pvname);
                pvPut(ch_units);
            }
        } state monitor_mode_changes

        when(efTestAndClear(ch_mode_sel_mon)) {
            sprintf(dmm_units, "%s", unit_strings[ch_mode_sel]);
            pvPut(dmm_units);
            pvPut(dmm_modes[ch_mode_sel]);
            previous_mode = ch_mode_sel;
            /* If 4-wire mode was selected open all channels */
            if (ch_mode_sel == 3) {
                sprintf(ch_close, "rout:open:all");
                pvPut(ch_close);
                previous_chan = -1;
            }
        } state monitor_mode_changes

        when(efTestAndClear(dmm_chan_mon)) {
            sprintf(ch_close, close_format, dmm_chan+1);
            pvPut(ch_close);
            previous_chan = dmm_chan;
        } state monitor_mode_changes

        when(efTestAndClear(init_dmm_mon)) {
            if (init_dmm == 1) {
                scan_chan = 0;
                previous_mode = -1;
                previous_chan = -1;
                init_dmm = 0;
                pvPut(init_dmm);
                pvPut(do_init);
            }
        } state monitor_mode_changes
    }
}

ss read_meter {
    state init {
        when() {
            scan_chan = 0;
            previous_mode = -1;
            previous_chan = -1;
            efClear(read_complete_mon);
            pvPut(do_init);
        } state wait_read
    }
    state wait_read {
        when(done_read == 1) {
        } state read_channel
    }
    state read_channel {
        when (single_multi == 0) {
            if (ch_mode_sel != previous_mode) {
                sprintf(dmm_units, "%s", unit_strings[ch_mode_sel]);
                pvPut(dmm_units);
                pvPut(dmm_modes[ch_mode_sel]);
            }
            /* Issue close channel if different */
            chan = dmm_chan;
            if (chan != previous_chan) {
                sprintf(ch_close, close_format, chan+1);
                pvPut(ch_close);
                previous_chan = chan;
            }
            previous_mode = ch_mode_sel;
            read_complete=0;
            pvPut(read_complete);
            efClear(read_complete_mon);
            if (ch_mode_sel != previous_mode) {
                /* Makeup for lack of handshaking by delaying multiple commands */
	        previous_mode = ch_mode_sel;
                pvPut(dmm_delay_read);
	    }
            else
                pvPut(dmm_read);
	    
        } state single_read_response

       when((single_multi != 0) && (ch_on_off[scan_chan] == 0)) {
            mode = ch_mode[scan_chan];
            if (mode != previous_mode) {
                pvPut(dmm_modes[mode]);
            }
            if (scan_chan != previous_chan) {
                sprintf(ch_close, close_format, scan_chan+1);
                pvPut(ch_close);
                previous_chan = scan_chan;
            }
            read_complete=0;
            pvPut(read_complete);
            efClear(read_complete_mon);
            if (mode != previous_mode) {
                /* Makeup for lack of handshaking by delaying multiple commands */
	        previous_mode = mode;
                pvPut(dmm_delay_read);
	    }
            else
                pvPut(dmm_read);
        } state multi_read_response

        when((single_multi != 0) && (ch_on_off[scan_chan] != 0)) {
        } state next_channel
    }
    state single_read_response {
        when(efTestAndClear(read_complete_mon) && read_complete) {
            pvPut(Dmm_raw);
            done_read = 0;
            pvPut(done_read);
        } state wait_read
    }
    state multi_read_response {
        when(efTestAndClear(read_complete_mon) && read_complete) {
            pvPut(ch_raw[scan_chan]);
        } state next_channel
    }
    state next_channel {
        when (scan_chan < (channels-1)) {
            scan_chan++;
        } state read_channel
        when (scan_chan >= (channels-1)) {
            scan_chan = 0;
            done_read = 0;
            pvPut(done_read);
        } state wait_read
    }
}
