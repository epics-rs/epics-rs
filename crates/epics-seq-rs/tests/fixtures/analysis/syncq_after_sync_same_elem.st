program syncq_after_sync_same_elem
int s;
int v[2];
evflag ef;
evflag ef2;
assign s to "s";
assign v to { "a", "b" };
sync v[1] to ef; syncq v[1] to ef2 3;
ss s1 { state a { when () {} exit } }
