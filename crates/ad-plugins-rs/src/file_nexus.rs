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

use std::path::{Path, PathBuf};

use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, PluginParamSnapshot, ProcessResult,
};

use rust_hdf5::{H5Dataset, H5File};

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
            while *pos < chars.len() && !(chars[*pos] == '?' && chars.get(*pos + 1) == Some(&'>'))
            {
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
        && !(chars[*pos] == '-' && chars.get(*pos + 1) == Some(&'-') && chars.get(*pos + 2) == Some(&'>'))
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
    NEXUS_GROUP_CLASSES.contains(&el.name.as_str())
        || el.attr("type") == Some("UserGroup")
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
    /// Parsed NeXus XML template tree, if a valid template was loaded.
    template: Option<XmlElement>,
    /// HDF5 path of the group that contains the main array dataset (template
    /// mode); the dataset itself is named by the template's `pArray` node.
    data_group_path: String,
    data_node_name: String,
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
            template: None,
            data_group_path: "entry/instrument/detector".to_string(),
            data_node_name: "data".to_string(),
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Whether a NeXus XML template is currently loaded.
    pub fn has_template(&self) -> bool {
        self.template.is_some()
    }

    /// Load (parse and validate) a NeXus XML template. On success the template
    /// drives the file structure; on parse failure the template is cleared and
    /// the writer falls back to its built-in NXentry/NXdata hierarchy. Returns
    /// whether the template parsed successfully (C++ `NDFileNexusTemplateValid`).
    pub fn load_template(&mut self, xml: &str) -> bool {
        match parse_nexus_template(xml) {
            Ok(root) => {
                self.template = Some(root);
                true
            }
            Err(_) => {
                self.template = None;
                false
            }
        }
    }

    /// Clear any loaded template (revert to the built-in hierarchy).
    pub fn clear_template(&mut self) {
        self.template = None;
    }

    /// Write the NeXus `NX_class` group marker.
    ///
    /// CRATE LIMITATION: real NeXus requires `NX_class` to be an HDF5 *group
    /// attribute*. `rust-hdf5` 0.2.12 only supports attributes on the root
    /// group (`H5File::set_attr_string`) and on datasets — its `GroupInfo`
    /// struct (writer.rs) has no `attributes` field, so a non-root group
    /// cannot carry an attribute through this crate at all.
    ///
    /// Closest correct alternative used here:
    /// - For the **root** group the class name IS written as a true group
    ///   attribute (`apply_root_nx_class`), which NeXus readers honour.
    /// - For **non-root** groups (`entry`, `instrument`, ...) the class name
    ///   is written as a child string dataset `NX_class` carrying a `value`
    ///   string attribute. This is NOT NeXus-spec-compliant; a NeXus-aware
    ///   reader will not recognise the group class. It is the best a
    ///   pure-Rust HDF5 writer with no group-attribute support can do.
    ///   Fixing it properly requires either a `rust-hdf5` release that adds
    ///   group attributes, or switching to the C `hdf5`/`libNeXus` bindings.
    fn write_nx_class(group: &rust_hdf5::H5Group, class_name: &str) -> ADResult<()> {
        let ds = group
            .new_dataset::<u8>()
            .shape([1usize])
            .create("NX_class")
            .map_err(|e| {
                ADError::UnsupportedConversion(format!("NX_class dataset error: {}", e))
            })?;
        ds.write_raw(&[0u8])
            .map_err(|e| ADError::UnsupportedConversion(format!("NX_class write error: {}", e)))?;
        let attr = ds
            .new_attr::<rust_hdf5::types::VarLenUnicode>()
            .shape(())
            .create("value")
            .map_err(|e| ADError::UnsupportedConversion(format!("NX_class attr error: {}", e)))?;
        attr.write_string(class_name).map_err(|e| {
            ADError::UnsupportedConversion(format!("NX_class attr write error: {}", e))
        })?;
        Ok(())
    }

    /// Write `NX_class` as a true HDF5 attribute on the root group — the one
    /// group `rust-hdf5` 0.2.12 supports attributes on.
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
        array: &NDArray,
    ) -> ADResult<(String, String)> {
        let mut data_group = String::new();
        let mut data_node = String::new();
        // The root template node is typically NXroot — iterate its children.
        let top_children: &[XmlElement] = if template.name == "NXroot" {
            &template.children
        } else {
            std::slice::from_ref(template)
        };
        for child in top_children {
            Self::process_node(
                h5file,
                None,
                "",
                child,
                array,
                &mut data_group,
                &mut data_node,
            )?;
        }
        if data_node.is_empty() {
            Ok((
                "entry/instrument/detector".to_string(),
                "data".to_string(),
            ))
        } else {
            Ok((data_group, data_node))
        }
    }

    /// Recursively process one template node. `parent` is the HDF5 group to
    /// create children in (None = file root); `parent_path` is its HDF5 path.
    fn process_node(
        h5file: &H5File,
        parent: Option<&rust_hdf5::H5Group>,
        parent_path: &str,
        node: &XmlElement,
        array: &NDArray,
        data_group: &mut String,
        data_node: &mut String,
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
                Self::process_node(
                    h5file,
                    Some(&group),
                    &child_path,
                    child,
                    array,
                    data_group,
                    data_node,
                )?;
            }
            return Ok(());
        }

        // Non-group node.
        match node_type.as_deref() {
            Some("pArray") => {
                // The main NDArray dataset is created lazily in write_file;
                // here we only record where it goes.
                *data_group = parent_path.to_string();
                *data_node = node.name.clone();
            }
            Some("CONST") => {
                // Constant dataset: string text written once.
                if let Some(parent) = parent {
                    Self::write_const_dataset(parent, &node.name, &node.text)?;
                }
            }
            Some("ND_ATTR") => {
                // Dataset sourced from an NDAttribute, written once.
                if let Some(parent) = parent {
                    let source = node.attr("source").unwrap_or(&node.name);
                    if let Some(attr) = array.attributes.get(source) {
                        Self::write_attr_dataset(parent, &node.name, &attr.value)?;
                    }
                }
            }
            Some("Attr") | None if node.name == "Attr" => {
                // Group/dataset attribute node — see write_nx_class crate
                // limitation: rust-hdf5 cannot attach attributes to non-root
                // groups. Emit it as a child string dataset instead.
                if let Some(parent) = parent {
                    let attr_name = node.attr("name").unwrap_or(&node.name);
                    let value = if node.attr("type") == Some("ND_ATTR") {
                        node.attr("source")
                            .and_then(|s| array.attributes.get(s))
                            .map(|a| a.value.as_string())
                            .unwrap_or_default()
                    } else {
                        node.text.clone()
                    };
                    Self::write_const_dataset(parent, attr_name, &value)?;
                }
            }
            _ => {
                // Untyped leaf → constant char dataset of its text content.
                if let Some(parent) = parent {
                    let text = if node.text.is_empty() {
                        "LEFT BLANK"
                    } else {
                        &node.text
                    };
                    Self::write_const_dataset(parent, &node.name, text)?;
                }
            }
        }
        Ok(())
    }

    /// Write a constant string dataset (NeXus CONST node, NX_CHAR).
    fn write_const_dataset(
        group: &rust_hdf5::H5Group,
        name: &str,
        text: &str,
    ) -> ADResult<()> {
        let bytes = text.as_bytes();
        let len = bytes.len().max(1);
        let ds = group
            .new_dataset::<u8>()
            .shape([len])
            .create(name)
            .map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus const dataset '{}': {}", name, e))
            })?;
        let mut buf = bytes.to_vec();
        if buf.is_empty() {
            buf.push(0);
        }
        ds.write_raw(&buf).map_err(|e| {
            ADError::UnsupportedConversion(format!("NeXus const write '{}': {}", name, e))
        })?;
        Ok(())
    }

    /// Write an NDAttribute as a typed scalar dataset (NeXus ND_ATTR node).
    /// The numeric NDAttrValue type is preserved, not stringified.
    fn write_attr_dataset(
        group: &rust_hdf5::H5Group,
        name: &str,
        value: &ad_core_rs::attributes::NDAttrValue,
    ) -> ADResult<()> {
        use ad_core_rs::attributes::NDAttrValue;
        macro_rules! scalar {
            ($t:ty, $v:expr) => {{
                let ds = group
                    .new_dataset::<$t>()
                    .shape([1usize])
                    .create(name)
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!(
                            "NeXus attr dataset '{}': {}",
                            name, e
                        ))
                    })?;
                ds.write_raw(&[$v]).map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus attr write '{}': {}", name, e))
                })?;
            }};
        }
        match value {
            NDAttrValue::Int8(v) => scalar!(i8, *v),
            NDAttrValue::UInt8(v) => scalar!(u8, *v),
            NDAttrValue::Int16(v) => scalar!(i16, *v),
            NDAttrValue::UInt16(v) => scalar!(u16, *v),
            NDAttrValue::Int32(v) => scalar!(i32, *v),
            NDAttrValue::UInt32(v) => scalar!(u32, *v),
            NDAttrValue::Int64(v) => scalar!(i64, *v),
            NDAttrValue::UInt64(v) => scalar!(u64, *v),
            NDAttrValue::Float32(v) => scalar!(f32, *v),
            NDAttrValue::Float64(v) => scalar!(f64, *v),
            NDAttrValue::String(s) => Self::write_const_dataset(group, name, s)?,
            NDAttrValue::Undefined => Self::write_const_dataset(group, name, "")?,
        }
        Ok(())
    }
}

