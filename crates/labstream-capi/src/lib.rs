//! The C ABI of LSL.
//!
//! Every other crate in this workspace forbids raw pointers. This one is where
//! they live, because a C application reaches the library through this surface
//! and no other.
//!
//! # What this has to satisfy
//!
//! An application binds these symbols by name at load time. `pylsl` sets an
//! argument list for all 85 symbols it uses while it imports, so a missing
//! symbol stops the import before any call happens. Every name it needs is
//! therefore present here, and a function that is not implemented yet says so
//! through the error code that the header already defines.
//!
//! # Ownership
//!
//! Each handle is a `Box` that crossed the boundary through `Box::into_raw`.
//! The matching destroy call takes it back with `Box::from_raw`. A caller that
//! never destroys leaks, which matches liblsl.

#![allow(non_camel_case_types)]
#![deny(missing_docs)]

use labstream_net::{desc, ContinuousResolver, Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, PackedSample, Sample, Value};
use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// error codes and constants, from include/lsl/common.h
// ---------------------------------------------------------------------------

/// No error.
pub const LSL_NO_ERROR: i32 = 0;
/// The operation ran out of time.
pub const LSL_TIMEOUT_ERROR: i32 = -1;
/// The stream is gone.
pub const LSL_LOST_ERROR: i32 = -2;
/// An argument was wrong.
pub const LSL_ARGUMENT_ERROR: i32 = -3;
/// Anything else.
pub const LSL_INTERNAL_ERROR: i32 = -4;

/// The value that means "no fixed rate".
pub const LSL_IRREGULAR_RATE: f64 = 0.0;
/// The value that asks the reader to derive a timestamp.
pub const LSL_DEDUCED_TIMESTAMP: f64 = -1.0;

const LSL_PROTOCOL_VERSION: i32 = 110;
/// `src/common.h:49`. The value is major times 100 plus minor, so 1.17
/// reads as 117. It is not the patch level.
const LSL_LIBRARY_VERSION: i32 = 117;

// ---------------------------------------------------------------------------
// handles
// ---------------------------------------------------------------------------

/// The stream description handle.
pub type lsl_streaminfo = *mut c_void;
/// The outlet handle.
pub type lsl_outlet = *mut c_void;
/// The inlet handle.
pub type lsl_inlet = *mut c_void;
/// A node of the description tree.
pub type lsl_xml_ptr = *mut c_void;
/// The background resolver handle.
pub type lsl_continuous_resolver = *mut c_void;

struct InfoBox {
    info: StreamInfo,
    /// The whole document, in the shape that the C ABI needs.
    ///
    /// liblsl hands out a pointer into one pugixml document that holds every
    /// field as well as the tree, so `lsl_parent` on the description reaches
    /// `<info>` and then the document itself. `oracle/desctree.c` measures
    /// that. The same shape is built here.
    ///
    /// The struct fields stay the authority for `<info>`, and the tree stays
    /// the authority for `<desc>`. [`InfoBox::sync`] copies each one to the
    /// other. A write to an `<info>` field through the tree is therefore lost.
    /// No call of the C ABI reaches those fields except by walking up from the
    /// description, and nothing writes through that path.
    doc: Box<XmlNode>,
}

/// The fields of `<info>`, in the order that liblsl writes them.
/// `src/stream_info_impl.cpp:60-82`.
const INFO_FIELDS: [&str; 17] = [
    "name",
    "type",
    "channel_count",
    "channel_format",
    "source_id",
    "nominal_srate",
    "version",
    "created_at",
    "uid",
    "session_id",
    "hostname",
    "v4address",
    "v4data_port",
    "v4service_port",
    "v6address",
    "v6data_port",
    "v6service_port",
];

impl InfoBox {
    /// Build a box around a description.
    fn new(info: StreamInfo) -> *mut InfoBox {
        let mut doc = XmlNode::new(desc::Kind::Element, "", "", std::ptr::null_mut());
        doc.document = true;
        let here: *mut XmlNode = &mut *doc;
        let mut node = XmlNode::new(desc::Kind::Element, "info", "", here);
        let inside: *mut XmlNode = &mut *node;
        for f in INFO_FIELDS {
            let mut e = XmlNode::new(desc::Kind::Element, f, "", inside);
            let text: *mut XmlNode = &mut *e;
            e.children
                .push(XmlNode::new(desc::Kind::Text, "", "", text));
            node.children.push(e);
        }
        node.children.push(XmlNode::from_desc(&info.desc, inside));
        doc.children.push(node);

        let mut b = Box::new(InfoBox { info, doc });
        b.write_fields();
        Box::into_raw(b)
    }

    /// The `<info>` element.
    fn info_node(&mut self) -> &mut XmlNode {
        &mut self.doc.children[0]
    }

    /// The `<desc>` element.
    fn desc_node(&mut self) -> &mut XmlNode {
        let last = self.info_node().children.len() - 1;
        &mut self.info_node().children[last]
    }

    /// Copy the struct fields into `<info>`.
    fn write_fields(&mut self) {
        let text = field_text(&self.info);
        let node = self.info_node();
        for (k, v) in text.iter().enumerate() {
            node.children[k].children[0].value = v.clone();
        }
    }

    /// Copy the tree that the application edited back into the description.
    fn sync(&mut self) {
        let mut tree = self.desc_node().to_desc();
        tree.name = "desc".to_string();
        self.info.desc = tree;
        self.write_fields();
    }
}

/// The text of every field of `<info>`, in the order of [`INFO_FIELDS`].
fn field_text(info: &StreamInfo) -> Vec<String> {
    vec![
        info.name.clone(),
        info.stream_type.clone(),
        info.channel_count.to_string(),
        labstream_net::info::format_name(info.format).to_string(),
        info.source_id.clone(),
        labstream_net::info::show_point_16(info.nominal_srate),
        "1.100000000000000".to_string(),
        labstream_net::info::show_point_16(info.created_at),
        info.uid.clone(),
        info.session_id.clone(),
        info.hostname.clone(),
        String::new(),
        info.v4data_port.to_string(),
        info.v4service_port.to_string(),
        String::new(),
        "0".to_string(),
        "0".to_string(),
    ]
}

/// One node of the description tree.
///
/// liblsl holds this in pugixml, and the C ABI hands out a pointer to one
/// node (`src/lsl_xml_element_c.cpp`). A pointer stays valid while the
/// application adds nodes around it, so every node lives in its own
/// allocation and every node knows its parent.
#[derive(Debug)]
struct XmlNode {
    kind: desc::Kind,
    name: String,
    value: String,
    attrs: Vec<(String, String)>,
    /// True for the node that holds the whole document. liblsl reports that
    /// node as text, because its type is not an element
    /// (`src/lsl_xml_element_c.cpp:35`).
    document: bool,
    // The box is not spare. A C caller holds a pointer to a node while it adds
    // more nodes around it. A `Vec<XmlNode>` moves its contents when it grows,
    // and every pointer the caller holds then names freed memory.
    #[allow(clippy::vec_box)]
    children: Vec<Box<XmlNode>>,
    /// The node above this one. Null at the root of the tree.
    parent: *mut XmlNode,
}

impl XmlNode {
    fn new(kind: desc::Kind, name: &str, value: &str, parent: *mut XmlNode) -> Box<XmlNode> {
        Box::new(XmlNode {
            kind,
            name: name.to_string(),
            value: value.to_string(),
            attrs: Vec::new(),
            document: false,
            children: Vec::new(),
            parent,
        })
    }

    /// Copy a safe tree in, and link every node to its parent.
    fn from_desc(src: &desc::Node, parent: *mut XmlNode) -> Box<XmlNode> {
        let mut me = Box::new(XmlNode {
            kind: src.kind,
            name: src.name.clone(),
            value: src.value.clone(),
            attrs: src.attrs.clone(),
            document: false,
            children: Vec::new(),
            parent,
        });
        let here: *mut XmlNode = &mut *me;
        for c in &src.children {
            me.children.push(XmlNode::from_desc(c, here));
        }
        me
    }

    /// Copy the tree back out.
    fn to_desc(&self) -> desc::Node {
        desc::Node {
            kind: self.kind,
            name: self.name.clone(),
            value: self.value.clone(),
            attrs: self.attrs.clone(),
            children: self.children.iter().map(|c| c.to_desc()).collect(),
        }
    }

    /// Where this node sits in the list of its parent.
    ///
    /// # Safety
    /// The parent pointer must be null or name a live node.
    unsafe fn index_in_parent(&self) -> Option<(*mut XmlNode, usize)> {
        let p = self.parent.as_mut()?;
        let me = self as *const XmlNode;
        let k = p.children.iter().position(|c| std::ptr::eq(&**c, me))?;
        Some((p, k))
    }
}

struct OutletBox {
    outlet: Outlet,
    info: StreamInfo,
}

struct InletBox {
    inlet: Option<Inlet>,
    info: StreamInfo,
    buffer_samples: i32,
    recover: bool,
    selection: Option<labstream_net::SelectParam>,
    postproc: u32,
    /// The half-time of the smoothing filter, in seconds.
    /// `src/api_config.cpp` names 90 as the default.
    halftime: f32,
    /// Reusable packed sample buffer for chunk pulls.
    scratch: Vec<PackedSample>,
}

impl InletBox {
    /// Open the stream if it is not open yet.
    ///
    /// liblsl starts the reader on the first pull, so `lsl_open_stream` is not
    /// required (`src/data_receiver.cpp:88`). An application that pulls
    /// straight after it creates the inlet must get samples, not an error.
    fn ensure_open(&mut self, timeout: f64) -> bool {
        if self.inlet.is_some() {
            return true;
        }
        let t = Duration::from_secs_f64(if timeout <= 0.0 { 32.0 } else { timeout });
        let opened = if let Some(sel) = &self.selection {
            Inlet::open_selected(
                &self.info,
                t,
                self.buffer_samples,
                self.recover,
                sel.clone(),
            )
        } else if self.recover {
            Inlet::open_recovering(&self.info, t, self.buffer_samples)
        } else {
            Inlet::open_with_buffer(&self.info, t, self.buffer_samples)
        };
        match opened {
            Ok(mut i) => {
                if self.postproc != 0 {
                    i.set_postprocessing(self.postproc);
                }
                self.inlet = Some(i);
                true
            }
            Err(_) => false,
        }
    }
}

fn format_from_c(v: c_int) -> Format {
    Format::from_u8(v as u8).unwrap_or(Format::Undefined)
}

/// Read a C string, treating a null pointer as empty.
///
/// # Safety
/// The pointer must be null or a valid zero-terminated string.
unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Hand a buffer of bytes to the caller, with a zero after them.
///
/// liblsl allocates these with `malloc` and frees them with `free`
/// (`src/lsl_streaminfo_c.cpp:63`, `src/common.cpp:53`). The same allocator
/// has to be used here. A C application is allowed to call `free` on the
/// result, and a Rust allocation would corrupt the heap.
fn out_bytes(bytes: &[u8]) -> *mut c_char {
    // SAFETY: the size is the length plus one byte for the terminator, and
    // every byte of the result is written before it is handed out.
    unsafe {
        let p = libc::malloc(bytes.len() + 1) as *mut u8;
        if p.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0;
        p as *mut c_char
    }
}

/// Hand a string to the caller. The caller frees it with `lsl_destroy_string`.
fn out_string(s: &str) -> *mut c_char {
    out_bytes(s.as_bytes())
}

/// A string that the caller must not free.
///
/// liblsl returns a pointer into its own storage for these. Each one here
/// leaks a small allocation once per distinct value, which keeps the pointer
/// valid for the life of the process.
fn static_string(s: &str) -> *const c_char {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some(p) = map.get(s) {
        return *p as *const c_char;
    }
    let c = match CString::new(s) {
        Ok(c) => c,
        Err(_) => return std::ptr::null(),
    };
    let p = c.into_raw();
    map.insert(s.to_string(), p as usize);
    p as *const c_char
}

// ---------------------------------------------------------------------------
// library information
// ---------------------------------------------------------------------------

/// The protocol version that this build speaks.
#[no_mangle]
pub extern "C" fn lsl_protocol_version() -> i32 {
    LSL_PROTOCOL_VERSION
}

/// The library version.
#[no_mangle]
pub extern "C" fn lsl_library_version() -> i32 {
    LSL_LIBRARY_VERSION
}

/// A line of text describing this build.
#[no_mangle]
pub extern "C" fn lsl_library_info() -> *const c_char {
    static_string("lsl-rust 0.0.0 (protocol 1.10)")
}

/// The clock that timestamps use. SPEC.md 3.0.
#[no_mangle]
pub extern "C" fn lsl_local_clock() -> c_double {
    labstream_net::clock()
}

/// Free a string that the library returned.
///
/// # Safety
/// The pointer must have come from this library and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn lsl_destroy_string(s: *mut c_char) {
    if !s.is_null() {
        libc::free(s as *mut c_void);
    }
}

// ---------------------------------------------------------------------------
// the stream description
// ---------------------------------------------------------------------------

/// Build a stream description.
///
/// # Safety
/// Every string pointer must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_streaminfo(
    name: *const c_char,
    stype: *const c_char,
    channel_count: c_int,
    nominal_srate: c_double,
    channel_format: c_int,
    source_id: *const c_char,
) -> lsl_streaminfo {
    let mut info = StreamInfo::new(
        &cstr(name),
        &cstr(stype),
        channel_count.max(0) as u32,
        format_from_c(channel_format),
        nominal_srate,
    );
    info.source_id = cstr(source_id);
    InfoBox::new(info) as lsl_streaminfo
}

/// Release a stream description.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_destroy_streaminfo(info: lsl_streaminfo) {
    if !info.is_null() {
        drop(Box::from_raw(info as *mut InfoBox));
    }
}

/// Copy a stream description.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_copy_streaminfo(info: lsl_streaminfo) -> lsl_streaminfo {
    match (info as *mut InfoBox).as_mut() {
        Some(b) => {
            b.sync();
            InfoBox::new(b.info.clone()) as lsl_streaminfo
        }
        None => std::ptr::null_mut(),
    }
}

macro_rules! info_getter_str {
    ($name:ident, $field:ident) => {
        /// Read a field of the description.
        ///
        /// # Safety
        /// The handle must have come from this library.
        #[no_mangle]
        pub unsafe extern "C" fn $name(info: lsl_streaminfo) -> *const c_char {
            match (info as *mut InfoBox).as_ref() {
                Some(b) => static_string(&b.info.$field),
                None => static_string(""),
            }
        }
    };
}

info_getter_str!(lsl_get_name, name);
info_getter_str!(lsl_get_source_id, source_id);
info_getter_str!(lsl_get_session_id, session_id);
info_getter_str!(lsl_get_hostname, hostname);
info_getter_str!(lsl_get_uid, uid);

/// The content type of the stream.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_type(info: lsl_streaminfo) -> *const c_char {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => static_string(&b.info.stream_type),
        None => static_string(""),
    }
}

