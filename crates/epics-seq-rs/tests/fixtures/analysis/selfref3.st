program selfref3
struct S { struct S *next; int v; };
struct S a, b, c;
ss s1 { state a { when () {} exit } }
