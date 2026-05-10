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
#[derive(Debug, Clone, Default)]
pub struct AccessRule {
    pub level: u8,
    pub write: bool, // true = WRITE rule, false = READ rule
    pub uag: Vec<String>,
    pub hag: Vec<String>,
    /// Authentication method scope (epics-base PR #563). When set,
    /// the rule only applies when the requesting client authenticated
    /// via one of the listed methods. Common values: `"anonymous"`,
    /// `"ca"`, `"x509"`, `"cap-token"`. Empty vector means "any method".
    pub method: Vec<String>,
    /// Cert authority / issuer scope (epics-base PR #563 + #618).
    /// When set, the rule only applies when the client's authenticator
    /// was vouched by one of the listed authorities — e.g. an
    /// X.509 issuer DN, or the cap-token issuer ID. Empty means "any
    /// authority".
    pub authority: Vec<String>,
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
    ///
    /// Convenience that omits the ASL gate (treats every rule as
    /// applicable). Equivalent to `check_access_asl(..., 0)` with
    /// rules typically declared at level 0/1. New code should call
    /// [`Self::check_access_asl`] so a per-record ASL can correctly
    /// disable a rule whose level is below the record's ASL.
    pub fn check_access(&self, asg_name: &str, host: &str, user: &str) -> AccessLevel {
        self.check_access_asl(asg_name, host, user, 0)
    }

    /// Method/authority-aware access check. Mirrors epics-base PR
    /// #563 (METHOD/AUTHORITY) and PR #618 (cert-based ACF). When
    /// `method` and `authority` are provided, rules with non-empty
    /// `method`/`authority` lists are gated on a literal match.
    /// Rules with empty `method`/`authority` ignore those scopes
    /// (legacy behaviour preserved).
    pub fn check_access_method(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
        method: &str,
        authority: &str,
    ) -> AccessLevel {
        let asg = match self.asg.get(asg_name) {
            Some(a) => a,
            None => match self.asg.get("DEFAULT") {
                Some(a) => a,
                None => return AccessLevel::ReadWrite,
            },
        };
        // Empty rule set: ASG declared but no RULE — legacy semantics
        // grant ReadWrite (matching `check_access_asl`'s pre-PR #563
        // behaviour). The C-G6 fix (record-ASL gate) does not apply
        // when no rules exist to gate.
        if asg.rules.is_empty() {
            return AccessLevel::ReadWrite;
        }
        if user.is_empty() || host.is_empty() {
            return self.unknown_access;
        }
        let mut can_read = false;
        let mut can_write = false;
        for rule in &asg.rules {
            if record_asl > rule.level {
                continue;
            }
            let user_match = rule.uag.is_empty()
                || rule.uag.iter().any(|g| {
                    self.uag
                        .get(g)
                        .map(|members| members.iter().any(|m| m == user))
                        .unwrap_or(false)
                });
            let host_match = rule.hag.is_empty()
                || rule.hag.iter().any(|g| {
                    self.hag
                        .get(g)
                        .map(|members| members.iter().any(|m| m == host))
                        .unwrap_or(false)
                });
            let method_match = rule.method.is_empty()
                || rule.method.iter().any(|m| m.eq_ignore_ascii_case(method));
            let authority_match = rule.authority.is_empty()
                || rule
                    .authority
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(authority));
            if user_match && host_match && method_match && authority_match {
                if rule.write {
                    can_write = true;
                    can_read = true;
                } else {
                    can_read = true;
                }
            }
        }
        match (can_read, can_write) {
            (_, true) => AccessLevel::ReadWrite,
            (true, false) => AccessLevel::Read,
            _ => AccessLevel::NoAccess,
        }
    }

    /// Check access taking the per-record ASL into account.
    ///
    /// Per epics-base `asLibRoutines.c::asCompute`: a rule with
    /// `RULE(N, …)` only applies when the record's ASL ≤ N. The
    /// canonical example is `RULE(0, READ) RULE(1, WRITE)` — every
    /// record is readable, but only records with ASL ≥ 1 are
    /// writable. Without this gate, a low-ASL record's protection
    /// is silently equivalent to ASL 0 (closes C-G6).
    pub fn check_access_asl(
        &self,
        asg_name: &str,
        host: &str,
        user: &str,
        record_asl: u8,
    ) -> AccessLevel {
        // Forward to the method-aware path with default scopes
        // (any method, any authority). Mirrors epics-base PR #563:
        // legacy ACF rules without `METHOD`/`AUTHORITY` clauses match
        // every authentication method and authority. New code should
        // call `check_access_method` directly when method/authority
        // negotiation is observable.
        self.check_access_method(asg_name, host, user, record_asl, "", "")
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
        method: Vec::new(),
        authority: Vec::new(),
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
