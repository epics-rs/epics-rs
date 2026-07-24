#!/usr/bin/env python3
"""Apply the heap-attribution measurement mutation to the box's epics-rs tree.

Idempotent: re-running restores from the .heapattr-orig backups first.
"""
import os, shutil, sys

HOME = os.path.expanduser("~")
REPO = os.path.join(HOME, "epics-rs")
BUILD = os.path.join(REPO, "crates/epics-rtems-boot/build.rs")
IOC = os.path.join(REPO, "crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs")

CONN_TMO = os.environ.get("HEAPATTR_CONN_TMO", "5")


def restore(path):
    bak = path + ".heapattr-orig"
    if os.path.exists(bak):
        shutil.copyfile(bak, path)
    else:
        shutil.copyfile(path, bak)


for p in (BUILD, IOC):
    restore(p)

# ---- build.rs: compile the shim ----------------------------------------
src = open(BUILD).read()
assert '.file("csrc/rtems_stats.c")' in src
src = src.replace('.file("csrc/rtems_stats.c")',
                  '.file("csrc/rtems_stats.c")\n        .file("csrc/heapattr.c")')
src = src.replace('println!("cargo::rerun-if-changed=csrc/rtems_stats.c");',
                  'println!("cargo::rerun-if-changed=csrc/rtems_stats.c");\n'
                  '    println!("cargo::rerun-if-changed=csrc/heapattr.c");')
open(BUILD, "w").write(src)

# ---- rtems-ca-ioc.rs ---------------------------------------------------
src = open(IOC).read()

# 1. extern decl, beside the other RTEMS-only C probes
anchor = ("        fn epics_rtems_boot_fd_census(tag: *const std::ffi::c_char);\n")
assert anchor in src, "fd_census extern anchor missing"
src = src.replace(anchor, anchor +
    "        // MEASUREMENT MUTATION (heap attribution rig): the --wrap=malloc\n"
    "        // accounting in `epics-rtems-boot/csrc/heapattr.c`.\n"
    "        fn epics_heapattr_report(seq: u32, attempts: u32, full: i32);\n")

# 2. call it from c6_report, right after the dialpool line is printed
anchor2 = ('             MEM_FREE={mem_free} MEM_USED={mem_used}",\n        );\n')
assert anchor2 in src, "dialpool println anchor missing"
src = src.replace(anchor2, anchor2 +
    "        // MEASUREMENT MUTATION (heap attribution rig): per-call-site live\n"
    "        // bytes beside the same attempt counter the residue is priced per.\n"
    "        // Full site listing once a minute; the size histogram every report.\n"
    "        #[cfg(target_os = \"rtems\")]\n"
    "        unsafe {\n"
    "            epics_heapattr_report(seq, dial_attempts as u32, i32::from(seq % 6 == 0));\n"
    "        }\n")

# 3. raise the redial cadence so an attempt count that resolves a ~40 B
#    per-attempt cost is reachable inside one 20-minute boot.
anchor3 = '            ("EPICS_CA_AUTO_ADDR_LIST", "NO"),\n'
assert anchor3 in src
src = src.replace(anchor3, anchor3 +
    '            // MEASUREMENT MUTATION (heap attribution rig): the shipped\n'
    '            // 30 s gives 40 attempts in 1200 s; this gives ~240.\n'
    f'            ("EPICS_CA_CONN_TMO", "{CONN_TMO}"),\n')

open(IOC, "w").write(src)
print("mutation applied; CONN_TMO=%s" % CONN_TMO)