/// The channel count.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_channel_count(info: lsl_streaminfo) -> i32 {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => b.info.channel_count as i32,
        None => 0,
    }
}

/// The nominal rate. Zero means no fixed rate.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_nominal_srate(info: lsl_streaminfo) -> c_double {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => b.info.nominal_srate,
        None => 0.0,
    }
}

/// The channel format.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_channel_format(info: lsl_streaminfo) -> i32 {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => b.info.format as i32,
        None => 0,
    }
}

/// The clock reading when the stream was created.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_created_at(info: lsl_streaminfo) -> c_double {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => b.info.created_at,
        None => 0.0,
    }
}

/// The protocol version that the sender speaks.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_version(_info: lsl_streaminfo) -> i32 {
    LSL_PROTOCOL_VERSION
}

/// The whole description as XML. The caller frees the result.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_xml(info: lsl_streaminfo) -> *mut c_char {
    match (info as *mut InfoBox).as_mut() {
        Some(b) => {
            b.sync();
            out_string(&b.info.to_fullinfo_xml())
        }
        None => out_string(""),
    }
}

/// The number of bytes in one channel value. Zero for a string.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_channel_bytes(info: lsl_streaminfo) -> i32 {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => b.info.format.width().unwrap_or(0) as i32,
        None => 0,
    }
}

/// The number of bytes in one sample.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_sample_bytes(info: lsl_streaminfo) -> i32 {
    unsafe { lsl_get_channel_bytes(info) * lsl_get_channel_count(info) }
}

// ---------------------------------------------------------------------------
// the description tree
// ---------------------------------------------------------------------------
//
// Every call below matches one line of `src/lsl_xml_element_c.cpp`, which is a
// thin wrapper over pugixml. Three of those lines carry behavior that a reader
// of the header cannot guess:
//
//   lsl_append_child_value  returns the **parent**, not the new child.
//   lsl_value               returns nothing for an element. The text sits in a
//                           child node, so `lsl_child_value` is the call that
//                           reads it.
//   lsl_set_value           fails on an element, because pugixml only lets a
//                           text node carry a value.
//
// A null pointer stands for "no node". `lsl_empty` asks exactly that.

/// The root of the description tree.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_desc(info: lsl_streaminfo) -> lsl_xml_ptr {
    match (info as *mut InfoBox).as_mut() {
        Some(b) => b.desc_node() as *mut XmlNode as lsl_xml_ptr,
        None => std::ptr::null_mut(),
    }
}

/// Read a node pointer, or return the default when it is null.
macro_rules! node {
    ($e:expr, $bad:expr) => {
        match ($e as *mut XmlNode).as_mut() {
            Some(n) => n,
            None => return $bad,
        }
    };
}

fn as_ptr(n: &mut XmlNode) -> lsl_xml_ptr {
    n as *mut XmlNode as lsl_xml_ptr
}

// --- reading -----------------------------------------------------------

/// The name of a node. A text node has none.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_name(e: lsl_xml_ptr) -> *const c_char {
    let n = node!(e, static_string(""));
    if n.kind == desc::Kind::Element {
        static_string(&n.name)
    } else {
        static_string("")
    }
}

/// The value of a node.
///
/// An element carries no value of its own. Its text sits in a child node, and
/// `lsl_child_value` reads that. `src/lsl_xml_element_c.cpp:38`.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_value(e: lsl_xml_ptr) -> *const c_char {
    let n = node!(e, static_string(""));
    if n.kind == desc::Kind::Element {
        static_string("")
    } else {
        static_string(&n.value)
    }
}

/// The text of the first child that holds text.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_child_value(e: lsl_xml_ptr) -> *const c_char {
    let n = node!(e, static_string(""));
    match n.children.iter().find(|c| c.kind != desc::Kind::Element) {
        Some(c) => static_string(&c.value),
        None => static_string(""),
    }
}

/// The text of the first child with this name.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_child_value_n(e: lsl_xml_ptr, name: *const c_char) -> *const c_char {
    let child = lsl_child(e, name);
    if child.is_null() {
        return static_string("");
    }
    lsl_child_value(child)
}

/// True when the pointer names no node.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_empty(e: lsl_xml_ptr) -> i32 {
    e.is_null() as i32
}

/// True when a node holds text rather than a tag.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_is_text(e: lsl_xml_ptr) -> i32 {
    let n = node!(e, 0);
    (n.document || n.kind != desc::Kind::Element) as i32
}

// --- moving through the tree -------------------------------------------

/// The first child of a node.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_first_child(e: lsl_xml_ptr) -> lsl_xml_ptr {
    let n = node!(e, std::ptr::null_mut());
    match n.children.first_mut() {
        Some(c) => as_ptr(c),
        None => std::ptr::null_mut(),
    }
}

/// The last child of a node.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_last_child(e: lsl_xml_ptr) -> lsl_xml_ptr {
    let n = node!(e, std::ptr::null_mut());
    match n.children.last_mut() {
        Some(c) => as_ptr(c),
        None => std::ptr::null_mut(),
    }
}

/// The node above this one.
///
/// The root of the tree is the `<desc>` element. Above it sits `<info>`, and
/// above that sits the document. `oracle/desctree.c` measures both.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_parent(e: lsl_xml_ptr) -> lsl_xml_ptr {
    let n = node!(e, std::ptr::null_mut());
    if n.parent.is_null() {
        std::ptr::null_mut()
    } else {
        n.parent as lsl_xml_ptr
    }
}

/// The first child with this name.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_child(e: lsl_xml_ptr, name: *const c_char) -> lsl_xml_ptr {
    let n = node!(e, std::ptr::null_mut());
    let want = cstr(name);
    match n.children.iter_mut().find(|c| c.name == want) {
        Some(c) => as_ptr(c),
        None => std::ptr::null_mut(),
    }
}

/// Step to a sibling.
///
/// `step` is 1 for the next one and -1 for the one before. An empty name
/// accepts any sibling.
///
/// # Safety
/// The pointer must name a node of a live description.
unsafe fn sibling(e: lsl_xml_ptr, step: isize, want: Option<String>) -> lsl_xml_ptr {
    let n = node!(e, std::ptr::null_mut());
    let (parent, here) = match n.index_in_parent() {
        Some(x) => x,
        None => return std::ptr::null_mut(),
    };
    let parent = match parent.as_mut() {
        Some(p) => p,
        None => return std::ptr::null_mut(),
    };
    let count = parent.children.len() as isize;
    let mut k = here as isize + step;
    while k >= 0 && k < count {
        let c = &mut parent.children[k as usize];
        match &want {
            None => return as_ptr(c),
            Some(w) if c.name == *w => return as_ptr(c),
            Some(_) => {}
        }
        k += step;
    }
    std::ptr::null_mut()
}

/// The sibling that follows this node.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_next_sibling(e: lsl_xml_ptr) -> lsl_xml_ptr {
    sibling(e, 1, None)
}

/// The sibling before this node.
///
/// # Safety
/// The pointer must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_previous_sibling(e: lsl_xml_ptr) -> lsl_xml_ptr {
    sibling(e, -1, None)
}

/// The next sibling with this name.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_next_sibling_n(e: lsl_xml_ptr, name: *const c_char) -> lsl_xml_ptr {
    sibling(e, 1, Some(cstr(name)))
}

/// The sibling before this one with this name.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_previous_sibling_n(
    e: lsl_xml_ptr,
    name: *const c_char,
) -> lsl_xml_ptr {
    sibling(e, -1, Some(cstr(name)))
}

// --- changing the tree -------------------------------------------------

/// Add a tag at the end and return it.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_append_child(e: lsl_xml_ptr, name: *const c_char) -> lsl_xml_ptr {
    let parent = node!(e, std::ptr::null_mut());
    let here: *mut XmlNode = parent;
    parent
        .children
        .push(XmlNode::new(desc::Kind::Element, &cstr(name), "", here));
    as_ptr(parent.children.last_mut().unwrap())
}

/// Add a tag at the start and return it.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_prepend_child(e: lsl_xml_ptr, name: *const c_char) -> lsl_xml_ptr {
    let parent = node!(e, std::ptr::null_mut());
    let here: *mut XmlNode = parent;
    parent
        .children
        .insert(0, XmlNode::new(desc::Kind::Element, &cstr(name), "", here));
    as_ptr(&mut parent.children[0])
}

/// Add a tag that holds text, at the end.
///
/// **The result is the parent**, not the new tag
/// (`src/lsl_xml_element_c.cpp:74`). An application can therefore chain
/// several calls on one node.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and both
/// strings must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_append_child_value(
    e: lsl_xml_ptr,
    name: *const c_char,
    value: *const c_char,
) -> lsl_xml_ptr {
    let child = lsl_append_child(e, name);
    if let Some(c) = (child as *mut XmlNode).as_mut() {
        let here: *mut XmlNode = c;
        c.children
            .push(XmlNode::new(desc::Kind::Text, "", &cstr(value), here));
    }
    e
}

/// Add a tag that holds text, at the start.
///
/// The result is the parent, the same way `lsl_append_child_value` returns it.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and both
/// strings must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_prepend_child_value(
    e: lsl_xml_ptr,
    name: *const c_char,
    value: *const c_char,
) -> lsl_xml_ptr {
    let child = lsl_prepend_child(e, name);
    if let Some(c) = (child as *mut XmlNode).as_mut() {
        let here: *mut XmlNode = c;
        c.children
            .push(XmlNode::new(desc::Kind::Text, "", &cstr(value), here));
    }
    e
}

/// Replace the name of a tag.
///
/// A text node has no name, and the call reports a failure for one.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_set_name(e: lsl_xml_ptr, name: *const c_char) -> i32 {
    let n = node!(e, 0);
    if n.kind != desc::Kind::Element {
        return 0;
    }
    n.name = cstr(name);
    1
}

/// Replace the value of a text node.
///
/// An element carries no value, and the call reports a failure for one. That
/// is what pugixml does (`src/lsl_xml_element_c.cpp:47`).
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the
/// value must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_set_value(e: lsl_xml_ptr, value: *const c_char) -> i32 {
    let n = node!(e, 0);
    if n.kind == desc::Kind::Element {
        return 0;
    }
    n.value = cstr(value);
    1
}

/// Replace the text of a named child.
///
/// liblsl sets the value of the **first** child of that tag, whatever it is
/// (`src/lsl_xml_element_c.cpp:69`). A tag whose first child is another tag
/// therefore reports a failure and changes nothing.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and both
/// strings must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_set_child_value(
    e: lsl_xml_ptr,
    name: *const c_char,
    value: *const c_char,
) -> i32 {
    let child = lsl_child(e, name);
    let first = lsl_first_child(child);
    lsl_set_value(first, value)
}

/// Copy a subtree in at the end, and return the copy.
///
/// # Safety
/// Both pointers must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_append_copy(e: lsl_xml_ptr, source: lsl_xml_ptr) -> lsl_xml_ptr {
    let src = match (source as *mut XmlNode).as_ref() {
        Some(s) => s.to_desc(),
        None => return std::ptr::null_mut(),
    };
    let parent = node!(e, std::ptr::null_mut());
    let here: *mut XmlNode = parent;
    parent.children.push(XmlNode::from_desc(&src, here));
    as_ptr(parent.children.last_mut().unwrap())
}

/// Copy a subtree in at the start, and return the copy.
///
/// # Safety
/// Both pointers must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_prepend_copy(e: lsl_xml_ptr, source: lsl_xml_ptr) -> lsl_xml_ptr {
    let src = match (source as *mut XmlNode).as_ref() {
        Some(s) => s.to_desc(),
        None => return std::ptr::null_mut(),
    };
    let parent = node!(e, std::ptr::null_mut());
    let here: *mut XmlNode = parent;
    parent.children.insert(0, XmlNode::from_desc(&src, here));
    as_ptr(&mut parent.children[0])
}

/// Remove the first child with this name.
///
/// # Safety
/// The pointer must be null or name a node of a live description, and the name
/// must be null or zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_remove_child_n(e: lsl_xml_ptr, name: *const c_char) {
    let n = match (e as *mut XmlNode).as_mut() {
        Some(n) => n,
        None => return,
    };
    let want = cstr(name);
    if let Some(k) = n.children.iter().position(|c| c.name == want) {
        n.children.remove(k);
    }
}

/// Remove one subtree.
///
/// Every pointer into that subtree stops being valid.
///
/// # Safety
/// Both pointers must be null or name a node of a live description.
#[no_mangle]
pub unsafe extern "C" fn lsl_remove_child(e: lsl_xml_ptr, child: lsl_xml_ptr) {
    let n = match (e as *mut XmlNode).as_mut() {
        Some(n) => n,
        None => return,
    };
    let target = child as *const XmlNode;
    if target.is_null() {
        return;
    }
    if let Some(k) = n
        .children
        .iter()
        .position(|c| std::ptr::eq(&**c as *const XmlNode, target))
    {
        n.children.remove(k);
    }
}

// ---------------------------------------------------------------------------
// the outlet
// ---------------------------------------------------------------------------

/// Publish a stream.
///
/// # Safety
/// The description handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_outlet(
    info: lsl_streaminfo,
    chunk_size: c_int,
    max_buffered: c_int,
) -> lsl_outlet {
    lsl_create_outlet_ex(info, chunk_size, max_buffered, 0)
}

/// Publish a stream, with transport flags.
///
/// # Safety
/// The description handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_outlet_ex(
    info: lsl_streaminfo,
    _chunk_size: c_int,
    _max_buffered: c_int,
    flags: c_int,
) -> lsl_outlet {
    let b = match (info as *mut InfoBox).as_mut() {
        Some(b) => b,
        None => return std::ptr::null_mut(),
    };
    // liblsl builds the answer for `LSL:fullinfo` once, when the server starts
    // (`src/tcp_server.cpp:378`). A change to the tree after this call never
    // reaches a consumer, on either implementation.
    b.sync();
    // `transp_sync_blocking` writes each sample before the push returns.
    // `include/lsl/common.h:172`.
    const SYNC_BLOCKING: c_int = 4;
    let built = if flags & SYNC_BLOCKING != 0 {
        Outlet::new_blocking(b.info.clone())
    } else {
        Outlet::new(b.info.clone())
    };
    match built {
        Ok(outlet) => {
            let published = outlet.info().clone();
            Box::into_raw(Box::new(OutletBox {
                outlet,
                info: published,
            })) as lsl_outlet
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Stop a stream.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_destroy_outlet(out: lsl_outlet) {
    if !out.is_null() {
        drop(Box::from_raw(out as *mut OutletBox));
    }
}

/// The description that an outlet publishes.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_info(out: lsl_outlet) -> lsl_streaminfo {
    match (out as *mut OutletBox).as_ref() {
        Some(b) => InfoBox::new(b.info.clone()) as lsl_streaminfo,
        None => std::ptr::null_mut(),
    }
}

/// True when at least one consumer is connected.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_have_consumers(out: lsl_outlet) -> i32 {
    match (out as *mut OutletBox).as_ref() {
        Some(b) => (b.outlet.consumer_count() > 0) as i32,
        None => 0,
    }
}

/// Wait until a consumer connects.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_wait_for_consumers(out: lsl_outlet, timeout: c_double) -> i32 {
    match (out as *mut OutletBox).as_ref() {
        Some(b) => {
            b.outlet
                .wait_for_consumers(Duration::from_secs_f64(timeout.max(0.0))) as i32
        }
        None => 0,
    }
}

