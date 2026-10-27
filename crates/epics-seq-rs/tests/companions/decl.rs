// The escaped C of examples/small/decl.st.
use epics_seq_rs::Zeroed;

// struct r { int x; } rv;
#[derive(Clone)]
pub struct r {
    pub x: i32,
}

impl Zeroed for r {
    fn zeroed() -> Self {
        r { x: 0 }
    }
}

pub static rv: r = r { x: 0 };
