program sync_after_syncq
int s;
int v[2];
evflag ef;
evflag ef2;
assign s to "s";
assign v to { "a", "b" };
syncq s to ef 3; sync s to ef2;
ss s1 { state a { when () {} exit } }