unsafe fn push_packed_numeric<T>(
    out: lsl_outlet,
    data: *const T,
    format: Format,
    timestamp: c_double,
    _pushthrough: c_int,
) -> i32 {
    if data.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    let n = b.info.channel_count as usize;
    let t = if timestamp == 0.0 {
        labstream_net::clock()
    } else {
        timestamp
    };
    let byte_len = n * std::mem::size_of::<T>();
    let raw = std::slice::from_raw_parts(data as *const u8, byte_len);
    b.outlet.push_numeric_raw(t, format, raw);
    LSL_NO_ERROR
}

unsafe fn push_typed(
    out: lsl_outlet,
    build: impl Fn(usize) -> Value,
    timestamp: c_double,
    _pushthrough: c_int,
) -> i32 {
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    let n = b.info.channel_count as usize;
    // A timestamp of zero means the current clock. SPEC.md 8.3.
    let t = if timestamp == 0.0 {
        labstream_net::clock()
    } else {
        timestamp
    };
    let values: Vec<Value> = (0..n).map(&build).collect();
    b.outlet.push(&Sample {
        timestamp: t,
        values,
    });
    LSL_NO_ERROR
}

macro_rules! push_sample {
    ($name:ident, $ty:ty, $fmt:expr) => {
        /// Send one sample.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold one value for each channel.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            out: lsl_outlet,
            data: *const $ty,
            timestamp: c_double,
            pushthrough: c_int,
        ) -> i32 {
            push_packed_numeric(out, data, $fmt, timestamp, pushthrough)
        }
    };
}

push_sample!(lsl_push_sample_ftp, f32, Format::Float32);
push_sample!(lsl_push_sample_dtp, f64, Format::Double64);
push_sample!(lsl_push_sample_itp, i32, Format::Int32);
push_sample!(lsl_push_sample_stp, i16, Format::Int16);

/// Send one sample of bytes.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold one
/// value for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_ctp(
    out: lsl_outlet,
    data: *const c_char,
    timestamp: c_double,
    pushthrough: c_int,
) -> i32 {
    push_packed_numeric(out, data, Format::Int8, timestamp, pushthrough)
}
push_sample!(lsl_push_sample_ltp, i64, Format::Int64);

/// Send one sample of strings.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold one
/// zero-terminated string for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_strtp(
    out: lsl_outlet,
    data: *const *const c_char,
    timestamp: c_double,
    pushthrough: c_int,
) -> i32 {
    if data.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    push_typed(
        out,
        |i| Value::Str(cstr(*data.add(i)).into_bytes()),
        timestamp,
        pushthrough,
    )
}

unsafe fn push_chunk_packed_numeric<T>(
    out: lsl_outlet,
    data: *const T,
    count_values: u64,
    format: Format,
    timestamp: c_double,
    _pushthrough: c_int,
) -> i32 {
    if data.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    let chans = b.info.channel_count as usize;
    if chans == 0 {
        return LSL_ARGUMENT_ERROR;
    }
    let total = count_values as usize;
    if total % chans != 0 {
        return LSL_ARGUMENT_ERROR;
    }
    let samples = total / chans;
    if samples == 0 {
        return LSL_NO_ERROR;
    }
    let mut first = if timestamp == 0.0 {
        labstream_net::clock()
    } else {
        timestamp
    };
    if b.info.nominal_srate != LSL_IRREGULAR_RATE {
        first -= (samples - 1) as f64 / b.info.nominal_srate;
    }
    let row_bytes = chans * std::mem::size_of::<T>();
    let raw = std::slice::from_raw_parts(data as *const u8, samples * row_bytes);
    let block: Vec<PackedSample> = (0..samples)
        .map(|k| {
            let t = if k == 0 { first } else { LSL_DEDUCED_TIMESTAMP };
            PackedSample::from_raw_bytes(
                t,
                format,
                Arc::from(&raw[k * row_bytes..(k + 1) * row_bytes]),
            )
        })
        .collect();
    b.outlet.push_samples_packed(block);
    LSL_NO_ERROR
}

unsafe fn push_chunk_stamped_packed_numeric<T>(
    out: lsl_outlet,
    data: *const T,
    count_values: u64,
    format: Format,
    stamps: *const c_double,
    _pushthrough: c_int,
) -> i32 {
    if data.is_null() || stamps.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    let chans = b.info.channel_count as usize;
    if chans == 0 || count_values as usize % chans != 0 {
        return LSL_ARGUMENT_ERROR;
    }
    let samples = count_values as usize / chans;
    if samples == 0 {
        return LSL_NO_ERROR;
    }
    let row_bytes = chans * std::mem::size_of::<T>();
    let raw = std::slice::from_raw_parts(data as *const u8, samples * row_bytes);
    let stamp_slice = std::slice::from_raw_parts(stamps, samples);
    b.outlet
        .push_chunk_numeric_raw(format, raw, row_bytes, stamp_slice);
    LSL_NO_ERROR
}

/// Send several samples that share one timestamp. SPEC.md 9.2.
///
/// The timestamp names the **last** sample. A stream with a rate counts
/// backward from it, so the first sample of the chunk is dated
/// `timestamp - (count - 1) / rate`. Only that first sample carries a
/// timestamp on the wire. The rest carry the deduced tag.
unsafe fn push_chunk_typed(
    out: lsl_outlet,
    count_values: u64,
    build: impl Fn(usize) -> Value,
    timestamp: c_double,
    _pushthrough: c_int,
) -> i32 {
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    let chans = b.info.channel_count as usize;
    if chans == 0 {
        return LSL_ARGUMENT_ERROR;
    }
    let total = count_values as usize;
    if total % chans != 0 {
        // A buffer that is not a whole number of samples is an error.
        return LSL_ARGUMENT_ERROR;
    }
    let samples = total / chans;
    if samples == 0 {
        return LSL_NO_ERROR;
    }

    let mut first = if timestamp == 0.0 {
        labstream_net::clock()
    } else {
        timestamp
    };
    if b.info.nominal_srate != LSL_IRREGULAR_RATE {
        first -= (samples - 1) as f64 / b.info.nominal_srate;
    }

    // One call for the whole chunk, so a consumer gets it with one write.
    let block: Vec<Sample> = (0..samples)
        .map(|k| Sample {
            timestamp: if k == 0 { first } else { LSL_DEDUCED_TIMESTAMP },
            values: (0..chans).map(|c| build(k * chans + c)).collect(),
        })
        .collect();
    b.outlet.push_samples(block);
    LSL_NO_ERROR
}

/// Send several samples, each with its own timestamp.
unsafe fn push_chunk_stamped(
    out: lsl_outlet,
    count_values: u64,
    build: impl Fn(usize) -> Value,
    stamps: *const c_double,
    _pushthrough: c_int,
) -> i32 {
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    if stamps.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let chans = b.info.channel_count as usize;
    if chans == 0 || count_values as usize % chans != 0 {
        return LSL_ARGUMENT_ERROR;
    }
    let samples = count_values as usize / chans;
    // One call for the whole chunk, so a consumer gets it with one write.
    // `push_samples` replaces a timestamp of zero with the current clock.
    let block: Vec<Sample> = (0..samples)
        .map(|k| Sample {
            timestamp: *stamps.add(k),
            values: (0..chans).map(|c| build(k * chans + c)).collect(),
        })
        .collect();
    b.outlet.push_samples(block);
    LSL_NO_ERROR
}

macro_rules! push_chunk_numeric {
    ($tp:ident, $tnp:ident, $ty:ty, $fmt:expr) => {
        /// Send several samples that share one timestamp. SPEC.md 9.2.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold `data_elements` values.
        #[no_mangle]
        pub unsafe extern "C" fn $tp(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
            timestamp: c_double,
            pushthrough: c_int,
        ) -> i32 {
            push_chunk_packed_numeric(out, data, data_elements, $fmt, timestamp, pushthrough)
        }

        /// Send several samples, each with its own timestamp.
        ///
        /// # Safety
        /// The handle must have come from this library, the data buffer must
        /// hold `data_elements` values, and the timestamp buffer must hold one
        /// value for each sample.
        #[no_mangle]
        pub unsafe extern "C" fn $tnp(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
            stamps: *const c_double,
            pushthrough: c_int,
        ) -> i32 {
            push_chunk_stamped_packed_numeric(out, data, data_elements, $fmt, stamps, pushthrough)
        }
    };
}

macro_rules! push_chunk {
    ($tp:ident, $tnp:ident, $ty:ty, $make:expr) => {
        /// Send several samples that share one timestamp. SPEC.md 9.2.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold `data_elements` values.
        #[no_mangle]
        pub unsafe extern "C" fn $tp(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
            timestamp: c_double,
            pushthrough: c_int,
        ) -> i32 {
            if data.is_null() {
                return LSL_ARGUMENT_ERROR;
            }
            let f = $make;
            push_chunk_typed(out, data_elements, |i| f(data, i), timestamp, pushthrough)
        }

        /// Send several samples, each with its own timestamp.
        ///
        /// # Safety
        /// The handle must have come from this library, the data buffer must
        /// hold `data_elements` values, and the timestamp buffer must hold one
        /// value for each sample.
        #[no_mangle]
        pub unsafe extern "C" fn $tnp(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
            stamps: *const c_double,
            pushthrough: c_int,
        ) -> i32 {
            if data.is_null() {
                return LSL_ARGUMENT_ERROR;
            }
            let f = $make;
            push_chunk_stamped(out, data_elements, |i| f(data, i), stamps, pushthrough)
        }
    };
}

push_chunk_numeric!(
    lsl_push_chunk_ftp,
    lsl_push_chunk_ftnp,
    f32,
    Format::Float32
);
push_chunk_numeric!(
    lsl_push_chunk_dtp,
    lsl_push_chunk_dtnp,
    f64,
    Format::Double64
);
push_chunk_numeric!(lsl_push_chunk_itp, lsl_push_chunk_itnp, i32, Format::Int32);
push_chunk_numeric!(lsl_push_chunk_stp, lsl_push_chunk_stnp, i16, Format::Int16);
push_chunk_numeric!(
    lsl_push_chunk_ctp,
    lsl_push_chunk_ctnp,
    c_char,
    Format::Int8
);
push_chunk_numeric!(lsl_push_chunk_ltp, lsl_push_chunk_ltnp, i64, Format::Int64);
push_chunk!(
    lsl_push_chunk_strtp,
    lsl_push_chunk_strtnp,
    *const c_char,
    |d: *const *const c_char, i: usize| Value::Str(cstr(*d.add(i)).into_bytes())
);

// ---------------------------------------------------------------------------
// resolving
// ---------------------------------------------------------------------------

unsafe fn fill_results(
    found: Vec<StreamInfo>,
    buffer: *mut lsl_streaminfo,
    buffer_elements: u32,
) -> i32 {
    if buffer.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let n = found.len().min(buffer_elements as usize);
    for (i, info) in found.into_iter().take(n).enumerate() {
        let h = InfoBox::new(info) as lsl_streaminfo;
        *buffer.add(i) = h;
    }
    n as i32
}

/// Find every stream on the network.
///
/// # Safety
/// The buffer must hold `buffer_elements` handles.
#[no_mangle]
pub unsafe extern "C" fn lsl_resolve_all(
    buffer: *mut lsl_streaminfo,
    buffer_elements: u32,
    wait_time: c_double,
) -> i32 {
    let found = labstream_net::resolve(
        &format!("session_id='{}'", session_id()),
        1,
        Duration::from_secs_f64(wait_time.max(0.1)),
    )
    .unwrap_or_default();
    fill_results(found, buffer, buffer_elements)
}

/// Find every stream whose named field holds a value.
///
/// # Safety
/// The buffer must hold `buffer_elements` handles, and the strings must be
/// zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_resolve_byprop(
    buffer: *mut lsl_streaminfo,
    buffer_elements: u32,
    prop: *const c_char,
    value: *const c_char,
    minimum: c_int,
    timeout: c_double,
) -> i32 {
    let q = labstream_net::build_property_query(&session_id(), &cstr(prop), &cstr(value));
    let found = labstream_net::resolve(
        &q,
        minimum.max(1) as usize,
        Duration::from_secs_f64(timeout.max(0.1)),
    )
    .unwrap_or_default();
    fill_results(found, buffer, buffer_elements)
}

/// Find every stream that matches a query.
///
/// # Safety
/// The buffer must hold `buffer_elements` handles.
#[no_mangle]
pub unsafe extern "C" fn lsl_resolve_bypred(
    buffer: *mut lsl_streaminfo,
    buffer_elements: u32,
    pred: *const c_char,
    minimum: c_int,
    timeout: c_double,
) -> i32 {
    let found = labstream_net::resolve(
        &cstr(pred),
        minimum.max(1) as usize,
        Duration::from_secs_f64(timeout.max(0.1)),
    )
    .unwrap_or_default();
    fill_results(found, buffer, buffer_elements)
}

/// Start a background resolver for every stream of this session.
///
/// The query is the one that liblsl builds with no property
/// (`src/resolver_impl.cpp:66`).
///
/// # Safety
/// The result must be released with `lsl_destroy_continuous_resolver`.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_continuous_resolver(
    forget_after: c_double,
) -> lsl_continuous_resolver {
    make_resolver(format!("session_id='{}'", session_id()), forget_after)
}

/// Start a background resolver for one field.
///
/// # Safety
/// The strings must be zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_continuous_resolver_byprop(
    prop: *const c_char,
    value: *const c_char,
    forget_after: c_double,
) -> lsl_continuous_resolver {
    let query = labstream_net::build_property_query(&session_id(), &cstr(prop), &cstr(value));
    make_resolver(query, forget_after)
}

/// Start a background resolver for a query.
///
/// liblsl joins the query to the session test with `and`
/// (`src/resolver_impl.cpp:70`), so a stream of another session never
/// matches.
///
/// # Safety
/// The string must be zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_continuous_resolver_bypred(
    pred: *const c_char,
    forget_after: c_double,
) -> lsl_continuous_resolver {
    let query = format!("session_id='{}' and {}", session_id(), cstr(pred));
    make_resolver(query, forget_after)
}

/// The session that this process belongs to. `lab.SessionID`.
fn session_id() -> String {
    labstream_net::config::get().session_id.clone()
}

