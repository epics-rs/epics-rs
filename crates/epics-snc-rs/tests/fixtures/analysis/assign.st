program p_assign
int a;
int a1[3];
int a2[2][3];
int a3[2][3][4];
struct S { int m; };
struct S s;
int *p;
int const c = 1;
evflag ef;
assign nope to "x";
assign s to "x";
assign p to "x";
assign c to "x";
assign a3 to "x";
assign ef to "x";
assign a to "pv:a";
assign a to "pv:a2";
assign a[0] to "pv:a0";
assign a1[0] to "pv:e0";
assign a1[0] to "pv:e0b";
assign a1[3] to "pv:e3";
assign a1[0x2] to "pv:e2";
assign a1 to "pv:whole";
assign a1 to { "x", "y" };
assign a2 to { "p", "q", "r" };
assign a2;
int b[2];
assign b to { "u", "v", "w", "z" };
int d[2];
assign d to {};
int e[2];
assign e[1] to "e1";
assign e to { "f0" };
ss s1 {
    int l;
    state a {
        int m;
        assign m to "st:m";
        assign a to "late";
        when () {} exit
    }
}
