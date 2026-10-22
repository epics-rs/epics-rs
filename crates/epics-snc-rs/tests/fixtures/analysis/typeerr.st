program typeerr
int x;
struct S { int (*fp)(void); int m; };
struct S s;
struct S *ps;
ss s1 {
    state a {
        when () {
            x[0]();
            x = s.fp();
            x = ps->fp();
            (*ps).fp();
            x[1][2]();
        } exit
    }
}
