use std::path::Path;

/// Test that expand_template convenience function works end-to-end.
#[test]
fn expand_template_basic() {
    let dir = tempfile::TempDir::new().unwrap();
    let tmpl = dir.path().join("test.template");
    std::fs::write(
        &tmpl,
        "record(ai, \"$(P)$(R)\")\n{\n   field(DTYP, \"$(DTYP=asynFloat64)\")\n}\n",
    )
    .unwrap();

    let result = msi_rs::expand_template(&tmpl, &[("P", "IOC:"), ("R", "ai1")], &[]).unwrap();
    assert!(result.contains("record(ai, \"IOC:ai1\")"));
    assert!(result.contains("asynFloat64"));
}

/// Test expand_template_to_file writes correctly.
#[test]
fn expand_template_to_file_basic() {
    let dir = tempfile::TempDir::new().unwrap();
    let tmpl = dir.path().join("test.template");
    let out = dir.path().join("test.db");
    std::fs::write(&tmpl, "value=$(X)\n").unwrap();

    msi_rs::expand_template_to_file(&tmpl, &out, &[("X", "42")], &[]).unwrap();

    let content = std::fs::read_to_string(&out).unwrap();
    assert_eq!(content.trim(), "value=42");
}

/// Test include resolution with expand_template.
#[test]
fn expand_template_with_includes() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("base.template"), "BASE_CONTENT\n").unwrap();
    let tmpl = dir.path().join("main.template");
    std::fs::write(&tmpl, "START\ninclude \"base.template\"\nEND\n").unwrap();

    let result = msi_rs::expand_template(&tmpl, &[], &[dir.path()]).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines, vec!["START", "BASE_CONTENT", "END"]);
}

/// Test that undefined macros in suppress mode pass through unchanged.
#[test]
fn undefined_macros_passthrough() {
    let dir = tempfile::TempDir::new().unwrap();
    let tmpl = dir.path().join("test.template");
    std::fs::write(&tmpl, "record(ai, \"$(P)$(R)\")\n").unwrap();

    let result = msi_rs::expand_template(&tmpl, &[], &[]).unwrap();
    assert!(result.contains("$(P)$(R)"));
}

/// Test substitution file parsing + template expansion end-to-end.
#[test]
fn subst_file_end_to_end() {
    let dir = tempfile::TempDir::new().unwrap();
    let tmpl = dir.path().join("test.template");
    std::fs::write(&tmpl, "record(ai, \"$(P)$(R)\")\n").unwrap();

    let subst = dir.path().join("test.substitutions");
    std::fs::write(
        &subst,
        &format!(
            "file \"{}\" {{\n  pattern {{ P, R }}\n  {{ \"IOC:\", \"ai1\" }}\n  {{ \"IOC:\", \"ai2\" }}\n}}\n",
            tmpl.display()
        ),
    )
    .unwrap();

    let sets = msi_rs::parse_subst_file(&subst).unwrap();
    assert_eq!(sets.len(), 2);

    let mut mac = msi_rs::MacHandle::new();
    mac.suppress_warnings(true);
    let proc = msi_rs::TemplateProcessor::new();

    let mut output = String::new();
    for set in &sets {
        mac.push_scope();
        let defs = msi_rs::MacHandle::parse_defns(&set.replacements);
        mac.install_macros(&defs);
        let tmpl_path = Path::new(set.filename.as_deref().unwrap());
        output.push_str(&proc.process_file(tmpl_path, &mut mac).unwrap());
        mac.pop_scope();
    }

    assert!(output.contains("\"IOC:ai1\""));
    assert!(output.contains("\"IOC:ai2\""));
}

/// Test with ADCore templates if available.
#[test]
fn adcore_golden_test() {
    let adcore_db = Path::new("/Users/stevek/codes/daq/ADCore/ADApp/Db");
    if !adcore_db.exists() {
        eprintln!("ADCore not found, skipping golden test");
        return;
    }

    // NDPluginBase includes NDArrayBase — verify include resolution
    let result = msi_rs::expand_template(
        &adcore_db.join("NDPluginBase.template"),
        &[],
        &[adcore_db],
    )
    .unwrap();

    // Should contain content from NDArrayBase (included file)
    assert!(result.contains("ADCoreVersion_RBV"));
    // Should contain content from NDPluginBase itself
    assert!(result.contains("PluginType_RBV"));
}

/// Test simDetector with nested include chain.
#[test]
fn simdetector_golden_test() {
    let adcore_db = Path::new("/Users/stevek/codes/daq/ADCore/ADApp/Db");
    let sim_db = Path::new("/Users/stevek/codes/daq/ADSimDetector/simDetectorApp/Db");
    if !adcore_db.exists() || !sim_db.exists() {
        eprintln!("ADCore/ADSimDetector not found, skipping golden test");
        return;
    }

    let result = msi_rs::expand_template(
        &sim_db.join("simDetector.template"),
        &[],
        &[adcore_db, sim_db],
    )
    .unwrap();

    // simDetector includes ADBase which includes NDArrayBase
    assert!(result.contains("ADCoreVersion_RBV")); // from NDArrayBase
    assert!(result.contains("Manufacturer_RBV"));   // from ADBase
    assert!(result.contains("SimMode"));             // from simDetector itself
}
