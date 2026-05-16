//! HDF5 custom-layout XML engine.
//!
//! Ports the C++ `NDFileHDF5LayoutXML.cpp` + `NDFileHDF5Layout.cpp` parser.
//! Builds the group/dataset tree from a user XML file (`HDF5_layoutFilename`),
//! mirroring `XML_schema/hdf5_xml_layout_schema.xsd`.
//!
//! The parser is hand-written in the same focused-parser style as
//! `ad-core-rs` `read_nd_attributes_file` — no external XML crate dependency.

use std::fmt;

/// Data source kind for a layout node (schema `dsetSourceEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutSource {
    /// Detector image data destination.
    Detector,
    /// Fixed constant value.
    Constant,
    /// Value taken from an NDAttribute.
    NdAttribute,
    /// No `source` attribute given (C++ `DataSource` default `notset`). Used
    /// by the built-in `<dataset name="timestamp">` performance dataset in
    /// the C ADCore `DEFAULT_LAYOUT`, whose data the writer fills directly.
    Unset,
}

/// Scalar datatype for constant/attribute layout nodes (schema `attrSourceTypeEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutDataType {
    Int,
    Float,
    String,
}

impl LayoutDataType {
    fn parse(s: &str) -> LayoutDataType {
        match s {
            "int" => LayoutDataType::Int,
            "float" => LayoutDataType::Float,
            _ => LayoutDataType::String,
        }
    }
}

/// When a constant/attribute value is written (schema `when` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutWhen {
    OnFileOpen,
    OnFileClose,
    OnFileWrite,
    /// Default — written per frame.
    OnFrame,
}

impl LayoutWhen {
    fn parse(s: &str) -> LayoutWhen {
        match s {
            "OnFileOpen" => LayoutWhen::OnFileOpen,
            "OnFileClose" => LayoutWhen::OnFileClose,
            "OnFileWrite" => LayoutWhen::OnFileWrite,
            _ => LayoutWhen::OnFrame,
        }
    }
}

/// An `<attribute>` element: HDF5 metadata attached to a group or dataset.
#[derive(Debug, Clone)]
pub struct LayoutAttribute {
    pub name: String,
    pub source: LayoutSource,
    pub data_type: LayoutDataType,
    pub value: String,
    /// NDAttribute name when `source == NdAttribute`.
    pub ndattribute: String,
    pub when: LayoutWhen,
}

/// A `<dataset>` element.
#[derive(Debug, Clone)]
pub struct LayoutDataset {
    pub name: String,
    pub source: LayoutSource,
    pub data_type: LayoutDataType,
    pub value: String,
    /// NDAttribute name when `source == NdAttribute`.
    pub ndattribute: String,
    /// True if this is the default detector dataset (`det_default="true"`).
    pub det_default: bool,
    pub when: LayoutWhen,
    /// HDF5 attributes attached to this dataset.
    pub attributes: Vec<LayoutAttribute>,
}

/// A `<hardlink>` element.
#[derive(Debug, Clone)]
pub struct LayoutHardlink {
    pub name: String,
    pub target: String,
}

/// A `<group>` element: an HDF5 group node.
#[derive(Debug, Clone)]
pub struct LayoutGroup {
    pub name: String,
    /// `ndattr_default="true"` — NDAttribute datasets land in this group.
    pub ndattr_default: bool,
    pub attributes: Vec<LayoutAttribute>,
    pub datasets: Vec<LayoutDataset>,
    pub hardlinks: Vec<LayoutHardlink>,
    pub groups: Vec<LayoutGroup>,
}

impl LayoutGroup {
    fn new(name: String, ndattr_default: bool) -> Self {
        Self {
            name,
            ndattr_default,
            attributes: Vec::new(),
            datasets: Vec::new(),
            hardlinks: Vec::new(),
            groups: Vec::new(),
        }
    }
}

