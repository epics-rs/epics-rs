program Keithley2kDMM_mf40 ("P=13LAB:, Dmm=DMM1, channels=40, model=2700")

/*  This sequencer program works with Keithley2kDMM.db.  This version of the
 *  Keithley2kDMM support has been significantly rearranged so that the database
 *  contains largely unlinked records and the flow control is done by this
 *  sequence program.  The support has been significantly enhanced to permit
 *  each channel to have a different mode (V DC, Ohms, etc.).
 *
 *  Mark Rivers
 *  March 17, 1998
 *
 *  Based on original database by Kurt Goetze
 *  Modifications:
 *  22-APR-1999 MLR  Added logic to work around bug with AC volts on scanner
 *  31-MAY-1999 MLR  Fixed logic so it does not close relay repeated if only
 *                   one channel being used in multi-channel mode
 *  27-OCT-2002 MLR  Modifications to:
 *                   Fix bug which could cause channels to read incorrectly
 *                   after an initialize
 *                   Added support for Model 2700, which uses different syntax
 *                   for selecting plugin channels
 *                   Added support for 20 channels.
 *                   Two new parameters in starting program:
 *                      channels: default=10, maximum=20
 *                      model:    default=2000, choices=2000 and 2700
 * 28-OCT-2002 MLR   Changed the order of the mode select and close commands in
 *                   multichannel mode.  Previously we were doing close first then
 *                   mode select.  This did not work in 4-wire ohms mode, must do
 *                   in the opposite order, because meter closes pairs of switches
 *                   in this mode.
 * 07-NOV-2002 MLR   Added support for single channel selection of channels 11-22.
 * 07-NOV-2005 JPS   Added support for 40 channels (2700)
*/

/* This program must be compiled with the recursive option so that multiple copies
 * can run at once */
option +r;

