/* ?: on two foreign structs, which C snc copies through
   (gen_ss_code.c:483-494) and C's compiler takes as struct operands */
program foreignStructSelect
%%struct struct_t { int i; struct { double d; } s; };
struct struct_t s, s2, s3;
int c;
ss a {
    state t {
        when () {
            s3 = c ? s : s2;
        } exit
    }
}