fn make_resolver(query: String, forget_after: c_double) -> lsl_continuous_resolver {
    // liblsl treats a value of zero or below as "keep every stream".
    let forget = if forget_after > 0.0 {
        forget_after
    } else {
        f64::INFINITY
    };
    Box::into_raw(Box::new(ContinuousResolver::new(&query, forget))) as lsl_continuous_resolver
}

/// Read what a background resolver found.
///
/// The caller releases each handle with `lsl_destroy_streaminfo`.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold
/// `buffer_elements` handles.
#[no_mangle]
pub unsafe extern "C" fn lsl_resolver_results(
    res: lsl_continuous_resolver,
    buffer: *mut lsl_streaminfo,
    buffer_elements: u32,
) -> i32 {
    let r = match (res as *mut ContinuousResolver).as_ref() {
        Some(r) => r,
        None => return LSL_ARGUMENT_ERROR,
    };
    if buffer.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let found = r.results(buffer_elements as usize);
    for (k, info) in found.iter().enumerate() {
        *buffer.add(k) = InfoBox::new(info.clone()) as lsl_streaminfo;
    }
    found.len() as i32
}

/// Stop a background resolver.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_destroy_continuous_resolver(res: lsl_continuous_resolver) {
    if !res.is_null() {
        drop(Box::from_raw(res as *mut ContinuousResolver));
    }
}

/// Subscribe to a stream.
///
/// # Safety
/// The description handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_inlet(
    info: lsl_streaminfo,
    max_buflen: c_int,
    max_chunklen: c_int,
    recover: c_int,
) -> lsl_inlet {
    lsl_create_inlet_ex(info, max_buflen, max_chunklen, recover, 0)
}

/// Subscribe to a stream, with transport flags.
///
/// The buffer length converts here, at the boundary, exactly as liblsl does
/// (`src/lsl_inlet_c.cpp:20`). The wire only ever carries samples. SPEC.md 8.6.
///
/// # Safety
/// The description handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_create_inlet_ex(
    info: lsl_streaminfo,
    max_buflen: c_int,
    _max_chunklen: c_int,
    recover: c_int,
    flags: c_int,
) -> lsl_inlet {
    let b = match (info as *mut InfoBox).as_ref() {
        Some(b) => b,
        None => return std::ptr::null_mut(),
    };
    let buffer_samples = b.info.buffer_samples(max_buflen, flags as u32);
    Box::into_raw(Box::new(InletBox {
        inlet: None,
        info: b.info.clone(),
        buffer_samples,
        recover: recover != 0,
        selection: None,
        postproc: 0,
        halftime: 90.0,
        scratch: Vec::new(),
    })) as lsl_inlet
}

/// Subscribe to a stream with optional channel and rate selection (`select/1`,
/// `docs/roadmap.md`, step 6).
///
/// Pass null for `channels` (or `channel_count == 0`) to keep every channel.
/// Pass `decimate <= 1` to keep the full sample rate. When the outlet does not
/// support `select/1`, the inlet falls back cleanly to the full stream.
///
/// # Safety
/// `info` must be a valid handle from this library. When `channel_count > 0`,
/// `channels` must point to `channel_count` valid `u32` channel indices.
#[no_mangle]
pub unsafe extern "C" fn labstream_create_inlet_selected(
    info: lsl_streaminfo,
    max_buflen: c_int,
    _max_chunklen: c_int,
    recover: c_int,
    channels: *const u32,
    channel_count: u32,
    decimate: u32,
) -> lsl_inlet {
    let b = match (info as *mut InfoBox).as_ref() {
        Some(b) => b,
        None => return std::ptr::null_mut(),
    };
    let mut chs = Vec::new();
    if channel_count > 0 {
        if channels.is_null() {
            return std::ptr::null_mut();
        }
        let slice = std::slice::from_raw_parts(channels, channel_count as usize);
        for &idx in slice {
            let Ok(v) = u16::try_from(idx) else {
                return std::ptr::null_mut();
            };
            chs.push(v);
        }
    }
    let sel = labstream_net::SelectParam {
        decimate: decimate.max(1),
        channels: chs,
    };
    let selection = sel.is_active().then_some(sel);
    let mut effective_for_buf = b.info.clone();
    if let Some(s) = &selection {
        effective_for_buf = effective_for_buf.apply_selection(s);
    }
    let buffer_samples = effective_for_buf.buffer_samples(max_buflen, 0);
    Box::into_raw(Box::new(InletBox {
        inlet: None,
        info: b.info.clone(),
        buffer_samples,
        recover: recover != 0,
        selection,
        postproc: 0,
        halftime: 90.0,
        scratch: Vec::new(),
    })) as lsl_inlet
}

/// Release an inlet.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_destroy_inlet(inlet: lsl_inlet) {
    if !inlet.is_null() {
        drop(Box::from_raw(inlet as *mut InletBox));
    }
}

/// Open the data feed.
///
/// # Safety
/// The handle must have come from this library, and `ec` must be null or point
/// to one `int32`.
#[no_mangle]
pub unsafe extern "C" fn lsl_open_stream(inlet: lsl_inlet, timeout: c_double, ec: *mut i32) {
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return;
        }
    };
    if !ec.is_null() {
        *ec = if b.ensure_open(timeout) {
            LSL_NO_ERROR
        } else {
            LSL_TIMEOUT_ERROR
        };
    } else {
        b.ensure_open(timeout);
    }
}

/// Close the data feed.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_close_stream(inlet: lsl_inlet) {
    if let Some(b) = (inlet as *mut InletBox).as_mut() {
        b.inlet = None;
    }
}

/// The description of the connected stream.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_get_fullinfo(
    inlet: lsl_inlet,
    timeout: c_double,
    ec: *mut i32,
) -> lsl_streaminfo {
    let b = match (inlet as *mut InletBox).as_ref() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return std::ptr::null_mut();
        }
    };
    if !ec.is_null() {
        *ec = LSL_NO_ERROR;
    }
    // The short description that discovery returned carries no tree. The tree
    // travels on the data port, so ask for it there. `src/info_receiver.cpp`.
    let dur = Duration::from_secs_f64(timeout.max(0.1));
    let info = if let Some(i) = b.inlet.as_ref() {
        match i.fullinfo(dur) {
            Ok(full) => full,
            Err(_) => i.info().clone(),
        }
    } else {
        match labstream_net::read_fullinfo(&b.info, dur) {
            Ok(full) => match &b.selection {
                Some(sel) => full.apply_selection(sel),
                None => full,
            },
            // liblsl blocks until the whole description arrives and reports a
            // timeout. A failure here leaves the short description, which holds
            // every field except the tree.
            Err(_) => match &b.selection {
                Some(sel) => b.info.apply_selection(sel),
                None => b.info.clone(),
            },
        }
    };
    InfoBox::new(info) as lsl_streaminfo
}

/// Choose the post-processing stages. SPEC.md 8.5.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_set_postprocessing(inlet: lsl_inlet, flags: u32) -> i32 {
    match (inlet as *mut InletBox).as_mut() {
        Some(b) => {
            b.postproc = flags;
            if let Some(i) = b.inlet.as_mut() {
                i.set_postprocessing(flags);
            }
            LSL_NO_ERROR
        }
        None => LSL_ARGUMENT_ERROR,
    }
}

/// The measured offset between the two clocks. SPEC.md 3.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_time_correction(
    inlet: lsl_inlet,
    timeout: c_double,
    ec: *mut i32,
) -> c_double {
    let b = match (inlet as *mut InletBox).as_ref() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0.0;
        }
    };
    match b.inlet.as_ref() {
        Some(i) => {
            let t = Duration::from_secs_f64(if timeout <= 0.0 { 5.0 } else { timeout });
            match i.wait_for_time_correction(t) {
                Some(v) => {
                    if !ec.is_null() {
                        *ec = LSL_NO_ERROR;
                    }
                    v
                }
                None => {
                    if !ec.is_null() {
                        *ec = LSL_TIMEOUT_ERROR;
                    }
                    0.0
                }
            }
        }
        None => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            0.0
        }
    }
}

/// True when the connection reset since the last call.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_was_clock_reset(inlet: lsl_inlet) -> i32 {
    match (inlet as *mut InletBox).as_ref() {
        Some(b) => b
            .inlet
            .as_ref()
            .map(|i| (i.recoveries() > 0) as i32)
            .unwrap_or(0),
        None => 0,
    }
}

/// How many samples never reached this inlet, since it opened.
///
/// This function is a labstream extension. liblsl has no function of this
/// name. The count comes from the `seq/1` numbers (`docs/extensions.md`).
/// The result is -1 when the count is unknown: the stream is not open, or
/// the source does not number its samples, for example a liblsl outlet.
///
/// # Safety
/// The handle must be null or have come from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_samples_lost(inlet: lsl_inlet) -> i64 {
    inlet_count(inlet, Inlet::samples_lost)
}

/// How many samples came again from the history of the source, since the
/// inlet opened.
///
/// This function is a labstream extension. liblsl has no function of this
/// name. The count comes from the `resume/1` feature (`docs/extensions.md`).
/// The result is -1 when the count is unknown: the stream is not open, or
/// the source keeps no history.
///
/// # Safety
/// The handle must be null or have come from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_samples_resent(inlet: lsl_inlet) -> i64 {
    inlet_count(inlet, Inlet::samples_resent)
}

/// A count of an open inlet, or -1 for unknown. A count above `i64::MAX`
/// stays at `i64::MAX`.
unsafe fn inlet_count(inlet: lsl_inlet, count: fn(&Inlet) -> Option<u64>) -> i64 {
    (inlet as *mut InletBox)
        .as_ref()
        .and_then(|b| b.inlet.as_ref())
        .and_then(count)
        .map_or(-1, |n| n.min(i64::MAX as u64) as i64)
}

/// How many samples wait for the application.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_samples_available(inlet: lsl_inlet) -> u32 {
    match (inlet as *mut InletBox).as_ref() {
        Some(b) => b
            .inlet
            .as_ref()
            .map(|i| i.samples_available() as u32)
            .unwrap_or(0),
        None => 0,
    }
}

/// Drop everything waiting and report how many went.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_inlet_flush(inlet: lsl_inlet) -> u32 {
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => return 0,
    };
    let mut n = 0usize;
    if let Some(i) = b.inlet.as_mut() {
        loop {
            b.scratch.clear();
            match i.pull_chunk_packed(&mut b.scratch, 4096, Duration::ZERO) {
                Ok(0) | Err(_) => break,
                Ok(got) => n += got,
            }
        }
    }
    n as u32
}

unsafe fn pull_packed_numeric<T>(
    inlet: lsl_inlet,
    buffer: *mut T,
    buffer_elements: c_int,
    format: Format,
    timeout: c_double,
    ec: *mut i32,
) -> c_double {
    if buffer.is_null() {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0.0;
    }
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0.0;
        }
    };
    b.ensure_open(timeout);
    let i = match b.inlet.as_mut() {
        Some(i) => i,
        None => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            return 0.0;
        }
    };
    let cap = buffer_elements.max(0) as usize;
    let width = std::mem::size_of::<T>();
    let dst = std::slice::from_raw_parts_mut(buffer as *mut u8, cap * width);
    match i.pull_numeric_into(format, dst, Duration::from_secs_f64(timeout.max(0.0))) {
        Ok(Some(ts)) => {
            if !ec.is_null() {
                *ec = LSL_NO_ERROR;
            }
            ts
        }
        Ok(None) => {
            if !ec.is_null() {
                *ec = LSL_NO_ERROR;
            }
            0.0
        }
        Err(_) => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            0.0
        }
    }
}

unsafe fn pull_typed(
    inlet: lsl_inlet,
    timeout: c_double,
    ec: *mut i32,
    mut store: impl FnMut(usize, &Value),
    capacity: usize,
) -> c_double {
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0.0;
        }
    };
    // liblsl starts the reader on the first pull. `src/data_receiver.cpp:88`.
    b.ensure_open(timeout);
    let i = match b.inlet.as_mut() {
        Some(i) => i,
        None => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            return 0.0;
        }
    };
    match i.pull(Duration::from_secs_f64(timeout.max(0.0))) {
        Ok(Some(s)) => {
            for (k, v) in s.values.iter().enumerate().take(capacity) {
                store(k, v);
            }
            if !ec.is_null() {
                *ec = LSL_NO_ERROR;
            }
            s.timestamp
        }
        Ok(None) => {
            // liblsl reports a timeout by returning zero with no error.
            if !ec.is_null() {
                *ec = LSL_NO_ERROR;
            }
            0.0
        }
        Err(_) => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            0.0
        }
    }
}

macro_rules! pull_sample {
    ($name:ident, $ty:ty, $fmt:expr) => {
        /// Read one sample.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold `buffer_elements` values.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            inlet: lsl_inlet,
            buffer: *mut $ty,
            buffer_elements: c_int,
            timeout: c_double,
            ec: *mut i32,
        ) -> c_double {
            pull_packed_numeric(inlet, buffer, buffer_elements, $fmt, timeout, ec)
        }
    };
}

pull_sample!(lsl_pull_sample_f, f32, Format::Float32);
pull_sample!(lsl_pull_sample_d, f64, Format::Double64);
pull_sample!(lsl_pull_sample_i, i32, Format::Int32);
pull_sample!(lsl_pull_sample_s, i16, Format::Int16);
pull_sample!(lsl_pull_sample_c, c_char, Format::Int8);
pull_sample!(lsl_pull_sample_l, i64, Format::Int64);

/// Read one sample of strings. The caller frees each string.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold
/// `buffer_elements` pointers.
#[no_mangle]
pub unsafe extern "C" fn lsl_pull_sample_str(
    inlet: lsl_inlet,
    buffer: *mut *mut c_char,
    buffer_elements: c_int,
    timeout: c_double,
    ec: *mut i32,
) -> c_double {
    if buffer.is_null() {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0.0;
    }
    let cap = buffer_elements.max(0) as usize;
    pull_typed(
        inlet,
        timeout,
        ec,
        |k, v| {
            if let Value::Str(b) = v {
                *buffer.add(k) = out_bytes(b);
            }
        },
        cap,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn pull_chunk_packed_numeric<T>(
    inlet: lsl_inlet,
    data: *mut T,
    stamps: *mut c_double,
    data_elements: u64,
    stamp_elements: u64,
    format: Format,
    timeout: c_double,
    ec: *mut i32,
) -> u64 {
    if data.is_null() {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0;
    }
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0;
        }
    };
    let chans = b.info.channel_count as usize;
    b.ensure_open(timeout);
    let i = match b.inlet.as_mut() {
        Some(i) => i,
        None => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            return 0;
        }
    };
    if chans == 0 {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0;
    }
    let room_samples = (data_elements as usize / chans).min(stamp_elements as usize);
    if room_samples == 0 {
        if !ec.is_null() {
            *ec = LSL_NO_ERROR;
        }
        return 0;
    }
    b.scratch.clear();
    let got = match i.pull_chunk_packed(
        &mut b.scratch,
        room_samples,
        Duration::from_secs_f64(timeout.max(0.0)),
    ) {
        Ok(n) => n,
        Err(_) => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            return 0;
        }
    };
    let width = std::mem::size_of::<T>();
    let row_bytes = chans * width;
    let dst = data as *mut u8;
    for (idx, s) in b.scratch.iter().enumerate() {
        if let Some(bytes) = s.as_numeric_bytes(format) {
            let copy_bytes = bytes.len().min(row_bytes);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(idx * row_bytes), copy_bytes);
        }
        if !stamps.is_null() {
            *stamps.add(idx) = s.timestamp;
        }
    }
    if !ec.is_null() {
        *ec = LSL_NO_ERROR;
    }
    (got * chans) as u64
}

