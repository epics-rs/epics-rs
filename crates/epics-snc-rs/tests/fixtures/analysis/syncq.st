program p_syncq
int a;
int a1[3];
int a2[3];
int b;
int c;
int d;
int n;
int v[2];
evflag ef;
evflag ef2;
evflag ef3;
int notef;
assign a to "a";
assign a1 to { "x", "" };
assign a2 to "a2";
assign b to "b";
assign c to "c";
assign d to "d";
assign v to { "v0", "v1" };
syncq nope to ef 5;
syncq n to ef 5;
syncq a to nope 5;
syncq a to notef 5;
syncq a to ef 0;
syncq a to ef 4294967295;
syncq a to ef 4294967294;
syncq a to ef 5;
syncq b to ef 5;
syncq c to ef2;
syncq a1[9] 2;
syncq a1[0] 2;
syncq a1[0] 2;
syncq a1[2] 2;
syncq a1 2;
syncq a2[1] 2;
syncq d 0x10;
syncq v[0] to ef3 3;
syncq v to ef3 3;
ss s1 {
    int m;
    assign m to "m";
    state x {
        int k;
        assign k to "k";
        syncq k 3;
        syncq m 3;
        when () {} exit
    }
}
