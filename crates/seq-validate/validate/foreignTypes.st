program foreignTypesTest

%%#include "../testSupport.h"

/* option +r; */

%%struct struct_t { int i; struct { double d; } s; };
%%union union_t { int i; struct struct_t *p; };
%%enum enum_t { zero, one, two };
%%typedef short typedef_t;

struct struct_t s = {1, {0.1}};
union union_t u = {13};
enum enum_t e = one;
typename typedef_t t = 42;

entry {
    seq_test_init(24);
}

ss simple {
    state simple {
        when () {
            struct struct_t *ps = &s;
            union union_t *pu = &u;
            enum enum_t *pe = &e;
            typename typedef_t *pt = &t;

            testOk1(s.i == 1);
            testOk1(s.s.d == 0.1);
            testOk1(u.i == 13);
            u.p = ps;
            testOk1(u.p == ps);
            testOk1(u.p->i == 1);
            testOk1(u.p->s.d == 0.1);
            u.i = 13; /* restore */
            testOk1(e == one);
            testOk1(t == 42);

            testOk1(ps->i == 1);
            testOk1(ps->s.d == 0.1);
            testOk1(pu->i == 13);
            pu->p = ps;
            testOk1(pu->p == ps);
            testOk1(pu->p->i == 1);
            testOk1(pu->p->s.d == 0.1);
            pu->i = 13; /* restore */
            testOk1(*pe == one);
            testOk1(*pt == 42);

            ps->i++;
            ps->s.d /= 3.0;
            pu->i--;
            *pe = two;
            *pt %= 10;

            testOk1(ps->i == 2);
            testOk1(ps->s.d == 0.1/3.0);
            testOk1(pu->i == 12);
            pu->p = ps;
            testOk1(pu->p == ps);
            testOk1(pu->p->i == 2);
            testOk1(pu->p->s.d == 0.1/3.0);
            testOk1(*pe == two);
            testOk1(*pt == 2);
        } exit
    }
}

exit {
    seq_test_done();
}