/// Read as many whole samples as the buffers hold.
///
/// Returns the number of **values** written, which is the sample count times
/// the channel count. liblsl returns the same number.
unsafe fn pull_chunk_typed(
    inlet: lsl_inlet,
    data_elements: u64,
    stamp_elements: u64,
    stamps: *mut c_double,
    timeout: c_double,
    ec: *mut i32,
    mut store: impl FnMut(usize, &Value),
) -> u64 {
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0;
        }
    };
    let chans = b.info.channel_count as usize;
    // liblsl starts the reader on the first pull. `src/data_receiver.cpp:88`.
    b.ensure_open(timeout);
    let i = match b.inlet.as_mut() {
        Some(i) => i,
        None => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            return 0;
        }
    };
    if chans == 0 {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0;
    }
    let room_samples = (data_elements as usize / chans).min(stamp_elements as usize);
    let mut written = 0usize;
    // The first sample waits for the timeout. Every later one is taken only if
    // it is already there, so a chunk read never waits once per sample.
    let mut wait = Duration::from_secs_f64(timeout.max(0.0));
    while written < room_samples {
        match i.pull(wait) {
            Ok(Some(s)) => {
                for (k, v) in s.values.iter().enumerate().take(chans) {
                    store(written * chans + k, v);
                }
                if !stamps.is_null() {
                    *stamps.add(written) = s.timestamp;
                }
                written += 1;
                wait = Duration::from_millis(0);
            }
            Ok(None) => break,
            Err(_) => {
                if !ec.is_null() {
                    *ec = LSL_LOST_ERROR;
                }
                return (written * chans) as u64;
            }
        }
    }
    if !ec.is_null() {
        *ec = LSL_NO_ERROR;
    }
    (written * chans) as u64
}

macro_rules! pull_chunk_numeric {
    ($name:ident, $ty:ty, $fmt:expr) => {
        /// Read several samples.
        ///
        /// # Safety
        /// The handle must have come from this library, and both buffers must
        /// hold the number of elements that the arguments name.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            inlet: lsl_inlet,
            data: *mut $ty,
            stamps: *mut c_double,
            data_elements: u64,
            stamp_elements: u64,
            timeout: c_double,
            ec: *mut i32,
        ) -> u64 {
            pull_chunk_packed_numeric(
                inlet,
                data,
                stamps,
                data_elements,
                stamp_elements,
                $fmt,
                timeout,
                ec,
            )
        }
    };
}

macro_rules! pull_chunk {
    ($name:ident, $ty:ty, $pat:pat => $expr:expr) => {
        /// Read several samples.
        ///
        /// # Safety
        /// The handle must have come from this library, and both buffers must
        /// hold the number of elements that the arguments name.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            inlet: lsl_inlet,
            data: *mut $ty,
            stamps: *mut c_double,
            data_elements: u64,
            stamp_elements: u64,
            timeout: c_double,
            ec: *mut i32,
        ) -> u64 {
            if data.is_null() {
                if !ec.is_null() {
                    *ec = LSL_ARGUMENT_ERROR;
                }
                return 0;
            }
            pull_chunk_typed(
                inlet,
                data_elements,
                stamp_elements,
                stamps,
                timeout,
                ec,
                |k, v| {
                    if let $pat = v {
                        *data.add(k) = $expr;
                    }
                },
            )
        }
    };
}

pull_chunk_numeric!(lsl_pull_chunk_f, f32, Format::Float32);
pull_chunk_numeric!(lsl_pull_chunk_d, f64, Format::Double64);
pull_chunk_numeric!(lsl_pull_chunk_i, i32, Format::Int32);
pull_chunk_numeric!(lsl_pull_chunk_s, i16, Format::Int16);
pull_chunk_numeric!(lsl_pull_chunk_c, c_char, Format::Int8);
pull_chunk_numeric!(lsl_pull_chunk_l, i64, Format::Int64);
pull_chunk!(lsl_pull_chunk_str, *mut c_char, Value::Str(b) => out_bytes(b));

/// Read several samples of strings, each with its own length.
///
/// The library allocates each string, and the caller frees it with
/// `lsl_destroy_string`. The length buffer is the only way to learn how long a
/// value is, because a string can hold a zero byte.
///
/// # Safety
/// The handle must have come from this library. Both buffers must hold
/// `data_elements` entries, and the timestamp buffer must hold
/// `stamp_elements`.
#[no_mangle]
pub unsafe extern "C" fn lsl_pull_chunk_buf(
    inlet: lsl_inlet,
    data: *mut *mut c_char,
    lengths: *mut u32,
    stamps: *mut c_double,
    data_elements: u64,
    stamp_elements: u64,
    timeout: c_double,
    ec: *mut i32,
) -> u64 {
    if data.is_null() || lengths.is_null() {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0;
    }
    pull_chunk_typed(
        inlet,
        data_elements,
        stamp_elements,
        stamps,
        timeout,
        ec,
        |k, v| {
            let bytes = match v {
                Value::Str(b) => b.clone(),
                other => format!("{other:?}").into_bytes(),
            };
            *data.add(k) = out_bytes(&bytes);
            *lengths.add(k) = bytes.len() as u32;
        },
    )
}

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

/// Point the library at a configuration file.
///
/// The file is read at the first call that needs a value from it, and never
/// again (`src/api_config.cpp:326`). A call after that moment has no effect.
/// The `LSLAPICFG` environment variable wins over this name
/// (`src/api_config.cpp:84`).
///
/// # Safety
/// The string must be zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_set_config_filename(name: *const c_char) {
    labstream_net::config::set_filename(&cstr(name));
}

/// Give the library a configuration as text.
///
/// The text wins over every file. liblsl uses it once and then drops it
/// (`src/api_config.cpp:57-70`).
///
/// # Safety
/// The string must be zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_set_config_content(content: *const c_char) {
    labstream_net::config::set_content(&cstr(content));
}

// ---------------------------------------------------------------------------
// the rest of the outlet and inlet surface
// ---------------------------------------------------------------------------
//
// liblsl exports a plain form and a timestamp form of every push call. Both
// forward to the form that also carries the pushthrough flag, with a timestamp
// of 0.0 and pushthrough on (`src/lsl_outlet_c.cpp:114-120`, and the default
// arguments at `src/stream_outlet_impl.h:122`). A timestamp of 0.0 means the
// current clock. SPEC.md 8.3.
//
// `lsl_cpp.h` reaches these plain names directly, so a C++ application cannot
// load a library that leaves them out.

/// Build the plain form and the timestamp form of one push call.
macro_rules! forward_sample {
    ($plain:ident, $stamped:ident, $target:ident, $ty:ty) => {
        /// Send one sample, timestamped now.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold one value for each channel.
        #[no_mangle]
        pub unsafe extern "C" fn $plain(out: lsl_outlet, data: *const $ty) -> i32 {
            $target(out, data, 0.0, 1)
        }

        /// Send one sample with a timestamp.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold one value for each channel.
        #[no_mangle]
        pub unsafe extern "C" fn $stamped(
            out: lsl_outlet,
            data: *const $ty,
            timestamp: c_double,
        ) -> i32 {
            $target(out, data, timestamp, 1)
        }
    };
}

forward_sample!(
    lsl_push_sample_f,
    lsl_push_sample_ft,
    lsl_push_sample_ftp,
    f32
);
forward_sample!(
    lsl_push_sample_d,
    lsl_push_sample_dt,
    lsl_push_sample_dtp,
    f64
);
forward_sample!(
    lsl_push_sample_i,
    lsl_push_sample_it,
    lsl_push_sample_itp,
    i32
);
forward_sample!(
    lsl_push_sample_s,
    lsl_push_sample_st,
    lsl_push_sample_stp,
    i16
);
forward_sample!(
    lsl_push_sample_l,
    lsl_push_sample_lt,
    lsl_push_sample_ltp,
    i64
);
forward_sample!(
    lsl_push_sample_c,
    lsl_push_sample_ct,
    lsl_push_sample_ctp,
    c_char
);
forward_sample!(
    lsl_push_sample_str,
    lsl_push_sample_strt,
    lsl_push_sample_strtp,
    *const c_char
);

/// Build the plain, timestamp, and timestamp-list forms of one chunk call.
macro_rules! forward_chunk {
    ($plain:ident, $stamped:ident, $each:ident, $tp:ident, $tnp:ident, $ty:ty) => {
        /// Send several samples, timestamped now.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold `data_elements` values.
        #[no_mangle]
        pub unsafe extern "C" fn $plain(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
        ) -> i32 {
            $tp(out, data, data_elements, 0.0, 1)
        }

        /// Send several samples that share one timestamp.
        ///
        /// # Safety
        /// The handle must have come from this library, and the buffer must
        /// hold `data_elements` values.
        #[no_mangle]
        pub unsafe extern "C" fn $stamped(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
            timestamp: c_double,
        ) -> i32 {
            $tp(out, data, data_elements, timestamp, 1)
        }

        /// Send several samples, each with its own timestamp.
        ///
        /// # Safety
        /// The handle must have come from this library, the data buffer must
        /// hold `data_elements` values, and the timestamp buffer must hold one
        /// value for each sample.
        #[no_mangle]
        pub unsafe extern "C" fn $each(
            out: lsl_outlet,
            data: *const $ty,
            data_elements: u64,
            timestamps: *const c_double,
        ) -> i32 {
            $tnp(out, data, data_elements, timestamps, 1)
        }
    };
}

forward_chunk!(
    lsl_push_chunk_f,
    lsl_push_chunk_ft,
    lsl_push_chunk_ftn,
    lsl_push_chunk_ftp,
    lsl_push_chunk_ftnp,
    f32
);
forward_chunk!(
    lsl_push_chunk_d,
    lsl_push_chunk_dt,
    lsl_push_chunk_dtn,
    lsl_push_chunk_dtp,
    lsl_push_chunk_dtnp,
    f64
);
forward_chunk!(
    lsl_push_chunk_i,
    lsl_push_chunk_it,
    lsl_push_chunk_itn,
    lsl_push_chunk_itp,
    lsl_push_chunk_itnp,
    i32
);
forward_chunk!(
    lsl_push_chunk_s,
    lsl_push_chunk_st,
    lsl_push_chunk_stn,
    lsl_push_chunk_stp,
    lsl_push_chunk_stnp,
    i16
);
forward_chunk!(
    lsl_push_chunk_l,
    lsl_push_chunk_lt,
    lsl_push_chunk_ltn,
    lsl_push_chunk_ltp,
    lsl_push_chunk_ltnp,
    i64
);
forward_chunk!(
    lsl_push_chunk_c,
    lsl_push_chunk_ct,
    lsl_push_chunk_ctn,
    lsl_push_chunk_ctp,
    lsl_push_chunk_ctnp,
    c_char
);
forward_chunk!(
    lsl_push_chunk_str,
    lsl_push_chunk_strt,
    lsl_push_chunk_strtn,
    lsl_push_chunk_strtp,
    lsl_push_chunk_strtnp,
    *const c_char
);

// --- the untyped form --------------------------------------------------

/// Send one sample from a raw buffer.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold one
/// value of the stream format for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_vtp(
    out: lsl_outlet,
    data: *const c_void,
    timestamp: c_double,
    _pushthrough: c_int,
) -> i32 {
    let b = match (out as *mut OutletBox).as_ref() {
        Some(b) => b,
        None => return LSL_ARGUMENT_ERROR,
    };
    let width = match b.info.format.width() {
        Some(w) => w,
        None => return LSL_ARGUMENT_ERROR,
    };
    if data.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let t = if timestamp == 0.0 {
        labstream_net::clock()
    } else {
        timestamp
    };
    let byte_len = width * b.info.channel_count as usize;
    let raw = std::slice::from_raw_parts(data as *const u8, byte_len);
    b.outlet.push_packed(PackedSample::from_raw_bytes(
        t,
        b.info.format,
        Arc::from(raw),
    ));
    LSL_NO_ERROR
}

/// Send one sample from a raw buffer, timestamped now.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold one
/// value of the stream format for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_v(out: lsl_outlet, data: *const c_void) -> i32 {
    lsl_push_sample_vtp(out, data, 0.0, 1)
}

/// Send one sample from a raw buffer, with a timestamp.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold one
/// value of the stream format for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_vt(
    out: lsl_outlet,
    data: *const c_void,
    timestamp: c_double,
) -> i32 {
    lsl_push_sample_vtp(out, data, timestamp, 1)
}

/// Read one sample into a raw buffer.
///
/// # Safety
/// The handle must have come from this library, and the buffer must hold
/// `buffer_bytes` bytes.
#[no_mangle]
pub unsafe extern "C" fn lsl_pull_sample_v(
    inlet: lsl_inlet,
    buffer: *mut c_void,
    buffer_bytes: c_int,
    timeout: c_double,
    ec: *mut i32,
) -> c_double {
    let b = match (inlet as *mut InletBox).as_mut() {
        Some(b) => b,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0.0;
        }
    };
    let format = b.info.format;
    let width = match format.width() {
        Some(w) => w,
        None => {
            if !ec.is_null() {
                *ec = LSL_ARGUMENT_ERROR;
            }
            return 0.0;
        }
    };
    let need = width * b.info.channel_count as usize;
    if buffer.is_null() || buffer_bytes < 0 || (buffer_bytes as usize) < need {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0.0;
    }
    b.ensure_open(timeout);
    let i = match b.inlet.as_mut() {
        Some(i) => i,
        None => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            return 0.0;
        }
    };
    match i.pull_packed(Duration::from_secs_f64(timeout.max(0.0))) {
        Ok(Some(s)) => {
            if let Some(bytes) = s.as_numeric_bytes(format) {
                let copy_bytes = bytes.len().min(need);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, copy_bytes);
            }
            if !ec.is_null() {
                *ec = LSL_NO_ERROR;
            }
            s.timestamp
        }
        Ok(None) => {
            if !ec.is_null() {
                *ec = LSL_NO_ERROR;
            }
            0.0
        }
        Err(_) => {
            if !ec.is_null() {
                *ec = LSL_LOST_ERROR;
            }
            0.0
        }
    }
}

