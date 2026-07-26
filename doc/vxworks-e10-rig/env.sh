S=$HOME/wrsdk-vxworks7-qemu-1.17.0
export WIND_SDK_HOME=$S WIND_HOME=$S WIND_BASE=$S/vxsdk
export WIND_CC_SYSROOT=$S/vxsdk/sysroot WIND_SDK_CC_SYSROOT=$S/vxsdk/sysroot
export WRSD_LICENSE_FILE=$S/license
export CONFIG_SITE=$S/vxsdk/sysroot/usr/mk/config.site
export WIND_SDK_CCBASE_PATH=$S/compilers/llvm-18.1.8.2/LINUX64/bin
export PATH=$HOME/.cargo/bin:$S/vxsdk/host/x86_64-linux/bin:$PATH
export LD_LIBRARY_PATH=$S/vxsdk/host/x86_64-linux/lib:${LD_LIBRARY_PATH:-}
export CARGO_HOME=$HOME/vx-rig-e10/cargo-home
export CARGO_TARGET_DIR=$HOME/vx-rig-e10/target
export RUSTUP_TOOLCHAIN=nightly
export RUSTUP_HOME=$HOME/.rustup
unset RUSTC_BOOTSTRAP RUSTFLAGS RUSTC
