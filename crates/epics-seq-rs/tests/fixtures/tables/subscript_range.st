/* constant subscripts of a multi-PV array outside its range */
program subscript_range

int a[5];
assign a to {"a0", "a1", "a2", "a3", "a4"};

ss s {
    state x {
        when (a[5]) {} state x
        when (a[1.5]) {} state x
        when (a[TRUE]) {} state x
        when (a[0x4]) {} state x
    }
}