// --- the length-prefixed string form ------------------------------------

/// Send one sample of strings, each with its own length.
///
/// This is the only way to send a string that holds a zero byte.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold one
/// entry for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_buftp(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    timestamp: c_double,
    pushthrough: c_int,
) -> i32 {
    if data.is_null() || lengths.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    push_typed(out, |i| buf_value(data, lengths, i), timestamp, pushthrough)
}

/// Read one string value out of a data buffer and a length buffer.
///
/// # Safety
/// Both buffers must hold an entry at `index`, and the data entry must hold
/// the number of bytes that the length entry names.
unsafe fn buf_value(data: *const *const c_char, lengths: *const u32, index: usize) -> Value {
    let p = *data.add(index);
    let n = *lengths.add(index) as usize;
    if p.is_null() || n == 0 {
        return Value::Str(Vec::new());
    }
    Value::Str(std::slice::from_raw_parts(p as *const u8, n).to_vec())
}

/// Send one sample of strings with lengths, timestamped now.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold one
/// entry for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_buf(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
) -> i32 {
    lsl_push_sample_buftp(out, data, lengths, 0.0, 1)
}

/// Send one sample of strings with lengths, with a timestamp.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold one
/// entry for each channel.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_sample_buft(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    timestamp: c_double,
) -> i32 {
    lsl_push_sample_buftp(out, data, lengths, timestamp, 1)
}

/// Send several samples of strings with lengths, sharing one timestamp.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold
/// `data_elements` entries.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_chunk_buftp(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    data_elements: u64,
    timestamp: c_double,
    pushthrough: c_int,
) -> i32 {
    if data.is_null() || lengths.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    push_chunk_typed(
        out,
        data_elements,
        |i| buf_value(data, lengths, i),
        timestamp,
        pushthrough,
    )
}

/// Send several samples of strings with lengths, each with its own timestamp.
///
/// # Safety
/// The handle must have come from this library, both buffers must hold
/// `data_elements` entries, and the timestamp buffer must hold one value for
/// each sample.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_chunk_buftnp(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    data_elements: u64,
    timestamps: *const c_double,
    pushthrough: c_int,
) -> i32 {
    if data.is_null() || lengths.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    push_chunk_stamped(
        out,
        data_elements,
        |i| buf_value(data, lengths, i),
        timestamps,
        pushthrough,
    )
}

/// Send several samples of strings with lengths, timestamped now.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold
/// `data_elements` entries.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_chunk_buf(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    data_elements: u64,
) -> i32 {
    lsl_push_chunk_buftp(out, data, lengths, data_elements, 0.0, 1)
}

/// Send several samples of strings with lengths, sharing one timestamp.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold
/// `data_elements` entries.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_chunk_buft(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    data_elements: u64,
    timestamp: c_double,
) -> i32 {
    lsl_push_chunk_buftp(out, data, lengths, data_elements, timestamp, 1)
}

/// Send several samples of strings with lengths, each with its own timestamp.
///
/// # Safety
/// The handle must have come from this library, both buffers must hold
/// `data_elements` entries, and the timestamp buffer must hold one value for
/// each sample.
#[no_mangle]
pub unsafe extern "C" fn lsl_push_chunk_buftn(
    out: lsl_outlet,
    data: *const *const c_char,
    lengths: *const u32,
    data_elements: u64,
    timestamps: *const c_double,
) -> i32 {
    lsl_push_chunk_buftnp(out, data, lengths, data_elements, timestamps, 1)
}

/// Read one sample of strings, with a length for each one.
///
/// The library allocates each string. The caller frees it with
/// `lsl_destroy_string`. liblsl copies the bytes with no zero terminator
/// (`src/lsl_inlet_c.cpp:157-160`), so the length buffer is the only way to
/// know how long a value is.
///
/// # Safety
/// The handle must have come from this library, and both buffers must hold
/// `buffer_elements` entries.
#[no_mangle]
pub unsafe extern "C" fn lsl_pull_sample_buf(
    inlet: lsl_inlet,
    buffer: *mut *mut c_char,
    buffer_lengths: *mut u32,
    buffer_elements: c_int,
    timeout: c_double,
    ec: *mut i32,
) -> c_double {
    if buffer.is_null() || buffer_lengths.is_null() {
        if !ec.is_null() {
            *ec = LSL_ARGUMENT_ERROR;
        }
        return 0.0;
    }
    let room = buffer_elements.max(0) as usize;
    pull_typed(
        inlet,
        timeout,
        ec,
        |k, v| {
            let bytes = match v {
                Value::Str(b) => b.clone(),
                other => format!("{other:?}").into_bytes(),
            };
            // liblsl copies the bytes with no terminator and reports the
            // length beside them. One is added here as well, so a caller that
            // treats the result as a C string still reads something valid.
            *buffer.add(k) = out_bytes(&bytes);
            *buffer_lengths.add(k) = bytes.len() as u32;
        },
        room,
    )
}

// --- the rest of the description and the inlet --------------------------

/// The text of the last error on this thread.
///
/// **liblsl always returns an empty string here.** `src/common.cpp:57` declares
/// a buffer of zeros inside the function and returns it, and nothing in the
/// library writes to that buffer. An implementation that returned a real
/// message would not match.
#[no_mangle]
pub extern "C" fn lsl_last_error() -> *const c_char {
    static_string("")
}

/// Build a description from a document.
///
/// A document that the reader refuses leaves a blank description whose name
/// holds the reason. SPEC.md 12.5.
///
/// # Safety
/// The string must be zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_streaminfo_from_xml(xml: *const c_char) -> lsl_streaminfo {
    let text = cstr(xml);
    match StreamInfo::from_fullinfo_xml(&text) {
        Ok(info) => InfoBox::new(info) as lsl_streaminfo,
        Err(reason) => {
            let mut blank = StreamInfo::new("", "", 0, Format::Undefined, 0.0);
            blank.hostname = String::new();
            blank.name = format!("(invalid: {reason})");
            InfoBox::new(blank) as lsl_streaminfo
        }
    }
}

/// Test a query against a description.
///
/// This covers the subset that a resolver builds, and no more. A query outside
/// the subset returns false. SPEC.md 2.4.
///
/// # Safety
/// The handle must have come from this library, and the string must be
/// zero-terminated.
#[no_mangle]
pub unsafe extern "C" fn lsl_stream_info_matches_query(
    info: lsl_streaminfo,
    query: *const c_char,
) -> i32 {
    match (info as *mut InfoBox).as_ref() {
        Some(b) => b.info.matches(&cstr(query)) as i32,
        None => 0,
    }
}

/// Give the description a new instance identifier and return it.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_reset_uid(info: lsl_streaminfo) -> *const c_char {
    match (info as *mut InfoBox).as_mut() {
        Some(b) => {
            b.info.uid = labstream_net::make_uid();
            b.sync();
            static_string(&b.info.uid)
        }
        None => static_string(""),
    }
}

/// Set the half-time of the smoothing filter, in seconds.
///
/// liblsl tests nothing here. `src/time_postprocessor.h:63` assigns the value
/// as it stands, and `src/lsl_inlet_c.cpp:298` always reports no error. A
/// value of zero or below is therefore accepted. Nothing on the wire carries
/// this number.
///
/// # Safety
/// The handle must have come from this library.
#[no_mangle]
pub unsafe extern "C" fn lsl_smoothing_halftime(inlet: lsl_inlet, value: f32) -> i32 {
    match (inlet as *mut InletBox).as_mut() {
        Some(b) => {
            b.halftime = value;
            LSL_NO_ERROR
        }
        None => LSL_ARGUMENT_ERROR,
    }
}

/// The clock offset, with the remote clock reading and the uncertainty.
///
/// # Safety
/// The handle must have come from this library. Each output pointer must be
/// null or hold room for one value.
#[no_mangle]
pub unsafe extern "C" fn lsl_time_correction_ex(
    inlet: lsl_inlet,
    remote_time: *mut c_double,
    uncertainty: *mut c_double,
    timeout: c_double,
    ec: *mut i32,
) -> c_double {
    let offset = lsl_time_correction(inlet, timeout, ec);
    if let Some(b) = (inlet as *mut InletBox).as_ref() {
        if let Some(i) = b.inlet.as_ref() {
            if !uncertainty.is_null() {
                *uncertainty = i.time_uncertainty().unwrap_or(0.0);
            }
            if !remote_time.is_null() {
                // The remote clock reading that belongs to this offset is the
                // local clock moved by it.
                *remote_time = labstream_net::clock() - offset;
            }
            return offset;
        }
    }
    if !uncertainty.is_null() {
        *uncertainty = 0.0;
    }
    if !remote_time.is_null() {
        *remote_time = 0.0;
    }
    offset
}

/// Gap kind for samples lost on the outlet or network (`GapKind::Lost`).
pub const LABSTREAM_GAP_LOST: i32 = 0;
/// Gap kind for samples dropped in the local inlet queue (`GapKind::Dropped`).
pub const LABSTREAM_GAP_DROPPED: i32 = 1;

/// Snapshot of the health of an inlet, for the C ABI (`docs/health.md`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct labstream_inlet_health {
    /// Measured arrival latency in seconds, or `NaN` when unknown.
    pub latency: f64,
    /// Round-trip time of the latest clock offset estimate in seconds, or `NaN`
    /// when unknown.
    pub clock_uncertainty: f64,
    /// Samples never delivered (`seq/1`), or `-1` when unknown.
    pub lost: i64,
    /// Samples dropped by this inlet's local buffer.
    pub dropped: u64,
    /// Samples delivered from the `resume/1` history, or `-1` when unknown.
    pub resent: i64,
    /// Number of times the connection was rebuilt.
    pub recoveries: u64,
    /// Samples currently waiting in the inlet buffer.
    pub buffered: u64,
    /// Capacity of the inlet buffer, in samples.
    pub buffer_capacity: u64,
    /// Number of gaps currently recorded on this inlet.
    pub gap_count: u64,
    /// `1` while connected, `0` otherwise.
    pub connected: i32,
    /// `1` while samples arrive over UDP (`dgram/1`), `0` otherwise.
    pub on_udp: i32,
}

/// One missing interval in a stream, for the C ABI (`docs/health.md`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct labstream_gap {
    /// Timestamp at the start of the gap.
    pub start: f64,
    /// Timestamp of the first sample after the gap.
    pub end: f64,
    /// Number of missing samples in the gap.
    pub count: u64,
    /// [`LABSTREAM_GAP_LOST`] or [`LABSTREAM_GAP_DROPPED`].
    pub kind: i32,
}

/// Snapshot of one consumer queue on an outlet, for the C ABI (`docs/health.md`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct labstream_consumer_health {
    /// Monotonic identifier assigned when the consumer connected.
    pub id: u64,
    /// Samples currently waiting in this consumer's queue.
    pub fill: u64,
    /// Maximum capacity of this consumer's queue, in samples.
    pub capacity: u64,
    /// Samples dropped from this consumer's queue.
    pub dropped: u64,
}

fn gap_to_c(g: labstream_net::Gap) -> labstream_gap {
    labstream_gap {
        start: g.start,
        end: g.end,
        count: g.count,
        kind: match g.kind {
            labstream_net::GapKind::Lost => LABSTREAM_GAP_LOST,
            labstream_net::GapKind::Dropped => LABSTREAM_GAP_DROPPED,
        },
    }
}

/// Fill `out` with the health snapshot of `inlet`.
///
/// Returns `LSL_NO_ERROR` on success, or `LSL_ARGUMENT_ERROR` when `inlet` or
/// `out` is null. When the inlet has not opened its stream yet, unknown floats
/// are `NaN`, unknown counts are `-1`, and `connected` is `0`.
///
/// # Safety
/// `inlet` must be a valid handle from this library and `out` must point to
/// writable storage for one `labstream_inlet_health`.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_health(
    inlet: lsl_inlet,
    out: *mut labstream_inlet_health,
) -> i32 {
    if out.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return LSL_ARGUMENT_ERROR;
    };
    let value = match b.inlet.as_ref() {
        Some(i) => {
            let h = i.health();
            labstream_inlet_health {
                latency: h.latency.unwrap_or(f64::NAN),
                clock_uncertainty: h.clock_uncertainty.unwrap_or(f64::NAN),
                lost: h.lost.map_or(-1, |n| n as i64),
                dropped: h.dropped,
                resent: h.resent.map_or(-1, |n| n as i64),
                recoveries: h.recoveries,
                buffered: h.buffered as u64,
                buffer_capacity: h.buffer_capacity as u64,
                gap_count: h.gap_count as u64,
                connected: i32::from(h.connected),
                on_udp: i32::from(h.on_udp),
            }
        }
        None => labstream_inlet_health {
            latency: f64::NAN,
            clock_uncertainty: f64::NAN,
            lost: -1,
            dropped: 0,
            resent: -1,
            recoveries: 0,
            buffered: 0,
            buffer_capacity: b.buffer_samples.max(0) as u64,
            gap_count: 0,
            connected: 0,
            on_udp: 0,
        },
    };
    *out = value;
    LSL_NO_ERROR
}

/// Copy recorded gaps from `inlet` into `out` (up to `max_gaps`).
///
/// Pass null for `out` to query the total number of recorded gaps without
/// copying. Returns the count written (or total count when `out` is null), or
/// `LSL_ARGUMENT_ERROR` when `inlet` is null.
///
/// # Safety
/// `inlet` must be a valid handle from this library. If `out` is non-null, it
/// must hold room for `max_gaps` entries.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_gaps(
    inlet: lsl_inlet,
    out: *mut labstream_gap,
    max_gaps: u32,
) -> i32 {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return LSL_ARGUMENT_ERROR;
    };
    let Some(i) = b.inlet.as_ref() else {
        return 0;
    };
    let gaps = i.gaps();
    if out.is_null() {
        return gaps.len().min(i32::MAX as usize) as i32;
    }
    let n = gaps.len().min(max_gaps as usize);
    for (k, g) in gaps.into_iter().take(n).enumerate() {
        *out.add(k) = gap_to_c(g);
    }
    n as i32
}

/// Drain finalized gaps from `inlet` into `out` (up to `max_gaps`).
///
/// Returns the count written, or `LSL_ARGUMENT_ERROR` when `inlet` or `out` is
/// null.
///
/// # Safety
/// `inlet` must be a valid handle from this library and `out` must hold room
/// for `max_gaps` entries.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_take_gaps(
    inlet: lsl_inlet,
    out: *mut labstream_gap,
    max_gaps: u32,
) -> i32 {
    if out.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return LSL_ARGUMENT_ERROR;
    };
    let Some(i) = b.inlet.as_ref() else {
        return 0;
    };
    let gaps = i.take_gaps();
    let n = gaps.len().min(max_gaps as usize);
    for (k, g) in gaps.into_iter().take(n).enumerate() {
        *out.add(k) = gap_to_c(g);
    }
    n as i32
}

