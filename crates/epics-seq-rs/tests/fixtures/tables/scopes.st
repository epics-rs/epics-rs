/* channels of state set and state variables, entry and exit functions,
   state options, several state sets */
program scopes("P=x:,Q=y")

int g;
assign g to "{P}g";
monitor g;

entry {}

ss first {
    int s;
    assign s to "{P}s";
    double sa[3];
    assign sa to {"{P}sa0", "{P}sa1", "{P}sa2"};
    monitor sa;

    state one {
        option -t;
        option +e;
        int t;
        assign t to "{Q}t";
        short ta[2];
        assign ta to {"{Q}ta0", "{Q}ta1"};
        monitor ta[1];
        entry {}
        when (t || ta[1] || s || sa[2] || g) {} state two
        exit {}
    }
    state two {
        option +x;
        option -e;
        when (sa) {} state one
    }
}

ss second {
    state only {
        option -t;
        option +x;
        option +e;
        when (g) {} exit
    }
}

exit {}
