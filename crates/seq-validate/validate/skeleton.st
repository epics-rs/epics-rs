program xxxTest

%%#include "../testSupport.h"

entry {
    seq_test_init(0);
}

ss test {
    state init {
        when () {
        } state done
    }
    state done {
        when () {
        } exit
    }
}

exit {
    seq_test_done();
}
