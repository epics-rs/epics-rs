program syncq_after_sync
int s;
int v[2];
evflag ef;
evflag ef2;
assign s to "s";
assign v to { "a", "b" };
sync s to ef; syncq s to ef2 3;
ss s1 { state a { when () {} exit } }
