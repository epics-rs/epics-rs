program functionInStructTest

%%#include "../testSupport.h"

option +W;

%%#include <stdlib.h>

struct mystruct {
    double val;
    int *phi;
    int *plo;
    %%void (*cfun)(struct mystruct *ps);   /* without implicit parameters */
    void (*fun)(struct mystruct *ps);       /* with implicit parameters */
    struct mystruct *children[3];
};

int hilo = 33;
struct mystruct s = { 42.23, &hilo, &hilo, mycfun, myfun, {&s,&s,&s}};

%{
void mycfun(struct mystruct *p);
}%

entry {
    struct mystruct *ps = *(&s.children[0]+1);

    seq_test_init(15);

    myfun(ps);
    ps->fun(ps);
    s.fun(ps);
    if (s.cfun)
        s.cfun(ps);
    ps->children[1]->fun(ps->children[0]);
}

ss trivial {
    state trivial {
        when () {} exit
    }
}

exit {
    seq_test_done();
}

void myfun(struct mystruct *p) {
    testDiag("myfun says: val=%f, *phi = %d, *plo=%d\n", p->val, *p->phi, *p->plo);
    testOk1(p->val==s.val);
    testOk1(*p->phi==hilo);
    testOk1(*p->plo==hilo);
}

%{
void mycfun(struct mystruct *p) {
    testDiag("mycfun says: val=%f, *phi = %d, *plo=%d\n", p->val, *p->phi, *p->plo);
    testOk1(p->val==s.val);
    testOk1(*p->phi==hilo);
    testOk1(*p->plo==hilo);
}
}%