/// Copy consumer queue health snapshots from `outlet` into `out` (up to
/// `max_consumers`).
///
/// Pass null for `out` to query the number of connected consumers without
/// copying. Returns the count written (or total consumer count when `out` is
/// null), or `LSL_ARGUMENT_ERROR` when `outlet` is null.
///
/// # Safety
/// `outlet` must be a valid handle from this library. If `out` is non-null, it
/// must hold room for `max_consumers` entries.
#[no_mangle]
pub unsafe extern "C" fn labstream_outlet_consumers(
    outlet: lsl_outlet,
    out: *mut labstream_consumer_health,
    max_consumers: u32,
) -> i32 {
    let Some(b) = (outlet as *mut OutletBox).as_ref() else {
        return LSL_ARGUMENT_ERROR;
    };
    let health = b.outlet.health();
    if out.is_null() {
        return health.consumers.len().min(i32::MAX as usize) as i32;
    }
    let n = health.consumers.len().min(max_consumers as usize);
    for (k, c) in health.consumers.into_iter().take(n).enumerate() {
        *out.add(k) = labstream_consumer_health {
            id: c.id,
            fill: c.fill as u64,
            capacity: c.capacity as u64,
            dropped: c.dropped,
        };
    }
    n as i32
}

/// Bytes currently held by the `resume` history of `outlet` in RAM.
///
/// Returns `-1` when `outlet` is null.
///
/// # Safety
/// `outlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_outlet_history_ram_bytes(outlet: lsl_outlet) -> i64 {
    let Some(b) = (outlet as *mut OutletBox).as_ref() else {
        return -1;
    };
    b.outlet.history_ram_bytes().min(i64::MAX as usize) as i64
}

/// Bytes currently held by the `resume` history of `outlet` on disk
/// (`labstream.ResumeDisk`, `docs/roadmap.md`, step 3).
///
/// Returns `-1` when `outlet` is null, or `0` when disk spooling is off or
/// has not spilled yet.
///
/// # Safety
/// `outlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_outlet_history_disk_bytes(outlet: lsl_outlet) -> i64 {
    let Some(b) = (outlet as *mut OutletBox).as_ref() else {
        return -1;
    };
    b.outlet.history_disk_bytes().min(i64::MAX as u64) as i64
}

/// True (`1`) when `inlet` is actively receiving samples over shared memory
/// (`shm/1`, `docs/roadmap.md`, step 9), or `0` otherwise.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_on_shm(inlet: lsl_inlet) -> i32 {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return 0;
    };
    b.inlet.as_ref().map_or(0, |i| i32::from(i.on_shm()))
}

/// True (`1`) when `inlet` negotiated lossless wire compression (`comp/1`,
/// `docs/roadmap.md`, step 7), or `0` otherwise.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_compressed(inlet: lsl_inlet) -> i32 {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return 0;
    };
    b.inlet.as_ref().map_or(0, |i| i32::from(i.is_compressed()))
}

/// Observed wire-to-raw byte ratio (`wire_bytes / raw_bytes`) for a `comp/1`
/// inlet, or `NaN` when compression is inactive or no samples have arrived.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_compression_ratio(inlet: lsl_inlet) -> c_double {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return f64::NAN;
    };
    b.inlet
        .as_ref()
        .and_then(|i| i.compression_ratio())
        .unwrap_or(f64::NAN)
}

/// Opt-in post-processing flag for the linear offset-and-drift clock model
/// (`docs/roadmap.md`, step 17). Pass to `lsl_set_postprocessing`.
pub const LABSTREAM_POST_DRIFT: u32 = 16;

/// Opt-in post-processing flag for device-delay subtraction
/// (`docs/roadmap.md`, step 15). Pass to `lsl_set_postprocessing`.
pub const LABSTREAM_POST_DEVICE_DELAY: u32 = 32;

/// Estimated relative clock drift (`d(offset)/d(t_local)` in s/s) for `inlet`
/// (`docs/roadmap.md`, step 17), or `NaN` before the first time-sync burst.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_clock_drift(inlet: lsl_inlet) -> c_double {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return f64::NAN;
    };
    b.inlet
        .as_ref()
        .and_then(|i| i.clock_drift())
        .unwrap_or(f64::NAN)
}

/// Smooth offset `local - remote` evaluated for `remote_ts` using the opt-in
/// drift clock (`docs/roadmap.md`, step 17), or `NaN` before the first burst.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_smooth_time_correction(
    inlet: lsl_inlet,
    remote_ts: c_double,
) -> c_double {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return f64::NAN;
    };
    b.inlet
        .as_ref()
        .and_then(|i| i.smooth_time_correction(remote_ts))
        .unwrap_or(f64::NAN)
}

/// Store a calibrated device delay (and optional RMS jitter; pass `NaN` to
/// omit) in `<desc><synchronization><offset_mean>` (`docs/roadmap.md`,
/// step 15).
///
/// # Safety
/// `info` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_streaminfo_set_device_delay(
    info: lsl_streaminfo,
    delay_s: c_double,
    rms_s: c_double,
) -> i32 {
    let Some(b) = (info as *mut InfoBox).as_mut() else {
        return LSL_ARGUMENT_ERROR;
    };
    if !delay_s.is_finite() {
        return LSL_ARGUMENT_ERROR;
    }
    let desc = lsl_get_desc(info);
    // Remove any existing `<synchronization>` child from the authoritative tree.
    let sync_name = b"synchronization\0".as_ptr() as *const c_char;
    while !lsl_child(desc, sync_name).is_null() {
        lsl_remove_child_n(desc, sync_name);
    }
    let sync = lsl_append_child(desc, sync_name);
    if let Ok(mean_c) = CString::new(format!("{delay_s:.6}")) {
        lsl_append_child_value(
            sync,
            b"offset_mean\0".as_ptr() as *const c_char,
            mean_c.as_ptr(),
        );
    }
    if rms_s.is_finite() {
        if let Ok(rms_c) = CString::new(format!("{rms_s:.6}")) {
            lsl_append_child_value(
                sync,
                b"offset_rms\0".as_ptr() as *const c_char,
                rms_c.as_ptr(),
            );
        }
    }
    b.sync();
    LSL_NO_ERROR
}

/// Read the calibrated device delay (in seconds) from
/// `<desc><synchronization><offset_mean>`, or `NaN` if absent.
///
/// # Safety
/// `info` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_streaminfo_device_delay(info: lsl_streaminfo) -> c_double {
    let Some(b) = (info as *mut InfoBox).as_mut() else {
        return f64::NAN;
    };
    b.sync();
    b.info.device_delay().unwrap_or(f64::NAN)
}

/// Set an explicit calibrated device delay (in seconds) on `inlet`, subtracted
/// when `LABSTREAM_POST_DEVICE_DELAY` is enabled (`docs/roadmap.md`, step 15).
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_set_device_delay(
    inlet: lsl_inlet,
    delay_s: c_double,
) -> i32 {
    let Some(b) = (inlet as *mut InletBox).as_mut() else {
        return LSL_ARGUMENT_ERROR;
    };
    if !delay_s.is_finite() {
        return LSL_ARGUMENT_ERROR;
    }
    if let Some(i) = b.inlet.as_mut() {
        i.set_device_delay(delay_s);
    }
    LSL_NO_ERROR
}

/// Return the device delay (in seconds) configured on `inlet`, or `0.0` when
/// not open or unset.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_device_delay(inlet: lsl_inlet) -> c_double {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return 0.0;
    };
    b.inlet.as_ref().map_or(0.0, |i| i.device_delay())
}

/// Emit a live metadata update (`key`, `value`, `timestamp`; `0.0` uses local
/// clock) to all connected `meta/1` consumers (`docs/roadmap.md`, step 16).
///
/// # Safety
/// `outlet` must be a valid handle from this library, and `key` and `value`
/// must be valid null-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn labstream_outlet_push_metadata(
    outlet: lsl_outlet,
    key: *const c_char,
    value: *const c_char,
    timestamp: c_double,
) -> i32 {
    let Some(b) = (outlet as *mut OutletBox).as_ref() else {
        return LSL_ARGUMENT_ERROR;
    };
    if key.is_null() || value.is_null() {
        return LSL_ARGUMENT_ERROR;
    }
    let Ok(k) = CStr::from_ptr(key).to_str() else {
        return LSL_ARGUMENT_ERROR;
    };
    let Ok(v) = CStr::from_ptr(value).to_str() else {
        return LSL_ARGUMENT_ERROR;
    };
    b.outlet.push_metadata_at(k, v, timestamp);
    LSL_NO_ERROR
}

/// Return `1` when `inlet` negotiated `meta/1` live metadata, or `0` otherwise.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_has_live_metadata(inlet: lsl_inlet) -> i32 {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return 0;
    };
    i32::from(b.inlet.as_ref().is_some_and(|i| i.has_live_metadata()))
}

/// Return the number of pending unread live metadata updates on `inlet`, or
/// `-1` if `inlet` is null or not open.
///
/// # Safety
/// `inlet` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_metadata_count(inlet: lsl_inlet) -> i64 {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return -1;
    };
    b.inlet
        .as_ref()
        .map_or(-1, |i| i.pending_metadata_count() as i64)
}

/// Pop the oldest pending live metadata update from `inlet`.
///
/// Returns `1` when an update was written into the output buffers, `0` when no
/// update is pending, or a negative LSL error code on invalid arguments.
/// `key_buf` and `val_buf` are null-terminated (truncated if smaller than the
/// string + NUL). `timestamp_out` and `seq_out` may be null.
///
/// # Safety
/// `inlet` must be a valid handle from this library. Non-null output buffers
/// must be writable for `key_buf_len` / `val_buf_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn labstream_inlet_pull_metadata(
    inlet: lsl_inlet,
    key_buf: *mut c_char,
    key_buf_len: u32,
    val_buf: *mut c_char,
    val_buf_len: u32,
    timestamp_out: *mut c_double,
    seq_out: *mut u64,
) -> i32 {
    let Some(b) = (inlet as *mut InletBox).as_ref() else {
        return LSL_ARGUMENT_ERROR;
    };
    let Some(i) = b.inlet.as_ref() else {
        return 0;
    };
    let Some(upd) = i.pull_metadata_one() else {
        return 0;
    };
    let copy_cstr = |src: &str, dst: *mut c_char, cap: u32| {
        if dst.is_null() || cap == 0 {
            return;
        }
        let max_copy = (cap as usize - 1).min(src.len());
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, max_copy);
        *dst.add(max_copy) = 0;
    };
    copy_cstr(&upd.key, key_buf, key_buf_len);
    copy_cstr(&upd.value, val_buf, val_buf_len);
    if !timestamp_out.is_null() {
        *timestamp_out = upd.timestamp;
    }
    if !seq_out.is_null() {
        *seq_out = upd.seq;
    }
    1
}

/// True (`1`) when `info` describes a stream published by a `labstream` relay,
/// or `0` otherwise (`docs/roadmap.md`, step 4).
///
/// # Safety
/// `info` must be null or a valid handle from this library.
#[no_mangle]
pub unsafe extern "C" fn labstream_streaminfo_is_relayed(info: lsl_streaminfo) -> i32 {
    let Some(b) = (info as *mut InfoBox).as_mut() else {
        return 0;
    };
    b.sync();
    i32::from(labstream_net::relay::is_relayed(&b.info))
}

/// Opaque handle to a running opt-in WebSocket bridge (`docs/roadmap.md`, step 13).
#[allow(non_camel_case_types)]
pub type labstream_ws_bridge = *mut c_void;

