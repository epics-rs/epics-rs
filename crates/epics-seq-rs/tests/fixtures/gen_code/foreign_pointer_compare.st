/* == on two foreign union pointer members, which C snc copies through
   (gen_ss_code.c:483-494) and C's compiler takes as pointer operands */
program foreignPointerCompare
%%struct struct_t { int i; struct { double d; } s; };
%%union union_t { int i; struct struct_t *p; };
union union_t u, u2;
int x;
ss a {
    state t {
        when () {
            if (u.p == u2.p)
                x = 1;
        } exit
    }
}
