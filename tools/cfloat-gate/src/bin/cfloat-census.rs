//! Print every float placeholder whose argument is not `CFloat(..)`,
//! exemptions included: `cargo run -p cfloat-gate --bin cfloat-census`.

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let crates = cfloat_gate::crates_in(root, &["crates", "examples"]);
    for s in cfloat_gate::census(root, &crates) {
        println!("{}:{}: {} <- {}", s.file, s.line, s.placeholder, s.arg);
    }
}
