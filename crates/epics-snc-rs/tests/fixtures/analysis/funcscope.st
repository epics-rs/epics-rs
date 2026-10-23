program funcscope
int g;
int f(int a, evflag e) { int g; return a; }
ss s1 {
    int h(int);
    state a {
        when () { int k(void); } exit
    }
}