impl Default for NexusWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl NDFileWriter for NexusWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        self.frame_count = 0;
        self.dataset = None;
        self.uid_dataset = None;
        self.ts_dataset = None;

        let h5file = H5File::create(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("NeXus create error: {}", e)))?;

        // Root-group NX_class is a true HDF5 attribute (the one group
        // rust-hdf5 0.2.12 supports attributes on).
        Self::apply_root_nx_class(&h5file, "NXroot");

        if let Some(template) = self.template.clone() {
            // Template mode: build the group/dataset tree from the user's
            // NeXus XML template (C++ NDFileNexus::loadTemplateFile).
            let (data_group, data_node) =
                Self::process_template(&h5file, &template, array)?;
            self.data_group_path = data_group;
            self.data_node_name = data_node;
        } else {
            // Built-in NXentry/NXdata hierarchy. NX_class on non-root groups
            // uses the documented child-dataset fallback (see write_nx_class).
            let entry = h5file.create_group("entry").map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus group error: {}", e))
            })?;
            Self::write_nx_class(&entry, "NXentry")?;
            let instrument = entry.create_group("instrument").map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus group error: {}", e))
            })?;
            Self::write_nx_class(&instrument, "NXinstrument")?;
            let detector = instrument.create_group("detector").map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus group error: {}", e))
            })?;
            Self::write_nx_class(&detector, "NXdetector")?;
            let data_group = entry.create_group("data").map_err(|e| {
                ADError::UnsupportedConversion(format!("NeXus group error: {}", e))
            })?;
            Self::write_nx_class(&data_group, "NXdata")?;
            self.data_group_path = "entry/instrument/detector".to_string();
            self.data_node_name = "data".to_string();
        }

        self.file = Some(h5file);
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let h5file = self
            .file
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no NeXus file open".into()))?;

        let frame_shape = array.dims.iter().rev().map(|d| d.size).collect::<Vec<_>>();
        let data_node_name = self.data_node_name.clone();

        // Resolve the data group from its HDF5 path.
        let data_group = {
            let mut group = h5file.root_group();
            for component in self.data_group_path.split('/') {
                if component.is_empty() {
                    continue;
                }
                group = group
                    .group(component)
                    .map_err(|e| ADError::UnsupportedConversion(e.to_string()))?;
            }
            group
        };

        if self.frame_count == 0 {
            // First frame: create a chunked dataset with leading frame dimension.
            // Shape: [1, dim0, dim1, ...], chunk: [1, dim0, dim1, ...]
            let mut ds_shape = vec![1usize];
            ds_shape.extend_from_slice(&frame_shape);
            let chunk_dims = ds_shape.clone();

            macro_rules! create_chunked {
                ($t:ty, $v:expr) => {{
                    let ds = data_group
                        .new_dataset::<$t>()
                        .shape(&ds_shape[..])
                        .chunk(&chunk_dims[..])
                        .resizable()
                        .create(&data_node_name)
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!("NeXus dataset error: {}", e))
                        })?;
                    let raw = unsafe {
                        std::slice::from_raw_parts(
                            $v.as_ptr() as *const u8,
                            $v.len() * std::mem::size_of::<$t>(),
                        )
                    };
                    ds.write_chunk(0, raw).map_err(|e| {
                        ADError::UnsupportedConversion(format!("NeXus write error: {}", e))
                    })?;
                    // Write NDArray attributes on the first frame
                    for attr in array.attributes.iter() {
                        let val_str = attr.value.as_string();
                        let _ = ds
                            .new_attr::<rust_hdf5::types::VarLenUnicode>()
                            .shape(())
                            .create(attr.name.as_str())
                            .and_then(|a| {
                                let s: rust_hdf5::types::VarLenUnicode =
                                    val_str.parse().unwrap_or_default();
                                a.write_scalar(&s)
                            });
                    }
                    ds
                }};
            }

            let ds = match &array.data {
                NDDataBuffer::U8(v) => create_chunked!(u8, v),
                NDDataBuffer::U16(v) => create_chunked!(u16, v),
                NDDataBuffer::I16(v) => create_chunked!(i16, v),
                NDDataBuffer::I32(v) => create_chunked!(i32, v),
                NDDataBuffer::U32(v) => create_chunked!(u32, v),
                NDDataBuffer::F32(v) => create_chunked!(f32, v),
                NDDataBuffer::F64(v) => create_chunked!(f64, v),
                _ => {
                    let raw = array.data.as_u8_slice();
                    let ds = data_group
                        .new_dataset::<u8>()
                        .shape(&ds_shape[..])
                        .chunk(&chunk_dims[..])
                        .resizable()
                        .create(&data_node_name)
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!("NeXus dataset error: {}", e))
                        })?;
                    ds.write_chunk(0, raw).map_err(|e| {
                        ADError::UnsupportedConversion(format!("NeXus write error: {}", e))
                    })?;
                    ds
                }
            };
            self.dataset = Some(ds);

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
            uid.write_chunk(0, &array.unique_id.to_ne_bytes())
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
            ts.write_chunk(0, &array.time_stamp.to_ne_bytes())
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

            let raw = array.data.as_u8_slice();
            ds.write_chunk(self.frame_count, raw)
                .map_err(|e| ADError::UnsupportedConversion(format!("NeXus write error: {}", e)))?;

            // Extend the per-frame metadata datasets and append this frame.
            if let Some(uid) = self.uid_dataset.as_ref() {
                uid.extend(&[new_frame_count]).map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus uniqueId extend: {}", e))
                })?;
                uid.write_chunk(self.frame_count, &array.unique_id.to_ne_bytes())
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!("NeXus uniqueId write: {}", e))
                    })?;
            }
            if let Some(ts) = self.ts_dataset.as_ref() {
                ts.extend(&[new_frame_count]).map_err(|e| {
                    ADError::UnsupportedConversion(format!("NeXus timeStamp extend: {}", e))
                })?;
                ts.write_chunk(self.frame_count, &array.time_stamp.to_ne_bytes())
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!("NeXus timeStamp write: {}", e))
                    })?;
            }
        }

        self.frame_count += 1;
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let h5file = H5File::open(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("NeXus open error: {}", e)))?;

        // Read from the data path established at open_file time (the built-in
        // entry/instrument/detector/data, or the template's pArray location).
        let data_path = format!("{}/{}", self.data_group_path, self.data_node_name);
        let ds = h5file
            .dataset(&data_path)
            .map_err(|e| ADError::UnsupportedConversion(format!("NeXus dataset error: {}", e)))?;

        let shape = ds.shape();
        let dims: Vec<NDDimension> = shape.iter().rev().map(|&s| NDDimension::new(s)).collect();

        if let Ok(data) = ds.read_raw::<u8>() {
            let mut arr = NDArray::new(dims, NDDataType::UInt8);
            arr.data = NDDataBuffer::U8(data);
            return Ok(arr);
        }
        if let Ok(data) = ds.read_raw::<u16>() {
            let mut arr = NDArray::new(dims, NDDataType::UInt16);
            arr.data = NDDataBuffer::U16(data);
            return Ok(arr);
        }
        if let Ok(data) = ds.read_raw::<f64>() {
            let mut arr = NDArray::new(dims, NDDataType::Float64);
            arr.data = NDDataBuffer::F64(data);
            return Ok(arr);
        }

        Err(ADError::UnsupportedConversion(
            "unsupported data type in NeXus file".into(),
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

    fn supports_multiple_arrays(&self) -> bool {
        true
    }
}

// ============================================================
// Processor
// ============================================================

pub struct NexusFileProcessor {
    ctrl: FilePluginController<NexusWriter>,
    template_path_idx: Option<usize>,
    template_file_idx: Option<usize>,
    template_valid_idx: Option<usize>,
    template_path: String,
    template_file: String,
}

impl NexusFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: FilePluginController::new(NexusWriter::new()),
            template_path_idx: None,
            template_file_idx: None,
            template_valid_idx: None,
            template_path: String::new(),
            template_file: String::new(),
        }
    }

    /// Load the NeXus XML template from `template_path + template_file`
    /// (C++ NDFileNexus::loadTemplateFile). Returns the validity flag value
    /// for `NEXUS_TEMPLATE_VALID`: 1 if a template was loaded and parsed,
    /// 0 if the file is unset, missing, or fails to parse.
    fn reload_template(&mut self) -> i32 {
        if self.template_file.is_empty() {
            self.ctrl.writer.clear_template();
            return 0;
        }
        let full = format!("{}{}", self.template_path, self.template_file);
        match std::fs::read_to_string(&full) {
            Ok(xml) => {
                if self.ctrl.writer.load_template(&xml) {
                    1
                } else {
                    0
                }
            }
            Err(_) => {
                self.ctrl.writer.clear_template();
                0
            }
        }
    }
}

