//! NeXus file writer plugin.
//!
//! Writes NDArray data in NeXus/HDF5 format using the rust-hdf5 library.
//! Follows the simplified NXdata convention:
//!
//! ```text
//! /entry (NX_class=NXentry)
//!   /instrument (NX_class=NXinstrument)
//!     /detector (NX_class=NXdetector)
//!       /data → dataset [frames × Y × X]
//!   /data (NX_class=NXdata)
//!     /data → same dataset
//! ```

use asyn_rs::port::InterfaceMask;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrValue, NDAttributeList};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDataView};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{
    FileControl, FilePluginController, SettingsStore, WriterSettings,
};
use ad_core_rs::plugin::runtime::{NDPluginProcess, PluginAttributes, PluginPool, ProcessResult};
use epics_libcom_rs::runtime::scanf::{ScanNum, sscanf};
use parking_lot::Mutex;

use rust_hdf5::format::messages::datatype::DatatypeMessage;
use rust_hdf5::{H5Dataset, H5File, H5Type};

use crate::file_hdf5::{AttrTarget, c_s1_nullterm, write_ndattr_value};

/// Serialize an NDArray data buffer to **little-endian** bytes. `rust-hdf5`
/// 0.2.15 records every numeric datatype message as little-endian and copies
/// the `&[u8]` passed to `write_chunk` verbatim, so a typed dataset fed
/// host-endian `as_u8_slice()` bytes is only correct on a little-endian host.
/// This makes the on-disk bytes match the declared LE datatype on every host.
fn nd_buffer_to_le_bytes(buf: &NDDataBuffer) -> Vec<u8> {
    match buf.view() {
        NDDataView::I8(v) => v.iter().map(|&x| x as u8).collect(),
        NDDataView::U8(v) => v.to_vec(),
        NDDataView::I16(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::U16(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::I32(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::U32(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::I64(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::U64(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::F32(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        NDDataView::F64(v) => v.iter().flat_map(|&x| x.to_le_bytes()).collect(),
    }
}

// ===========================================================================
// NeXus XML template parser
// ===========================================================================

/// A parsed XML element of a NeXus template (C++ `NDFileNexus` `loadTemplateFile`).
/// Text content of leaf elements is preserved (CONST nodes need it).
#[derive(Debug, Clone)]
pub struct XmlElement {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<XmlElement>,
    /// Concatenated direct text content (trimmed).
    pub text: String,
}

impl XmlElement {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Error from NeXus XML template parsing.
#[derive(Debug)]
pub struct NexusTemplateError(pub String);

impl std::fmt::Display for NexusTemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NeXus template error: {}", self.0)
    }
}

/// Parse a NeXus XML template into a single root `XmlElement` tree.
///
/// Hand-written recursive parser — handles elements, attributes (single or
/// double quoted), self-closing tags, text content, comments, and the
/// `<?xml?>` prolog. No external XML crate is pulled in.
pub fn parse_nexus_template(text: &str) -> Result<XmlElement, NexusTemplateError> {
    let chars: Vec<char> = text.chars().collect();
    let mut pos = 0;
    skip_prolog_and_ws(&chars, &mut pos);
    let root = parse_element(&chars, &mut pos)?;
    Ok(root)
}

fn skip_prolog_and_ws(chars: &[char], pos: &mut usize) {
    loop {
        while *pos < chars.len() && chars[*pos].is_whitespace() {
            *pos += 1;
        }
        if chars[*pos..].starts_with(&['<', '?']) {
            // <?xml ... ?>
            while *pos < chars.len() && !(chars[*pos] == '?' && chars.get(*pos + 1) == Some(&'>')) {
                *pos += 1;
            }
            *pos += 2;
        } else if chars[*pos..].starts_with(&['<', '!', '-', '-']) {
            skip_comment(chars, pos);
        } else {
            break;
        }
    }
}

fn skip_comment(chars: &[char], pos: &mut usize) {
    // assumes pos at "<!--"
    *pos += 4;
    while *pos < chars.len()
        && !(chars[*pos] == '-'
            && chars.get(*pos + 1) == Some(&'-')
            && chars.get(*pos + 2) == Some(&'>'))
    {
        *pos += 1;
    }
    *pos += 3;
}

fn parse_element(chars: &[char], pos: &mut usize) -> Result<XmlElement, NexusTemplateError> {
    if *pos >= chars.len() || chars[*pos] != '<' {
        return Err(NexusTemplateError("expected element start '<'".into()));
    }
    *pos += 1;
    // Element name.
    let name = read_name(chars, pos);
    if name.is_empty() {
        return Err(NexusTemplateError("empty element name".into()));
    }
    // Attributes.
    let mut attrs = Vec::new();
    loop {
        skip_ws(chars, pos);
        if *pos >= chars.len() {
            return Err(NexusTemplateError("unterminated tag".into()));
        }
        if chars[*pos] == '/' && chars.get(*pos + 1) == Some(&'>') {
            *pos += 2;
            return Ok(XmlElement {
                name,
                attrs,
                children: Vec::new(),
                text: String::new(),
            });
        }
        if chars[*pos] == '>' {
            *pos += 1;
            break;
        }
        let attr_name = read_name(chars, pos);
        if attr_name.is_empty() {
            return Err(NexusTemplateError(format!(
                "malformed attribute in tag '{}'",
                name
            )));
        }
        skip_ws(chars, pos);
        if *pos >= chars.len() || chars[*pos] != '=' {
            return Err(NexusTemplateError(format!(
                "attribute '{}' missing '='",
                attr_name
            )));
        }
        *pos += 1;
        skip_ws(chars, pos);
        let value = read_quoted(chars, pos)?;
        attrs.push((attr_name, value));
    }
    // Children + text until the matching close tag.
    let mut children = Vec::new();
    let mut text = String::new();
    loop {
        if *pos >= chars.len() {
            return Err(NexusTemplateError(format!("unclosed element '{}'", name)));
        }
        if chars[*pos..].starts_with(&['<', '!', '-', '-']) {
            skip_comment(chars, pos);
            continue;
        }
        if chars[*pos] == '<' && chars.get(*pos + 1) == Some(&'/') {
            // Close tag.
            *pos += 2;
            let close_name = read_name(chars, pos);
            skip_ws(chars, pos);
            if *pos < chars.len() && chars[*pos] == '>' {
                *pos += 1;
            }
            if close_name != name {
                return Err(NexusTemplateError(format!(
                    "mismatched close tag: expected '{}', got '{}'",
                    name, close_name
                )));
            }
            break;
        }
        if chars[*pos] == '<' {
            children.push(parse_element(chars, pos)?);
        } else {
            text.push(chars[*pos]);
            *pos += 1;
        }
    }
    Ok(XmlElement {
        name,
        attrs,
        children,
        text: text.trim().to_string(),
    })
}

fn skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() && chars[*pos].is_whitespace() {
        *pos += 1;
    }
}

fn read_name(chars: &[char], pos: &mut usize) -> String {
    skip_ws(chars, pos);
    let start = *pos;
    while *pos < chars.len()
        && !chars[*pos].is_whitespace()
        && !matches!(chars[*pos], '=' | '>' | '/' | '<')
    {
        *pos += 1;
    }
    chars[start..*pos].iter().collect()
}

fn read_quoted(chars: &[char], pos: &mut usize) -> Result<String, NexusTemplateError> {
    if *pos >= chars.len() || (chars[*pos] != '"' && chars[*pos] != '\'') {
        return Err(NexusTemplateError("expected quoted attribute value".into()));
    }
    let quote = chars[*pos];
    *pos += 1;
    let start = *pos;
    while *pos < chars.len() && chars[*pos] != quote {
        *pos += 1;
    }
    if *pos >= chars.len() {
        return Err(NexusTemplateError("unterminated attribute value".into()));
    }
    let value: String = chars[start..*pos].iter().collect();
    *pos += 1;
    Ok(value)
}

/// The set of NeXus base-class names recognised as group nodes
/// (NDFileNexus.cpp:205-247). A node whose tag is one of these, or whose
/// `type` attribute is `UserGroup`, becomes an HDF5 group.
const NEXUS_GROUP_CLASSES: &[&str] = &[
    "NXentry",
    "NXinstrument",
    "NXsample",
    "NXmonitor",
    "NXsource",
    "NXuser",
    "NXdata",
    "NXdetector",
    "NXaperature",
    "NXattenuator",
    "NXbeam_stop",
    "NXbending_magnet",
    "NXcollimator",
    "NXcrystal",
    "NXdisk_chopper",
    "NXfermi_chopper",
    "NXfilter",
    "NXflipper",
    "NXguide",
    "NXinsertion_device",
    "NXmirror",
    "NXmoderator",
    "NXmonochromator",
    "NXpolarizer",
    "NXpositioner",
    "NXvelocity_selector",
    "NXevent_data",
    "NXprocess",
    "NXcharacterization",
    "NXlog",
    "NXnote",
    "NXbeam",
    "NXgeometry",
    "NXtranslation",
    "NXshape",
    "NXorientation",
    "NXenvironment",
    "NXsensor",
    "NXcapillary",
    "NXcollection",
    "NXdetector_group",
    "NXparameters",
    "NXsubentry",
    "NXxraylens",
];

/// Classify an XML element as a NeXus group node.
fn is_nexus_group(el: &XmlElement) -> bool {
    NEXUS_GROUP_CLASSES.contains(&el.name.as_str()) || el.attr("type") == Some("UserGroup")
}

/// NeXus file writer using HDF5 with NeXus group structure.
pub struct NexusWriter {
    current_path: Option<PathBuf>,
    file: Option<H5File>,
    frame_count: usize,
    /// Reusable dataset handle for the main array data, multi-frame writes.
    dataset: Option<H5Dataset>,
    /// Per-frame `uniqueId` dataset (proper 1-D resizable dataset).
    uid_dataset: Option<H5Dataset>,
    /// Per-frame `timeStamp` dataset.
    ts_dataset: Option<H5Dataset>,
    /// Parsed NeXus XML template tree, if a valid template was loaded; read
    /// at open, where C's `openFile` walks `configDoc`.
    template: WriterSettings<Option<XmlElement>>,
    /// HDF5 path of the group that contains the main array dataset (template
    /// mode); the dataset itself is named by the template's `pArray` node.
    data_group_path: String,
    data_node_name: String,
    /// The `pArray` node's `Attr` children, resolved at open and written on
    /// the main array dataset when the first frame creates it.
    data_attrs: Vec<AttrNode>,
    /// HDF5 path of the NXdata group that receives the image-data copy
    /// (built-in hierarchy only — `Some("entry/data")`). `None` in template
    /// mode, where the template controls all dataset placement.
    nxdata_group_path: Option<String>,
    /// The plugin's own set, C `getAttributes` in `openFile`.
    plugin_attributes: NDAttributeList,
}

impl NexusWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            file: None,
            frame_count: 0,
            dataset: None,
            uid_dataset: None,
            ts_dataset: None,
            template: WriterSettings::new(None),
            data_group_path: "entry/instrument/detector".to_string(),
            data_node_name: "data".to_string(),
            data_attrs: Vec::new(),
            nxdata_group_path: None,
            plugin_attributes: NDAttributeList::new(),
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Whether a NeXus XML template is currently loaded.
    pub fn has_template(&self) -> bool {
        self.template.store().get(Option::is_some)
    }

    /// Load (parse and validate) a NeXus XML template. On success the template
    /// drives the file structure; on parse failure the last good template
    /// stays. Returns whether the template parsed successfully (C++
    /// `NDFileNexusTemplateValid`).
    pub fn load_template(&mut self, xml: &str) -> bool {
        store_template(&self.template.store(), xml)
    }

    /// Clear any loaded template (revert to the built-in hierarchy).
    pub fn clear_template(&mut self) {
        self.template.store().update(|t| *t = None);
    }

    /// Write the NeXus `NX_class` group marker as a true HDF5 group attribute.
    ///
    /// NeXus requires `NX_class` to be an HDF5 *group attribute*. rust-hdf5
    /// 0.2.15 exposes `H5Group::set_attr_string`, so the class name is written
    /// as a real attribute on the group itself — NeXus-aware readers (nexpy,
    /// DAWN, h5py NeXus) recognise the group class.
    fn write_nx_class(group: &rust_hdf5::H5Group, class_name: &str) -> ADResult<()> {
        group.set_attr_string("NX_class", class_name).map_err(|e| {
            ADError::UnsupportedConversion(format!("NX_class group attr error: {}", e))
        })
    }

    /// Write `NX_class` as a true HDF5 attribute on the root group.
    fn apply_root_nx_class(file: &H5File, class_name: &str) {
        let _ = file.set_attr_string("NX_class", class_name);
    }

    /// Process the NeXus XML template to build the file's group/dataset tree
    /// (port of C++ `NDFileNexus::processNode` / `iterateNodes`).
    ///
    /// Returns the HDF5 path of the group that should contain the main array
    /// dataset and the dataset name, identified by the template's `pArray`
    /// node. If the template contains no `pArray` node the built-in default
    /// (`entry/instrument/detector` / `data`) is kept.
    fn process_template(
        h5file: &H5File,
        template: &XmlElement,
        attributes: &NDAttributeList,
    ) -> ADResult<DataNode> {
        let mut data = DataNode::default();
        // The root template node is typically NXroot — iterate its children.
        let top_children: &[XmlElement] = if template.name == "NXroot" {
            &template.children
        } else {
            std::slice::from_ref(template)
        };
        for child in top_children {
            Self::process_node(h5file, None, "", child, attributes, &mut data)?;
        }
        if data.name.is_empty() {
            data.group = "entry/instrument/detector".to_string();
            data.name = "data".to_string();
        }
        Ok(data)
    }

    /// Recursively process one template node. `parent` is the HDF5 group to
    /// create children in (None = file root); `parent_path` is its HDF5 path.
    fn process_node(
        h5file: &H5File,
        parent: Option<&rust_hdf5::H5Group>,
        parent_path: &str,
        node: &XmlElement,
        attributes: &NDAttributeList,
        data: &mut DataNode,
    ) -> ADResult<()> {
        let node_type = node.attr("type").map(|s| s.to_string());

        if is_nexus_group(node) {
            // Group node: HDF5 group named by `name` attr or the tag itself,
            // NX_class = the tag.
            let group_name = node.attr("name").unwrap_or(&node.name).to_string();
            let group = match parent {
                Some(p) => p.create_group(&group_name),
                None => h5file.create_group(&group_name),
            }
            .map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus group '{}': {}", group_name, e))
            })?;
            let class_name = if NEXUS_GROUP_CLASSES.contains(&node.name.as_str()) {
                node.name.clone()
            } else {
                // UserGroup: NX_class taken from an explicit attr if present.
                node.attr("type").unwrap_or("NXcollection").to_string()
            };
            Self::write_nx_class(&group, &class_name)?;
            let child_path = if parent_path.is_empty() {
                group_name.clone()
            } else {
                format!("{}/{}", parent_path, group_name)
            };
            for child in &node.children {
                Self::process_node(h5file, Some(&group), &child_path, child, attributes, data)?;
            }
            return Ok(());
        }

        // C takes an `Attr` element by its name before its type
        // (NDFileNexus.cpp:269), so it is never a dataset.
        if node.name == "Attr" {
            if let Some(attr) = AttrNode::resolve(node, attributes) {
                match parent {
                    Some(group) => attr.write(group)?,
                    None => attr.write(&h5file.root_group())?,
                }
            }
            return Ok(());
        }

        // Non-group node.
        match node_type.as_deref() {
            Some("pArray") => {
                // The main NDArray dataset is created lazily in write_file;
                // here we only record where it goes.
                data.group = parent_path.to_string();
                data.name = node.name.clone();
                data.attrs = AttrNode::children(node, attributes);
            }
            Some("CONST") => {
                if let (Some(parent), Some(value)) =
                    (parent, NxValue::constant(node.attr("outtype"), &node.text))
                {
                    let ds = value.write_dataset(parent, &node.name)?;
                    AttrNode::write_all(&AttrNode::children(node, attributes), &ds)?;
                }
            }
            Some("ND_ATTR") => {
                if let Some(parent) = parent {
                    let source = node.attr("source").unwrap_or(&node.name);
                    if let Some(value) = attributes
                        .get(source)
                        .and_then(|a| NxValue::attribute(&a.value))
                    {
                        let ds = value.write_dataset(parent, &node.name)?;
                        AttrNode::write_all(&AttrNode::children(node, attributes), &ds)?;
                    }
                }
            }
            // C :451-455: any other type is invalid and writes nothing.
            Some(_) => {}
            None => {
                // Untyped leaf → constant char dataset of its text content.
                if let Some(parent) = parent {
                    let text = if node.text.is_empty() {
                        "LEFT BLANK"
                    } else {
                        &node.text
                    };
                    let ds = NxValue::Char(text.into()).write_dataset(parent, &node.name)?;
                    AttrNode::write_all(&AttrNode::children(node, attributes), &ds)?;
                }
            }
        }
        Ok(())
    }
}

/// Where the template's `pArray` node puts the frames.
#[derive(Default)]
struct DataNode {
    group: String,
    name: String,
    attrs: Vec<AttrNode>,
}

/// A template `Attr` node resolved against the file's attribute list, as
/// NDFileNexus.cpp:269-310 hands it to `NXputattr` on the group or dataset
/// open at that point.
enum AttrNode {
    /// `ND_ATTR`: the NDAttribute in its own type (getAttrTypeNSize, :552).
    NdAttr { name: String, value: NDAttrValue },
    /// `CONST`: the node text in its `outtype`.
    Const { name: String, value: NxValue },
}

impl AttrNode {
    /// `None` where C writes nothing: an unknown source, or a type other
    /// than ND_ATTR and CONST.
    fn resolve(node: &XmlElement, attributes: &NDAttributeList) -> Option<Self> {
        let name = node.attr("name").unwrap_or(&node.name).to_string();
        match node.attr("type")? {
            "ND_ATTR" => {
                let value = attributes.get(node.attr("source")?)?.value.clone();
                Some(Self::NdAttr { name, value })
            }
            "CONST" => Some(Self::Const {
                name,
                value: NxValue::constant(node.attr("outtype"), &node.text)?,
            }),
            _ => None,
        }
    }

    /// The `Attr` children of a dataset node, which C writes on that dataset
    /// (iterateNodes while it is open, :336, :424, :448, :473).
    fn children(node: &XmlElement, attributes: &NDAttributeList) -> Vec<Self> {
        node.children
            .iter()
            .filter(|c| c.name == "Attr")
            .filter_map(|c| Self::resolve(c, attributes))
            .collect()
    }

    fn write(&self, mut target: impl AttrTarget) -> ADResult<()> {
        match self {
            Self::NdAttr { name, value } => write_ndattr_value(&mut target, name, value),
            Self::Const { name, value } => value.write_attr(&mut target, name),
        }
        .map_err(|e| ADError::UnsupportedConversion(format!("NeXus attribute error: {e}")))
    }

    fn write_all(attrs: &[Self], dataset: &H5Dataset) -> ADResult<()> {
        attrs.iter().try_for_each(|a| a.write(dataset))
    }
}

/// A value as NDFileNexus passes it to `NXputattr` or `NXputdata`, in the NX
/// type it is written as.
#[derive(Debug, Clone, PartialEq)]
enum NxValue {
    Int8(i8),
    UInt8(u8),
    Int16(i16),
    UInt16(u16),
    Int32(i32),
    UInt32(u32),
    Int64(i64),
    UInt64(u64),
    Float32(f32),
    Float64(f64),
    /// `NX_CHAR`: as many bytes as the length C passes with them.
    Char(Vec<u8>),
}

impl NxValue {
    /// A `CONST` node's text in its `outtype`, NX_CHAR when absent, read as
    /// constTextToDataType does (NDFileNexus.cpp:293-308, :427-443, :670-768).
    /// `None` for an outtype typeStringToVal does not name: nxToHDF5Type
    /// refuses it, so C creates nothing.
    fn constant(outtype: Option<&str>, text: &str) -> Option<Self> {
        // C leaves its `sscanf` target indeterminate when the text holds no
        // number; this reads that as 0.
        fn scan<T: ScanNum + Default>(text: &str, format: &str) -> T {
            let value = Cell::new(T::default());
            sscanf(text, format, &[&value]);
            value.get()
        }
        let int = || scan::<i32>(text, "%d");
        Some(match outtype.unwrap_or("NX_CHAR") {
            "NX_CHAR" => Self::Char(text.into()),
            "NX_INT8" => Self::Int8(int() as i8),
            "NX_UINT8" => Self::UInt8(int() as u8),
            "NX_INT16" => Self::Int16(int() as i16),
            "NX_UINT16" => Self::UInt16(int() as u16),
            "NX_INT32" => Self::Int32(int()),
            "NX_UINT32" => Self::UInt32(int() as u32),
            "NX_INT64" => Self::Int64(scan(text, "%lld")),
            "NX_UINT64" => Self::UInt64(scan(text, "%llu")),
            "NX_FLOAT32" => Self::Float32(scan::<f64>(text, "%lf") as f32),
            "NX_FLOAT64" => Self::Float64(scan(text, "%lf")),
            _ => return None,
        })
    }

    /// An `ND_ATTR` dataset's value as getAttrTypeNSize types it
    /// (NDFileNexus.cpp:319-333, :552-613): a string with its terminator, as
    /// getValueInfo sizes it; `None` for an undefined one, which C skips.
    fn attribute(value: &NDAttrValue) -> Option<Self> {
        Some(match value {
            NDAttrValue::Int8(v) => Self::Int8(*v),
            NDAttrValue::UInt8(v) => Self::UInt8(*v),
            NDAttrValue::Int16(v) => Self::Int16(*v),
            NDAttrValue::UInt16(v) => Self::UInt16(*v),
            NDAttrValue::Int32(v) => Self::Int32(*v),
            NDAttrValue::UInt32(v) => Self::UInt32(*v),
            NDAttrValue::Int64(v) => Self::Int64(*v),
            NDAttrValue::UInt64(v) => Self::UInt64(*v),
            NDAttrValue::Float32(v) => Self::Float32(*v),
            NDAttrValue::Float64(v) => Self::Float64(*v),
            NDAttrValue::String(s) => Self::Char([s.as_bytes(), b"\0"].concat()),
            NDAttrValue::Undefined => return None,
        })
    }

    /// The string type `H5Tset_size(len)` leaves (napi5.c NX5putattr,
    /// NX5compmakedata64): HDF5 refuses a size of 0, so H5T_C_S1 keeps its
    /// one byte, written from C's zeroed buffer.
    fn char_type(bytes: &[u8]) -> (DatatypeMessage, Vec<u8>) {
        let bytes = if bytes.is_empty() {
            vec![0]
        } else {
            bytes.to_vec()
        };
        (c_s1_nullterm(bytes.len()), bytes)
    }

    /// NX5putattr: a scalar attribute.
    fn write_attr(&self, target: &mut impl AttrTarget, name: &str) -> rust_hdf5::Result<()> {
        match self {
            Self::Int8(v) => target.scalar(name, v),
            Self::UInt8(v) => target.scalar(name, v),
            Self::Int16(v) => target.scalar(name, v),
            Self::UInt16(v) => target.scalar(name, v),
            Self::Int32(v) => target.scalar(name, v),
            Self::UInt32(v) => target.scalar(name, v),
            Self::Int64(v) => target.scalar(name, v),
            Self::UInt64(v) => target.scalar(name, v),
            Self::Float32(v) => target.scalar(name, v),
            Self::Float64(v) => target.scalar(name, v),
            Self::Char(bytes) => {
                let (datatype, bytes) = Self::char_type(bytes);
                target.typed(name, datatype, bytes)
            }
        }
    }

    /// NXmakedata with rank 1 and NXputdata: one element, a string being one
    /// element of its length. A length of 0 is an unlimited dimension, which
    /// napi5 chunks by one.
    fn write_dataset(&self, group: &rust_hdf5::H5Group, name: &str) -> ADResult<H5Dataset> {
        fn one<T: H5Type>(
            group: &rust_hdf5::H5Group,
            name: &str,
            value: T,
        ) -> rust_hdf5::Result<H5Dataset> {
            let ds = group.new_dataset::<T>().shape([1]).create(name)?;
            ds.write_raw(&[value])?;
            Ok(ds)
        }
        match self {
            Self::Int8(v) => one(group, name, *v),
            Self::UInt8(v) => one(group, name, *v),
            Self::Int16(v) => one(group, name, *v),
            Self::UInt16(v) => one(group, name, *v),
            Self::Int32(v) => one(group, name, *v),
            Self::UInt32(v) => one(group, name, *v),
            Self::Int64(v) => one(group, name, *v),
            Self::UInt64(v) => one(group, name, *v),
            Self::Float32(v) => one(group, name, *v),
            Self::Float64(v) => one(group, name, *v),
            Self::Char(text) => {
                let (datatype, bytes) = Self::char_type(text);
                let mut builder = group.new_dataset::<u8>().datatype(datatype).shape([1]);
                if text.is_empty() {
                    builder = builder.max_shape(&[None]).chunk(&[1]);
                }
                builder.create(name).and_then(|ds| {
                    ds.write_raw_bytes(&bytes)?;
                    Ok(ds)
                })
            }
        }
        .map_err(|e| ADError::UnsupportedConversion(format!("NeXus dataset '{name}': {e}")))
    }
}

impl Default for NexusWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl NDFileWriter for NexusWriter {
    fn open_file(&mut self, path: &Path, mode: NDFileOpenMode, array: &NDArray) -> ADResult<()> {
        // NDFileNexus.cpp:45-48
        mode.refuse_read_or_append("NDFileNexus")?;
        self.current_path = Some(path.to_path_buf());
        self.frame_count = 0;
        self.dataset = None;
        self.uid_dataset = None;
        self.ts_dataset = None;

        let h5file = H5File::create(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("NeXus create error: {}", e)))?;

        // Root-group NX_class is a true HDF5 attribute.
        Self::apply_root_nx_class(&h5file, "NXroot");

        self.dataset = None;
        self.uid_dataset = None;
        self.ts_dataset = None;
        self.nxdata_group_path = None;

        self.template.refresh();
        if let Some(template) = (*self.template).clone() {
            // Template mode: build the group/dataset tree from the user's
            // NeXus XML template (C++ NDFileNexus::loadTemplateFile).
            // C walks the template against `pFileAttributes`: the plugin's
            // set with the frame's list on top (NDFileNexus.cpp:53-71).
            let mut attributes = self.plugin_attributes.clone();
            attributes.copy_from(&array.attributes);
            let data = Self::process_template(&h5file, &template, &attributes)?;
            self.data_group_path = data.group;
            self.data_node_name = data.name;
            self.data_attrs = data.attrs;
        } else {
            // Built-in NXentry/NXdata hierarchy. NX_class on every group is a
            // true HDF5 group attribute (rust-hdf5 0.2.15).
            let entry = h5file
                .create_group("entry")
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus group error: {}", e)))?;
            Self::write_nx_class(&entry, "NXentry")?;
            let instrument = entry
                .create_group("instrument")
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus group error: {}", e)))?;
            Self::write_nx_class(&instrument, "NXinstrument")?;
            let detector = instrument
                .create_group("detector")
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus group error: {}", e)))?;
            Self::write_nx_class(&detector, "NXdetector")?;
            let data_group = entry
                .create_group("data")
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus group error: {}", e)))?;
            Self::write_nx_class(&data_group, "NXdata")?;
            self.data_group_path = "entry/instrument/detector".to_string();
            self.data_node_name = "data".to_string();
            self.data_attrs.clear();
            // The image data must appear inside the NXdata group so NeXus
            // readers locate the signal. write_file hard-links the detector
            // dataset into this group (rust-hdf5 0.2.15 `H5Group::link`).
            self.nxdata_group_path = Some("entry/data".to_string());
        }

        self.file = Some(h5file);
        Ok(())
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        let h5file = self
            .file
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no NeXus file open".into()))?;

        let frame_shape = array.dims.iter().rev().map(|d| d.size).collect::<Vec<_>>();
        let data_node_name = self.data_node_name.clone();
        let nxdata_group_path = self.nxdata_group_path.clone();

        // Resolve a group from its slash-separated HDF5 path.
        let resolve_group = |path: &str| -> ADResult<rust_hdf5::H5Group> {
            let mut group = h5file.root_group();
            for component in path.split('/') {
                if component.is_empty() {
                    continue;
                }
                group = group
                    .group(component)
                    .map_err(|e| ADError::UnsupportedConversion(e.to_string()))?;
            }
            Ok(group)
        };
        let data_group = resolve_group(&self.data_group_path)?;

        // The frame bytes serialized explicitly little-endian (rust-hdf5
        // 0.2.15 records LE datatypes and copies write_chunk bytes verbatim).
        let frame_bytes = nd_buffer_to_le_bytes(array.data());

        if self.frame_count == 0 {
            // First frame: create a chunked dataset with leading frame dim.
            // Shape: [1, dim0, dim1, ...], chunk: [1, dim0, dim1, ...]
            let mut ds_shape = vec![1usize];
            ds_shape.extend_from_slice(&frame_shape);
            let chunk_dims = ds_shape.clone();
            // Only the leading frame axis is unlimited. rust-hdf5 0.2.15 picks
            // the chunk index from the unlimited-dimension count: one unlimited
            // dim → extensible array (linear `write_chunk`); two or more →
            // v2 B-tree (which requires `write_chunk_at`). `.resizable()` would
            // make every axis unlimited and force the v2 B-tree path.
            let mut image_max_shape: Vec<Option<usize>> = vec![None];
            image_max_shape.extend(frame_shape.iter().map(|&d| Some(d)));

            // Create the image dataset typed per the NDArray data type; all
            // ten NDArray types are covered so read_file can recover them.
            macro_rules! create_image_ds {
                ($group:expr, $t:ty, $name:expr) => {{
                    $group
                        .new_dataset::<$t>()
                        .shape(&ds_shape[..])
                        .chunk(&chunk_dims[..])
                        .max_shape(&image_max_shape[..])
                        .create($name)
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!("NeXus dataset error: {}", e))
                        })?
                }};
            }
            macro_rules! create_typed {
                ($group:expr, $name:expr) => {{
                    match array.data().data_type() {
                        NDDataType::Int8 => create_image_ds!($group, i8, $name),
                        NDDataType::UInt8 => create_image_ds!($group, u8, $name),
                        NDDataType::Int16 => create_image_ds!($group, i16, $name),
                        NDDataType::UInt16 => create_image_ds!($group, u16, $name),
                        NDDataType::Int32 => create_image_ds!($group, i32, $name),
                        NDDataType::UInt32 => create_image_ds!($group, u32, $name),
                        NDDataType::Int64 => create_image_ds!($group, i64, $name),
                        NDDataType::UInt64 => create_image_ds!($group, u64, $name),
                        NDDataType::Float32 => create_image_ds!($group, f32, $name),
                        NDDataType::Float64 => create_image_ds!($group, f64, $name),
                    }
                }};
            }

            let ds = create_typed!(data_group, &data_node_name);
            ds.write_chunk(0, &frame_bytes)
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus write error: {}", e)))?;
            AttrNode::write_all(&self.data_attrs, &ds)?;
            self.dataset = Some(ds);

            // Built-in hierarchy: also place the image data inside the
            // NXdata group so a NeXus reader can locate the signal there.
            if let Some(ref nxpath) = nxdata_group_path {
                let nxdata_group = resolve_group(nxpath)?;
                // Hard-link the detector dataset into the NXdata group: one
                // physical dataset, two names (rust-hdf5 0.2.15 H5Group::link).
                // Extending the detector dataset per frame is automatically
                // visible through the link — no duplicate copy.
                let target = format!("/{}/{}", self.data_group_path, data_node_name);
                nxdata_group.link("data", &target).map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus NXdata link error: {}", e))
                })?;
            }

            // Per-frame uniqueId / timeStamp are proper resizable 1-D
            // datasets (C++ writes them as datasets, not as N numbered
            // attributes). Created here on the first frame.
            let uid = data_group
                .new_dataset::<i32>()
                .shape([1usize])
                .chunk(&[1usize])
                .resizable()
                .create("uniqueId")
                .map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus uniqueId dataset: {}", e))
                })?;
            uid.write_chunk(0, &array.unique_id.to_le_bytes())
                .map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus uniqueId write: {}", e))
                })?;
            self.uid_dataset = Some(uid);

            let ts = data_group
                .new_dataset::<f64>()
                .shape([1usize])
                .chunk(&[1usize])
                .resizable()
                .create("timeStamp")
                .map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus timeStamp dataset: {}", e))
                })?;
            ts.write_chunk(0, &array.time_stamp.to_le_bytes())
                .map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus timeStamp write: {}", e))
                })?;
            self.ts_dataset = Some(ts);
        } else {
            // Subsequent frames: extend dataset and write new chunk.
            let ds = self.dataset.as_ref().ok_or_else(|| {
                ADError::UnsupportedConversion("no dataset for multi-frame write".into())
            })?;

            let new_frame_count = self.frame_count + 1;
            let mut new_shape = vec![new_frame_count];
            new_shape.extend_from_slice(&frame_shape);
            ds.extend(&new_shape).map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus extend error: {}", e))
            })?;
            ds.write_chunk(self.frame_count, &frame_bytes)
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus write error: {}", e)))?;

            // The NXdata-group `data` is a hard link to this same dataset, so
            // the extension above is already visible there — no separate copy.

            // Extend the per-frame metadata datasets and append this frame.
            if let Some(uid) = self.uid_dataset.as_ref() {
                uid.extend(&[new_frame_count]).map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus uniqueId extend: {}", e))
                })?;
                uid.write_chunk(self.frame_count, &array.unique_id.to_le_bytes())
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!("NeXus uniqueId write: {}", e))
                    })?;
            }
            if let Some(ts) = self.ts_dataset.as_ref() {
                ts.extend(&[new_frame_count]).map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus timeStamp extend: {}", e))
                })?;
                ts.write_chunk(self.frame_count, &array.time_stamp.to_le_bytes())
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!("NeXus timeStamp write: {}", e))
                    })?;
            }
        }

        self.frame_count += 1;
        Ok(())
    }

    /// Unreachable through `readFileBase`, whose open is refused first; C's
    /// `readFile` is a stub that fails (NDFileNexus.cpp:115-130).
    fn read_file(&mut self, _pool: &NDArrayPool) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion(
            "NDFileNexus cannot read".into(),
        ))
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.dataset = None;
        self.uid_dataset = None;
        self.ts_dataset = None;
        self.file = None;
        self.current_path = None;
        Ok(())
    }

    fn set_plugin_attributes(&mut self, attributes: &NDAttributeList) {
        self.plugin_attributes.clone_from(attributes);
    }

    fn supports_multiple_arrays(&self) -> bool {
        true
    }
}

