program p_monitor
int a;
int a1[3];
int a2[3];
int a3[3];
int n;
assign a to "a";
assign a1 to { "x", "" };
assign a2 to "a2";
monitor nope;
monitor n;
monitor n[0];
monitor a;
monitor a;
monitor a[0];
monitor a1[5];
monitor a1[0];
monitor a1[0];
monitor a1[2];
monitor a1;
monitor a1[1];
monitor a2[1];
monitor a3[0];
monitor a1;
ss s1 {
    int m;
    assign m to "m";
    state x {
        int k;
        assign k to "k";
        monitor k;
        monitor m;
        when () {} exit
    }
}
