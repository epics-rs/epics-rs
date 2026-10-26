/* arithmetic, a comparison and ?: on a double and an int member of a
   foreign struct, which C's compiler converts by the usual arithmetic
   conversions */
program foreignMixedArith
%%struct struct_t { int i; struct { double d; } s; };
struct struct_t s;
double x;
int c;
ss a {
    state t {
        when () {
            x = s.s.d * s.i;
            s.s.d *= s.i;
            c = s.s.d > s.i;
            x = c ? s.s.d : s.i;
            s.i *= 3;
        } exit
    }
}