// ============================================================
// Processor
// ============================================================

/// Parse `xml` into `store`; whether it parsed. One that does not leaves the
/// last good template, as C sets `rootNode` only on a load that succeeds
/// (NDFileNexus.cpp:829-847).
fn store_template(store: &SettingsStore<Option<XmlElement>>, xml: &str) -> bool {
    match parse_nexus_template(xml) {
        Ok(root) => {
            store.update(|t| *t = Some(root));
            true
        }
        Err(_) => false,
    }
}

pub struct NexusFileProcessor {
    ctrl: Mutex<FilePluginController<NexusWriter>>,
    template: SettingsStore<Option<XmlElement>>,
    template_path_idx: Option<usize>,
    template_file_idx: Option<usize>,
    template_valid_idx: Option<usize>,
    attributes: Option<PluginAttributes>,
}

impl NexusFileProcessor {
    pub fn new() -> Self {
        let writer = NexusWriter::new();
        Self {
            template: writer.template.store(),
            ctrl: Mutex::new(FilePluginController::new(writer)),
            template_path_idx: None,
            template_file_idx: None,
            template_valid_idx: None,
            attributes: None,
        }
    }

    /// Load the NeXus XML template at `full` (C++
    /// NDFileNexus::loadTemplateFile); a file that cannot be read or parsed
    /// leaves the last good template in use.
    fn load_template(&self, full: &str) -> bool {
        std::fs::read_to_string(full).is_ok_and(|xml| store_template(&self.template, &xml))
    }
}