#define MAX_CHANNELS 40
#define NUM_FRONT_MODES 10
#define NUM_REAR_MODES 8
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
            "{P}{Dmm}ch10_mode_sel.VAL",
            "{P}{Dmm}ch11_mode_sel.VAL",
            "{P}{Dmm}ch12_mode_sel.VAL",
            "{P}{Dmm}ch13_mode_sel.VAL",
            "{P}{Dmm}ch14_mode_sel.VAL",
            "{P}{Dmm}ch15_mode_sel.VAL",
            "{P}{Dmm}ch16_mode_sel.VAL",
            "{P}{Dmm}ch17_mode_sel.VAL",
            "{P}{Dmm}ch18_mode_sel.VAL",
            "{P}{Dmm}ch19_mode_sel.VAL",
            "{P}{Dmm}ch20_mode_sel.VAL",
            "{P}{Dmm}ch21_mode_sel.VAL",
            "{P}{Dmm}ch22_mode_sel.VAL",
            "{P}{Dmm}ch23_mode_sel.VAL",
            "{P}{Dmm}ch24_mode_sel.VAL",
            "{P}{Dmm}ch25_mode_sel.VAL",
            "{P}{Dmm}ch26_mode_sel.VAL",
            "{P}{Dmm}ch27_mode_sel.VAL",
            "{P}{Dmm}ch28_mode_sel.VAL",
            "{P}{Dmm}ch29_mode_sel.VAL",
            "{P}{Dmm}ch30_mode_sel.VAL",
            "{P}{Dmm}ch31_mode_sel.VAL",
            "{P}{Dmm}ch32_mode_sel.VAL",
            "{P}{Dmm}ch33_mode_sel.VAL",
            "{P}{Dmm}ch34_mode_sel.VAL",
            "{P}{Dmm}ch35_mode_sel.VAL",
            "{P}{Dmm}ch36_mode_sel.VAL",
            "{P}{Dmm}ch37_mode_sel.VAL",
            "{P}{Dmm}ch38_mode_sel.VAL",
            "{P}{Dmm}ch39_mode_sel.VAL",
            "{P}{Dmm}ch40_mode_sel.VAL"};

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
            "{P}{Dmm}Ch10_on_off.VAL",
            "{P}{Dmm}Ch11_on_off.VAL",
            "{P}{Dmm}Ch12_on_off.VAL",
            "{P}{Dmm}Ch13_on_off.VAL",
            "{P}{Dmm}Ch14_on_off.VAL",
            "{P}{Dmm}Ch15_on_off.VAL",
            "{P}{Dmm}Ch16_on_off.VAL",
            "{P}{Dmm}Ch17_on_off.VAL",
            "{P}{Dmm}Ch18_on_off.VAL",
            "{P}{Dmm}Ch19_on_off.VAL",
            "{P}{Dmm}Ch20_on_off.VAL",
            "{P}{Dmm}Ch21_on_off.VAL",
            "{P}{Dmm}Ch22_on_off.VAL",
            "{P}{Dmm}Ch23_on_off.VAL",
            "{P}{Dmm}Ch24_on_off.VAL",
            "{P}{Dmm}Ch25_on_off.VAL",
            "{P}{Dmm}Ch26_on_off.VAL",
            "{P}{Dmm}Ch27_on_off.VAL",
            "{P}{Dmm}Ch28_on_off.VAL",
            "{P}{Dmm}Ch29_on_off.VAL",
            "{P}{Dmm}Ch30_on_off.VAL",
            "{P}{Dmm}Ch31_on_off.VAL",
            "{P}{Dmm}Ch32_on_off.VAL",
            "{P}{Dmm}Ch33_on_off.VAL",
            "{P}{Dmm}Ch34_on_off.VAL",
            "{P}{Dmm}Ch35_on_off.VAL",
            "{P}{Dmm}Ch36_on_off.VAL",
            "{P}{Dmm}Ch37_on_off.VAL",
            "{P}{Dmm}Ch38_on_off.VAL",
            "{P}{Dmm}Ch39_on_off.VAL",
            "{P}{Dmm}Ch40_on_off.VAL"};
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
            "{P}{Dmm}Ch10_raw.PROC",
            "{P}{Dmm}Ch11_raw.PROC",
            "{P}{Dmm}Ch12_raw.PROC",
            "{P}{Dmm}Ch13_raw.PROC",
            "{P}{Dmm}Ch14_raw.PROC",
            "{P}{Dmm}Ch15_raw.PROC",
            "{P}{Dmm}Ch16_raw.PROC",
            "{P}{Dmm}Ch17_raw.PROC",
            "{P}{Dmm}Ch18_raw.PROC",
            "{P}{Dmm}Ch19_raw.PROC",
            "{P}{Dmm}Ch20_raw.PROC",
            "{P}{Dmm}Ch21_raw.PROC",
            "{P}{Dmm}Ch22_raw.PROC",
            "{P}{Dmm}Ch23_raw.PROC",
            "{P}{Dmm}Ch24_raw.PROC",
            "{P}{Dmm}Ch25_raw.PROC",
            "{P}{Dmm}Ch26_raw.PROC",
            "{P}{Dmm}Ch27_raw.PROC",
            "{P}{Dmm}Ch28_raw.PROC",
            "{P}{Dmm}Ch29_raw.PROC",
            "{P}{Dmm}Ch30_raw.PROC",
            "{P}{Dmm}Ch31_raw.PROC",
            "{P}{Dmm}Ch32_raw.PROC",
            "{P}{Dmm}Ch33_raw.PROC",
            "{P}{Dmm}Ch34_raw.PROC",
            "{P}{Dmm}Ch35_raw.PROC",
            "{P}{Dmm}Ch36_raw.PROC",
            "{P}{Dmm}Ch37_raw.PROC",
            "{P}{Dmm}Ch38_raw.PROC",
            "{P}{Dmm}Ch39_raw.PROC",
            "{P}{Dmm}Ch40_raw.PROC"};


/* Note:  dci and aci must be the last 2 modes.  These are defined for
   front panel use only. */
int dmm_modes[NUM_FRONT_MODES];
assign dmm_modes to 
           {"{P}{Dmm}conf_dcv.PROC",
            "{P}{Dmm}conf_acv.PROC",
            "{P}{Dmm}conf_ohm2w.PROC",
            "{P}{Dmm}conf_ohm4w.PROC",
            "{P}{Dmm}conf_freq.PROC",
            "{P}{Dmm}conf_tempj.PROC",
            "{P}{Dmm}conf_tempk.PROC",
            "{P}{Dmm}conf_tempt.PROC",
            "{P}{Dmm}conf_dci.PROC",
            "{P}{Dmm}conf_aci.PROC"};

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

