// Minimal RTEMS link glue for the reproduction. Point RTEMS_BSP_PREFIX at your
// RSB install prefix (the directory containing `arm-rtems6/`) and RTEMS_BSP at
// the BSP name; nothing else is needed.
fn main() {
    println!("cargo::rerun-if-changed=csrc/rtems_config.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("rtems") {
        return;
    }
    let prefix = std::env::var("RTEMS_BSP_PREFIX").expect("set RTEMS_BSP_PREFIX");
    let bsp = std::env::var("RTEMS_BSP").unwrap_or_else(|_| "xilinx_zynq_a9_qemu".into());
    let lib = format!("{prefix}/arm-rtems6/{bsp}/lib");
    let inc = format!("{lib}/include");

    // The ABI flags select the multilib; they must precede -qrtems on the
    // gcc driver line, because -qrtems expands to crt objects and search
    // paths that are chosen from the flags already seen.
    let abi = ["-march=armv7-a", "-mthumb", "-mfpu=neon", "-mfloat-abi=hard"];

    let mut b = cc::Build::new();
    // cc-rs has no target mapping for armv7-rtems-eabihf and would pick the
    // host `cc`; name the cross compiler explicitly.
    b.compiler(format!("{prefix}/bin/arm-rtems6-gcc"));
    b.file("csrc/rtems_config.c").include(&inc);
    for f in abi {
        b.flag(f);
    }
    b.compile("rtemsconfig");

    for f in abi {
        println!("cargo::rustc-link-arg={f}");
    }
    println!("cargo::rustc-link-arg=-B{lib}");
    println!("cargo::rustc-link-arg=-qrtems");
}
