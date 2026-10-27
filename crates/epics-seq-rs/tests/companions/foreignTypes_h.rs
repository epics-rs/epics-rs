// test/compiler/foreignTypes.h, and the stdio.h test/compiler/foreignTypes.st
// calls.
use epics_seq_rs::{Ptr, UnionBytes, UnionMember, Zeroed};
use std::ffi::c_void;

// enum enum_test { E0, E1 };
pub type enum_test = std::ffi::c_int;
pub const E0: enum_test = 0;
pub const E1: enum_test = 1;

// struct struct_test {
//     struct { union { double x; } y; } s; int i; double d; enum enum_test e;
// };
#[derive(Clone)]
pub struct struct_test {
    pub s: struct_test_s,
    pub i: i32,
    pub d: f64,
    pub e: enum_test,
}

#[derive(Clone)]
pub struct struct_test_s {
    pub y: struct_test_s_y,
}

#[derive(Clone)]
pub struct struct_test_s_y(UnionBytes<8>);

impl struct_test_s_y {
    pub fn x(&self) -> f64 {
        self.0.get()
    }
    pub fn set_x(&mut self, v: f64) {
        self.0.set(v)
    }
}

impl Zeroed for struct_test {
    fn zeroed() -> Self {
        struct_test {
            s: struct_test_s {
                y: struct_test_s_y(Zeroed::zeroed()),
            },
            i: 0,
            d: 0.0,
            e: E0,
        }
    }
}

impl<X: Into<f64>, D: Into<f64>> From<(((X,),), i32, D, enum_test)> for struct_test {
    fn from((((x,),), i, d, e): (((X,),), i32, D, enum_test)) -> Self {
        let mut t = struct_test::zeroed();
        t.s.y.set_x(x.into());
        (t.i, t.d, t.e) = (i, d.into(), e);
        t
    }
}

// a struct_test in a union, at C's offsets on LP64
impl UnionMember for struct_test {
    fn load(bytes: &[u8], ptr: &Ptr<c_void>) -> Self {
        let mut t = struct_test::zeroed();
        t.s.y.set_x(f64::load(&bytes[0..], ptr));
        t.i = i32::load(&bytes[8..], ptr);
        t.d = f64::load(&bytes[16..], ptr);
        t.e = i32::load(&bytes[24..], ptr);
        t
    }

    fn store(self, bytes: &mut [u8], ptr: &mut Ptr<c_void>) {
        self.s.y.x().store(&mut bytes[0..], ptr);
        self.i.store(&mut bytes[8..], ptr);
        self.d.store(&mut bytes[16..], ptr);
        self.e.store(&mut bytes[24..], ptr);
    }
}

// typedef struct struct_test typedef_test;
pub type typedef_test = struct_test;

// union union_test { struct struct_test t; int i; double d; enum enum_test e; };
#[derive(Clone)]
pub struct union_test(UnionBytes<32>);

impl union_test {
    pub fn t(&self) -> struct_test {
        self.0.get()
    }
    pub fn set_t(&mut self, v: struct_test) {
        self.0.set(v)
    }
    pub fn i(&self) -> i32 {
        self.0.get()
    }
    pub fn set_i(&mut self, v: i32) {
        self.0.set(v)
    }
    pub fn d(&self) -> f64 {
        self.0.get()
    }
    pub fn set_d(&mut self, v: f64) {
        self.0.set(v)
    }
    pub fn e(&self) -> enum_test {
        self.0.get()
    }
    pub fn set_e(&mut self, v: enum_test) {
        self.0.set(v)
    }
}

impl Zeroed for union_test {
    fn zeroed() -> Self {
        union_test(Zeroed::zeroed())
    }
}

pub struct FILE;

pub fn fopen(_: &str, _: &str) -> Ptr<FILE> {
    Ptr::default()
}

pub fn fread(_: i32, _: i32, _: i32, _: Ptr<FILE>) -> usize {
    0
}

pub fn fclose(_: Ptr<FILE>) -> i32 {
    0
}