/// The parsed HDF5 layout tree (`<hdf5_layout>` root).
#[derive(Debug, Clone, Default)]
pub struct Hdf5Layout {
    /// Top-level groups.
    pub groups: Vec<LayoutGroup>,
    /// `<global name="detector_data_destination" ndattribute="..."/>`.
    pub detector_data_destination: Option<String>,
}

/// Error from layout XML parsing.
#[derive(Debug, Clone)]
pub struct LayoutError(pub String);

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LayoutError {}

impl Hdf5Layout {
    /// Parse a layout XML file from disk.
    pub fn from_file(path: &std::path::Path) -> Result<Hdf5Layout, LayoutError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LayoutError(format!("cannot read layout file: {}", e)))?;
        Self::parse(&text)
    }

    /// Parse a layout XML document from a string.
    pub fn parse(text: &str) -> Result<Hdf5Layout, LayoutError> {
        let tokens = tokenize(text)?;
        let mut parser = Parser {
            tokens,
            pos: 0,
        };
        parser.parse_document()
    }

    /// Walk every dataset in the tree, yielding `(full_group_path, dataset)`.
    pub fn for_each_dataset<F: FnMut(&str, &LayoutDataset)>(&self, mut f: F) {
        fn recurse<F: FnMut(&str, &LayoutDataset)>(g: &LayoutGroup, path: &str, f: &mut F) {
            let here = if path.is_empty() {
                format!("/{}", g.name)
            } else {
                format!("{}/{}", path, g.name)
            };
            for d in &g.datasets {
                f(&here, d);
            }
            for sub in &g.groups {
                recurse(sub, &here, f);
            }
        }
        for g in &self.groups {
            recurse(g, "", &mut f);
        }
    }

    /// Find the dataset flagged `det_default`, returning its full path.
    pub fn detector_dataset_path(&self) -> Option<String> {
        let mut found = None;
        self.for_each_dataset(|path, d| {
            if d.det_default && d.source == LayoutSource::Detector && found.is_none() {
                found = Some(format!("{}/{}", path, d.name));
            }
        });
        if found.is_none() {
            // Fall back to the first detector-source dataset.
            self.for_each_dataset(|path, d| {
                if d.source == LayoutSource::Detector && found.is_none() {
                    found = Some(format!("{}/{}", path, d.name));
                }
            });
        }
        found
    }

    /// Find the group path of the first dataset named `name`, returning the
    /// owning group's full path (the C `NDFileHDF5` performance dataset is a
    /// `<dataset name="timestamp">` in the layout tree).
    pub fn dataset_group_path(&self, name: &str) -> Option<String> {
        let mut found = None;
        self.for_each_dataset(|path, d| {
            if d.name == name && found.is_none() {
                found = Some(path.to_string());
            }
        });
        found
    }

    /// Find the group flagged `ndattr_default`, returning its full path.
    pub fn ndattr_default_group(&self) -> Option<String> {
        fn recurse(g: &LayoutGroup, path: &str) -> Option<String> {
            let here = if path.is_empty() {
                format!("/{}", g.name)
            } else {
                format!("{}/{}", path, g.name)
            };
            if g.ndattr_default {
                return Some(here.clone());
            }
            for sub in &g.groups {
                if let Some(p) = recurse(sub, &here) {
                    return Some(p);
                }
            }
            None
        }
        for g in &self.groups {
            if let Some(p) = recurse(g, "") {
                return Some(p);
            }
        }
        None
    }
}

// ===========================================================================
// Tokenizer
// ===========================================================================

#[derive(Debug, Clone)]
enum Token {
    /// `<tag attr="val" ...>` — name, attributes, self-closing flag.
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    /// `</tag>`.
    Close(String),
}

