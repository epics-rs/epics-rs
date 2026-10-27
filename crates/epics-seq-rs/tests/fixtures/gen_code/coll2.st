program coll2
option +r;
ss a {
    state b_c {
        int y;
        when () {} state b_c
    }
}
ss a_b {
    state c {
        int z;
        when () {} state c
    }
}