impl Default for NexusFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for NexusFileProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    // NDFileNexus.cpp:873, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        self.ctrl.lock().posts_before_write(reason, value)
    }

    fn processing_required(&self, array: &NDArray) -> bool {
        self.ctrl.lock().processing_required(array)
    }

    /// C NDPluginFile::writeInt32 (NDPluginFile.cpp:799-870).
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        let mut ctrl = self.ctrl.lock();
        ctrl.write_int32(reason, value, verdict, params)
    }

    /// C `NDFileNexus::writeOctet` (NDFileNexus.cpp:776-813) loads the
    /// template inside the write, from the path and file in the param
    /// library, so a capture opened next uses it; a file write in progress
    /// does not hold the write up. NDPluginFile's failure carries Nexus's
    /// errorMessage: C overwrites that status with callParamCallbacks'
    /// (:801), so the write succeeds; not copied.
    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        value: &str,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<String>,
        params: &asyn_rs::port::PortDriverBase,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<String> {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let verdict = match verdict {
            WriteVerdict::Refuse { keep, .. } => WriteVerdict::Refuse {
                keep,
                message: format!(
                    "NDFileNexus:writeOctet: status=3, function={reason}, value={value}"
                ),
            },
            accepted => accepted,
        };
        let out = ad_core_rs::plugin::runtime::WriteOutcome::from(verdict);
        let stored = |idx: Option<usize>| {
            if Some(reason) == idx {
                return out.verdict.kept();
            }
            idx.and_then(|i| params.get_string_param(i, 0).ok())
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default()
        };
        if Some(reason) != self.template_path_idx && Some(reason) != self.template_file_idx {
            return self.ctrl.lock().write_octet(reason, out.verdict);
        }
        let file = stored(self.template_file_idx);
        // C returns before touching the template or its flag (:825).
        if file.is_empty() {
            return out;
        }
        let full = format!("{}{file}", stored(self.template_path_idx));
        let valid = self.load_template(&full);
        let out = match self.template_valid_idx {
            Some(i) => out.set_int32(i, 0, valid as i32),
            None => out,
        };
        if valid {
            return out;
        }
        out.print(
            asyn_rs::trace::TraceMask::ERROR,
            format!("NDFileNexus:loadTemplateFile: Parameter file {full} is invalid\n"),
        )
    }
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // Made before the controller lock: its reads wait on the actor,
        // which takes that lock in its writes.
        let mut post = ad_core_rs::plugin::runtime::PortPost::of(self.attributes.as_ref());
        let mut ctrl = self.ctrl.lock();
        ctrl.process_array(array, &mut post)
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn bind_pool(&mut self, pool: PluginPool) {
        self.ctrl.get_mut().bind_pool(pool);
    }

    fn plugin_type(&self) -> &str {
        "NDFileNexus"
    }

    /// C `NDPluginFile.cpp:948` (base of every file writer) sets
    /// `NDArrayCallbacks = 0`: file plugins write to disk, not downstream.
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.lock().register_params(base)?;
        use asyn_rs::param::ParamType;
        // NDFileNexus.h:20-22.
        let path_idx = base.create_param("TEMPLATE_FILE_PATH", ParamType::Octet)?;
        let file_idx = base.create_param("TEMPLATE_FILE_NAME", ParamType::Octet)?;
        let valid_idx = base.create_param("TEMPLATE_FILE_VALID", ParamType::Int32)?;
        // C++ NDFileNexus.cpp:883 seeds NDFileNexusTemplateValid = 0.
        base.set_int32_param(valid_idx, 0, 0)?;
        self.template_path_idx = Some(path_idx);
        self.template_file_idx = Some(file_idx);
        self.template_valid_idx = Some(valid_idx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NDPluginDriver's FilePath refusal fails the write under this
    /// plugin's errorMessage.
    #[test]
    fn a_refused_file_path_write_fails() {
        let (message, function, value) =
            crate::test_util::refused_file_path_write("NEXUS_PATH", NexusFileProcessor::new());
        assert_eq!(
            message,
            format!("NDFileNexus:writeOctet: status=3, function={function}, value={value}")
        );
    }
    use ad_core_rs::ndarray::{NDDataViewMut, NDDimension};

    fn temp_path(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "adcore_test_{}_{}_{}.nxs",
            std::process::id(),
            prefix,
            n
        ))
    }

    /// C `openFile` refuses NDFileModeRead (NDFileNexus.cpp:45), so ReadFile
    /// fails at the open.
    #[test]
    fn read_file_is_refused_at_the_open() {
        let path = temp_path("nexus_read_refused");
        let mut writer = NexusWriter::new();
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let mut proc = NexusFileProcessor::new();
        let (out, _base) = crate::test_util::read_file(&mut proc, &path);
        assert_eq!(
            out.trace,
            crate::test_util::read_refused_lines(&path, &_base)
        );
        assert_eq!(out.publish, ad_core_rs::plugin::runtime::Published::Nothing);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_nexus_write_read() {
        let path = temp_path("nexus_basic");
        let mut writer = NexusWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        // Verify NeXus structure
        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 16);
        assert_eq!(data[0], 0);
        assert_eq!(data[15], 15);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_nexus_multiple_frames() {
        let path = temp_path("nexus_multi");
        let mut writer = NexusWriter::new();

        let mut arr1 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr1.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        let mut arr2 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr2.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = (i + 100) as u8;
            }
        }

        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr1,
            )
            .unwrap();
        writer.write_file(&Arc::new(arr1)).unwrap();
        writer.write_file(&Arc::new(arr2)).unwrap();
        writer.close_file().unwrap();

        assert_eq!(writer.frame_count(), 2);

        // Verify single dataset with leading frame dimension [2, 4, 4]
        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("entry/instrument/detector/data").unwrap();
        let shape = ds.shape();
        assert_eq!(shape, vec![2, 4, 4]);

        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 32);
        // First frame
        assert_eq!(data[0], 0);
        assert_eq!(data[15], 15);
        // Second frame
        assert_eq!(data[16], 100);
        assert_eq!(data[31], 115);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_per_frame_metadata_are_datasets_not_attributes() {
        let path = temp_path("nexus_meta");
        let mut writer = NexusWriter::new();

        let mut a1 = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        a1.unique_id = 10;
        a1.time_stamp = 1.5;
        let mut a2 = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        a2.unique_id = 11;
        a2.time_stamp = 2.5;

        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a1)
            .unwrap();
        writer.write_file(&Arc::new(a1)).unwrap();
        writer.write_file(&Arc::new(a2)).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        // uniqueId / timeStamp are proper datasets with a leading frame dim,
        // not N numbered attributes on the data dataset.
        let uid = h5file
            .dataset("entry/instrument/detector/uniqueId")
            .unwrap();
        assert_eq!(uid.shape(), vec![2]);
        let uid_data: Vec<i32> = uid.read_raw().unwrap();
        assert_eq!(uid_data, vec![10, 11]);

        let ts = h5file
            .dataset("entry/instrument/detector/timeStamp")
            .unwrap();
        assert_eq!(ts.shape(), vec![2]);
        let ts_data: Vec<f64> = ts.read_raw().unwrap();
        assert_eq!(ts_data, vec![1.5, 2.5]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_xml_template_parser() {
        let xml = r#"<?xml version="1.0"?>
            <NXroot>
              <NXentry name="entry">
                <NXdata name="data">
                  <data type="pArray"/>
                </NXdata>
                <title>My Experiment</title>
              </NXentry>
            </NXroot>"#;
        let root = parse_nexus_template(xml).unwrap();
        assert_eq!(root.name, "NXroot");
        assert_eq!(root.children.len(), 1);
        let entry = &root.children[0];
        assert_eq!(entry.name, "NXentry");
        assert_eq!(entry.attr("name"), Some("entry"));
        assert_eq!(entry.children.len(), 2);
        assert_eq!(entry.children[1].name, "title");
        assert_eq!(entry.children[1].text, "My Experiment");
        let data = &entry.children[0].children[0];
        assert_eq!(data.attr("type"), Some("pArray"));
    }

    #[test]
    fn test_template_drives_file_structure() {
        // Template places the array dataset at a non-default path.
        let xml = r#"<NXroot>
              <NXentry name="scan">
                <NXdata name="measurement">
                  <frames type="pArray"/>
                  <title>const-title</title>
                </NXdata>
              </NXentry>
            </NXroot>"#;
        let mut writer = NexusWriter::new();
        assert!(writer.load_template(xml));
        assert!(writer.has_template());

        let path = temp_path("nexus_tmpl");
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for (i, x) in v.iter_mut().enumerate() {
                *x = i as u8;
            }
        }
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        // Array dataset lives at the template-specified path.
        let ds = h5file.dataset("scan/measurement/frames").unwrap();
        assert_eq!(ds.shape(), vec![1, 2, 3]);
        // Constant title dataset created from the template node text.
        let title = h5file.dataset("scan/measurement/title").unwrap();
        assert_eq!(title.read_strings().unwrap(), ["const-title"]);

        std::fs::remove_file(&path).ok();
    }

    /// C walks the template against `pFileAttributes`, the plugin's
    /// `getAttributes` set with the frame's list on top (NDFileNexus.cpp:
    /// 53-71, :322).
    #[test]
    fn template_nd_attr_nodes_read_the_plugin_attributes_under_the_frame() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        let template = r#"<NXroot>
              <NXentry name="entry">
                <NXdata name="data">
                  <frames type="pArray"/>
                  <model type="ND_ATTR" source="Model"/>
                  <make type="ND_ATTR" source="Manufacturer"/>
                </NXdata>
              </NXentry>
            </NXroot>"#;
        let proc = NexusFileProcessor::new();
        assert!(proc.ctrl.lock().writer.lock().load_template(template));
        let xml = r#"<Attributes>
            <Attribute name="Model" type="CONST" source="PluginModel"/>
            <Attribute name="Manufacturer" type="CONST" source="PluginMake"/>
        </Attributes>"#;
        let (handle, _rx, _jh) = with_attributes_file("NEXUS_ATTRS", proc, xml);
        let path = temp_path("plugin_attrs");
        auto_save_to(&handle, &path);
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes
            .add(ad_core_rs::attributes::NDAttribute::new_static(
                "Manufacturer",
                "",
                ad_core_rs::attributes::NDAttrSource::Driver,
                ad_core_rs::attributes::NDAttrValue::String("FrameMake".into()),
            ));
        publish(&handle, &arr);
        wait_saved(&handle, 1);
        let h5file = H5File::open(&path).unwrap();
        let text = |name: &str| {
            h5file
                .dataset(&format!("entry/data/{name}"))
                .unwrap()
                .read_strings()
                .unwrap()
        };
        assert_eq!(text("model"), ["PluginModel"]);
        assert_eq!(text("make"), ["FrameMake"]);
        std::fs::remove_file(&path).ok();
    }

    use ad_core_rs::attributes::NDAttribute;
    use rust_hdf5::format::messages::datatype::DatatypeMessage;

    fn attribute(name: &str, value: NDAttrValue) -> NDAttribute {
        NDAttribute::new_static(
            name,
            "",
            ad_core_rs::attributes::NDAttrSource::Driver,
            value,
        )
    }

    fn write_one_frame(template: &str, name: &str, attrs: Vec<NDAttribute>) -> PathBuf {
        let path = temp_path(name);
        let mut writer = NexusWriter::new();
        assert!(writer.load_template(template));
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        for a in attrs {
            arr.attributes.add(a);
        }
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        path
    }

    /// The data dataset carries the `pArray` node's `ND_ATTR` children, each
    /// in its attribute's own type, and no other NDArray attribute
    /// (NDFileNexus.cpp:269-283, :552-610).
    #[test]
    fn the_data_carries_only_the_template_attributes_in_their_own_types() {
        let template = r#"<NXroot><NXentry name="entry"><NXdata name="data">
              <frames type="pArray">
                <Attr name="make" type="ND_ATTR" source="Manufacturer"/>
                <Attr name="gain" type="ND_ATTR" source="Gain"/>
                <Attr name="count" type="ND_ATTR" source="Count"/>
              </frames>
            </NXdata></NXentry></NXroot>"#;
        let path = write_one_frame(
            template,
            "nexus_data_attrs",
            vec![
                attribute("Manufacturer", NDAttrValue::String("FrameMake".into())),
                attribute("Gain", NDAttrValue::Float64(2.5)),
                attribute("Count", NDAttrValue::Int32(7)),
                attribute("Extra", NDAttrValue::String("not in template".into())),
            ],
        );
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("entry/data/frames").unwrap();
        let mut names = ds.attr_names().unwrap();
        names.sort();
        assert_eq!(names, ["count", "gain", "make"]);
        assert_eq!(ds.attr("gain").unwrap().read_numeric::<f64>().unwrap(), 2.5);
        assert_eq!(ds.attr("count").unwrap().read_numeric::<i32>().unwrap(), 7);
        let make = ds.attr("make").unwrap();
        // NX_CHAR sized as getValueInfo reports a string: its length + 1.
        assert!(matches!(
            make.datatype().unwrap(),
            DatatypeMessage::FixedString { size: 10, .. }
        ));
        assert_eq!(make.read_string().unwrap(), "FrameMake");
        std::fs::remove_file(&path).ok();
    }

    /// An `Attr` element is an attribute of the group or dataset it sits in,
    /// never a dataset of its own (NDFileNexus.cpp:269).
    #[test]
    fn an_nd_attr_attr_node_types_the_attribute_of_its_owner() {
        let template = r#"<NXroot>
              <Attr name="origin" type="ND_ATTR" source="Count"/>
              <NXentry name="entry"><NXdata name="data">
                <Attr name="units" type="ND_ATTR" source="Count"/>
                <energy type="ND_ATTR" source="Gain">
                  <Attr name="scale" type="ND_ATTR" source="Gain"/>
                </energy>
                <frames type="pArray"/>
              </NXdata></NXentry>
            </NXroot>"#;
        let path = write_one_frame(
            template,
            "nexus_group_attrs",
            vec![
                attribute("Gain", NDAttrValue::Float64(2.5)),
                attribute("Count", NDAttrValue::Int32(7)),
            ],
        );
        let h5 = H5File::open(&path).unwrap();
        let group = h5
            .root_group()
            .group("entry")
            .unwrap()
            .group("data")
            .unwrap();
        let mut datasets = group.dataset_names().unwrap();
        datasets.sort();
        assert_eq!(datasets, ["energy", "frames", "timeStamp", "uniqueId"]);
        assert_eq!(
            group.attr("units").unwrap().read_numeric::<i32>().unwrap(),
            7
        );
        let energy = h5.dataset("entry/data/energy").unwrap();
        assert_eq!(
            energy.attr("scale").unwrap().read_numeric::<f64>().unwrap(),
            2.5
        );
        assert_eq!(
            h5.root_group()
                .attr("origin")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            7
        );
        std::fs::remove_file(&path).ok();
    }

    /// A `CONST` node is written in its `outtype`, NX_CHAR when it has none,
    /// the text read as `sscanf` reads it; an outtype NeXus does not name
    /// writes nothing (NDFileNexus.cpp:293-310, :427-447, :670-768).
    #[test]
    fn const_nodes_are_written_in_their_outtype() {
        let template = r#"<NXroot><NXentry name="entry"><NXdata name="data">
              <Attr name="ratio" type="CONST" outtype="NX_FLOAT64">2.5</Attr>
              <Attr name="offset" type="CONST" outtype="NX_INT16">-5 mm</Attr>
              <Attr name="wrapped" type="CONST" outtype="NX_UINT8">300</Attr>
              <Attr name="signal" type="CONST">1</Attr>
              <Attr name="blank" type="CONST" outtype="NX_CHAR"></Attr>
              <Attr name="bogus" type="CONST" outtype="NX_COMPLEX">1</Attr>
              <count type="CONST" outtype="NX_INT32">12 counts</count>
              <pi type="CONST" outtype="NX_FLOAT32">3.1415</pi>
              <big type="CONST" outtype="NX_UINT64">-1</big>
              <bogus type="CONST" outtype="NX_COMPLEX">1</bogus>
              <frames type="pArray"/>
            </NXdata></NXentry></NXroot>"#;
        let path = write_one_frame(template, "nexus_const_outtype", vec![]);
        let h5 = H5File::open(&path).unwrap();
        let group = h5
            .root_group()
            .group("entry")
            .unwrap()
            .group("data")
            .unwrap();
        let mut attrs = group.attr_names().unwrap();
        attrs.sort();
        assert_eq!(
            attrs,
            ["NX_class", "blank", "offset", "ratio", "signal", "wrapped"]
        );
        let attr = |name: &str| group.attr(name).unwrap();
        assert_eq!(attr("ratio").read_numeric::<f64>().unwrap(), 2.5);
        assert_eq!(attr("offset").read_numeric::<i16>().unwrap(), -5);
        assert_eq!(attr("wrapped").read_numeric::<u8>().unwrap(), 44);
        // NX_CHAR is as long as its text; an empty one is HDF5's one byte.
        for (name, text) in [("signal", "1"), ("blank", "")] {
            assert!(matches!(
                attr(name).datatype().unwrap(),
                DatatypeMessage::FixedString { size: 1, .. }
            ));
            assert_eq!(attr(name).read_string().unwrap(), text);
        }

        let mut datasets = group.dataset_names().unwrap();
        datasets.sort();
        assert_eq!(
            datasets,
            ["big", "count", "frames", "pi", "timeStamp", "uniqueId"]
        );
        let one = |name: &str| {
            let ds = h5.dataset(&format!("entry/data/{name}")).unwrap();
            assert_eq!(ds.shape(), [1]);
            ds
        };
        assert_eq!(one("count").read_raw::<i32>().unwrap(), [12]);
        assert_eq!(one("pi").read_raw::<f32>().unwrap(), [3.1415f32]);
        assert_eq!(one("big").read_raw::<u64>().unwrap(), [u64::MAX]);
        std::fs::remove_file(&path).ok();
    }

    /// An NX_CHAR dataset holds one string as long as the length C gives
    /// NXmakedata: a `CONST` or untyped node's text, an `ND_ATTR` string
    /// with its terminator. An empty one is HDF5's one-byte string in an
    /// unlimited dimension, and an undefined `ND_ATTR` writes nothing
    /// (NDFileNexus.cpp:319-333, :427-475; napi5.c NX5compmakedata64).
    #[test]
    fn nx_char_datasets_hold_one_string_of_the_c_length() {
        let template = r#"<NXroot><NXentry name="entry"><NXdata name="data">
              <title type="CONST">const-title</title>
              <empty type="CONST" outtype="NX_CHAR"></empty>
              <note>free text</note>
              <make type="ND_ATTR" source="Manufacturer"/>
              <unset type="ND_ATTR" source="Unset"/>
              <frames type="pArray"/>
            </NXdata></NXentry></NXroot>"#;
        let path = write_one_frame(
            template,
            "nexus_nx_char",
            vec![
                attribute("Manufacturer", NDAttrValue::String("FrameMake".into())),
                attribute("Unset", NDAttrValue::Undefined),
            ],
        );
        let h5 = H5File::open(&path).unwrap();
        for (name, size, text) in [
            ("title", 11, "const-title"),
            ("empty", 1, ""),
            ("note", 9, "free text"),
            ("make", 10, "FrameMake"),
        ] {
            let ds = h5.dataset(&format!("entry/data/{name}")).unwrap();
            assert_eq!(ds.shape(), [1], "{name}");
            match ds.datatype().unwrap() {
                DatatypeMessage::FixedString { size: s, .. } => assert_eq!(s, size, "{name}"),
                other => panic!("{name} is {other:?}"),
            }
            assert_eq!(ds.read_strings().unwrap(), [text], "{name}");
        }
        assert_eq!(
            h5.dataset("entry/data/empty").unwrap().max_shape().unwrap(),
            [None]
        );
        assert!(h5.dataset("entry/data/unset").is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_load_template_invalid_xml_returns_false() {
        let mut writer = NexusWriter::new();
        assert!(!writer.load_template("<NXroot><unclosed>"));
        assert!(!writer.has_template());
    }

    #[test]
    fn a_template_write_stores_it_without_waiting_on_a_file_write() {
        use ad_core_rs::plugin::runtime::SideParam;
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};

        let dir = std::env::temp_dir();
        let tmpl_path = dir.join(format!(
            "adcore_nexus_template_test_{}.xml",
            std::process::id()
        ));
        std::fs::write(
            &tmpl_path,
            r#"<NXroot><NXentry name="e"><NXdata name="d"><x type="pArray"/></NXdata></NXentry></NXroot>"#,
        )
        .unwrap();

        let mut base = PortDriverBase::new(
            "nexus_tmpl_test",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let mut proc = NexusFileProcessor::new();
        proc.register_params(&mut base).unwrap();

        // The NDFileNexus.template records address these names.
        let file_idx = base.find_param("TEMPLATE_FILE_NAME").unwrap();
        let valid_idx = base.find_param("TEMPLATE_FILE_VALID").unwrap();
        assert_eq!(base.get_int32_param(valid_idx, 0).unwrap(), 0);

        let path = tmpl_path.to_string_lossy().into_owned();
        // A file write in progress does not hold the template write up; the
        // writer takes the template at its next open.
        let writer = Arc::clone(&proc.ctrl.lock().writer);
        let mut out = None;
        assert!(crate::test_util::returns_while_busy(&writer, || {
            out = Some(proc.validate_octet(
                file_idx,
                0,
                &path,
                ad_core_rs::plugin::runtime::WriteVerdict::Accept(path.clone()),
                &base,
            ));
        }));
        assert!(out.unwrap().side.iter().any(|u| matches!(
            u,
            SideParam::Int32 { reason, value: 1, .. } if *reason == valid_idx
        )));
        assert!(proc.template.get(Option::is_some));
        assert!(
            writer.lock().template.is_none(),
            "not read before the next open"
        );

        std::fs::remove_file(&tmpl_path).ok();

        // :825 returns on an empty file; a bad one prints and keeps the last
        // good one (:830-836, :847).
        let write = |v: &str| {
            proc.validate_octet(
                file_idx,
                0,
                v,
                ad_core_rs::plugin::runtime::WriteVerdict::Accept(v.to_owned()),
                &base,
            )
        };
        let out = write("");
        assert!(out.side.is_empty() && out.trace.is_empty());
        assert!(proc.template.get(Option::is_some));
        let out = write(&path);
        assert!(out.side.iter().any(|u| matches!(
            u,
            SideParam::Int32 { reason, value: 0, .. } if *reason == valid_idx
        )));
        assert_eq!(
            out.trace[0].1,
            format!("NDFileNexus:loadTemplateFile: Parameter file {path} is invalid\n")
        );
        assert!(proc.template.get(Option::is_some));
    }

    /// A template file that does not load leaves the last good one laying out
    /// the next file (NDFileNexus.cpp:71, :829-847).
    #[test]
    fn a_bad_template_file_keeps_the_last_good_layout() {
        use crate::test_util::{auto_save_to, publish, running, wait_saved};
        let good = temp_path("template_good").with_extension("xml");
        std::fs::write(
            &good,
            r#"<NXroot><NXentry name="e"><NXdata name="d"><x type="pArray"/></NXdata></NXentry></NXroot>"#,
        )
        .unwrap();
        let bad = temp_path("template_bad").with_extension("xml");
        std::fs::write(&bad, "<NXroot><unclosed>").unwrap();

        let (handle, _rx, _jh) = running("NEXUS_KEEP_TEMPLATE", NexusFileProcessor::new());
        let port = handle.port_runtime().port_handle();
        let reason = |name: &str| {
            port.drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new(name, 0))
                .unwrap()
                .reason
        };
        let load = |file: &Path| {
            port.submit_blocking(
                asyn_rs::request::RequestOp::OctetWrite {
                    data: file.to_str().unwrap().as_bytes().to_vec(),
                },
                asyn_rs::user::AsynUser::new(reason("TEMPLATE_FILE_NAME")),
            )
            .unwrap();
            assert!(handle.wait_params_applied(fact_budget::FACT_BUDGET));
            port.read_int32_blocking(reason("TEMPLATE_FILE_VALID"), 0)
                .unwrap()
        };
        assert_eq!(load(&good), 1);
        assert_eq!(load(&bad), 0);

        let path = temp_path("kept_template");
        auto_save_to(&handle, &path);
        publish(
            &handle,
            &NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8),
        );
        wait_saved(&handle, 1);
        let h5 = H5File::open(&path).unwrap();
        assert_eq!(h5.dataset("e/d/x").unwrap().shape(), vec![1, 4]);
        assert!(h5.dataset("entry/instrument/detector/data").is_err());
        for f in [&path, &good, &bad] {
            std::fs::remove_file(f).ok();
        }
    }

    #[test]
    fn test_nxdata_group_contains_image_data() {
        // N2: the built-in NXdata group `/entry/data` must actually contain
        // the image dataset, not be an empty group.
        let path = temp_path("nexus_nxdata");
        let mut writer = NexusWriter::new();

        let mk = |base: u8| {
            let mut arr = NDArray::new(
                vec![NDDimension::new(3), NDDimension::new(2)],
                NDDataType::UInt8,
            );
            if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
                for (i, x) in v.iter_mut().enumerate() {
                    *x = base + i as u8;
                }
            }
            arr
        };

        let a0 = mk(0);
        writer
            .open_file(&path, NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE, &a0)
            .unwrap();
        writer.write_file(&Arc::new(a0)).unwrap();
        writer.write_file(&Arc::new(mk(100))).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // The NXdata group dataset exists and carries the data.
        let nx = h5
            .dataset("entry/data/data")
            .expect("NXdata group must contain the `data` dataset");
        assert_eq!(nx.shape(), vec![2, 2, 3]);
        let nx_vals: Vec<u8> = nx.read_raw().unwrap();
        assert_eq!(nx_vals.len(), 2 * 6);
        assert_eq!(nx_vals[0], 0);
        assert_eq!(nx_vals[6], 100);
        // It mirrors the detector dataset content.
        let det: Vec<u8> = h5
            .dataset("entry/instrument/detector/data")
            .unwrap()
            .read_raw()
            .unwrap();
        assert_eq!(nx_vals, det);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_nx_class_is_true_group_attribute() {
        // NeXus requires NX_class to be an HDF5 group attribute, not a child
        // dataset. rust-hdf5 0.2.15 supports group attributes on every group.
        let path = temp_path("nexus_nxclass");
        let mut writer = NexusWriter::new();
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            v.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8);
        }
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // Non-root groups carry NX_class as a real group attribute.
        for (group, class) in [
            ("entry", "NXentry"),
            ("entry/instrument", "NXinstrument"),
            ("entry/instrument/detector", "NXdetector"),
            ("entry/data", "NXdata"),
        ] {
            let g = h5.root_group().group(group).expect("group exists");
            let got = g
                .attr_string("NX_class")
                .unwrap_or_else(|_| panic!("{} must have NX_class group attribute", group));
            assert_eq!(got, class, "{} NX_class", group);
        }
        // No child `NX_class` dataset (the old workaround) anywhere.
        assert!(h5.dataset("entry/NX_class").is_err());

        std::fs::remove_file(&path).ok();
    }

    /// C records no NDArray data type attribute on the data (NDFileNexus.cpp
    /// writes only the template's attributes).
    #[test]
    fn the_data_carries_no_data_type_attribute() {
        let path = temp_path("nexus_no_dtype");
        let mut writer = NexusWriter::new();
        let arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::Int16,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        let data_path = format!("{}/{}", writer.data_group_path, writer.data_node_name);
        writer.close_file().unwrap();
        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset(&data_path).unwrap().attr_names().unwrap();
        assert!(!names.iter().any(|n| n == "NDArrayDataType"), "{names:?}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn every_data_type_is_stored_as_its_native_type() {
        // N4: every NDArray data type must round-trip, not just u8/u16/f64.
        use ad_core_rs::ndarray::NDDataType::*;
        for dt in [
            Int8, UInt8, Int16, UInt16, Int32, UInt32, Int64, UInt64, Float32, Float64,
        ] {
            let path = temp_path(&format!("nexus_type_{:?}", dt));
            let mut writer = NexusWriter::new();
            let mut arr = NDArray::new(vec![NDDimension::new(2), NDDimension::new(2)], dt);
            // Distinct, type-revealing values: negatives for signed types.
            match arr.data_mut().view_mut() {
                NDDataViewMut::I8(v) => v.copy_from_slice(&[-1, 2, -3, 4]),
                NDDataViewMut::U8(v) => v.copy_from_slice(&[1, 2, 3, 4]),
                NDDataViewMut::I16(v) => v.copy_from_slice(&[-1, 2, -3, 4]),
                NDDataViewMut::U16(v) => v.copy_from_slice(&[1, 2, 3, 4]),
                NDDataViewMut::I32(v) => v.copy_from_slice(&[-1, 2, -3, 4]),
                NDDataViewMut::U32(v) => v.copy_from_slice(&[1, 2, 3, 4]),
                NDDataViewMut::I64(v) => v.copy_from_slice(&[-1, 2, -3, 4]),
                NDDataViewMut::U64(v) => v.copy_from_slice(&[1, 2, 3, 4]),
                NDDataViewMut::F32(v) => v.copy_from_slice(&[-1.5, 2.5, -3.5, 4.5]),
                NDDataViewMut::F64(v) => v.copy_from_slice(&[-1.5, 2.5, -3.5, 4.5]),
            }

            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .unwrap();
            writer.write_file(&Arc::new(arr.clone())).unwrap();
            let data_path = format!("{}/{}", writer.data_group_path, writer.data_node_name);
            writer.close_file().unwrap();

            let h5 = H5File::open(&path).unwrap();
            let read = crate::test_util::read_native(&h5.dataset(&data_path).unwrap(), None);
            assert_eq!(read.data_type(), dt, "{:?}: type must round-trip", dt);
            // Leading frame dim [1] + 2x2 => 3 dims, 4 elements.
            assert_eq!(read.len(), 4, "{:?}: element count", dt);
            for i in 0..4 {
                assert_eq!(
                    arr.data().get_as_f64(i),
                    read.get_as_f64(i),
                    "{:?}: element {} must round-trip",
                    dt,
                    i
                );
            }

            std::fs::remove_file(&path).ok();
        }
    }
}