/// Tokenize an XML document into element open/close tokens. Text content,
/// comments, processing instructions and the prolog are discarded — the
/// layout schema has no text-bearing elements.
fn tokenize(text: &str) -> Result<Vec<Token>, LayoutError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Comment <!-- ... -->
        if text[i..].starts_with("<!--") {
            match text[i..].find("-->") {
                Some(end) => {
                    i += end + 3;
                    continue;
                }
                None => return Err(LayoutError("unterminated XML comment".into())),
            }
        }
        // Processing instruction / declaration <? ... ?>  or  <! ... >
        if text[i..].starts_with("<?") || text[i..].starts_with("<!") {
            match text[i..].find('>') {
                Some(end) => {
                    i += end + 1;
                    continue;
                }
                None => return Err(LayoutError("unterminated XML declaration".into())),
            }
        }
        // Find the matching '>'
        let close = text[i..]
            .find('>')
            .ok_or_else(|| LayoutError("unterminated XML tag".into()))?;
        let inner = &text[i + 1..i + close];
        i += close + 1;

        let inner_trim = inner.trim();
        if let Some(rest) = inner_trim.strip_prefix('/') {
            tokens.push(Token::Close(rest.trim().to_string()));
            continue;
        }
        let self_closing = inner_trim.ends_with('/');
        let body = if self_closing {
            inner_trim[..inner_trim.len() - 1].trim()
        } else {
            inner_trim
        };
        let (name, attrs) = parse_tag_body(body)?;
        tokens.push(Token::Open {
            name,
            attrs,
            self_closing,
        });
    }
    Ok(tokens)
}

/// Parse `name attr="val" attr2='val2'` into a name and attribute list.
fn parse_tag_body(body: &str) -> Result<(String, Vec<(String, String)>), LayoutError> {
    let chars: Vec<char> = body.chars().collect();
    let mut idx = 0;
    // Element name
    let name_start = idx;
    while idx < chars.len() && !chars[idx].is_whitespace() {
        idx += 1;
    }
    let name: String = chars[name_start..idx].iter().collect();
    if name.is_empty() {
        return Err(LayoutError("empty XML tag name".into()));
    }
    let mut attrs = Vec::new();
    loop {
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() {
            break;
        }
        // attribute name up to '='
        let attr_start = idx;
        while idx < chars.len() && chars[idx] != '=' && !chars[idx].is_whitespace() {
            idx += 1;
        }
        let attr_name: String = chars[attr_start..idx].iter().collect();
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() || chars[idx] != '=' {
            return Err(LayoutError(format!(
                "malformed attribute '{}' in tag '{}'",
                attr_name, name
            )));
        }
        idx += 1; // skip '='
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() || (chars[idx] != '"' && chars[idx] != '\'') {
            return Err(LayoutError(format!(
                "unquoted attribute value for '{}' in tag '{}'",
                attr_name, name
            )));
        }
        let quote = chars[idx];
        idx += 1;
        let val_start = idx;
        while idx < chars.len() && chars[idx] != quote {
            idx += 1;
        }
        if idx >= chars.len() {
            return Err(LayoutError(format!(
                "unterminated attribute value for '{}'",
                attr_name
            )));
        }
        let raw: String = chars[val_start..idx].iter().collect();
        idx += 1; // skip closing quote
        attrs.push((attr_name, unescape(&raw)));
    }
    Ok((name, attrs))
}

