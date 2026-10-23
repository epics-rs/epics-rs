/* escape sequences in channel names and program parameters */
program escapes("name=\"q\"\\x")

int a;
assign a to "{P}\tx\101\"";
int b;
assign b to "\1012\x41g\?";
int c;
assign c to "c\0d";

ss s {
    state x {
        when () {} exit
    }
}
