/* sync, syncQ with and without a size, unnamed and empty channel names,
   elements without a name, types */
program queues

evflag e1;
evflag e2;
evflag e3;
int q;
assign q;
monitor q;
syncq q to e1 5;
float r;
assign r to "";
monitor r;
syncq r 3;
char c[4];
assign c to {"c0", "c1", "c2", "c3"};
monitor c;
sync c[1] to e2;
syncq c[2] to e3;
unsigned short us;
assign us to "us";
string str;
assign str to "str";
unsigned char uc[10];
assign uc to "uc";
int8_t i8;
assign i8 to "i8";
uint32_t u32;
assign u32 to "u32";
long m[3];
assign m to {"m0"};
int n[3];
assign n[1] to "n1";

ss s {
    state x {
        when (efTest(e1) && efTest(e2) && efTest(e3) && q) {} state x
    }
}
