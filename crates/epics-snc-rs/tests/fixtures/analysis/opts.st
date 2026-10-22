program opts
option +rxyz;
option -w;
option +W;
ss s1 {
    state a {
        option +tq;
        option -ex;
        when () {} exit
    }
}
