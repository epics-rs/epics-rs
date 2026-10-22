program syncq_elem_after_sync
int v[2];
evflag ef;
evflag ef2;
assign v to { "a", "b" };
sync v to ef;
syncq v[0] to ef2 3;
ss s1 { state a { when () {} exit } }
