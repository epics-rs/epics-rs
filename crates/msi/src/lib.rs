pub mod error;
pub mod maclib;
pub mod subst;
pub mod template;

pub use error::MsiError;
pub use maclib::MacHandle;
pub use subst::{parse_subst_file, parse_subst_string, SubstSet};
pub use template::TemplateProcessor;

use std::path::Path;

/// Convenience function: expand a template file with given macros and include paths.
/// Runs in suppress_warnings mode so undefined macros (like $(P), $(R)) pass through.
pub fn expand_template(
    template_path: &Path,
    macros: &[(&str, &str)],
    include_paths: &[&Path],
) -> Result<String, MsiError> {
    let mut mac = MacHandle::new();
    mac.suppress_warnings(true);
    for &(name, value) in macros {
        mac.put_value(name, Some(value));
    }
    let mut proc = TemplateProcessor::new();
    for &path in include_paths {
        proc.add_include_path(path);
    }
    proc.process_file(template_path, &mut mac)
}

/// Convenience function: expand a template file and write the result to an output file.
pub fn expand_template_to_file(
    template_path: &Path,
    output_path: &Path,
    macros: &[(&str, &str)],
    include_paths: &[&Path],
) -> Result<(), MsiError> {
    let result = expand_template(template_path, macros, include_paths)?;
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| MsiError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    std::fs::write(output_path, &result).map_err(|e| MsiError::Io {
        path: output_path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}
