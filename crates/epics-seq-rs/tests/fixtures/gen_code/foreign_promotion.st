/* -, ~, + and << on an unsigned char member of a foreign struct and on
   ?: of two unsigned chars, which C's compiler promotes to int */
program foreignPromotion
%%struct uc_t { unsigned char uc; short sh; };
struct uc_t s;
unsigned char u;
int c;
int x;
ss a {
    state t {
        when () {
            x = (c ? s.uc : u) << 4;
            x = -(c ? s.uc : u);
            x = ~(c ? s.uc : u);
            x = (c ? s.uc : 0) << 4;
            x = -s.uc;
            x = ~s.uc;
            x = +s.uc;
            x = s.uc << s.sh;
        } exit
    }
}
