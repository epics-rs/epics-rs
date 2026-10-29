program xdefchange2
ss s1 {
    state a { when () {} state b }
    state b { when () {} exit }
}
exit { }
void g(void) { state b; }
