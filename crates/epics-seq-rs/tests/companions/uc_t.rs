// The escaped C type of the fixture foreign_promotion.st.
use epics_seq_rs::Zeroed;

// struct uc_t { unsigned char uc; short sh; };
#[derive(Clone)]
pub struct uc_t {
    pub uc: u8,
    pub sh: i16,
}

impl Zeroed for uc_t {
    fn zeroed() -> Self {
        uc_t { uc: 0, sh: 0 }
    }
}
