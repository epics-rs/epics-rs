/*************************************************************************\
Copyright (c) 2013-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program indirectCallTest

option +r;

%%#include "../testSupport.h"

%{
typedef int fun_t(int);
struct s {
    fun_t *f;
};
int id(int x) {
    return x;
}
int inc(int x) {
    return x + 1;
}
}%

struct s gx;
struct s *p;

entry {
    seq_test_init(3*8);
}

ss test {
    struct s ssx;

    state test {
        struct s stx;
        when () {

            p = &gx;
            gx.f = id;
            testOk1(42==p->f(42));
            testOk1(42==(*p->f)(42));
            testOk1(42==gx.f(42));
            testOk1(42==(*gx.f)(42));
            gx.f = inc;
            testOk1(1==p->f(0));
            testOk1(1==(*p->f)(0));
            testOk1(1==gx.f(0));
            testOk1(1==(*gx.f)(0));

            p = &ssx;
            ssx.f = id;
            testOk1(42==p->f(42));
            testOk1(42==(*p->f)(42));
            testOk1(42==ssx.f(42));
            testOk1(42==(*ssx.f)(42));
            ssx.f = inc;
            testOk1(1==p->f(0));
            testOk1(1==(*p->f)(0));
            testOk1(1==ssx.f(0));
            testOk1(1==(*ssx.f)(0));

            p = &stx;
            stx.f = id;
            testOk1(42==p->f(42));
            testOk1(42==(*p->f)(42));
            testOk1(42==stx.f(42));
            testOk1(42==(*stx.f)(42));
            stx.f = inc;
            testOk1(1==p->f(0));
            testOk1(1==(*p->f)(0));
            testOk1(1==stx.f(0));
            testOk1(1==(*stx.f)(0));

        } exit
    }
}

exit {
    seq_test_done();
}
