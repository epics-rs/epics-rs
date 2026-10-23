program shadow
int pvPut;
int TRUE;
int x;
ss s1 {
    int x;
    state a {
        when (pvPut(x)) { x = TRUE; } exit
    }
}
