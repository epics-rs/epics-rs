program tables
option -c;
option +s;
evflag e1;
int a;
int v[3][2];
evflag e2;
double d;
assign a to "pv:a";
monitor a;
assign v to { "pv:v0", "", "pv:v2" };
monitor v[2];
sync v[0] to e2;
syncq a to e1 7;
syncq v[2] 4;
assign d;
ss one {
    state first {
        option -t;
        option +x;
        when (efTest(e1)) {} state second
    }
    state second { when () {} state first }
    state third { when () {} exit }
}
ss two {
    state only { when () {} exit }
}
