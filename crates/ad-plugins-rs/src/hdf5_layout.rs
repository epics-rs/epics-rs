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
    /// No `source` attribute, or one C does not know (C++ `DataSource`
    /// default `notset`). Used by the built-in `<dataset name="timestamp">`
    /// performance dataset in the C ADCore `DEFAULT_LAYOUT`, whose data the
    /// writer fills directly.
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

/// The parsed HDF5 layout tree: C's `Root`.
#[derive(Debug, Clone)]
pub struct Hdf5Layout {
    /// Top-level groups.
    pub groups: Vec<LayoutGroup>,
    /// Datasets declared outside every group after the first one. C's
    /// `createTree` makes none of them, but its `find_*` walks see them
    /// first, so an ndattribute one takes its NDAttribute at the file root.
    pub datasets: Vec<LayoutDataset>,
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
            datasets: Vec::new(),
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
        let mut b = Builder::default();
        for token in xml::parse(bytes)? {
            b.token(token);
        }
        b.finish()
    }

    /// Walk every dataset in the tree as C's `find_*` walks do, yielding
    /// `(full_group_path, dataset)`: the root's own datasets first, under
    /// `""`.
    pub fn for_each_dataset<'a, F: FnMut(&str, &'a LayoutDataset)>(&'a self, mut f: F) {
        fn recurse<'a, F: FnMut(&str, &'a LayoutDataset)>(
            g: &'a LayoutGroup,
            path: &str,
            f: &mut F,
        ) {
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
        for d in &self.datasets {
            f("", d);
        }
        for g in &self.groups {
            recurse(g, "", &mut f);
        }
    }

    /// Find the dataset flagged `det_default`, returning its full path.
    /// A root dataset is passed over: C's `find_detector_default_dset` takes
    /// it although `createTree` never makes it, a C defect.
    pub fn detector_dataset_path(&self) -> Option<String> {
        let mut found = None;
        self.for_each_dataset(|path, d| {
            if d.det_default
                && d.source == LayoutSource::Detector
                && found.is_none()
                && !path.is_empty()
            {
                found = Some(format!("{}/{}", path, d.name));
            }
        });
        if found.is_none() {
            // Fall back to the first detector-source dataset.
            self.for_each_dataset(|path, d| {
                if d.source == LayoutSource::Detector && found.is_none() && !path.is_empty() {
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
    /// them by the `detector_data_destination` NDAttribute. `createTree`
    /// makes none at the root.
    pub fn detector_dataset_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.for_each_dataset(|path, d| {
            if d.source == LayoutSource::Detector && !path.is_empty() {
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
        let mut found = None;
        self.for_each_dataset(|path, d| {
            if found.is_none() && d.source == LayoutSource::NdAttribute && d.ndattribute == name {
                found = Some((path.to_string(), d));
            }
        });
        found
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
        for d in &self.datasets {
            push_attrs(&mut out, &format!("/{}", d.name), true, &d.attributes);
        }
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

    /// The `<attribute>` nodes C `createTree` hands `writeHdfAttributes`
    /// (NDFileHDF5.cpp:638-708), in its order: depth first, per group those
    /// of each dataset it creates there (detector, constant, but not
    /// `performance/timestamp`), then under StoreAttr the group's own.
    pub fn tree_attributes(&self, store_attributes: bool) -> Vec<&LayoutAttribute> {
        fn walk<'a>(g: &'a LayoutGroup, store: bool, out: &mut Vec<&'a LayoutAttribute>) {
            for d in &g.datasets {
                if matches!(d.source, LayoutSource::Detector | LayoutSource::Constant)
                    && !(g.name == "performance" && d.name == "timestamp")
                {
                    out.extend(&d.attributes);
                }
            }
            if store {
                out.extend(&g.attributes);
            }
            for sub in &g.groups {
                walk(sub, store, out);
            }
        }
        let mut out = Vec::new();
        for g in &self.groups {
            walk(g, store_attributes, &mut out);
        }
        out
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
// Builder
// ===========================================================================

/// An open `<group>` or `<dataset>`.
enum Frame {
    Group(LayoutGroup),
    Dataset(LayoutDataset),
    /// One C refuses (no name, a name its parent holds) or cannot hold (under
    /// a dataset, before the first group). Its children act on its parent, as
    /// C leaves its cursor there. C then pops that parent at the end tag,
    /// re-parenting later siblings, casts a dataset to a group, and before the
    /// first group reads the cursor `unload_xml` left dangling: C defects, not
    /// copied.
    Dropped,
}

/// The element C's `ptr_curr_element` stands on.
enum Cursor<'a> {
    /// C's `Root`, whose attributes and hardlinks C never writes.
    Root(&'a mut LayoutGroup),
    Group(&'a mut LayoutGroup),
    Dataset(&'a mut LayoutDataset),
    None,
}

/// C `LayoutXML::load_xml` / `process_node` (NDFileHDF5LayoutXML.cpp:
/// 124-157, :244-310): a `<group>` or `<dataset>` holds the cursor until its
/// end tag; any other element acts on the cursor, wherever it appears, and
/// one C does not know passes its children through.
#[derive(Default)]
struct Builder {
    layout: Hdf5Layout,
    /// Made by the first `<group>` (C `new_group`).
    root: Option<LayoutGroup>,
    open: Vec<Frame>,
}

impl Builder {
    fn cursor(&mut self) -> Cursor<'_> {
        let held = self.open.iter_mut().rev().find_map(|f| match f {
            Frame::Group(g) => Some(Cursor::Group(g)),
            Frame::Dataset(d) => Some(Cursor::Dataset(d)),
            Frame::Dropped => None,
        });
        held.unwrap_or_else(|| self.root.as_mut().map_or(Cursor::None, Cursor::Root))
    }

    fn token(&mut self, token: Token) {
        let (name, attrs, empty) = match token {
            Token::Open {
                name,
                attrs,
                self_closing,
            } => (name, attrs, self_closing),
            Token::Close(name) => {
                if matches!(name.as_str(), "group" | "dataset") {
                    self.close();
                }
                return;
            }
        };
        match name.as_str() {
            // parse_root: only "false" changes the default.
            "hdf5_layout" => {
                if attr_get(&attrs, "auto_ndattr_default").as_deref() == Some("false") {
                    self.layout.auto_ndattr_default = false;
                }
            }
            "group" | "dataset" => {
                if name == "group" {
                    self.open_group(&attrs);
                } else {
                    self.open_dataset(&attrs);
                }
                if empty {
                    self.close();
                }
            }
            "attribute" => {
                let Some(a) = parse_attribute(&attrs) else {
                    return;
                };
                match self.cursor() {
                    Cursor::Group(g) => add_attribute(&mut g.attributes, a),
                    Cursor::Dataset(d) => add_attribute(&mut d.attributes, a),
                    Cursor::Root(_) | Cursor::None => {}
                }
            }
            "hardlink" => {
                let (Some(name), Some(target)) =
                    (attr_get(&attrs, "name"), attr_get(&attrs, "target"))
                else {
                    return;
                };
                if let Cursor::Group(g) = self.cursor()
                    && !g.name_exist(&name)
                    && !g.hardlinks.iter().any(|h| h.name == name)
                {
                    g.hardlinks.push(LayoutHardlink { name, target });
                }
            }
            "global" => {
                if attr_get(&attrs, "name").as_deref() == Some("detector_data_destination")
                    && let Some(v) = attr_get(&attrs, "ndattribute")
                {
                    self.layout.detector_data_destination = Some(v);
                }
            }
            _ => {}
        }
    }

    fn open_group(&mut self, attrs: &[(String, String)]) {
        let frame = match attr_get(attrs, "name") {
            Some(name) => {
                if self.open.iter().all(|f| matches!(f, Frame::Dropped)) {
                    self.root
                        .get_or_insert_with(|| LayoutGroup::new(String::new(), false));
                }
                match self.cursor() {
                    Cursor::Root(p) | Cursor::Group(p) if !p.name_exist(&name) => {
                        Frame::Group(LayoutGroup::new(name, attr_true(attrs, "ndattr_default")))
                    }
                    _ => Frame::Dropped,
                }
            }
            None => Frame::Dropped,
        };
        self.open.push(frame);
    }

    fn open_dataset(&mut self, attrs: &[(String, String)]) {
        let frame = match (attr_get(attrs, "name"), self.cursor()) {
            (Some(name), Cursor::Root(p) | Cursor::Group(p)) if !p.name_exist(&name) => {
                Frame::Dataset(parse_dataset(name, attrs))
            }
            _ => Frame::Dropped,
        };
        self.open.push(frame);
    }

    /// The end of a `<group>` or `<dataset>`: it joins the parent it was
    /// checked against when it opened.
    fn close(&mut self) {
        let frame = self.open.pop();
        if let Cursor::Root(p) | Cursor::Group(p) = self.cursor() {
            match frame {
                Some(Frame::Group(g)) => p.groups.push(g),
                Some(Frame::Dataset(d)) => p.datasets.push(d),
                Some(Frame::Dropped) | None => {}
            }
        }
    }

    fn finish(mut self) -> Result<Hdf5Layout, LayoutError> {
        let mut root = self
            .root
            .ok_or_else(|| LayoutError("Unable to create the Root tree!".into()))?;
        root.sort_like_c();
        self.layout.groups = root.groups;
        self.layout.datasets = root.datasets;
        Ok(self.layout)
    }
}

/// C `Element::add_attribute` (NDFileHDF5Layout.cpp:251-259): an element
/// keeps its first `<attribute>` of a name and drops the rest.
fn add_attribute(attrs: &mut Vec<LayoutAttribute>, a: LayoutAttribute) {
    if !attrs.iter().any(|b| b.name == a.name) {
        attrs.push(a);
    }
}

/// C `new_attribute`: no name, no attribute; a missing or unknown source
/// still adds it, `notset` (process_attribute_xml_attribute).
fn parse_attribute(attrs: &[(String, String)]) -> Option<LayoutAttribute> {
    Some(LayoutAttribute {
        name: attr_get(attrs, "name")?,
        source: match attr_get(attrs, "source").as_deref() {
            Some("constant") => LayoutSource::Constant,
            Some("ndattribute") => LayoutSource::NdAttribute,
            _ => LayoutSource::Unset,
        },
        data_type: LayoutDataType::parse(
            &attr_get(attrs, "type").unwrap_or_else(|| "string".into()),
        ),
        value: attr_get(attrs, "value").unwrap_or_default(),
        ndattribute: attr_get(attrs, "ndattribute").unwrap_or_default(),
        when: LayoutWhen::parse(&attr_get(attrs, "when").unwrap_or_default()),
    })
}

/// C `new_dataset`: a missing or unknown source is `notset`
/// (process_dset_xml_attribute).
fn parse_dataset(name: String, attrs: &[(String, String)]) -> LayoutDataset {
    let data_type = attr_get(attrs, "type");
    LayoutDataset {
        name,
        source: match attr_get(attrs, "source").as_deref() {
            Some("detector") => LayoutSource::Detector,
            Some("constant") => LayoutSource::Constant,
            Some("ndattribute") => LayoutSource::NdAttribute,
            _ => LayoutSource::Unset,
        },
        // C keeps a dataset constant's value only alongside a `type`
        // (NDFileHDF5LayoutXML.cpp:583-592).
        value: match data_type {
            Some(_) => attr_get(attrs, "value").unwrap_or_default(),
            None => String::new(),
        },
        data_type: LayoutDataType::parse(&data_type.unwrap_or_else(|| "string".into())),
        ndattribute: attr_get(attrs, "ndattribute").unwrap_or_default(),
        det_default: attr_true(attrs, "det_default"),
        when: LayoutWhen::parse(&attr_get(attrs, "when").unwrap_or_default()),
        attributes: Vec::new(),
    }
}

fn attr_get(attrs: &[(String, String)], key: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// C sets a flag only on the lower-case `"true"`.
fn attr_true(attrs: &[(String, String)], key: &str) -> bool {
    attr_get(attrs, key).as_deref() == Some("true")
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

    fn names<'a>(it: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
        it.into_iter().collect()
    }

    /// C load_xml fails only when no `<group>` made its Root.
    #[test]
    fn a_layout_without_a_group_is_refused() {
        for xml in [
            "<foo/>",
            r#"<hdf5_layout><global name="detector_data_destination" ndattribute="x"/></hdf5_layout>"#,
            r#"<hdf5_layout><group/></hdf5_layout>"#,
        ] {
            let err = Hdf5Layout::parse(xml).unwrap_err();
            assert!(err.0.contains("Root tree"), "{xml}: {err}");
        }
    }

    /// process_node ignores an element it does not know but not its
    /// children, and reads `<hdf5_layout>` wherever it is.
    #[test]
    fn unknown_elements_pass_their_children_through() {
        let layout = Hdf5Layout::parse(
            r#"<layout foo="bar">
              <foo/>
              <group name="entry">
                <bar x="1"><attribute name="viaBar" source="constant" value="b"/></bar>
                <wrap><group name="inner"/></wrap>
                <dataset name="data" source="detector"><junk/><attribute name="signal" source="constant" value="1" type="int"/></dataset>
              </group>
              <hdf5_layout auto_ndattr_default="no"/>
              <x><hdf5_layout auto_ndattr_default="false"/></x>
              <hdf5_layout auto_ndattr_default="true"/>
            </layout>"#,
        )
        .unwrap();
        let entry = &layout.groups[0];
        assert_eq!(
            names(entry.attributes.iter().map(|a| a.name.as_str())),
            ["viaBar"]
        );
        assert_eq!(
            names(entry.groups.iter().map(|g| g.name.as_str())),
            ["inner"]
        );
        assert_eq!(entry.datasets[0].attributes[0].name, "signal");
        assert!(!layout.auto_ndattr_default);
    }

    /// An `<attribute>` or `<global>` holds no cursor: what it encloses acts
    /// on the element around it.
    #[test]
    fn elements_inside_an_attribute_act_on_its_element() {
        let layout = Hdf5Layout::parse(
            r#"<hdf5_layout><group name="entry">
              <attribute name="outer" source="constant" value="o"><attribute name="nested" source="constant" value="n"/></attribute>
              <attribute name="g" source="constant" value="1"><group name="ingroup"/></attribute>
              <global name="detector_data_destination" ndattribute="D"><group name="ing2"/></global>
              <global name="detector_data_destination"/>
            </group></hdf5_layout>"#,
        )
        .unwrap();
        let entry = &layout.groups[0];
        assert_eq!(
            names(entry.attributes.iter().map(|a| a.name.as_str())),
            ["g", "nested", "outer"]
        );
        assert_eq!(
            names(entry.groups.iter().map(|g| g.name.as_str())),
            ["ing2", "ingroup"]
        );
        assert_eq!(layout.detector_data_destination.as_deref(), Some("D"));
    }

    /// A missing or unknown source is C's `notset`; an attribute keeps the
    /// name it took first; flags are set by `"true"` only.
    #[test]
    fn sources_and_flags_read_as_c() {
        let layout = Hdf5Layout::parse(
            r#"<hdf5_layout><group name="entry">
              <attribute source="constant" value="noname"/>
              <attribute name="nosrc" value="x"/>
              <attribute name="bogus" source="bogus"/>
              <attribute name="nosrc" source="constant" value="second"/>
              <dataset name="data" source="detector" det_default="1"/>
              <dataset name="cbog" source="bogus" value="3" type="int"/>
              <group name="NDAttributes" ndattr_default="1"/>
            </group></hdf5_layout>"#,
        )
        .unwrap();
        let entry = &layout.groups[0];
        let attrs: Vec<_> = entry
            .attributes
            .iter()
            .map(|a| (a.name.as_str(), a.source))
            .collect();
        assert_eq!(
            attrs,
            [
                ("bogus", LayoutSource::Unset),
                ("nosrc", LayoutSource::Unset)
            ]
        );
        assert_eq!(entry.datasets[0].name, "cbog");
        assert_eq!(entry.datasets[0].source, LayoutSource::Unset);
        assert!(!entry.datasets[1].det_default);
        assert!(!entry.groups[0].ndattr_default);
    }

    /// A group or dataset C refuses leaves its cursor on the parent, so its
    /// children land there (C oracle).
    #[test]
    fn a_refused_node_passes_its_children_to_its_parent() {
        let layout = Hdf5Layout::parse(
            r#"<hdf5_layout>
              <group name="entry">
                <dataset name="data" source="detector"/>
                <group ndattr_default="true">
                  <dataset name="lost" source="constant" value="1" type="int"/>
                  <group name="lostg"><attribute name="lga" source="constant" value="l"/></group>
                  <attribute name="ga" source="constant" value="g"/>
                  <hardlink name="hl" target="/entry/data"/>
                </group>
              </group>
              <group name="dup">
                <dataset name="d" source="constant" value="1" type="int"/>
                <dataset name="d" source="constant" value="2" type="int"><attribute name="dupattr" source="constant" value="7" type="int"/></dataset>
              </group>
              <group name="nameless">
                <dataset source="constant" value="1" type="int"><attribute name="na" source="constant" value="x"/></dataset>
              </group>
              <group name="last"><dataset name="k" source="constant" value="3" type="int"/></group>
            </hdf5_layout>"#,
        )
        .unwrap();
        let group = |name: &str| layout.groups.iter().find(|g| g.name == name).unwrap();
        fn attrs(g: &LayoutGroup) -> Vec<&str> {
            names(g.attributes.iter().map(|a| a.name.as_str()))
        }
        let entry = group("entry");
        assert_eq!(
            names(entry.datasets.iter().map(|d| d.name.as_str())),
            ["data", "lost"]
        );
        assert_eq!(
            names(entry.groups.iter().map(|g| g.name.as_str())),
            ["lostg"]
        );
        assert!(!entry.ndattr_default);
        assert_eq!(attrs(&entry.groups[0]), ["lga"]);
        assert_eq!(attrs(entry), ["ga"]);
        assert_eq!(
            names(entry.hardlinks.iter().map(|h| h.name.as_str())),
            ["hl"]
        );
        let dup = group("dup");
        assert_eq!(attrs(dup), ["dupattr"]);
        assert_eq!(dup.datasets.len(), 1);
        assert_eq!(dup.datasets[0].value, "1");
        assert!(dup.datasets[0].attributes.is_empty());
        let nameless = group("nameless");
        assert_eq!(attrs(nameless), ["na"]);
        assert!(nameless.datasets.is_empty());
        assert_eq!(group("last").datasets[0].name, "k");
    }

    /// C's end tag of a refused node pops its parent, re-parenting later
    /// siblings; a node under a dataset or before the first group reaches a
    /// cast or a dangling cursor. Neither is copied: the siblings stay, and
    /// those nodes are refused.
    #[test]
    fn a_refused_node_keeps_later_siblings_in_its_parent() {
        let layout = Hdf5Layout::parse(
            r#"<hdf5_layout>
              <attribute name="early" source="constant" value="e"/>
              <dataset name="early_ds" source="constant" value="1" type="int"/>
              <group name="entry">
                <group><dataset name="lost" source="constant" value="1" type="int"/></group>
                <dataset source="detector"/>
                <dataset name="d" source="ndattribute" ndattribute="A">
                  <dataset name="sub" source="detector"/>
                  <group name="subgroup"/>
                  <hardlink name="l" target="/x"/>
                </dataset>
                <dataset name="data" source="detector"/>
              </group>
              <group name="other"/>
            </hdf5_layout>"#,
        )
        .unwrap();
        assert_eq!(
            names(layout.groups.iter().map(|g| g.name.as_str())),
            ["entry", "other"]
        );
        let entry = &layout.groups[0];
        assert!(entry.groups.is_empty() && entry.hardlinks.is_empty());
        assert_eq!(
            names(entry.datasets.iter().map(|d| d.name.as_str())),
            ["d", "data", "lost"]
        );
        assert!(entry.datasets[0].attributes.is_empty());
        assert!(layout.datasets.is_empty());
    }

    /// After the first group closes the cursor is C's Root: its datasets are
    /// kept and found first, its attributes and hardlinks are not.
    #[test]
    fn root_datasets_are_found_first_and_never_detector_data() {
        let layout = Hdf5Layout::parse(
            r#"<hdf5_layout>
              <group name="entry">
                <dataset name="data" source="detector"/>
                <dataset name="g" source="ndattribute" ndattribute="Gain"/>
                <group name="performance"><dataset name="timestamp"/></group>
              </group>
              <attribute name="rootattr" source="constant" value="r"/>
              <hardlink name="rootlink" target="/entry/data"/>
              <dataset name="Gain" source="ndattribute" ndattribute="Gain"/>
              <dataset name="raw" source="detector" det_default="true"/>
              <dataset name="timestamp"/>
              <dataset name="entry" source="constant" value="1" type="int"/>
            </hdf5_layout>"#,
        )
        .unwrap();
        assert_eq!(
            names(layout.datasets.iter().map(|d| d.name.as_str())),
            ["Gain", "raw", "timestamp"]
        );
        let (group, d) = layout.ndattribute_dataset("Gain").unwrap();
        assert_eq!((group.as_str(), d.name.as_str()), ("", "Gain"));
        assert_eq!(layout.detector_dataset_paths(), ["/entry/data"]);
        assert_eq!(
            layout.detector_dataset_path().as_deref(),
            Some("/entry/data")
        );
        assert_eq!(layout.dataset_group_path("timestamp").as_deref(), Some(""));
        assert_eq!(layout.performance_group().as_deref(), Some(""));
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