/// Start an opt-in WebSocket bridge (`docs/roadmap.md`, step 13).
///
/// - `bind_addr`: socket address such as `"127.0.0.1:8765"` (null defaults to `"127.0.0.1:8765"`).
/// - `allow_csv`: optional comma-separated non-loopback peer IPs or `"*"` (null = loopback only).
/// - `session_id`: optional LSL session identifier (null = active configuration).
///
/// Returns null on invalid arguments or bind error. Free with
/// [`labstream_ws_bridge_stop`].
///
/// # Safety
/// Any non-null pointer must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn labstream_ws_bridge_start(
    bind_addr: *const c_char,
    allow_csv: *const c_char,
    session_id: *const c_char,
) -> labstream_ws_bridge {
    let mut opts = labstream_net::ws::WsOptions::default();
    if !bind_addr.is_null() {
        let Ok(s) = CStr::from_ptr(bind_addr).to_str() else {
            return std::ptr::null_mut();
        };
        let Ok(addr) = s.parse() else {
            return std::ptr::null_mut();
        };
        opts.bind = addr;
    }
    if !allow_csv.is_null() {
        let Ok(s) = CStr::from_ptr(allow_csv).to_str() else {
            return std::ptr::null_mut();
        };
        for item in s.split(',') {
            let t = item.trim();
            if t.is_empty() {
                continue;
            }
            if t == "*" {
                opts.allow_any = true;
            } else if let Ok(ip) = t.parse() {
                opts.allowed_peers.push(ip);
            } else {
                return std::ptr::null_mut();
            }
        }
    }
    if !session_id.is_null() {
        let Ok(s) = CStr::from_ptr(session_id).to_str() else {
            return std::ptr::null_mut();
        };
        if !s.is_empty() {
            opts.session_id = Some(s.to_string());
        }
    }
    match labstream_net::ws::WsBridge::start(opts) {
        Ok(b) => Box::into_raw(Box::new(b)) as labstream_ws_bridge,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Return the bound TCP port of a running WebSocket bridge, or `0` if null.
///
/// # Safety
/// `bridge` must be null or a valid handle from [`labstream_ws_bridge_start`].
#[no_mangle]
pub unsafe extern "C" fn labstream_ws_bridge_port(bridge: labstream_ws_bridge) -> u16 {
    let Some(b) = (bridge as *const labstream_net::ws::WsBridge).as_ref() else {
        return 0;
    };
    b.local_addr().port()
}

/// Stop and destroy a WebSocket bridge returned by [`labstream_ws_bridge_start`].
///
/// # Safety
/// `bridge` must be null or a valid handle that has not yet been stopped.
#[no_mangle]
pub unsafe extern "C" fn labstream_ws_bridge_stop(bridge: labstream_ws_bridge) {
    if !bridge.is_null() {
        drop(Box::from_raw(bridge as *mut labstream_net::ws::WsBridge));
    }
}

/// Run network and discovery diagnostics and return the report as JSON.
///
/// Pass null or an empty string for `peers` to skip remote peer checks, or a
/// comma-separated list of IP addresses or hostnames. Pass a positive
/// `timeout` in seconds, or zero to use the default (1.5 s).
///
/// The caller frees the returned string with `lsl_destroy_string`.
///
/// # Safety
/// `peers` must be null or a valid zero-terminated string.
#[no_mangle]
pub unsafe extern "C" fn labstream_doctor_json(
    peers: *const c_char,
    timeout: c_double,
) -> *mut c_char {
    let mut opts = labstream_net::doctor::Options::default();
    if timeout.is_finite() && timeout > 0.0 {
        opts.timeout = Duration::from_secs_f64(timeout);
    }
    let raw_peers = cstr(peers);
    for peer in raw_peers.split(',') {
        let trimmed = peer.trim();
        if !trimmed.is_empty() {
            opts.peers.push(trimmed.to_string());
        }
    }
    let report = labstream_net::doctor::diagnose(&opts);
    out_string(&report.to_json())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_json_returns_report_and_frees_cleanly() {
        unsafe {
            let ptr = labstream_doctor_json(std::ptr::null(), 0.2);
            assert!(!ptr.is_null());
            let json = cstr(ptr);
            lsl_destroy_string(ptr);
            assert!(json.starts_with('{') && json.ends_with('}'));
            assert!(json.contains("\"checks\":"));
            assert!(json.contains("\"fallback_discovery\":"));
        }
    }

    #[test]
    fn a_count_is_unknown_for_no_inlet_and_for_a_closed_one() {
        unsafe {
            assert_eq!(labstream_samples_lost(std::ptr::null_mut()), -1);
            assert_eq!(labstream_samples_resent(std::ptr::null_mut()), -1);
            let info = lsl_create_streaminfo(
                b"CapiCount0\0".as_ptr() as *const c_char,
                b"Test\0".as_ptr() as *const c_char,
                1,
                100.0,
                1,
                b"capi0\0".as_ptr() as *const c_char,
            );
            // The inlet has not opened its stream yet.
            let inlet = lsl_create_inlet(info, 360, 0, 1);
            assert_eq!(labstream_samples_lost(inlet), -1);
            assert_eq!(labstream_samples_resent(inlet), -1);
            lsl_destroy_inlet(inlet);
            lsl_destroy_streaminfo(info);
        }
    }

    #[test]
    fn two_labstream_peers_give_both_counts() {
        unsafe {
            let info = lsl_create_streaminfo(
                b"CapiCount1\0".as_ptr() as *const c_char,
                b"Test\0".as_ptr() as *const c_char,
                1,
                100.0,
                1,
                b"capi1\0".as_ptr() as *const c_char,
            );
            let outlet = lsl_create_outlet(info, 0, 360);
            if outlet.is_null() {
                eprintln!("no local network; skipping");
                lsl_destroy_streaminfo(info);
                return;
            }
            let full = lsl_get_info(outlet);
            let inlet = lsl_create_inlet(full, 360, 0, 1);
            let mut ec = 0;
            lsl_open_stream(inlet, 5.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(labstream_inlet_on_shm(inlet), 1);
            // SHM transfers raw bytes directly, so wire compression stays off.
            assert_eq!(labstream_inlet_compressed(inlet), 0);
            assert!(labstream_inlet_compression_ratio(inlet).is_nan());
            assert_eq!(labstream_samples_lost(inlet), 0);
            assert_eq!(labstream_samples_resent(inlet), 0);

            let mut health = std::mem::zeroed::<labstream_inlet_health>();
            assert_eq!(
                labstream_inlet_health(std::ptr::null_mut(), &mut health),
                LSL_ARGUMENT_ERROR
            );
            assert_eq!(labstream_inlet_health(inlet, &mut health), LSL_NO_ERROR);
            assert_eq!(health.connected, 1);
            assert_eq!(health.lost, 0);
            assert_eq!(health.resent, 0);
            assert_eq!(health.dropped, 0);
            assert_eq!(health.recoveries, 0);
            assert_eq!(health.gap_count, 0);

            // Outlet consumer health reports the connected inlet.
            assert_eq!(lsl_wait_for_consumers(outlet, 5.0), 1);
            let n_consumers = labstream_outlet_consumers(outlet, std::ptr::null_mut(), 0);
            assert_eq!(n_consumers, 1);
            let mut consumers = [std::mem::zeroed::<labstream_consumer_health>(); 4];
            assert_eq!(
                labstream_outlet_consumers(outlet, consumers.as_mut_ptr(), 4),
                1
            );
            assert!(consumers[0].id >= 1);
            assert!(consumers[0].capacity > 0);
            assert_eq!(consumers[0].dropped, 0);

            lsl_destroy_inlet(inlet);
            lsl_destroy_streaminfo(full);
            lsl_destroy_outlet(outlet);
            lsl_destroy_streaminfo(info);
        }
    }

    #[test]
    fn capi_inlet_gaps_reports_dropped_interval() {
        unsafe {
            let info = lsl_create_streaminfo(
                b"CapiGap0\0".as_ptr() as *const c_char,
                b"Test\0".as_ptr() as *const c_char,
                1,
                LSL_IRREGULAR_RATE,
                1,
                b"capigap0\0".as_ptr() as *const c_char,
            );
            let outlet = lsl_create_outlet(info, 0, 360);
            if outlet.is_null() {
                eprintln!("no local network; skipping");
                lsl_destroy_streaminfo(info);
                return;
            }
            let full = lsl_get_info(outlet);
            // For an irregular stream, `max_buflen = 1` sets a 100-sample buffer.
            let inlet = lsl_create_inlet(full, 1, 0, 1);
            let mut ec = 0;
            lsl_open_stream(inlet, 5.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(lsl_wait_for_consumers(outlet, 5.0), 1);

            // Pull sample 0 at ts = 100.0 so the reader has a baseline.
            let v0 = 0.0f32;
            lsl_push_sample_ft(outlet, &v0, 100.0);
            let mut got = 0.0f32;
            let t0 = lsl_pull_sample_f(inlet, &mut got, 1, 2.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(t0, 100.0);

            // Push 150 samples in small paced groups so the outlet queue drains
            // cleanly and the 100-sample inlet queue overflows by 50.
            for k in 1..=150 {
                let v = k as f32;
                lsl_push_sample_ft(outlet, &v, 100.0 + k as f64);
                if k % 25 == 0 {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut health = std::mem::zeroed::<labstream_inlet_health>();
            while std::time::Instant::now() < deadline {
                labstream_inlet_health(inlet, &mut health);
                if health.dropped >= 50 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(health.dropped >= 50, "expected drops, got {health:?}");

            // Pull the surviving sample to finalize the drop gap.
            let t_next = lsl_pull_sample_f(inlet, &mut got, 1, 2.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert!(t_next > 100.0);

            let count = labstream_inlet_gaps(inlet, std::ptr::null_mut(), 0);
            assert!(count >= 1, "expected at least one gap, got {count}");
            let mut gaps = [std::mem::zeroed::<labstream_gap>(); 8];
            let taken = labstream_inlet_take_gaps(inlet, gaps.as_mut_ptr(), 8);
            assert!(taken >= 1);
            assert_eq!(gaps[0].kind, LABSTREAM_GAP_DROPPED);
            assert_eq!(gaps[0].start, 100.0);
            assert_eq!(gaps[0].end, t_next);
            assert_eq!(gaps[0].count, health.dropped);
            assert_eq!(labstream_inlet_gaps(inlet, std::ptr::null_mut(), 0), 0);

            lsl_destroy_inlet(inlet);
            lsl_destroy_streaminfo(full);
            lsl_destroy_outlet(outlet);
            lsl_destroy_streaminfo(info);
        }
    }

    #[test]
    fn capi_create_inlet_selected_filters_channels_and_updates_fullinfo() {
        unsafe {
            let info = lsl_create_streaminfo(
                b"CapiSel0\0".as_ptr() as *const c_char,
                b"EEG\0".as_ptr() as *const c_char,
                6,
                4000.0,
                1,
                b"capisel0\0".as_ptr() as *const c_char,
            );
            let desc = lsl_get_desc(info);
            let chs_node = lsl_append_child(desc, b"channels\0".as_ptr() as *const c_char);
            for idx in 0..6 {
                let ch = lsl_append_child(chs_node, b"channel\0".as_ptr() as *const c_char);
                let label = format!("E{idx}\0");
                lsl_append_child_value(
                    ch,
                    b"label\0".as_ptr() as *const c_char,
                    label.as_ptr() as *const c_char,
                );
            }
            let outlet = lsl_create_outlet(info, 0, 360);
            if outlet.is_null() {
                eprintln!("no local network; skipping");
                lsl_destroy_streaminfo(info);
                return;
            }
            let pub_info = lsl_get_info(outlet);
            let wanted = [4u32, 1];
            let inlet = labstream_create_inlet_selected(
                pub_info,
                360,
                0,
                0,
                wanted.as_ptr(),
                wanted.len() as u32,
                1,
            );
            assert!(!inlet.is_null());
            let mut ec = 0;
            lsl_open_stream(inlet, 5.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(lsl_wait_for_consumers(outlet, 5.0), 1);

            let full = lsl_get_fullinfo(inlet, 5.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(lsl_get_channel_count(full), 2);
            let xml_ptr = lsl_get_xml(full);
            let xml = cstr(xml_ptr);
            lsl_destroy_string(xml_ptr);
            assert!(xml.contains("<label>E4</label>"), "xml={xml}");
            assert!(xml.contains("<label>E1</label>"), "xml={xml}");
            assert!(!xml.contains("<label>E0</label>"), "xml={xml}");
            lsl_destroy_streaminfo(full);

            let sample = [10.0f32, 20.0, 30.0, 40.0, 50.0, 60.0];
            lsl_push_sample_ft(outlet, sample.as_ptr(), 123.5);
            let mut got = [0.0f32; 2];
            let ts = lsl_pull_sample_f(inlet, got.as_mut_ptr(), 2, 2.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(ts, 123.5);
            assert_eq!(got, [50.0, 20.0]);

            lsl_destroy_inlet(inlet);
            lsl_destroy_streaminfo(pub_info);
            lsl_destroy_outlet(outlet);
            lsl_destroy_streaminfo(info);
        }
    }

    #[test]
    fn capi_device_delay_and_drift_clock_are_opt_in_and_accurate() {
        unsafe {
            let info = lsl_create_streaminfo(
                b"CapiCalib0\0".as_ptr() as *const c_char,
                b"EEG\0".as_ptr() as *const c_char,
                1,
                500.0,
                1,
                b"capicalib0\0".as_ptr() as *const c_char,
            );
            assert!(labstream_streaminfo_device_delay(info).is_nan());
            assert_eq!(
                labstream_streaminfo_set_device_delay(info, 0.025, 0.0003),
                LSL_NO_ERROR
            );
            assert!((labstream_streaminfo_device_delay(info) - 0.025).abs() < 1e-6);

            let outlet = lsl_create_outlet(info, 0, 360);
            if outlet.is_null() {
                eprintln!("no local network; skipping");
                lsl_destroy_streaminfo(info);
                return;
            }
            let pub_info = lsl_get_info(outlet);
            let inlet = lsl_create_inlet(pub_info, 360, 0, 0);
            let mut ec = 0;
            lsl_open_stream(inlet, 5.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(lsl_wait_for_consumers(outlet, 5.0), 1);

            // Default: timestamps are unchanged (opt-in criterion).
            let v = 1.0f32;
            lsl_push_sample_ft(outlet, &v, 100.025);
            let mut got = 0.0f32;
            let ts_raw = lsl_pull_sample_f(inlet, &mut got, 1, 2.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(ts_raw.to_bits(), 100.025f64.to_bits());

            // Enable opt-in DEVICE_DELAY: reads `<desc><synchronization><offset_mean>`
            // automatically and subtracts 0.025 s.
            assert_eq!(
                lsl_set_postprocessing(inlet, LABSTREAM_POST_DEVICE_DELAY),
                LSL_NO_ERROR
            );
            assert!((labstream_inlet_device_delay(inlet) - 0.025).abs() < 1e-6);
            lsl_push_sample_ft(outlet, &v, 200.025);
            let ts_adj = lsl_pull_sample_f(inlet, &mut got, 1, 2.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert!((ts_adj - 200.0).abs() < 1e-6);

            // Wait for time sync burst and check drift clock C API.
            let _ = lsl_time_correction(inlet, 2.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert!(labstream_inlet_clock_drift(inlet).is_finite());
            assert!(labstream_inlet_smooth_time_correction(inlet, 200.0).is_finite());

            lsl_destroy_inlet(inlet);
            lsl_destroy_streaminfo(pub_info);
            lsl_destroy_outlet(outlet);
            lsl_destroy_streaminfo(info);
        }
    }

    #[test]
    fn capi_live_metadata_round_trips_in_order() {
        unsafe {
            let info = lsl_create_streaminfo(
                b"CapiMeta0\0".as_ptr() as *const c_char,
                b"EEG\0".as_ptr() as *const c_char,
                1,
                250.0,
                1,
                b"capimeta0\0".as_ptr() as *const c_char,
            );
            let outlet = lsl_create_outlet(info, 0, 360);
            if outlet.is_null() {
                eprintln!("no local network; skipping");
                lsl_destroy_streaminfo(info);
                return;
            }
            let pub_info = lsl_get_info(outlet);
            let inlet = lsl_create_inlet(pub_info, 360, 0, 0);
            let mut ec = 0;
            lsl_open_stream(inlet, 5.0, &mut ec);
            assert_eq!(ec, LSL_NO_ERROR);
            assert_eq!(lsl_wait_for_consumers(outlet, 5.0), 1);
            assert_eq!(labstream_inlet_has_live_metadata(inlet), 1);

            let v = 1.0f32;
            lsl_push_sample_ft(outlet, &v, 50.0);
            assert_eq!(
                labstream_outlet_push_metadata(
                    outlet,
                    b"impedance/Fp1\0".as_ptr() as *const c_char,
                    b"3.8 kOhm\0".as_ptr() as *const c_char,
                    50.5,
                ),
                LSL_NO_ERROR
            );

            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while labstream_inlet_metadata_count(inlet) < 1
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(labstream_inlet_metadata_count(inlet) >= 1);

            let mut kbuf = [0 as c_char; 64];
            let mut vbuf = [0 as c_char; 64];
            let mut ts = 0.0;
            let mut seq = 0u64;
            assert_eq!(
                labstream_inlet_pull_metadata(
                    inlet,
                    kbuf.as_mut_ptr(),
                    kbuf.len() as u32,
                    vbuf.as_mut_ptr(),
                    vbuf.len() as u32,
                    &mut ts,
                    &mut seq,
                ),
                1
            );
            assert_eq!(CStr::from_ptr(kbuf.as_ptr()).to_str().unwrap(), "impedance/Fp1");
            assert_eq!(CStr::from_ptr(vbuf.as_ptr()).to_str().unwrap(), "3.8 kOhm");
            assert_eq!(ts.to_bits(), 50.5f64.to_bits());
            assert_eq!(seq, 1);

            lsl_destroy_inlet(inlet);
            lsl_destroy_streaminfo(pub_info);
            lsl_destroy_outlet(outlet);
            lsl_destroy_streaminfo(info);
        }
    }
}
