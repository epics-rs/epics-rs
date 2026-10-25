program structdefTest

%%#include "../testSupport.h"

option +W;

%%#include <stdlib.h>

struct limits {
    double low;
    double high;
};

struct mystruct {
    char *name;
    double value[5];
    struct limits warning;
    struct limits alarm;
    double deadband;
    %%void (*process)(struct mystruct *ps);
    struct mystruct *children[2];
    int mark;
};

%{
void test_array(double *v, double *w, int n);
void test_mystruct(struct mystruct *p);
}%

struct mystruct s = { "xxx", {0.0,1.1,2.2,3.3,4.4}, {-8,+8}, {-10,+10}, 0.01, test_mystruct, {&s,&s}, 0};

entry {
    struct mystruct *ps = *(&s.children[0]+1);

    seq_test_init(24);

    if (s.process)
        s.process(ps);
    ps->children[1]->process(ps->children[0]);
}

ss trivial {
    state trivial {
        when (delay(1)) {} exit
    }
}

exit {
    seq_test_done();
}

%{
void test_array(double *v, double *w, int n) {
    int i;
    for (i=0; i<n; i++) {
        testDiag("v[%d]=%.1f",i,v[i]);
        testDiag("w[%d]=%.1f",i,w[i]);
        testOk1(v[i]==w[i]);
    }
}

void test_mystruct(struct mystruct *p) {
    int i;
    if (p->mark) {
        testDiag("%s already dumped\n", p->name);
    } else {
        p->mark = 1;
        testDiag("name=%s, value=", p->name);
        testOk1(p->name==s.name);
        test_array(p->value, s.value, 5);
        testDiag("  warning={%.1f,%.1f}, alarm={%.1f,%.1f}, deadband=%.1f, process=%p\n",
            p->warning.low, p->warning.high,
            p->alarm.low, p->alarm.high,
            p->deadband, p->process);
        testOk1(p->warning.low==s.warning.low);
        testOk1(p->warning.high==s.warning.high);
        testOk1(p->alarm.low==s.alarm.low);
        testOk1(p->alarm.high==s.alarm.high);
        testOk1(p->deadband==s.deadband);
        testOk1(p->process==s.process);
        for (i=0; i<2; i++) {
            test_mystruct(p->children[i]);
        }
        p->mark = 0;
    }
}
}%
