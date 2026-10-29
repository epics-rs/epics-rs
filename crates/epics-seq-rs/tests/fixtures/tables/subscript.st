/* event mask bits for array subscripts in when conditions */
program subscript

evflag f;
int a[5];
assign a to {"a0", "a1", "a2", "a3", "a4"};
int b[3];
assign b to "b";
int c[4];
assign c to {"c0", "c1", "c2", "c3"};
int d[2];
int i;
assign i to "i";

ss s {
    state constant {
        when (a[2]) {} state constant
        when (b[1]) {} state whole
        when (efTest(f)) {} state whole
    }
    state whole {
        when (c[0] && a) {} state expression
    }
    state expression {
        when (a[c[i]] || d[i]) {} state single
    }
    state single {
        when (b[c[i]]) {} state constant
    }
}
