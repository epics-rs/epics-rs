use std::collections::HashMap;

use crate::error::{CaError, CaResult};

/// Access level for a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessLevel {
    NoAccess,
    Read,
    ReadWrite,
}

/// A single access rule within an ASG.
#[derive(Debug, Clone)]
pub struct AccessRule {
    pub level: u8,
    pub write: bool, // true = WRITE rule, false = READ rule
    pub uag: Vec<String>,
    pub hag: Vec<String>,
}

/// Access Security Group.
#[derive(Debug, Clone, Default)]
pub struct AccessSecurityGroup {
    pub rules: Vec<AccessRule>,
}

/// Access Security Configuration parsed from an ACF file.
#[derive(Debug, Clone)]
pub struct AccessSecurityConfig {
    pub uag: HashMap<String, Vec<String>>,
    pub hag: HashMap<String, Vec<String>>,
    pub asg: HashMap<String, AccessSecurityGroup>,
    pub unknown_access: AccessLevel,
}

impl AccessSecurityConfig {
    /// Check access for a given ASG, hostname, and username.
    pub fn check_access(&self, asg_name: &str, host: &str, user: &str) -> AccessLevel {
        let asg = match self.asg.get(asg_name) {
            Some(a) => a,
            None => {
                // Fall back to DEFAULT ASG
                match self.asg.get("DEFAULT") {
                    Some(a) => a,
                    None => return AccessLevel::ReadWrite, // No ACF = full access
                }
            }
        };

        // If user or host unknown, use conservative default
        if user.is_empty() || host.is_empty() {
            return self.unknown_access;
        }

        let mut can_read = false;
        let mut can_write = false;

        for rule in &asg.rules {
            let user_match = if rule.uag.is_empty() {
                true // No UAG restriction = all users
            } else {
                rule.uag.iter().any(|uag_name| {
                    self.uag
                        .get(uag_name)
                        .map(|members| members.iter().any(|m| m == user))
                        .unwrap_or(false)
                })
            };

            let host_match = if rule.hag.is_empty() {
                true // No HAG restriction = all hosts
            } else {
                rule.hag.iter().any(|hag_name| {
                    self.hag
                        .get(hag_name)
                        .map(|members| members.iter().any(|m| m == host))
                        .unwrap_or(false)
                })
            };

            if user_match && host_match {
                if rule.write {
                    can_write = true;
                    can_read = true;
                } else {
                    can_read = true;
                }
            }
        }

        // If no rules at all, default to ReadWrite
        if asg.rules.is_empty() {
            return AccessLevel::ReadWrite;
        }

        if can_write {
            AccessLevel::ReadWrite
        } else if can_read {
            AccessLevel::Read
        } else {
            AccessLevel::NoAccess
        }
    }
}

/// Parse an ACF (Access Control File).
pub fn parse_acf(content: &str) -> CaResult<AccessSecurityConfig> {
    let mut config = AccessSecurityConfig {
        uag: HashMap::new(),
        hag: HashMap::new(),
        asg: HashMap::new(),
        unknown_access: AccessLevel::Read,
    };

    let mut chars = content.chars().peekable();
    let mut buf = String::new();

    while chars.peek().is_some() {
        skip_ws_comments(&mut chars);
        buf.clear();
        read_word(&mut chars, &mut buf);

        match buf.as_str() {
            "UAG" => {
                let name = read_paren_name(&mut chars)?;
                let members = read_brace_list(&mut chars)?;
                config.uag.insert(name, members);
            }
            "HAG" => {
                let name = read_paren_name(&mut chars)?;
                let members = read_brace_list(&mut chars)?;
                config.hag.insert(name, members);
            }
            "ASG" => {
                let name = read_paren_name(&mut chars)?;
                let asg = parse_asg_body(&mut chars)?;
                config.asg.insert(name, asg);
            }
            "" => break,
            other => {
                return Err(CaError::Protocol(format!(
                    "ACF: unexpected keyword '{other}'"
                )));
            }
        }
    }

    Ok(config)
}

fn skip_ws_comments(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            // Skip line comment
            while let Some(&c) = chars.peek() {
                chars.next();
                if c == '\n' {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

fn read_word(chars: &mut std::iter::Peekable<std::str::Chars>, buf: &mut String) {
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            buf.push(c);
            chars.next();
        } else {
            break;
        }
    }
}

fn read_paren_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<String> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '('".into()));
    }
    skip_ws_comments(chars);
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c == ')' {
            chars.next();
            break;
        }
        if !c.is_whitespace() {
            name.push(c);
        }
        chars.next();
    }
    Ok(name)
}