int dmm_chan2;
assign  dmm_chan2 to "{P}{Dmm}dmm_chan2.VAL";
monitor dmm_chan2;
evflag  dmm_chan2_mon;
sync    dmm_chan2 dmm_chan2_mon;

int dmm_chan3;
assign  dmm_chan3 to "{P}{Dmm}dmm_chan3.VAL";
monitor dmm_chan3;
evflag  dmm_chan3_mon;
sync    dmm_chan3 dmm_chan3_mon;

int dmm_chan4;
assign  dmm_chan4 to "{P}{Dmm}dmm_chan4.VAL";
monitor dmm_chan4;
evflag  dmm_chan4_mon;
sync    dmm_chan4 dmm_chan4_mon;

int dmm_ch_range;
assign  dmm_ch_range to "{P}{Dmm}dmm_ch_range.VAL";
monitor dmm_ch_range;

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
            unit_strings[0] = "VDC";
            unit_strings[1] = "VAC";
            unit_strings[2] = "OHM";
            unit_strings[3] = "OHM";
            unit_strings[4] = "HZ";
            unit_strings[5] = "DegC";
            unit_strings[6] = "DegC";
            unit_strings[7] = "DegC";
            unit_strings[8] = "ADC";
            unit_strings[9] = "AAC";
            P = macValueGet("P");
            Dmm = macValueGet("Dmm");
            strncpy(model, macValueGet("model"), sizeof(model));
            model[sizeof(model)-1] = 0;
            pvPut(model);
            channels = atoi(macValueGet("channels"));
            pvPut(channels);
            if (strcmp(model, "2000") == 0) {
               strncpy(close_format, "rout:clos (@%d)", sizeof(close_format));
            } else {
               strncpy(close_format, "rout:clos (@1%2.2d)", sizeof(close_format));
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
                /* Channels 21 and 22 on the model 2700 can only do DC and AC current, 
                   and these are the first two choices, different from all other channels
                */
                /* 
                if ((strcmp(model, "2700") == 0) && (i > 19)) {
                    mode = mode + 8;
                }
                */
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

        when(efTestAndClear(dmm_chan2_mon) && (channels > 10)) {
            sprintf(ch_close, close_format, dmm_chan2+11);
            pvPut(ch_close);
            previous_chan = dmm_chan2+10;
        } state monitor_mode_changes


        when(efTestAndClear(dmm_chan3_mon) && (channels > 20)) {
            sprintf(ch_close, close_format, dmm_chan3+21);
            pvPut(ch_close);
            previous_chan = dmm_chan3+20;
        } state monitor_mode_changes


        when(efTestAndClear(dmm_chan4_mon) && (channels > 30)) {
            sprintf(ch_close, close_format, dmm_chan4+31);
            pvPut(ch_close);
            previous_chan = dmm_chan4+30;
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
            if (dmm_ch_range == 1) chan = dmm_chan2+10;
            else if (dmm_ch_range == 2) chan = dmm_chan3+20;
            else if (dmm_ch_range == 3) chan = dmm_chan4+30;
            if (chan != previous_chan) {
                sprintf(ch_close, close_format, chan+1);
                pvPut(ch_close);
                previous_chan = chan;
            }
            previous_mode = ch_mode_sel;
            read_complete=0;
            pvPut(read_complete);
            efClear(read_complete_mon);
            pvPut(dmm_read);
        } state single_read_response

       when((single_multi != 0) && (ch_on_off[scan_chan] == 0)) {
            mode = ch_mode[scan_chan];
            /* Channels 21 and 22 on the model 2700 can only do DC and AC current, and these
               are the first two choices, different from all other channels
            */
            /*
            if ((strcmp(model, "2700") == 0) && (scan_chan > 19)) {
                mode = mode + 8;
            }
	    */
            if (mode != previous_mode) {
                pvPut(dmm_modes[mode]);
                previous_mode = mode;
            }
            if (scan_chan != previous_chan) {
                sprintf(ch_close, close_format, scan_chan+1);
                pvPut(ch_close);
                previous_chan = scan_chan;
            }
            read_complete=0;
            pvPut(read_complete);
            efClear(read_complete_mon);
            /* This logic adds a delay between the ch_clos and the
             * dmm_read if the function is AC volts, to work around bug in
             * Keithley 2000. */
            if (mode == 1)
                pvPut(dmm_delay_read);
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