/// Decode the five standard XML entities.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ===========================================================================
// Parser
// ===========================================================================

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn parse_document(&mut self) -> Result<Hdf5Layout, LayoutError> {
        // Find the <hdf5_layout> root.
        let mut layout = Hdf5Layout::default();
        match self.tokens.get(self.pos).cloned() {
            Some(Token::Open {
                name,
                self_closing,
                ..
            }) if name == "hdf5_layout" => {
                self.pos += 1;
                if self_closing {
                    return Ok(layout);
                }
            }
            _ => {
                return Err(LayoutError(
                    "root element <hdf5_layout> not found".into(),
                ))
            }
        }

        loop {
            match self.tokens.get(self.pos).cloned() {
                Some(Token::Open {
                    name,
                    attrs,
                    self_closing,
                }) => {
                    self.pos += 1;
                    match name.as_str() {
                        "group" => {
                            let g = self.parse_group(attrs, self_closing)?;
                            layout.groups.push(g);
                        }
                        "global" => {
                            let n = attr_get(&attrs, "name").unwrap_or_default();
                            if n == "detector_data_destination" {
                                layout.detector_data_destination =
                                    attr_get(&attrs, "ndattribute");
                            }
                            if !self_closing {
                                self.skip_to_close("global")?;
                            }
                        }
                        other => {
                            return Err(LayoutError(format!(
                                "unexpected element <{}> in <hdf5_layout>",
                                other
                            )))
                        }
                    }
                }
                Some(Token::Close(name)) if name == "hdf5_layout" => {
                    self.pos += 1;
                    break;
                }
                Some(Token::Close(other)) => {
                    return Err(LayoutError(format!(
                        "unexpected </{}> at document level",
                        other
                    )))
                }
                None => {
                    return Err(LayoutError(
                        "unterminated <hdf5_layout> element".into(),
                    ))
                }
            }
        }
        Ok(layout)
    }

    fn parse_group(
        &mut self,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    ) -> Result<LayoutGroup, LayoutError> {
        let name = attr_get(&attrs, "name")
            .ok_or_else(|| LayoutError("<group> missing required 'name'".into()))?;
        let ndattr_default = attr_bool(&attrs, "ndattr_default");
        let mut group = LayoutGroup::new(name.clone(), ndattr_default);
        if self_closing {
            return Ok(group);
        }
        loop {
            match self.tokens.get(self.pos).cloned() {
                Some(Token::Open {
                    name: child,
                    attrs: cattrs,
                    self_closing: sc,
                }) => {
                    self.pos += 1;
                    match child.as_str() {
                        "group" => group.groups.push(self.parse_group(cattrs, sc)?),
                        "dataset" => group.datasets.push(self.parse_dataset(cattrs, sc)?),
                        "attribute" => {
                            group.attributes.push(parse_attribute(&cattrs)?);
                            if !sc {
                                self.skip_to_close("attribute")?;
                            }
                        }
                        "hardlink" => {
                            group.hardlinks.push(LayoutHardlink {
                                name: attr_get(&cattrs, "name").ok_or_else(|| {
                                    LayoutError("<hardlink> missing 'name'".into())
                                })?,
                                target: attr_get(&cattrs, "target").ok_or_else(|| {
                                    LayoutError("<hardlink> missing 'target'".into())
                                })?,
                            });
                            if !sc {
                                self.skip_to_close("hardlink")?;
                            }
                        }
                        other => {
                            return Err(LayoutError(format!(
                                "unexpected <{}> inside <group name=\"{}\">",
                                other, name
                            )))
                        }
                    }
                }
                Some(Token::Close(close_name)) if close_name == "group" => {
                    self.pos += 1;
                    break;
                }
                Some(Token::Close(other)) => {
                    return Err(LayoutError(format!(
                        "mismatched </{}>, expected </group>",
                        other
                    )))
                }
                None => {
                    return Err(LayoutError(format!(
                        "unterminated <group name=\"{}\">",
                        name
                    )))
                }
            }
        }
        Ok(group)
    }

    fn parse_dataset(
        &mut self,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    ) -> Result<LayoutDataset, LayoutError> {
        let name = attr_get(&attrs, "name")
            .ok_or_else(|| LayoutError("<dataset> missing required 'name'".into()))?;
        let source = parse_source(&attrs, &name)?;
        let mut ds = LayoutDataset {
            name: name.clone(),
            source,
            data_type: LayoutDataType::parse(
                &attr_get(&attrs, "type").unwrap_or_else(|| "string".into()),
            ),
            value: attr_get(&attrs, "value").unwrap_or_default(),
            ndattribute: attr_get(&attrs, "ndattribute").unwrap_or_default(),
            det_default: attr_bool(&attrs, "det_default"),
            when: LayoutWhen::parse(&attr_get(&attrs, "when").unwrap_or_default()),
            attributes: Vec::new(),
        };
        if self_closing {
            return Ok(ds);
        }
        loop {
            match self.tokens.get(self.pos).cloned() {
                Some(Token::Open {
                    name: child,
                    attrs: cattrs,
                    self_closing: sc,
                }) => {
                    self.pos += 1;
                    if child == "attribute" {
                        ds.attributes.push(parse_attribute(&cattrs)?);
                        if !sc {
                            self.skip_to_close("attribute")?;
                        }
                    } else {
                        return Err(LayoutError(format!(
                            "unexpected <{}> inside <dataset name=\"{}\">",
                            child, name
                        )));
                    }
                }
                Some(Token::Close(close_name)) if close_name == "dataset" => {
                    self.pos += 1;
                    break;
                }
                Some(Token::Close(other)) => {
                    return Err(LayoutError(format!(
                        "mismatched </{}>, expected </dataset>",
                        other
                    )))
                }
                None => {
                    return Err(LayoutError(format!(
                        "unterminated <dataset name=\"{}\">",
                        name
                    )))
                }
            }
        }
        Ok(ds)
    }

    /// Skip tokens until the matching close tag (for childless elements that
    /// were not written self-closing).
    fn skip_to_close(&mut self, tag: &str) -> Result<(), LayoutError> {
        let mut depth = 1;
        while let Some(tok) = self.tokens.get(self.pos).cloned() {
            self.pos += 1;
            match tok {
                Token::Open {
                    name, self_closing, ..
                } if name == tag && !self_closing => depth += 1,
                Token::Close(name) if name == tag => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(LayoutError(format!("unterminated <{}> element", tag)))
    }
}

fn parse_attribute(attrs: &[(String, String)]) -> Result<LayoutAttribute, LayoutError> {
    let name = attr_get(attrs, "name")
        .ok_or_else(|| LayoutError("<attribute> missing required 'name'".into()))?;
    let source_str = attr_get(attrs, "source")
        .ok_or_else(|| LayoutError(format!("<attribute name=\"{}\"> missing 'source'", name)))?;
    let source = match source_str.as_str() {
        "constant" => LayoutSource::Constant,
        "ndattribute" => LayoutSource::NdAttribute,
        other => {
            return Err(LayoutError(format!(
                "<attribute name=\"{}\"> invalid source '{}'",
                name, other
            )))
        }
    };
    Ok(LayoutAttribute {
        name,
        source,
        data_type: LayoutDataType::parse(
            &attr_get(attrs, "type").unwrap_or_else(|| "string".into()),
        ),
        value: attr_get(attrs, "value").unwrap_or_default(),
        ndattribute: attr_get(attrs, "ndattribute").unwrap_or_default(),
        when: LayoutWhen::parse(&attr_get(attrs, "when").unwrap_or_default()),
    })
}

fn parse_source(attrs: &[(String, String)], name: &str) -> Result<LayoutSource, LayoutError> {
    // C++ `process_dset_xml_attribute` leaves the DataSource at its default
    // (`notset`) when no `source` attribute is present, so a sourceless
    // `<dataset>` (the built-in performance `timestamp` dataset) is valid.
    let s = match attr_get(attrs, "source") {
        Some(s) => s,
        None => return Ok(LayoutSource::Unset),
    };
    match s.as_str() {
        "detector" => Ok(LayoutSource::Detector),
        "constant" => Ok(LayoutSource::Constant),
        "ndattribute" => Ok(LayoutSource::NdAttribute),
        other => Err(LayoutError(format!(
            "<dataset name=\"{}\"> invalid source '{}'",
            name, other
        ))),
    }
}

fn attr_get(attrs: &[(String, String)], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn attr_bool(attrs: &[(String, String)], key: &str) -> bool {
    matches!(attr_get(attrs, key).as_deref(), Some("true") | Some("1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<hdf5_layout>
  <global name="detector_data_destination" ndattribute="detdest" />
  <group name="entry">
    <attribute name="NX_class" source="constant" value="NXentry" type="string" />
    <group name="data" ndattr_default="true">
      <dataset name="data" source="detector" det_default="true">
        <attribute name="signal" source="constant" value="1" type="int" />
      </dataset>
      <dataset name="exposure" source="ndattribute" ndattribute="AcquireTime" type="float" />
    </group>
    <group name="instrument">
      <dataset name="name" source="constant" value="MyBeamline" type="string" />
      <hardlink name="link_to_data" target="/entry/data/data" />
    </group>
  </group>
</hdf5_layout>"#;

    #[test]
    fn parses_full_tree() {
        let layout = Hdf5Layout::parse(SAMPLE).unwrap();
        assert_eq!(layout.groups.len(), 1);
        assert_eq!(
            layout.detector_data_destination.as_deref(),
            Some("detdest")
        );
        let entry = &layout.groups[0];
        assert_eq!(entry.name, "entry");
        assert_eq!(entry.attributes.len(), 1);
        assert_eq!(entry.attributes[0].name, "NX_class");
        assert_eq!(entry.attributes[0].value, "NXentry");
        assert_eq!(entry.groups.len(), 2);

        let data_group = &entry.groups[0];
        assert!(data_group.ndattr_default);
        assert_eq!(data_group.datasets.len(), 2);
        assert!(data_group.datasets[0].det_default);
        assert_eq!(data_group.datasets[0].source, LayoutSource::Detector);
        assert_eq!(data_group.datasets[0].attributes.len(), 1);
        assert_eq!(
            data_group.datasets[0].attributes[0].data_type,
            LayoutDataType::Int
        );
        assert_eq!(data_group.datasets[1].source, LayoutSource::NdAttribute);
        assert_eq!(data_group.datasets[1].ndattribute, "AcquireTime");
        assert_eq!(data_group.datasets[1].data_type, LayoutDataType::Float);

        let instr = &entry.groups[1];
        assert_eq!(instr.hardlinks.len(), 1);
        assert_eq!(instr.hardlinks[0].target, "/entry/data/data");
    }

    #[test]
    fn detector_path_resolves() {
        let layout = Hdf5Layout::parse(SAMPLE).unwrap();
        assert_eq!(
            layout.detector_dataset_path().as_deref(),
            Some("/entry/data/data")
        );
        assert_eq!(
            layout.ndattr_default_group().as_deref(),
            Some("/entry/data")
        );
    }

    #[test]
    fn rejects_missing_root() {
        let err = Hdf5Layout::parse("<foo/>").unwrap_err();
        assert!(err.0.contains("hdf5_layout"));
    }

    #[test]
    fn rejects_missing_dataset_name() {
        let xml = r#"<hdf5_layout><group name="g"><dataset source="detector"/></group></hdf5_layout>"#;
        let err = Hdf5Layout::parse(xml).unwrap_err();
        assert!(err.0.contains("name"));
    }

    #[test]
    fn rejects_bad_source() {
        let xml = r#"<hdf5_layout><group name="g"><dataset name="d" source="bogus"/></group></hdf5_layout>"#;
        let err = Hdf5Layout::parse(xml).unwrap_err();
        assert!(err.0.contains("bogus"));
    }

    #[test]
    fn handles_comments_and_entities() {
        let xml = r#"<hdf5_layout>
          <!-- a comment -->
          <group name="g">
            <dataset name="d" source="constant" value="a &amp; b" type="string"/>
          </group>
        </hdf5_layout>"#;
        let layout = Hdf5Layout::parse(xml).unwrap();
        assert_eq!(layout.groups[0].datasets[0].value, "a & b");
    }
}
