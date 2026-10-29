/* event mask bit of an element of a two-dimensional multi-PV array */
program subscript_2d

int m[2][3];
assign m to {"m0", "m1"};

ss s {
    state x {
        when (m[1][2]) {} state x
    }
}
