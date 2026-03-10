/// Simple C preprocessor for SNL files.
///
/// Supports object-like `#define NAME value` macros (integer, float, string literals).
/// Function-like macros are not supported and produce a warning.
/// `#include` is skipped with a warning.
/// Line mapping is maintained so Span refers to original source lines.

use std::collections::HashMap;

/// Result of preprocessing: transformed source + line mapping.
pub struct PreprocessResult {
    /// The preprocessed source text.
    pub source: String,
    /// Maps output line number (0-based) → original line number (0-based).
    pub line_map: Vec<usize>,
    /// Any warnings generated during preprocessing.
    pub warnings: Vec<String>,
}

/// Preprocess an SNL source string, expanding object-like #define macros.
pub fn preprocess(input: &str) -> PreprocessResult {
    let mut defines: HashMap<String, String> = HashMap::new();
    let mut output_lines = Vec::new();
    let mut line_map = Vec::new();
    let mut warnings = Vec::new();

    for (line_no, line) in input.lines().enumerate() {
        let trimmed = line.trim_start();

        if trimmed.starts_with('#') {
            let directive = trimmed.trim_start_matches('#').trim_start();

            if directive.starts_with("define") {
                let rest = directive["define".len()..].trim_start();
                if let Some(result) = parse_define(rest) {
                    match result {
                        DefineResult::ObjectLike { name, value } => {
                            defines.insert(name, value);
                        }
                        DefineResult::FunctionLike { name } => {
                            warnings.push(format!(
                                "line {}: function-like macro '{}' not supported, skipping",
                                line_no + 1,
                                name
                            ));
                        }
                        DefineResult::Empty { name } => {
                            // #define NAME with no value — define as empty string
                            defines.insert(name, String::new());
                        }
                    }
                }
                // Emit empty line to preserve line count
                output_lines.push(String::new());
                line_map.push(line_no);
            } else if directive.starts_with("include") {
                warnings.push(format!(
                    "line {}: #include not supported, skipping",
                    line_no + 1
                ));
                output_lines.push(String::new());
                line_map.push(line_no);
            } else if directive.starts_with("undef") {
                let rest = directive["undef".len()..].trim_start();
                let name = rest.split_whitespace().next().unwrap_or("").to_string();
                defines.remove(&name);
                output_lines.push(String::new());
                line_map.push(line_no);
            } else if directive.starts_with("ifdef")
                || directive.starts_with("ifndef")
                || directive.starts_with("if ")
                || directive.starts_with("elif")
                || directive.starts_with("else")
                || directive.starts_with("endif")
            {
                // Skip conditional compilation directives
                output_lines.push(String::new());
                line_map.push(line_no);
            } else {
                // Unknown directive, pass through as empty
                output_lines.push(String::new());
                line_map.push(line_no);
            }
        } else {
            // Substitute macros in this line
            let substituted = substitute_macros(line, &defines);
            output_lines.push(substituted);
            line_map.push(line_no);
        }
    }

    PreprocessResult {
        source: output_lines.join("\n"),
        line_map,
        warnings,
    }
}

enum DefineResult {
    ObjectLike { name: String, value: String },
    FunctionLike { name: String },
    Empty { name: String },
}

fn parse_define(rest: &str) -> Option<DefineResult> {
    let mut chars = rest.chars().peekable();

    // Read macro name
    let mut name = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            name.push(ch);
            chars.next();
        } else {
            break;
        }
    }

    if name.is_empty() {
        return None;
    }

    // Check for function-like macro: name immediately followed by '('
    if chars.peek() == Some(&'(') {
        return Some(DefineResult::FunctionLike { name });
    }

    // Skip whitespace after name
    while chars.peek().map_or(false, |c| c.is_ascii_whitespace()) {
        chars.next();
    }

    let value: String = chars.collect();
    let value = value.trim().to_string();

    // Strip any trailing comments
    let value = if let Some(idx) = value.find("//") {
        value[..idx].trim().to_string()
    } else if let Some(idx) = value.find("/*") {
        value[..idx].trim().to_string()
    } else {
        value
    };

    if value.is_empty() {
        Some(DefineResult::Empty { name })
    } else {
        Some(DefineResult::ObjectLike { name, value })
    }
}

