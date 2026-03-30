use crate::types::EpicsValue;

/// Link processing policy for input/output links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkProcessPolicy {
    NoProcess,
    #[default]
    ProcessPassive,
    /// CP: subscribe to source; when source changes, process this record.
    ChannelProcess,
}

/// Parsed link address pointing to another record's field.
#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
}

/// Parsed link — distinguishes constants, DB links, CA/PVA links, and empty.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedLink {
    None,
    Constant(String),
    Db(DbLink),
    Ca(String),
    Pva(String),
}

/// A database link to another record's field.
#[derive(Clone, Debug, PartialEq)]
pub struct DbLink {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
}

impl ParsedLink {
    /// Extract the constant as an EpicsValue (Double if numeric, else String).
    pub fn constant_value(&self) -> Option<EpicsValue> {
        if let ParsedLink::Constant(s) = self {
            if let Ok(v) = s.parse::<f64>() {
                Some(EpicsValue::Double(v))
            } else {
                Some(EpicsValue::String(s.clone()))
            }
        } else {
            None
        }
    }

    pub fn is_db(&self) -> bool {
        matches!(self, ParsedLink::Db(_))
    }
}

/// Parse a link string into a ParsedLink (v2 — distinguishes constants from DB links).
pub fn parse_link_v2(s: &str) -> ParsedLink {
    let s = s.trim();
    if s.is_empty() {
        return ParsedLink::None;
    }

    // CA/PVA protocol links
    if let Some(rest) = s.strip_prefix("ca://") {
        return ParsedLink::Ca(rest.to_string());
    }
    if let Some(rest) = s.strip_prefix("pva://") {
        return ParsedLink::Pva(rest.to_string());
    }

    // Strip trailing link attributes: PP, NPP, CP, CPP, MS, NMS, MSS, MSI
    // They can appear in any order: "REC.FIELD NPP NMS", "REC CP", etc.
    let mut policy = LinkProcessPolicy::ProcessPassive;
    let mut link_part = s;
    loop {
        let trimmed = link_part.trim_end();
        if let Some(rest) = trimmed.strip_suffix(" NMS").or_else(|| trimmed.strip_suffix(" MS"))
            .or_else(|| trimmed.strip_suffix(" MSS")).or_else(|| trimmed.strip_suffix(" MSI"))
        {
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" NPP") {
            policy = LinkProcessPolicy::NoProcess;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" CP").or_else(|| trimmed.strip_suffix(" CPP")) {
            policy = LinkProcessPolicy::ChannelProcess;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" PP") {
            policy = LinkProcessPolicy::ProcessPassive;
            link_part = rest;
            continue;
        }
        link_part = trimmed;
        break;
    }

    // Numeric constant
    if link_part.parse::<f64>().is_ok() {
        return ParsedLink::Constant(link_part.to_string());
    }

    // Quoted string constant
    if link_part.starts_with('"') && link_part.ends_with('"') && link_part.len() >= 2 {
        return ParsedLink::Constant(link_part[1..link_part.len()-1].to_string());
    }

    // DB link: try rsplit on '.', validate field part is uppercase alpha 1-4 chars
    if let Some((rec, field)) = link_part.rsplit_once('.') {
        let field_upper = field.to_ascii_uppercase();
        let is_valid_field = !field_upper.is_empty()
            && field_upper.len() <= 4
            && field_upper.chars().all(|c| c.is_ascii_uppercase());
        if is_valid_field {
            return ParsedLink::Db(DbLink {
                record: rec.to_string(),
                field: field_upper,
                policy,
            });
        }
    }

    // No dot or invalid field part → DB link with default field VAL
    ParsedLink::Db(DbLink {
        record: link_part.to_string(),
        field: "VAL".to_string(),
        policy,
    })
}

/// Parse a link string into a LinkAddress (legacy wrapper around parse_link_v2).
/// Formats: "REC.FIELD", "REC", "REC.FIELD PP", "REC.FIELD NPP", "" → None
pub fn parse_link(s: &str) -> Option<LinkAddress> {
    match parse_link_v2(s) {
        ParsedLink::Db(db) => Some(LinkAddress {
            record: db.record,
            field: db.field,
            policy: db.policy,
        }),
        _ => None,
    }
}
