program selfref4
struct S { struct S *next; int v; };
struct S a, b;
ss s1 { state a { when () {} exit } }