impl Default for NexusFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for NexusFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileNexus"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.register_params(base)?;
        use asyn_rs::param::ParamType;
        let path_idx = base.create_param("NEXUS_TEMPLATE_PATH", ParamType::Octet)?;
        let file_idx = base.create_param("NEXUS_TEMPLATE_FILE", ParamType::Octet)?;
        let valid_idx = base.create_param("NEXUS_TEMPLATE_VALID", ParamType::Int32)?;
        base.create_param("TEMPLATE_FILE_PATH", ParamType::Octet)?;
        base.create_param("TEMPLATE_FILE_NAME", ParamType::Octet)?;
        base.create_param("TEMPLATE_FILE_VALID", ParamType::Int32)?;
        // C++ NDFileNexus.cpp:883 seeds NDFileNexusTemplateValid = 0.
        base.set_int32_param(valid_idx, 0, 0)?;
        self.template_path_idx = Some(path_idx);
        self.template_file_idx = Some(file_idx);
        self.template_valid_idx = Some(valid_idx);
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        use ad_core_rs::plugin::runtime::ParamChangeValue;

        // NeXus XML template path / file changes trigger a template reload
        // (C++ NDFileNexus::writeInt32 / writeOctet → loadTemplateFile).
        if Some(reason) == self.template_path_idx {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.template_path = s.clone();
            }
            let valid = self.reload_template();
            return self.template_valid_result(valid);
        }
        if Some(reason) == self.template_file_idx {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.template_file = s.clone();
            }
            let valid = self.reload_template();
            return self.template_valid_result(valid);
        }
        self.ctrl.on_param_change(reason, params)
    }
}

