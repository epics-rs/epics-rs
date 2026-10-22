program syncq_elem_twice
int s;
int v[2];
evflag ef;
evflag ef2;
assign s to "s";
assign v to { "a", "b" };
syncq v[0] to ef 3; syncq v[0] to ef2 3;
ss s1 { state a { when () {} exit } }
