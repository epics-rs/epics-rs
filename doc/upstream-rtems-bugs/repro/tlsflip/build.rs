//! Emits the RTEMS link arguments (the non-propagating half of the contract)
//! and stamps the image with which target spec AND which phase produced it.
fn main() {
    println!("cargo::rerun-if-env-changed=TLSDTOR_BUILD");
    println!("cargo::rerun-if-env-changed=TLSDTOR_PHASE");
    let tag = std::env::var("TLSDTOR_BUILD").unwrap_or_else(|_| "unknown".to_string());
    let phase = std::env::var("TLSDTOR_PHASE").unwrap_or_else(|_| "plain".to_string());
    println!("cargo::rustc-env=TLSDTOR_BUILD={tag}");
    println!("cargo::rustc-env=TLSDTOR_PHASE={phase}");
    epics_rtems_boot::contract::emit_link_args();
}
