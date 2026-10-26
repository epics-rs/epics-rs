// The escaped C types of test/validate/foreignTypes.st,
// test/compiler/member.st and the fixtures foreign_struct_select.st,
// foreign_pointer_compare.st and foreign_mixed_arith.st.
use epics_seq_rs::arith::AsIs;
use epics_seq_rs::{Ptr, UnionBytes, Zeroed};

// struct struct_t { int i; struct { double d; } s; };
#[derive(Clone)]
pub struct struct_t {
    pub i: i32,
    pub s: struct_t_s,
}

#[derive(Clone)]
pub struct struct_t_s {
    pub d: f64,
}

impl Zeroed for struct_t {
    fn zeroed() -> Self {
        struct_t {
            i: 0,
            s: struct_t_s { d: 0.0 },
        }
    }
}

// foreign_struct_select.st's `c ? s : s2`
impl AsIs for struct_t {}

impl<D: Into<f64>> From<(i32, (D,))> for struct_t {
    fn from((i, (d,)): (i32, (D,))) -> Self {
        struct_t {
            i,
            s: struct_t_s { d: d.into() },
        }
    }
}

// union union_t { int i; struct struct_t *p; };
#[derive(Clone)]
pub struct union_t(UnionBytes<8>);

impl union_t {
    pub fn i(&self) -> i32 {
        self.0.get()
    }
    pub fn set_i(&mut self, v: i32) {
        self.0.set(v)
    }
    pub fn p(&self) -> Ptr<struct_t> {
        self.0.get()
    }
    pub fn set_p(&mut self, v: Ptr<struct_t>) {
        self.0.set(v)
    }
}

impl Zeroed for union_t {
    fn zeroed() -> Self {
        union_t(Zeroed::zeroed())
    }
}

impl From<(i32,)> for union_t {
    fn from((i,): (i32,)) -> Self {
        let mut u = union_t::zeroed();
        u.set_i(i);
        u
    }
}

// enum enum_t { zero, one, two };
pub type enum_t = std::ffi::c_int;
pub const zero: enum_t = 0;
pub const one: enum_t = 1;
pub const two: enum_t = 2;

// typedef short typedef_t;
pub type typedef_t = i16;
