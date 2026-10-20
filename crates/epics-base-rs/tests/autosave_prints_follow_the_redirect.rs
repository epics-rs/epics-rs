//! autosave's `printf`s go to the thread's stdout (`epicsStdio.h`), so an
//! iocsh `> file` on the command that reaches them, at any depth, takes
//! them. The expected files are the C oracle IOC's for the same script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_PRINTS_FOLLOW_THE_REDIRECT_SCRIPT";

const HEADER: &str =
    "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n";

/// The st.cmd, with `asSupportNfs.dbd`'s command, in the child only.
#[epics_macros_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let mut app = IocApplication::new()
        .port(0)
        .autosave_startup(holder.clone());
    for cmd in AutosaveStartupConfig::nfs_commands(holder) {
        app = app.register_startup_command(cmd);
    }
    app.startup_script(script.to_str().unwrap())
        .run(|config| async move {
            if let Some(mgr) = config.autosave_manager {
                mgr.shutdown();
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// Runs `script` (`{d}` is the directory) and returns each named file's
/// lines, `{d}` put back, each pointer reading `*` and iocInit's banner gone.
fn redirected(files: &[(&str, &str)], script: &str, names: &[&str]) -> Vec<Vec<String>> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for (name, text) in files {
        std::fs::write(d.join(name), text).unwrap();
    }
    let d = d.display().to_string();
    std::fs::write(format!("{d}/st.cmd"), script.replace("{d}", &d)).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{d}/st.cmd"))
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    names
        .iter()
        .map(|name| {
            std::fs::read_to_string(format!("{d}/{name}"))
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with("##"))
                .map(|l| unaddressed(l.replace(&d, "{d}").trim_end()))
                .collect()
        })
        .collect()
}

fn unaddressed(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("0x") {
        out += &rest[..i];
        out.push('*');
        rest = rest[i + 2..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out + rest
}

/// dbrestore.c's: `autosaveBuild`, the `dbLoadRecords` hook, and iocInit's
/// boot restore.
#[test]
fn the_build_hook_and_boot_restore_lines_follow_the_redirect() {
    let files = [
        ("a.db", "record(ao, \"A\") { field(VAL, \"3\") }\n"),
        ("b.db", "record(ao, \"X\") { field(VAL, \"1\") }\n"),
        ("b_settings.req", "X\n"),
        ("p.sav", &format!("{HEADER}A 7\n<END>\n")),
    ];
    let script = "dbLoadRecords(\"{d}/a.db\")\n\
                  set_requestfile_path(\"{d}\")\n\
                  set_savefile_path(\"{d}\")\n\
                  save_restoreSet_DatedBackupFiles(0)\n\
                  save_restoreSet_Debug(5)\n\
                  autosaveBuild(\"built.req\", \"_settings.req\", 1) > {d}/build.txt\n\
                  dbLoadRecords(\"{d}/b.db\") > {d}/load.txt\n\
                  save_restoreSet_Debug(2)\n\
                  set_pass0_restoreFile(\"p.sav\")\n\
                  iocInit > {d}/init.txt\n\
                  save_restoreSet_Debug(0)\n";
    let [build, load, init] = <[_; 3]>::try_from(redirected(
        &files,
        script,
        &["build.txt", "load.txt", "init.txt"],
    ))
    .unwrap();
    assert_eq!(
        build,
        ["autosaveBuild: initialized and enabled filename 'built.req' and suffix '_settings.req'."]
    );
    // The request-file search between the hook's lines is save_restore.c's.
    assert_eq!(
        load,
        [
            "myDbLoadRecordsHook: dbFileName='{d}/b.db'; subs=''",
            "save_restore:concatenate_paths: dest='{d}/b_settings.req'",
            "myDbLoadRecordsHook: found 'b_settings.req'",
        ]
    );
    assert_eq!(
        init,
        [
            "checkFile: entry",
            "checkFile: version=5.000000",
            "fopen_and_check: checkFile returned *",
        ]
    );
}

/// save_restore.c's, from the commands and from the request-file read
/// a set's creation does on the command's own thread.
#[test]
fn the_command_lines_follow_the_redirect() {
    let files = [
        (
            "a.db",
            "record(ao, \"A\") { field(VAL, \"3\") }\n\
             record(longout, \"L\") { field(VAL, \"9\") }\n",
        ),
        ("x.req", "A\nL\n"),
        ("m.sav", &format!("{HEADER}A 5\n<END>\n")),
    ];
    let script = "dbLoadRecords(\"{d}/a.db\")\n\
                  save_restoreSet_Debug(20)\n\
                  set_savefile_path(\"{d}\") > {d}/path.txt\n\
                  set_requestfile_path(\"{d}\")\n\
                  save_restoreSet_DatedBackupFiles(0)\n\
                  save_restoreSet_FilePermissions(0644) > {d}/perm.txt\n\
                  save_restoreSet_CallbackTimeout(3) > {d}/timeout.txt\n\
                  save_restoreSet_Debug(0)\n\
                  iocInit\n\
                  save_restoreSet_Debug(20)\n\
                  create_manual_set(\"x.req\") > {d}/create.txt\n\
                  epicsThreadSleep 1\n\
                  fdbrestore(\"m.sav\") > {d}/restore.txt\n\
                  epicsThreadSleep 2\n\
                  manual_save(\"x.req\") > {d}/save.txt\n\
                  epicsThreadSleep 2\n\
                  save_restoreSet_Debug(0)\n";
    let [path, perm, timeout, create, restore, save] = <[_; 6]>::try_from(redirected(
        &files,
        script,
        &[
            "path.txt",
            "perm.txt",
            "timeout.txt",
            "create.txt",
            "restore.txt",
            "save.txt",
        ],
    ))
    .unwrap();
    assert_eq!(path, ["save_restore:concatenate_paths: dest='{d}'"]);
    assert_eq!(perm, ["save_restore: File permissions set to 0644"]);
    assert_eq!(
        timeout,
        ["save_restoreCallbackTimeout must be either negative (forever) or >= 4 seconds"]
    );
    assert_eq!(
        create,
        [
            "save_restore:create_data_set: file 'x.req', method 10, period 0, trig_chan 'NONE', mon_period 0",
            "lockList: listLock=1",
            "unlockList: listLock=0",
            "save_restore:concatenate_paths: dest='{d}/x.sav'",
            "save_restore:readReqFile: entry: reqFile='x.req', plist=*, macrostring='NULL'",
            "save_restore:openReqFile: entry: reqFile='x.req', fpp=*",
            "save_restore:concatenate_paths: dest='{d}/x.req'",
            "openReqFile: found 'x.req' by searching",
            "save_restore:readReqFile: line='A",
            "', eline='A",
            "', name='A'",
            "save_restore:readReqFile: line='L",
            "', eline='L",
            "', name='L'",
            "save_restore:readReqFile: exit: reqFile='x.req'.",
            "lockList: listLock=1",
            "unlockList: listLock=0",
        ]
    );
    assert_eq!(
        restore,
        [
            "save_restore:fdbrestore:entry",
            "save_restore:request_manual_restore: entry",
        ]
    );
    assert_eq!(
        save,
        [
            "manual_save: request_file='x.req', save_file='(null)', callbackFunction=(nil), puserPvt=(nil)"
        ]
    );
}

/// configMenuSub.c's, from the routine a `dbpf` processes on its thread.
#[test]
fn the_config_menu_line_follows_the_redirect() {
    let mut db = String::from(
        "record(aSub, \"LIST\") {\n\
         field(INAM, \"configMenuList_init\") field(SNAM, \"configMenuList_do\")\n\
         field(FTA, \"STRING\") field(FTB, \"SHORT\") field(FTC, \"SHORT\")\n",
    );
    for f in 'A'..='T' {
        db += &format!("field(FTV{f}, \"STRING\")\n");
    }
    db += "}\n";
    let script = "dbLoadRecords(\"{d}/list.db\")\n\
                  set_savefile_path(\"{d}/none\")\n\
                  iocInit\n\
                  dbpf LIST.A X\n\
                  dbpf LIST.PROC 1 > {d}/list.txt\n";
    let [list] =
        <[_; 1]>::try_from(redirected(&[("list.db", &db)], script, &["list.txt"])).unwrap();
    assert_eq!(
        list,
        [
            "configMenuList_do(X): findConfigFiles returned -1",
            "DBF_UCHAR:          1 = *",
        ]
    );
}

/// nfs_utils.c's; os/Linux/osdNfs.c's line, a plain `printf`, stays on the
/// process's stdout. No oracle IOC registers the command, so the lines are
/// nfs_utils.c:27 and :91.
#[test]
fn the_nfs_lines_follow_the_redirect() {
    let script = "save_restoreSet_NFSHost(\"h\", \"1.2.3.4\", \"\") > {d}/none.txt\n\
                  save_restoreSet_NFSHost(\"h\", \"1.2.3.4\", \"/mnt/x\") > {d}/mount.txt\n";
    let [none, mount] =
        <[_; 2]>::try_from(redirected(&[], script, &["none.txt", "mount.txt"])).unwrap();
    assert_eq!(
        none,
        ["save_restore: Mount point not specified, cannot mount NFS."]
    );
    assert_eq!(mount, ["save_restore: Can't mount '/mnt/x'"]);
}