impl NexusFileProcessor {
    /// Build a ParamChangeResult that updates `NEXUS_TEMPLATE_VALID`.
    fn template_valid_result(&self, valid: i32) -> ParamChangeResult {
        use ad_core_rs::plugin::runtime::ParamUpdate;
        match self.template_valid_idx {
            Some(idx) => ParamChangeResult::updates(vec![ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: valid,
            }]),
            None => ParamChangeResult::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("adcore_test_{}_{}.nxs", prefix, n))
    }

    #[test]
    fn test_nexus_write_read() {
        let path = temp_path("nexus_basic");
        let mut writer = NexusWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
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
        if let NDDataBuffer::U8(ref mut v) = arr1.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        let mut arr2 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr2.data {
            for i in 0..16 {
                v[i] = (i + 100) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Stream, &arr1).unwrap();
        writer.write_file(&arr1).unwrap();
        writer.write_file(&arr2).unwrap();
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

        writer.open_file(&path, NDFileMode::Stream, &a1).unwrap();
        writer.write_file(&a1).unwrap();
        writer.write_file(&a2).unwrap();
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
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = i as u8;
            }
        }
        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        // Array dataset lives at the template-specified path.
        let ds = h5file.dataset("scan/measurement/frames").unwrap();
        assert_eq!(ds.shape(), vec![1, 2, 3]);
        // Constant title dataset created from the template node text.
        let title = h5file.dataset("scan/measurement/title").unwrap();
        let title_bytes: Vec<u8> = title.read_raw().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&title_bytes).trim_end_matches('\0'),
            "const-title"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_load_template_invalid_xml_returns_false() {
        let mut writer = NexusWriter::new();
        assert!(!writer.load_template("<NXroot><unclosed>"));
        assert!(!writer.has_template());
    }

    #[test]
    fn test_template_param_change_sets_valid_flag() {
        use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};
        use asyn_rs::port::{PortDriverBase, PortFlags};

        let dir = std::env::temp_dir();
        let tmpl_path = dir.join("adcore_nexus_template_test.xml");
        std::fs::write(
            &tmpl_path,
            r#"<NXroot><NXentry name="e"><NXdata name="d"><x type="pArray"/></NXdata></NXentry></NXroot>"#,
        )
        .unwrap();

        let mut base = PortDriverBase::new("nexus_tmpl_test", 1, PortFlags::default());
        let mut proc = NexusFileProcessor::new();
        proc.register_params(&mut base).unwrap();

        let file_idx = proc.template_file_idx.unwrap();
        let valid_idx = proc.template_valid_idx.unwrap();

        let result = proc.on_param_change(
            file_idx,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: file_idx,
                addr: 0,
                value: ParamChangeValue::Octet(tmpl_path.to_string_lossy().into_owned()),
            },
        );
        assert!(result.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 { reason, value: 1, .. } if *reason == valid_idx
        )));
        assert!(proc.ctrl.writer.has_template());

        std::fs::remove_file(&tmpl_path).ok();
    }
}
