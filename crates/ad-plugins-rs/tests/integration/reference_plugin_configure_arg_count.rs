//! Every ADCore plugin `*Configure` command is registered, with the arguments
//! its C `iocshFuncDef` registers, which is what `help` prints and how many
//! arguments iocsh converts.
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::collections::BTreeMap;

use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::reference::{ReferenceTree, reference_path};

/// `name -> nargs` of every `iocshFuncDef` in ADCore's `pluginSrc`.
fn c_nargs() -> BTreeMap<String, usize> {
    let dir = reference_path(ReferenceTree::Modules, "ADCore/ADApp/pluginSrc");
    let mut defs = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        // A checkout copied from macOS carries `._*.cpp` resource forks.
        let hidden = path.file_name().unwrap().to_string_lossy().starts_with('.');
        if hidden || path.extension().is_none_or(|e| e != "cpp") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for (at, _) in source.match_indices("iocshFuncDef") {
            // `iocshFuncDef <ident> = {"<name>", <nargs>, ...`
            let Some(rest) = source[at..].split_once('=').map(|(_, r)| r) else {
                continue;
            };
            let Some(body) = rest.trim_start().strip_prefix('{') else {
                continue;
            };
            let mut fields = body.splitn(3, ',');
            let name = fields.next().unwrap().trim().trim_matches('"').to_string();
            let nargs =
                fields.next().unwrap().trim().parse().unwrap_or_else(|_| {
                    panic!("{}: nargs of {name} is not a literal", path.display())
                });
            defs.insert(name, nargs);
        }
    }
    defs
}

#[test]
fn each_c_configure_command_is_registered_with_c_s_argument_count() {
    let c = c_nargs();
    assert!(c.len() > 20, "found only {c:?}");
    let ioc = AdIoc::new();
    let defs = ioc.app().startup_commands();
    let registered: Vec<&str> = {
        let mut r: Vec<&str> = defs
            .iter()
            .map(|d| d.name.as_str())
            .filter(|n| n.starts_with("ND") && n.contains("Config"))
            .collect();
        r.sort();
        r
    };
    let table: Vec<&str> = c.keys().map(String::as_str).collect();
    assert_eq!(
        registered, table,
        "the registered Configure commands are C's"
    );

    let wrong: Vec<String> = c
        .iter()
        .filter_map(|(name, &nargs)| {
            // C registers 9 but reads a 10th, maxThreads (NDPluginBadPixel.cpp:402-408).
            let want = if name == "NDBadPixelConfigure" {
                nargs + 1
            } else {
                nargs
            };
            let def = defs.iter().find(|d| d.name == *name).unwrap();
            (def.args.len() != want).then(|| format!("{name}: {} != {want}", def.args.len()))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The file plugins take `priority, stackSize` right after `NDArrayAddr`
/// (NDFileTIFF.cpp:651-660), not the generic `maxBuffers, maxMemory`.
#[test]
fn file_plugins_name_priority_and_stack_size_at_five_and_six() {
    let ioc = AdIoc::new();
    for def in ioc
        .app()
        .startup_commands()
        .iter()
        .filter(|d| d.name.starts_with("NDFile"))
    {
        let names: Vec<&str> = def.args[5..].iter().map(|a| a.name).collect();
        assert_eq!(names, ["priority", "stackSize"], "{}", def.name);
    }
}
