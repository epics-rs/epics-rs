program xdefchange
ss s1 {
    state a { when () {} state b }
    state b { when () {} exit }
}
void g(void) { state b; state nope; }
