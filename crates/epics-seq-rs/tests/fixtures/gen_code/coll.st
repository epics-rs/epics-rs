program coll
option +r;
ss a_b {
    int x;
    state s {
        when () {} state s
    }
}
ss a {
    state b {
        int y;
        when () {} state b
    }
}
