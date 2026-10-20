//! HDF5 custom-layout XML engine.
//!
//! Ports the C++ `NDFileHDF5LayoutXML.cpp` + `NDFileHDF5Layout.cpp` parser.
//! Builds the group/dataset tree from a user XML file (`HDF5_layoutFilename`),
//! mirroring `XML_schema/hdf5_xml_layout_schema.xsd`.
//!
//! `xml` reads the document as C's libxml2 does; no external XML crate
//! dependency.

use std::borrow::Cow;
use std::fmt;

mod xml;

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

/// When an NDAttribute value is written: C `hdf5::When_t`
/// (NDFileHDF5Layout.h:21-25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutWhen {
    OnFileOpen,
    OnFileClose,
    /// Default — written per frame.
    OnFrame,
}

impl LayoutWhen {
    /// C knows only these two names; any other `when` is `OnFrame`
    /// (NDFileHDF5LayoutXML.cpp:401-405, :568-572).
    fn parse(s: &str) -> LayoutWhen {
        match s {
            "OnFileOpen" => LayoutWhen::OnFileOpen,
            "OnFileClose" => LayoutWhen::OnFileClose,
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

/// A `<attribute source="ndattribute">` declared on a group or dataset,
/// resolved to the owning element's full path. Used by the writer to attach
/// an HDF5 attribute carrying the live NDAttribute value (C
/// `storeOnOpenCloseAttribute`).
#[derive(Debug, Clone)]
pub struct NdAttrElementAttr {
    /// Full path (leading slash) of the owning group or dataset.
    pub element_path: String,
    /// True if the owning element is a dataset, false if a group.
    pub is_dataset: bool,
    /// HDF5 attribute name to create on the element.
    pub attr_name: String,
    /// NDAttribute name supplying the value.
    pub ndattribute: String,
    /// `OnFileOpen`/`OnFrame` take the first frame's value, `OnFileClose` the
    /// last (C `is_onFileOpen`/`is_onFileClose`, NDFileHDF5Layout.cpp:54-64).
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

impl LayoutDataset {
    /// The dataset's `<attribute source="constant">` nodes.
    pub fn constant_attributes(&self) -> impl Iterator<Item = &LayoutAttribute> {
        self.attributes
            .iter()
            .filter(|a| a.source == LayoutSource::Constant)
    }
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
    /// C keeps a group's children in `std::map`s keyed by name
    /// (NDFileHDF5Layout.h:130, :251-253), so every walk and first-match
    /// lookup over them runs in name order.
    fn sort_like_c(&mut self) {
        self.attributes.sort_by(|a, b| a.name.cmp(&b.name));
        self.datasets.sort_by(|a, b| a.name.cmp(&b.name));
        for d in &mut self.datasets {
            d.attributes.sort_by(|a, b| a.name.cmp(&b.name));
        }
        self.hardlinks.sort_by(|a, b| a.name.cmp(&b.name));
        self.groups.sort_by(|a, b| a.name.cmp(&b.name));
        for g in &mut self.groups {
            g.sort_like_c();
        }
    }

    /// C `Group::name_exist` (NDFileHDF5Layout.cpp:615-625).
    fn name_exist(&self, name: &str) -> bool {
        self.datasets.iter().any(|d| d.name == name) || self.groups.iter().any(|g| g.name == name)
    }

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
#[derive(Debug, Clone)]
pub struct Hdf5Layout {
    /// Top-level groups.
    pub groups: Vec<LayoutGroup>,
    /// `<global name="detector_data_destination" ndattribute="..."/>`.
    pub detector_data_destination: Option<String>,
    /// The root's `auto_ndattr_default`: only `"false"` clears it
    /// (NDFileHDF5LayoutXML.cpp:440-460).
    pub auto_ndattr_default: bool,
}

impl Default for Hdf5Layout {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            detector_data_destination: None,
            auto_ndattr_default: true,
        }
    }
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
    /// Whether an XMLFileName is the layout itself rather than a path: C
    /// loads one containing `<?xml` from memory (NDFileHDF5LayoutXML.cpp:
    /// 129-130, :167-168).
    pub fn is_inline(spec: &str) -> bool {
        spec.contains("<?xml")
    }

    /// Load the layout an XMLFileName names, inline or from disk.
    pub fn load(spec: &str) -> Result<Hdf5Layout, LayoutError> {
        Self::from_bytes(&Self::read_spec(spec)?)
    }

    /// C `verifyLayoutXMLFile`'s check of an XMLFileName: libxml
    /// well-formedness only, so a document that is no layout passes.
    pub fn verify(spec: &str) -> Result<(), LayoutError> {
        xml::check(&Self::read_spec(spec)?)
    }

    fn read_spec(spec: &str) -> Result<Cow<'_, [u8]>, LayoutError> {
        if Self::is_inline(spec) {
            return Ok(Cow::Borrowed(spec.as_bytes()));
        }
        std::fs::read(spec)
            .map(Cow::Owned)
            .map_err(|e| LayoutError(format!("cannot read layout file: {}", e)))
    }

    /// Parse a layout XML file from disk.
    pub fn from_file(path: &std::path::Path) -> Result<Hdf5Layout, LayoutError> {
        let bytes = std::fs::read(path)
            .map_err(|e| LayoutError(format!("cannot read layout file: {}", e)))?;
        Self::from_bytes(&bytes)
    }

    /// Parse a layout XML document from a string.
    pub fn parse(text: &str) -> Result<Hdf5Layout, LayoutError> {
        Self::from_bytes(text.as_bytes())
    }

    fn from_bytes(bytes: &[u8]) -> Result<Hdf5Layout, LayoutError> {
        let tokens = xml::parse(bytes)?;
        Parser { tokens, pos: 0 }.parse_document()
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

    /// Enumerate every detector-source dataset's full path (leading slash), in
    /// tree order. C `NDFileHDF5` holds one `NDFileHDF5Dataset` per
    /// `<dataset source="detector">` node in `detDataMap`, each created by
    /// `createDatasetDetector` (NDFileHDF5.cpp:1324-1357) and keyed by
    /// `get_full_name()` — the same leading-slash full path produced here. The
    /// writer creates *all* of them up front and routes each frame to one of
    /// them by the `detector_data_destination` NDAttribute.
    pub fn detector_dataset_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.for_each_dataset(|path, d| {
            if d.source == LayoutSource::Detector {
                out.push(format!("{}/{}", path, d.name));
            }
        });
        out
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

    /// Find the layout `<dataset source="ndattribute" ndattribute="name">`
    /// declared for the NDAttribute `name`, returning `(parent group full path,
    /// dataset)`, first in [`Self::for_each_dataset`] order. C
    /// `find_dset_ndattr` (NDFileHDF5.cpp:2792) routes a matching NDAttribute
    /// into its XML-declared dataset/group instead of the default attribute
    /// group.
    pub fn ndattribute_dataset(&self, name: &str) -> Option<(String, &LayoutDataset)> {
        fn find<'a>(
            g: &'a LayoutGroup,
            path: &str,
            name: &str,
        ) -> Option<(String, &'a LayoutDataset)> {
            let here = format!("{}/{}", path, g.name);
            g.datasets
                .iter()
                .find(|d| d.source == LayoutSource::NdAttribute && d.ndattribute == name)
                .map(|d| (here.clone(), d))
                .or_else(|| g.groups.iter().find_map(|sub| find(sub, &here, name)))
        }
        self.groups.iter().find_map(|g| find(g, "", name))
    }

    /// Enumerate every `<attribute source="ndattribute">` declared on a group
    /// or dataset in the tree, in tree order, with the owning element's
    /// full path. C `storeOnOpenCloseAttribute` (NDFileHDF5.cpp:553-632) writes
    /// each as an HDF5 attribute on its element, sourcing the value from the
    /// live NDAttribute (`get_attributes()` over groups *and* datasets).
    pub fn ndattribute_element_attrs(&self) -> Vec<NdAttrElementAttr> {
        fn push_attrs(
            out: &mut Vec<NdAttrElementAttr>,
            element_path: &str,
            is_dataset: bool,
            attrs: &[LayoutAttribute],
        ) {
            for a in attrs {
                if a.source == LayoutSource::NdAttribute {
                    out.push(NdAttrElementAttr {
                        element_path: element_path.to_string(),
                        is_dataset,
                        attr_name: a.name.clone(),
                        ndattribute: a.ndattribute.clone(),
                        when: a.when,
                    });
                }
            }
        }
        fn recurse(out: &mut Vec<NdAttrElementAttr>, g: &LayoutGroup, path: &str) {
            let here = if path.is_empty() {
                format!("/{}", g.name)
            } else {
                format!("{}/{}", path, g.name)
            };
            push_attrs(out, &here, false, &g.attributes);
            for d in &g.datasets {
                let dpath = format!("{}/{}", here, d.name);
                push_attrs(out, &dpath, true, &d.attributes);
            }
            for sub in &g.groups {
                recurse(out, sub, &here);
            }
        }
        let mut out = Vec::new();
        for g in &self.groups {
            recurse(&mut out, g, "");
        }
        out
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

    /// Where C puts an NDAttribute dataset the layout does not declare: the
    /// `ndattr_default` group, else the file root (`""`) unless
    /// `auto_ndattr_default` is false (NDFileHDF5.cpp:2766-2782).
    pub fn ndattr_home(&self) -> Option<String> {
        self.ndattr_default_group()
            .or_else(|| self.auto_ndattr_default.then(String::new))
    }

    /// C `createPerformanceDataset`'s group (NDFileHDF5.cpp:2603-2632): the
    /// parent of the layout's `timestamp` dataset when that group is named
    /// `performance` and the dataset is not ndattribute-sourced, else
    /// [`Self::ndattr_home`].
    pub fn performance_group(&self) -> Option<String> {
        let mut found = None;
        self.for_each_dataset(|path, d| {
            if d.name == "timestamp" && found.is_none() {
                found = Some((path.to_string(), d.source));
            }
        });
        match found {
            Some((path, source))
                if path.rsplit('/').next() == Some("performance")
                    && source != LayoutSource::NdAttribute =>
            {
                Some(path)
            }
            _ => self.ndattr_home(),
        }
    }
}

// ===========================================================================
// Tokens
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
                attrs,
                self_closing,
            }) if name == "hdf5_layout" => {
                self.pos += 1;
                layout.auto_ndattr_default =
                    attr_get(&attrs, "auto_ndattr_default").as_deref() != Some("false");
                if self_closing {
                    return Ok(layout);
                }
            }
            _ => return Err(LayoutError("root element <hdf5_layout> not found".into())),
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
                            if !layout.groups.iter().any(|h| h.name == g.name) {
                                layout.groups.push(g);
                            }
                        }
                        "global" => {
                            let n = attr_get(&attrs, "name").unwrap_or_default();
                            if n == "detector_data_destination" {
                                layout.detector_data_destination = attr_get(&attrs, "ndattribute");
                            }
                            if !self_closing {
                                self.skip_to_close("global")?;
                            }
                        }
                        other => {
                            return Err(LayoutError(format!(
                                "unexpected element <{}> in <hdf5_layout>",
                                other
                            )));
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
                    )));
                }
                None => return Err(LayoutError("unterminated <hdf5_layout> element".into())),
            }
        }
        layout.groups.sort_by(|a, b| a.name.cmp(&b.name));
        for g in &mut layout.groups {
            g.sort_like_c();
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
                    // C refuses a name the group already holds (new_group,
                    // new_dset, new_hardlink; NDFileHDF5Layout.cpp:329-416) and
                    // parses on, but its cursor then pops at the refused
                    // element's end tag and re-parents the later siblings. That
                    // is a C defect, not copied: the refused subtree is dropped.
                    match child.as_str() {
                        "group" => {
                            let g = self.parse_group(cattrs, sc)?;
                            if !group.name_exist(&g.name) {
                                group.groups.push(g);
                            }
                        }
                        "dataset" => {
                            let d = self.parse_dataset(cattrs, sc)?;
                            if !group.name_exist(&d.name) {
                                group.datasets.push(d);
                            }
                        }
                        "attribute" => {
                            add_attribute(&mut group.attributes, parse_attribute(&cattrs)?);
                            if !sc {
                                self.skip_to_close("attribute")?;
                            }
                        }
                        "hardlink" => {
                            let h = LayoutHardlink {
                                name: attr_get(&cattrs, "name").ok_or_else(|| {
                                    LayoutError("<hardlink> missing 'name'".into())
                                })?,
                                target: attr_get(&cattrs, "target").ok_or_else(|| {
                                    LayoutError("<hardlink> missing 'target'".into())
                                })?,
                            };
                            if !group.name_exist(&h.name)
                                && !group.hardlinks.iter().any(|k| k.name == h.name)
                            {
                                group.hardlinks.push(h);
                            }
                            if !sc {
                                self.skip_to_close("hardlink")?;
                            }
                        }
                        other => {
                            return Err(LayoutError(format!(
                                "unexpected <{}> inside <group name=\"{}\">",
                                other, name
                            )));
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
                    )));
                }
                None => {
                    return Err(LayoutError(format!(
                        "unterminated <group name=\"{}\">",
                        name
                    )));
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
        let data_type = attr_get(&attrs, "type");
        let mut ds = LayoutDataset {
            name: name.clone(),
            source,
            // C keeps a dataset constant's value only alongside a `type`
            // (NDFileHDF5LayoutXML.cpp:583-592).
            value: match data_type {
                Some(_) => attr_get(&attrs, "value").unwrap_or_default(),
                None => String::new(),
            },
            data_type: LayoutDataType::parse(&data_type.unwrap_or_else(|| "string".into())),
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
                        add_attribute(&mut ds.attributes, parse_attribute(&cattrs)?);
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
                    )));
                }
                None => {
                    return Err(LayoutError(format!(
                        "unterminated <dataset name=\"{}\">",
                        name
                    )));
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

/// C `Element::add_attribute` (NDFileHDF5Layout.cpp:251-259): an element
/// keeps its first `<attribute>` of a name and drops the rest.
fn add_attribute(attrs: &mut Vec<LayoutAttribute>, a: LayoutAttribute) {
    if !attrs.iter().any(|b| b.name == a.name) {
        attrs.push(a);
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
            )));
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
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
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
        assert_eq!(layout.detector_data_destination.as_deref(), Some("detdest"));
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
        let xml =
            r#"<hdf5_layout><group name="g"><dataset source="detector"/></group></hdf5_layout>"#;
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
