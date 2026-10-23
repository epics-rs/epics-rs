program p_sync
int a;
int a1[3];
int a2[3];
int n;
int v[2];
evflag ef;
evflag ef2;
int notef;
assign a to "a";
assign a1 to { "x", "" };
assign a2 to "a2";
assign v to { "v0", "v1" };
sync nope to ef;
sync n to ef;
sync a to nope;
sync a to notef;
sync a to ef;
sync a to ef2;
sync a1[7] to ef;
sync a1[0] to ef;
sync a1[0] to ef2;
sync a1[2] to ef;
sync a1 to ef;
sync a2[1] to ef;
sync n[0] to ef;
sync v to ef;
ss s1 {
    int m;
    assign m to "m";
    state x {
        int k;
        assign k to "k";
        sync k to ef;
        sync m to ef;
        when () {} exit
    }
}
