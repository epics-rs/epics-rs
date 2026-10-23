program states
int x;
void fn(void) { state a; }
ss s1 {
    state a {
        entry { state b; }
        when (x) { state b; } state c
        when (x) { state nope; } state a
        when () {} state missing
        exit { state a; }
    }
    state b { when () {} state a }
    state a { when () {} exit }
    state c { when () {} exit }
    state d { when () { state e; } exit }
    state e { when () {} exit }
    state f { when () {} state f }
}
ss s1 {
    state a { when () {} exit }
}
ss s2 {
    state a { when () {} state z }
}