/// Substitute all known macros in a line.
/// Uses word-boundary matching to avoid replacing inside identifiers.
fn substitute_macros(line: &str, defines: &HashMap<String, String>) -> String {
    if defines.is_empty() {
        return line.to_string();
    }

    let mut result = line.to_string();
    for (name, value) in defines {
        if !result.contains(name.as_str()) {
            continue;
        }
        // Replace whole-word occurrences only
        let mut new_result = String::with_capacity(result.len());
        let bytes = result.as_bytes();
        let name_bytes = name.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + name_bytes.len() <= bytes.len()
                && &bytes[i..i + name_bytes.len()] == name_bytes
            {
                // Check word boundaries
                let before_ok = i == 0
                    || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                let after_ok = i + name_bytes.len() >= bytes.len()
                    || !(bytes[i + name_bytes.len()].is_ascii_alphanumeric()
                        || bytes[i + name_bytes.len()] == b'_');
                if before_ok && after_ok {
                    new_result.push_str(value);
                    i += name_bytes.len();
                    continue;
                }
            }
            new_result.push(bytes[i] as char);
            i += 1;
        }
        result = new_result;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_define() {
        let input = "#define MAX 100\nint x = MAX;\n";
        let result = preprocess(input);
        assert!(result.source.contains("int x = 100;"));
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn test_float_define() {
        let input = "#define PI 3.14159\ndouble x = PI;\n";
        let result = preprocess(input);
        assert!(result.source.contains("double x = 3.14159;"));
    }

    #[test]
    fn test_string_define() {
        let input = "#define PV_NAME \"MOTOR:POS\"\nassign x to PV_NAME;\n";
        let result = preprocess(input);
        assert!(result.source.contains("assign x to \"MOTOR:POS\";"));
    }

    #[test]
    fn test_function_like_macro_warning() {
        let input = "#define SQR(x) ((x)*(x))\nint y = SQR(3);\n";
        let result = preprocess(input);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("function-like"));
        // SQR should NOT be expanded in the source
        assert!(result.source.contains("SQR(3)"));
    }

    #[test]
    fn test_include_warning() {
        let input = "#include \"foo.h\"\nint x;\n";
        let result = preprocess(input);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("#include"));
    }

    #[test]
    fn test_undef() {
        let input = "#define X 10\nint a = X;\n#undef X\nint b = X;\n";
        let result = preprocess(input);
        assert!(result.source.contains("int a = 10;"));
        assert!(result.source.contains("int b = X;"));
    }

    #[test]
    fn test_line_mapping() {
        let input = "#define N 5\nint x = N;\nint y = N;\n";
        let result = preprocess(input);
        // Line 0 = #define (empty), Line 1 = int x = 5, Line 2 = int y = 5
        assert_eq!(result.line_map.len(), 3);
        assert_eq!(result.line_map[0], 0);
        assert_eq!(result.line_map[1], 1);
        assert_eq!(result.line_map[2], 2);
    }

    #[test]
    fn test_word_boundary() {
        let input = "#define N 5\nint NMAX = 10;\nint x = N;\n";
        let result = preprocess(input);
        // NMAX should NOT be replaced
        assert!(result.source.contains("int NMAX = 10;"));
        assert!(result.source.contains("int x = 5;"));
    }

    #[test]
    fn test_empty_define() {
        let input = "#define FEATURE\nint x;\n";
        let result = preprocess(input);
        assert!(result.warnings.is_empty());
        // FEATURE with empty value — should replace with nothing
        assert!(result.source.contains("int x;"));
    }

    #[test]
    fn test_define_with_comment() {
        let input = "#define MAX 100 // maximum value\nint x = MAX;\n";
        let result = preprocess(input);
        assert!(result.source.contains("int x = 100;"));
    }

    #[test]
    fn test_conditional_directives_skipped() {
        let input = "#ifdef FOO\nint x;\n#endif\nint y;\n";
        let result = preprocess(input);
        // #ifdef and #endif should be empty lines, but content between is passed through
        assert!(result.source.contains("int x;"));
        assert!(result.source.contains("int y;"));
    }

    #[test]
    fn test_multiple_defines() {
        let input = "#define A 1\n#define B 2\nint x = A + B;\n";
        let result = preprocess(input);
        assert!(result.source.contains("int x = 1 + 2;"));
    }
}
