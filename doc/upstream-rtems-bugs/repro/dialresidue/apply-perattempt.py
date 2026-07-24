#!/usr/bin/env python3
"""MEASUREMENT MUTATION 2: put the CA client's dial back on the pre-`9daff491`
shape -- one transient thread per dial attempt, no pool -- so the residue the
DialPool removes can be read directly instead of inferred from a differential.

Run AFTER apply-mutation.py (it keeps that file's edits).
"""
import os, shutil

HOME = os.path.expanduser("~")
T = os.path.join(HOME, "epics-rs/crates/epics-ca-rs/src/client/transport.rs")
bak = T + ".pooled-orig"
if os.path.exists(bak):
    shutil.copyfile(bak, T)
else:
    shutil.copyfile(T, bak)

src = open(T).read()

anchor = "    let dialed_rx = match CA_DIAL_POOL.dial(server_addr) {"
assert anchor in src, "dial call anchor missing"
src = src.replace(anchor, "    let dialed_rx = match dial_one_shot(server_addr) {")

helper = '''
/// MEASUREMENT MUTATION (heap attribution rig): the pre-`9daff491` dial shape.
///
/// One transient thread per attempt, created and retired per dial, with the
/// same `oneshot` reply channel and the same name/priority/stack class the pool
/// gives its workers. This is the shape §13.4's differential priced at 176 B
/// per attempt; measuring it directly under `--wrap=malloc` is what turns that
/// difference into attributed bytes.
fn dial_one_shot(
    target: SocketAddr,
) -> std::io::Result<tokio::sync::oneshot::Receiver<std::io::Result<std::net::TcpStream>>> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    epics_base_rs::runtime::task::spawn_dedicated_thread(
        format!("CAC-connect {target}"),
        CAC_RECV_PRIORITY,
        epics_base_rs::runtime::task::StackSizeClass::Small,
        move || {
            let dialed = std::net::TcpStream::connect(target);
            let _ = reply.send(dialed);
        },
    )?;
    Ok(rx)
}

'''

anchor2 = "async fn dial_blocking(server_addr: SocketAddr)"
assert anchor2 in src
src = src.replace(anchor2, helper.lstrip("\n") + anchor2, 1)

open(T, "w").write(src)
print("per-attempt mutation applied")
