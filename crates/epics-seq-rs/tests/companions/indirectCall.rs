// The escaped C of test/validate/indirectCall.st.
use epics_seq_rs::Zeroed;

// typedef int fun_t(int);
pub type fun_t = fn(i32) -> i32;

// struct s { fun_t *f; };
#[derive(Clone)]
pub struct s {
    pub f: fun_t,
}

impl Zeroed for s {
    fn zeroed() -> Self {
        s { f: null_fun }
    }
}

fn null_fun(_: i32) -> i32 {
    panic!("call through a null function pointer")
}

pub fn id(x: i32) -> i32 {
    x
}

pub fn inc(x: i32) -> i32 {
    x + 1
}
