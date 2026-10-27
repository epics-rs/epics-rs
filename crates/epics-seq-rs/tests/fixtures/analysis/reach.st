program reach
ss s1 {
    state a {
        entry { state c; }
        when () { state b; } state a
        when () {} exit
    }
    state b { when () {} exit }
    state c { when () {} exit }
    state d { when () {} exit exit { state e; } }
    state e { when () {} exit }
    state f { when () { if (1) { state g; } else { while (1) { state h; } } } exit }
    state g { when () {} exit }
    state h { when () {} state f }
}