fn read_brace_list(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<Vec<String>> {
    skip_ws_comments(chars);
    if chars.next() != Some('{') {
        return Err(CaError::Protocol("ACF: expected '{'".into()));
    }
    let mut items = Vec::new();
    let mut current = String::new();

    loop {
        skip_ws_comments(chars);
        match chars.peek() {
            Some(&'}') => {
                chars.next();
                break;
            }
            Some(&',') => {
                chars.next();
                if !current.is_empty() {
                    items.push(current.clone());
                    current.clear();
                }
            }
            Some(&c) if c.is_alphanumeric() || c == '_' || c == '.' || c == '-' => {
                current.push(c);
                chars.next();
            }
            Some(_) => {
                chars.next();
            }
            None => return Err(CaError::Protocol("ACF: unterminated '{'".into())),
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    Ok(items)
}

fn parse_asg_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> CaResult<AccessSecurityGroup> {
    skip_ws_comments(chars);
    if chars.next() != Some('{') {
        return Err(CaError::Protocol("ACF: expected '{' after ASG name".into()));
    }

    let mut asg = AccessSecurityGroup::default();

    loop {
        skip_ws_comments(chars);
        match chars.peek() {
            Some(&'}') => {
                chars.next();
                break;
            }
            Some(_) => {
                let mut kw = String::new();
                read_word(chars, &mut kw);
                if kw == "RULE" {
                    let rule = parse_rule(chars)?;
                    asg.rules.push(rule);
                } else if kw.is_empty() {
                    chars.next(); // skip unknown char
                }
            }
            None => return Err(CaError::Protocol("ACF: unterminated ASG".into())),
        }
    }

    Ok(asg)
}

fn parse_rule(chars: &mut std::iter::Peekable<std::str::Chars>) -> CaResult<AccessRule> {
    skip_ws_comments(chars);
    if chars.next() != Some('(') {
        return Err(CaError::Protocol("ACF: expected '(' after RULE".into()));
    }

    // Read level
    skip_ws_comments(chars);
    let mut level_str = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            level_str.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let level: u8 = level_str.parse().unwrap_or(1);

    skip_ws_comments(chars);
    if chars.peek() == Some(&',') {
        chars.next();
    }

    // Read access type
    skip_ws_comments(chars);
    let mut access_str = String::new();
    read_word(chars, &mut access_str);
    let write = access_str.eq_ignore_ascii_case("WRITE");

    skip_ws_comments(chars);
    if chars.peek() == Some(&')') {
        chars.next();
    }

    // Optional body with UAG/HAG
    let mut uag = Vec::new();
    let mut hag = Vec::new();

    skip_ws_comments(chars);
    if chars.peek() == Some(&'{') {
        chars.next();
        loop {
            skip_ws_comments(chars);
            match chars.peek() {
                Some(&'}') => {
                    chars.next();
                    break;
                }
                Some(_) => {
                    let mut kw = String::new();
                    read_word(chars, &mut kw);
                    if kw == "UAG" {
                        let name = read_paren_name(chars)?;
                        uag.push(name);
                    } else if kw == "HAG" {
                        let name = read_paren_name(chars)?;
                        hag.push(name);
                    }
                }
                None => break,
            }
        }
    }

    Ok(AccessRule {
        level,
        write,
        uag,
        hag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_acf_basic() {
        let acf = r#"
UAG(admins) { user1, user2 }
HAG(operators) { host1, host2 }
ASG(DEFAULT) {
    RULE(1, WRITE) { UAG(admins) HAG(operators) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag.get("admins").unwrap(), &["user1", "user2"]);
        assert_eq!(config.hag.get("operators").unwrap(), &["host1", "host2"]);
        assert!(config.asg.contains_key("DEFAULT"));
        assert_eq!(config.asg["DEFAULT"].rules.len(), 2);
    }

    #[test]
    fn test_parse_acf_hag_uag() {
        let acf = r#"
UAG(ops) { alice, bob }
HAG(lab) { lab-pc1 }
ASG(SECURE) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        assert_eq!(config.uag["ops"], vec!["alice", "bob"]);
        assert_eq!(config.hag["lab"], vec!["lab-pc1"]);
    }

    #[test]
    fn test_check_access_default_rw() {
        let acf = "ASG(DEFAULT) { RULE(1, WRITE) RULE(1, READ) }";
        let config = parse_acf(acf).unwrap();
        assert_eq!(
            config.check_access("DEFAULT", "host1", "user1"),
            AccessLevel::ReadWrite
        );
    }

    #[test]
    fn test_check_access_read_only() {
        let acf = r#"
UAG(admins) { admin1 }
ASG(READONLY) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) }
}
"#;
        let config = parse_acf(acf).unwrap();
        // admin1 gets RW
        assert_eq!(
            config.check_access("READONLY", "host1", "admin1"),
            AccessLevel::ReadWrite
        );
        // Other users get read only
        assert_eq!(
            config.check_access("READONLY", "host1", "regular"),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_check_access_hag_uag_match() {
        let acf = r#"
UAG(ops) { alice }
HAG(lab) { lab-pc1 }
ASG(CONTROLLED) {
    RULE(1, WRITE) { UAG(ops) HAG(lab) }
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Alice on lab-pc1 gets RW
        assert_eq!(
            config.check_access("CONTROLLED", "lab-pc1", "alice"),
            AccessLevel::ReadWrite
        );
        // Alice on wrong host gets READ
        assert_eq!(
            config.check_access("CONTROLLED", "other-host", "alice"),
            AccessLevel::Read
        );
        // Wrong user on lab-pc1 gets READ
        assert_eq!(
            config.check_access("CONTROLLED", "lab-pc1", "bob"),
            AccessLevel::Read
        );
    }

    #[test]
    fn test_check_access_unknown_user() {
        let acf = r#"
ASG(DEFAULT) {
    RULE(1, WRITE)
    RULE(1, READ)
}
"#;
        let config = parse_acf(acf).unwrap();
        // Unknown user/host → conservative default
        assert_eq!(config.check_access("DEFAULT", "", ""), AccessLevel::Read);
    }
}
