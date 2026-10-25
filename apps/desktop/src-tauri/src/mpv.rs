//! S3 - libmpv binding.
//!
//! Dynamically loads `libmpv-2.dll` and calls the stable libmpv v2 client API
//! via hand-declared FFI. This keeps the shell DLL-agnostic: it needs only a
//! `libmpv-2.dll` at runtime (any official build), no dev headers or import lib.
//!
//! Playback uses mpv's `wid` embedding: mpv renders into a native window handle
//! we hand it, so we don't manage a GL/D3D render context ourselves. This module
//! is the raw binding; window wiring + the web bridge build on top.

use std::ffi::{c_char, c_int, c_ulong, c_void, CString};
use std::path::{Path, PathBuf};

use libloading::Library;

// libmpv v2 client API signatures (mpv/client.h). Only what playback needs.
type MpvClientApiVersion = unsafe extern "C" fn() -> c_ulong;
type MpvCreate = unsafe extern "C" fn() -> *mut c_void;
type MpvInitialize = unsafe extern "C" fn(*mut c_void) -> c_int;
type MpvTerminateDestroy = unsafe extern "C" fn(*mut c_void);
type MpvSetOptionString = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type MpvSetPropertyString = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type MpvCommand = unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int;
type MpvErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
// mpv_observe_property(handle, reply_userdata, name, format) - subscribe to a
// property; changes (and one initial value) arrive as PROPERTY_CHANGE events.
type MpvObserveProperty = unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int) -> c_int;
// mpv_wait_event(handle, timeout_s) - block for the next event on this handle.
// The returned pointer is owned by mpv and valid until the next wait_event.
type MpvWaitEvent = unsafe extern "C" fn(*mut c_void, f64) -> *mut MpvEventRaw;
// mpv_request_log_messages(handle, min_level) - stream mpv's own log at
// `min_level` ("info", "v", "debug", …) as LOG_MESSAGE events.
type MpvRequestLogMessages = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type MpvGetPropertyString = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type MpvFree = unsafe extern "C" fn(*mut c_void);

// ---------------------------------------------------------------------------
// stream_cb: custom read-only protocols (mpv/stream_cb.h)
// ---------------------------------------------------------------------------
//
// Lets us hand mpv the bytes directly instead of going through a loopback HTTP
// socket. mpv calls these SYNCHRONOUSLY from its demuxer/stream threads, and
// blocking in them is expected (that is exactly how a network stream behaves).
//
// Contract (mpv/stream_cb.h), reproduced here because we hand-declare the ABI:
//   open_fn(user_data, uri, info) -> 0 on success, a negative mpv error
//     (MPV_ERROR_LOADING_FAILED) otherwise. `uri` is the FULL url including the
//     `scheme://` prefix. On success the callee fills `info` (cookie + fn ptrs).
//   read_fn(cookie, buf, nbytes) -> bytes read; 0 means EOF; negative = error.
//   seek_fn(cookie, offset) -> the new absolute position, or a negative error
//     (MPV_ERROR_UNSUPPORTED for a non-seekable stream).
//   size_fn(cookie) -> the stream length, or MPV_ERROR_UNSUPPORTED if unknown.
//   close_fn(cookie) -> release the cookie. mpv_terminate_destroy calls this for
//     every still-open stream, so the cookie must stay valid until then.
//   cancel_fn(cookie) -> called from ANOTHER thread to abort a blocked read or
//     seek. Optional; NULL means "not cancellable".

/// `mpv_stream_cb_read_fn`.
pub type MpvStreamCbReadFn = unsafe extern "C" fn(*mut c_void, *mut c_char, u64) -> i64;
/// `mpv_stream_cb_seek_fn` (absolute offset).
pub type MpvStreamCbSeekFn = unsafe extern "C" fn(*mut c_void, i64) -> i64;
/// `mpv_stream_cb_size_fn`.
pub type MpvStreamCbSizeFn = unsafe extern "C" fn(*mut c_void) -> i64;
/// `mpv_stream_cb_close_fn`.
pub type MpvStreamCbCloseFn = unsafe extern "C" fn(*mut c_void);
/// `mpv_stream_cb_cancel_fn`.
pub type MpvStreamCbCancelFn = unsafe extern "C" fn(*mut c_void);
/// `mpv_stream_cb_open_ro_fn`.
pub type MpvStreamCbOpenRoFn =
    unsafe extern "C" fn(*mut c_void, *mut c_char, *mut MpvStreamCbInfo) -> c_int;

/// `mpv_stream_cb_info` - what `open_fn` fills in. Every function pointer is
/// declared `Option<..>` so a NULL (unsupported) entry is expressible; mpv reads
/// NULL as "this operation is not available".
#[repr(C)]
pub struct MpvStreamCbInfo {
    pub cookie: *mut c_void,
    pub read_fn: Option<MpvStreamCbReadFn>,
    pub seek_fn: Option<MpvStreamCbSeekFn>,
    pub size_fn: Option<MpvStreamCbSizeFn>,
    pub close_fn: Option<MpvStreamCbCloseFn>,
    pub cancel_fn: Option<MpvStreamCbCancelFn>,
}

type MpvStreamCbAddRo =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void, MpvStreamCbOpenRoFn) -> c_int;

/// `MPV_ERROR_UNSUPPORTED` - the documented return for a stream that cannot
/// seek or cannot report its size.
pub const MPV_ERROR_UNSUPPORTED: i64 = -18;
/// `MPV_ERROR_LOADING_FAILED` - the documented `open_fn` failure code.
pub const MPV_ERROR_LOADING_FAILED: c_int = -13;
/// `MPV_ERROR_GENERIC`.
pub const MPV_ERROR_GENERIC: i64 = -20;

/// `MPV_END_FILE_REASON_ERROR`: an END_FILE whose file failed (vs EOF, stop,
/// quit or redirect); `error` then carries the mpv error code.
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;

/// The client API version that introduced `mpv_stream_cb_add_ro` (mpv 0.24,
/// client API 1.24). Shipped libmpv-2 builds are 2.x, so this is a floor, not a
/// realistic gate; it exists so an ancient DLL degrades to the HTTP path instead
/// of calling a symbol whose ABI predates the one declared above.
pub const MPV_CLIENT_API_STREAM_CB: c_ulong = (1 << 16) | 24;

// mpv_format values (mpv/client.h). We observe every property as NODE so the
// event carries a self-describing value we can map straight to JSON.
const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;
const MPV_FORMAT_NODE: c_int = 6;
const MPV_FORMAT_NODE_ARRAY: c_int = 7;
const MPV_FORMAT_NODE_MAP: c_int = 8;

// mpv_event_id values we care about.
const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_LOG_MESSAGE: c_int = 2;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

/// `mpv_node` (mpv/client.h). The C type is a tagged union; on 64-bit every
/// union member (pointer / int64 / double) is 8 bytes, so we hold the payload as
/// a raw `u64` and reinterpret per `format`. Layout matches the C struct
/// (8-byte payload + 4-byte tag, 8-byte aligned = 16 bytes).
#[repr(C)]
struct MpvNode {
    u: u64,
    format: c_int,
}

/// `mpv_node_list` - the backing store for NODE_ARRAY / NODE_MAP nodes.
#[repr(C)]
struct MpvNodeList {
    num: c_int,
    values: *mut MpvNode,
    keys: *mut *mut c_char,
}

/// `mpv_event_property` - payload of a PROPERTY_CHANGE event.
#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

/// `mpv_event_end_file` - payload of an END_FILE event.
#[repr(C)]
struct MpvEventEndFile {
    reason: c_int,
    error: c_int,
}

/// `mpv_event_log_message` - payload of a LOG_MESSAGE event.
#[repr(C)]
struct MpvEventLogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: c_int,
}

/// `mpv_event` - the fixed header every event shares.
#[repr(C)]
struct MpvEventRaw {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

/// A translated mpv event, ready to forward to the web client.
pub enum MpvEvent {
    /// An observed property changed (or reported its initial value).
    PropertyChange { name: String, value: serde_json::Value },
    /// Playback of the current file ended. `reason`/`error` are mpv's raw codes.
    EndFile { reason: c_int, error: c_int },
    /// A line from mpv's own log (diagnostics): `prefix`, `level`, `text`.
    LogMessage { prefix: String, level: String, text: String },
    /// mpv is shutting down; the event loop should stop.
    Shutdown,
    /// No event: the queue was empty when the timeout ran out (or a wakeup).
    /// What tells a caller draining the queue that it is drained.
    Empty,
    /// Anything we don't translate (reconfig, replies, …).
    Other,
}

struct Api {
    client_api_version: MpvClientApiVersion,
    create: MpvCreate,
    initialize: MpvInitialize,
    terminate_destroy: MpvTerminateDestroy,
    set_option_string: MpvSetOptionString,
    set_property_string: MpvSetPropertyString,
    command: MpvCommand,
    error_string: MpvErrorString,
    observe_property: MpvObserveProperty,
    wait_event: MpvWaitEvent,
    request_log_messages: MpvRequestLogMessages,
    get_property_string: MpvGetPropertyString,
    free: MpvFree,
    /// OPTIONAL - a libmpv that predates stream_cb (or a stripped build) simply
    /// does not export this. Loading must NOT fail in that case: the caller
    /// falls back to the HTTP stream URL. Every symbol above stays mandatory.
    stream_cb_add_ro: Option<MpvStreamCbAddRo>,
}

/// A `user_data` block handed to `mpv_stream_cb_add_ro`, plus the function that
/// frees it. mpv keeps the pointer for the lifetime of the mpv core, so we free
/// these only after `mpv_terminate_destroy` has returned.
struct StreamCbUserData {
    ptr: *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
}

/// A loaded mpv instance. `Drop` destroys it. Not `Sync`; drive it from one
/// thread (or wrap in a mutex).
pub struct Mpv {
    api: Api,
    ctx: *mut c_void,
    /// Registered stream_cb `user_data` blocks, freed in `Drop` after
    /// `mpv_terminate_destroy` (which itself closes any still-open stream).
    stream_cb_user_data: std::sync::Mutex<Vec<StreamCbUserData>>,
    // The Library must outlive every function pointer above; dropped last.
    _lib: Library,
}

impl Mpv {
    /// Load `libmpv-2.dll` from `dll_path` and create an (uninitialized) mpv
    /// context. Set options with [`set_option`], then call [`initialize`].
    pub fn load(dll_path: &Path) -> Result<Self, String> {
        // Ensure the DLL's own directory is searched for its dependencies (a
        // non-self-contained build sits next to its ffmpeg DLLs). A distribution
        // build ships a self-contained libmpv-2.dll and this is a no-op.
        // APPEND (not prepend) the dir so the system directories keep precedence,
        // prepending would let a planted DLL in this dir shadow a system one.
        if let Some(dir) = dll_path.parent() {
            let path = std::env::var("PATH").unwrap_or_default();
            std::env::set_var("PATH", format!("{path};{}", dir.display()));
        }
        unsafe {
            let lib = Library::new(dll_path).map_err(|e| format!("load {dll_path:?}: {e}"))?;
            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let s = lib
                        .get::<$ty>(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("symbol {}: {e}", $name))?;
                    *s
                }};
            }
            // Optional symbol: absent DLL => None => HTTP fallback, never a
            // load failure. Deliberately NOT the `sym!` macro, which hard-fails.
            let stream_cb_add_ro = lib
                .get::<MpvStreamCbAddRo>(b"mpv_stream_cb_add_ro\0")
                .map(|s| *s)
                .ok();
            if stream_cb_add_ro.is_none() {
                tracing::warn!(
                    "mpv: {dll_path:?} does not export mpv_stream_cb_add_ro; \
                     in-process streaming is unavailable (HTTP fallback)"
                );
            }
            let api = Api {
                client_api_version: sym!("mpv_client_api_version", MpvClientApiVersion),
                create: sym!("mpv_create", MpvCreate),
                initialize: sym!("mpv_initialize", MpvInitialize),
                terminate_destroy: sym!("mpv_terminate_destroy", MpvTerminateDestroy),
                set_option_string: sym!("mpv_set_option_string", MpvSetOptionString),
                set_property_string: sym!("mpv_set_property_string", MpvSetPropertyString),
                command: sym!("mpv_command", MpvCommand),
                error_string: sym!("mpv_error_string", MpvErrorString),
                observe_property: sym!("mpv_observe_property", MpvObserveProperty),
                wait_event: sym!("mpv_wait_event", MpvWaitEvent),
                request_log_messages: sym!("mpv_request_log_messages", MpvRequestLogMessages),
                get_property_string: sym!("mpv_get_property_string", MpvGetPropertyString),
                free: sym!("mpv_free", MpvFree),
                stream_cb_add_ro,
            };
            let ctx = (api.create)();
            if ctx.is_null() {
                return Err("mpv_create returned null".into());
            }
            Ok(Self {
                api,
                ctx,
                stream_cb_user_data: std::sync::Mutex::new(Vec::new()),
                _lib: lib,
            })
        }
    }

    /// Whether this DLL can serve a custom protocol in-process: the optional
    /// `mpv_stream_cb_add_ro` symbol is present AND the client API is new enough
    /// for the ABI declared above. `false` means the caller must keep using the
    /// HTTP stream URL.
    pub fn supports_stream_cb(&self) -> bool {
        self.api.stream_cb_add_ro.is_some()
            && self.client_api_version() >= MPV_CLIENT_API_STREAM_CB
    }

    /// Register a custom read-only protocol (`mpv_stream_cb_add_ro`).
    ///
    /// `protocol` is the bare scheme (`"rillio"` for `rillio://...`). `open_fn`
    /// is invoked by mpv with `user_data` and the FULL uri whenever a matching
    /// url is loaded.
    ///
    /// Returns `Err` when the symbol is missing or the client API is too old
    /// (see [`supports_stream_cb`](Self::supports_stream_cb)); the caller then
    /// falls back to HTTP. Call this BEFORE [`initialize`](Self::initialize).
    ///
    /// # Safety
    /// `user_data` must stay valid until this `Mpv` is dropped; on success this
    /// instance takes ownership and calls `free_user_data` after
    /// `mpv_terminate_destroy`. On `Err` ownership stays with the caller.
    /// `open_fn` must be unwind-safe (never let a Rust panic cross into mpv).
    pub unsafe fn add_stream_protocol(
        &self,
        protocol: &str,
        user_data: *mut c_void,
        open_fn: MpvStreamCbOpenRoFn,
        free_user_data: unsafe extern "C" fn(*mut c_void),
    ) -> Result<(), String> {
        let Some(add_ro) = self.api.stream_cb_add_ro else {
            return Err("libmpv does not export mpv_stream_cb_add_ro".into());
        };
        let version = self.client_api_version();
        if version < MPV_CLIENT_API_STREAM_CB {
            return Err(format!(
                "libmpv client API {:#x} is older than the stream_cb ABI {:#x}",
                version, MPV_CLIENT_API_STREAM_CB
            ));
        }
        let p = cstr(protocol)?;
        self.check(add_ro(self.ctx, p.as_ptr(), user_data, open_fn))?;
        // Only registered blocks are owned; a failed add leaves the caller's.
        match self.stream_cb_user_data.lock() {
            Ok(mut v) => v.push(StreamCbUserData { ptr: user_data, free: free_user_data }),
            Err(e) => return Err(format!("stream_cb registry lock poisoned: {e}")),
        }
        Ok(())
    }

    /// The libmpv client API version the loaded DLL reports (sanity check).
    pub fn client_api_version(&self) -> c_ulong {
        unsafe { (self.api.client_api_version)() }
    }

    /// Set an option before [`initialize`] (e.g. `wid`, `vo`, `hwdec`).
    pub fn set_option(&self, name: &str, value: &str) -> Result<(), String> {
        let (n, v) = (cstr(name)?, cstr(value)?);
        self.check(unsafe { (self.api.set_option_string)(self.ctx, n.as_ptr(), v.as_ptr()) })
    }

    /// Finish initialization (creates the audio/video output).
    pub fn initialize(&self) -> Result<(), String> {
        self.check(unsafe { (self.api.initialize)(self.ctx) })
    }

    /// Set a property at runtime (e.g. `pause`, `volume`, `time-pos`).
    pub fn set_property(&self, name: &str, value: &str) -> Result<(), String> {
        let (n, v) = (cstr(name)?, cstr(value)?);
        self.check(unsafe { (self.api.set_property_string)(self.ctx, n.as_ptr(), v.as_ptr()) })
    }

    /// Run a command, e.g. `["loadfile", url]`, `["stop"]`, `["seek", "10"]`.
    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let cstrings: Vec<CString> = args.iter().map(|a| cstr(a)).collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null()); // NULL-terminated argv
        self.check(unsafe { (self.api.command)(self.ctx, ptrs.as_ptr()) })
    }

    /// Read a property as a string (mpv stringifies any type). `None` when the
    /// property is unavailable (e.g. `time-pos` before anything is loaded).
    /// The C string is owned by mpv and freed with `mpv_free` after copying.
    pub fn get_property_string(&self, name: &str) -> Option<String> {
        let n = cstr(name).ok()?;
        unsafe {
            let raw = (self.api.get_property_string)(self.ctx, n.as_ptr());
            if raw.is_null() {
                return None;
            }
            let value = cstr_to_string(raw);
            (self.api.free)(raw as *mut c_void);
            Some(value)
        }
    }

    /// Subscribe to a property. mpv delivers one initial value immediately and
    /// then a PROPERTY_CHANGE event on every change. We always request
    /// `MPV_FORMAT_NODE` so [`wait_event`](Self::wait_event) can render any type.
    pub fn observe_property(&self, name: &str) -> Result<(), String> {
        let n = cstr(name)?;
        self.check(unsafe { (self.api.observe_property)(self.ctx, 0, n.as_ptr(), MPV_FORMAT_NODE) })
    }

    /// Ask mpv to stream its own log at `min_level` ("info", "v", "debug", …)
    /// as LOG_MESSAGE events. Diagnostics only.
    pub fn request_log_messages(&self, min_level: &str) -> Result<(), String> {
        let l = cstr(min_level)?;
        self.check(unsafe { (self.api.request_log_messages)(self.ctx, l.as_ptr()) })
    }

    /// Block up to `timeout` seconds (negative = forever) for the next event and
    /// translate it. Call this from a single dedicated thread; other threads may
    /// concurrently issue commands/property sets on the same handle.
    pub fn wait_event(&self, timeout: f64) -> MpvEvent {
        unsafe {
            let ev = (self.api.wait_event)(self.ctx, timeout);
            if ev.is_null() {
                return MpvEvent::Other;
            }
            match (*ev).event_id {
                MPV_EVENT_NONE => MpvEvent::Empty,
                MPV_EVENT_SHUTDOWN => MpvEvent::Shutdown,
                MPV_EVENT_LOG_MESSAGE => {
                    let m = (*ev).data as *const MpvEventLogMessage;
                    if m.is_null() {
                        return MpvEvent::Other;
                    }
                    MpvEvent::LogMessage {
                        prefix: cstr_to_string((*m).prefix),
                        level: cstr_to_string((*m).level),
                        text: cstr_to_string((*m).text).trim_end().to_string(),
                    }
                }
                MPV_EVENT_END_FILE => {
                    let ef = (*ev).data as *const MpvEventEndFile;
                    if ef.is_null() {
                        MpvEvent::EndFile { reason: 0, error: 0 }
                    } else {
                        MpvEvent::EndFile { reason: (*ef).reason, error: (*ef).error }
                    }
                }
                MPV_EVENT_PROPERTY_CHANGE => {
                    let p = (*ev).data as *const MpvEventProperty;
                    if p.is_null() || (*p).name.is_null() {
                        return MpvEvent::Other;
                    }
                    let name = cstr_to_string((*p).name);
                    let value = if (*p).format == MPV_FORMAT_NODE && !(*p).data.is_null() {
                        node_to_json(&*((*p).data as *const MpvNode))
                    } else {
                        serde_json::Value::Null
                    };
                    MpvEvent::PropertyChange { name, value }
                }
                _ => MpvEvent::Other,
            }
        }
    }

    /// Human-readable text for an mpv error code (for END_FILE errors).
    pub fn error_string(&self, code: c_int) -> String {
        unsafe {
            std::ffi::CStr::from_ptr((self.api.error_string)(code))
                .to_string_lossy()
                .into_owned()
        }
    }

    fn check(&self, code: c_int) -> Result<(), String> {
        if code >= 0 {
            Ok(())
        } else {
            let msg = unsafe {
                std::ffi::CStr::from_ptr((self.api.error_string)(code))
                    .to_string_lossy()
                    .into_owned()
            };
            Err(format!("mpv error {code}: {msg}"))
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // terminate_destroy calls close_fn for every still-open stream, so the
        // stream_cb user_data blocks must only be freed AFTER it returns.
        unsafe { (self.api.terminate_destroy)(self.ctx) }
        if let Ok(mut blocks) = self.stream_cb_user_data.lock() {
            for b in blocks.drain(..) {
                unsafe { (b.free)(b.ptr) }
            }
        }
    }
}

// The libmpv client API is thread-safe: an `mpv_handle` may be used from
// multiple threads. We serialize access behind a Mutex in Tauri state anyway.
unsafe impl Send for Mpv {}
unsafe impl Sync for Mpv {}

fn cstr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("interior NUL in {s:?}"))
}

/// Copy a C string into an owned `String` (lossy). NULL → empty string.
unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Convert an `mpv_node` into a `serde_json::Value`, recursing through
/// arrays/maps. The node's `format` tag drives how the 8-byte payload is read
/// (little-endian x86-64: a flag/int occupies the low bytes of `u`).
unsafe fn node_to_json(node: &MpvNode) -> serde_json::Value {
    use serde_json::Value;
    match node.format {
        MPV_FORMAT_STRING => {
            Value::String(cstr_to_string(node.u as usize as *const c_char))
        }
        MPV_FORMAT_FLAG => Value::Bool((node.u as u32) != 0),
        MPV_FORMAT_INT64 => Value::from(node.u as i64),
        MPV_FORMAT_DOUBLE => serde_json::Number::from_f64(f64::from_bits(node.u))
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MPV_FORMAT_NODE_ARRAY => {
            let list = node.u as usize as *const MpvNodeList;
            if list.is_null() {
                return Value::Array(Vec::new());
            }
            let n = (*list).num.max(0) as usize;
            let mut arr = Vec::with_capacity(n);
            for i in 0..n {
                arr.push(node_to_json(&*(*list).values.add(i)));
            }
            Value::Array(arr)
        }
        MPV_FORMAT_NODE_MAP => {
            let list = node.u as usize as *const MpvNodeList;
            if list.is_null() {
                return Value::Object(serde_json::Map::new());
            }
            let n = (*list).num.max(0) as usize;
            let mut map = serde_json::Map::with_capacity(n);
            for i in 0..n {
                let key = cstr_to_string(*(*list).keys.add(i));
                map.insert(key, node_to_json(&*(*list).values.add(i)));
            }
            Value::Object(map)
        }
        // MPV_FORMAT_NONE / BYTE_ARRAY / anything unexpected.
        _ => Value::Null,
    }
}

/// Android: hand the JavaVM (and app context) to ffmpeg BEFORE mpv init.
/// mediacodec hwdec, the audiotrack AO, and mpv's own android VO
/// (ANativeWindow_fromSurface on the `wid` Surface) all reach Java over JNI;
/// ffmpeg has no way to discover the VM itself (mpv-android does exactly this
/// call in its JNI_OnLoad). The symbols live in the bundled libavcodec.so,
/// already mapped as a libmpv dependency, so this dlopen returns the existing
/// handle rather than loading anything new.
#[cfg(target_os = "android")]
pub fn set_ffmpeg_java_vm() -> Result<(), String> {
    use std::ffi::c_void;
    type SetVm = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
    type SetCtx = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

    // Captured by the SurfaceBridge JNI calls (MainActivity.onCreate); always
    // set long before any playback. ndk-context is deliberately NOT used: its
    // singleton is never initialized under Tauri and aborts the process.
    let vm = crate::surface::android::java_vm_ptr()
        .ok_or("JavaVM not captured yet (SurfaceBridge.onActivityCreated has not run)")?;
    unsafe {
        let lib = Library::new("libavcodec.so").map_err(|e| format!("libavcodec.so: {e}"))?;
        let set_vm = lib
            .get::<SetVm>(b"av_jni_set_java_vm\0")
            .map_err(|e| format!("av_jni_set_java_vm: {e}"))?;
        let rc = set_vm(vm, std::ptr::null_mut());
        if rc < 0 {
            return Err(format!("av_jni_set_java_vm failed: rc={rc}"));
        }
        // ffmpeg 7+: mediacodec also wants the Application context (codec
        // enumeration via the app's PackageManager). Best-effort: warn-only,
        // the VM handoff above is the hard requirement.
        if let Ok(set_ctx) = lib.get::<SetCtx>(b"av_jni_set_android_app_ctx\0") {
            if let Some(app) = crate::surface::android::app_context_ptr() {
                let rc = set_ctx(app, std::ptr::null_mut());
                if rc < 0 {
                    tracing::warn!("mpv: av_jni_set_android_app_ctx failed: rc={rc}");
                }
            }
        }
        // Keep libavcodec pinned for the process lifetime (it is anyway, via
        // libmpv); dropping the handle must never unmap it under mpv.
        std::mem::forget(lib);
    }
    tracing::info!("mpv: JavaVM handed to ffmpeg");
    Ok(())
}

/// Default location of the dev/bundled `libmpv-2.dll`: next to the executable,
/// falling back to the crate dir during `cargo test`/`run`.
///
/// Android: the bare soname. `libmpv.so` ships in the APK's jniLibs next to our
/// own cdylib, and dlopen resolves a plain soname from the app's linker
/// namespace (nativeLibraryDir); its ffmpeg dependencies resolve from the same
/// directory automatically. No filesystem probing applies there.
pub fn default_dll_path() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        return PathBuf::from("libmpv.so");
    }
    #[cfg(not(target_os = "android"))]
    {
        // Dev override: point at any libmpv-2.dll (e.g. an on-machine build).
        if let Ok(p) = std::env::var("RILLIO_LIBMPV") {
            let p = PathBuf::from(p);
            if p.exists() {
                return p;
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let p = dir.join("libmpv-2.dll");
                if p.exists() {
                    return p;
                }
            }
        }
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("libmpv-2.dll")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Candidate libmpv locations for the dev machine: env override, our
    /// (possibly non-self-contained) copy, or a known on-machine libmpv.
    fn dev_dll() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("RILLIO_LIBMPV") {
            let p = PathBuf::from(p);
            if p.exists() {
                return Some(p);
            }
        }
        let candidates = [
            default_dll_path(),
            // Known self-consistent libmpv on this machine (loaded from its own
            // dir so its ffmpeg deps resolve).
            PathBuf::from(r"F:\Topaz Labs LLC\Topaz Video AI\mpv-2.dll"),
        ];
        candidates.into_iter().find(|p| p.exists())
    }

    /// Loads the on-disk libmpv and initializes a headless mpv instance
    /// (`vo=null`, `ao=null`). Skips if no libmpv is present (it is provided
    /// per-machine; distribution bundles a self-contained one).
    #[test]
    fn loads_and_initializes_libmpv() {
        let Some(dll) = dev_dll() else {
            eprintln!("[skip] no libmpv-2.dll found");
            return;
        };
        let mpv = Mpv::load(&dll).expect("load libmpv");
        assert!(mpv.client_api_version() >= (2 << 16), "expect client API v2+");
        // Headless init: no window, no audio device.
        mpv.set_option("vo", "null").unwrap();
        mpv.set_option("ao", "null").unwrap();
        mpv.initialize().expect("mpv_initialize");
        // A benign runtime property set proves the live handle works.
        mpv.set_property("volume", "100").expect("set volume");
    }

    /// The stream_cb symbol is OPTIONAL: an absent one must degrade to "no
    /// in-process byte plane", never to a failed [`Mpv::load`]. Proven on the
    /// mechanism (the same libloading probe, against a name no DLL exports)
    /// plus the fact that load succeeds either way.
    #[test]
    fn optional_stream_cb_symbol_never_fails_load() {
        let Some(dll) = dev_dll() else {
            eprintln!("[skip] no libmpv-2.dll found");
            return;
        };
        let mpv = Mpv::load(&dll).expect("load libmpv");
        unsafe {
            let lib = Library::new(&dll).expect("reopen libmpv");
            let absent = lib
                .get::<MpvStreamCbAddRo>(b"mpv_stream_cb_add_ro_not_a_real_symbol\0")
                .map(|s| *s)
                .ok();
            assert!(absent.is_none(), "a missing symbol must probe as None");
            let present = lib
                .get::<MpvStreamCbAddRo>(b"mpv_stream_cb_add_ro\0")
                .map(|s| *s)
                .ok();
            eprintln!(
                "[stream_cb] {dll:?}: exports mpv_stream_cb_add_ro = {}, api = {:#x}",
                present.is_some(),
                mpv.client_api_version()
            );
        }
        // Whatever the DLL exports, load itself succeeded.
        assert!(mpv.client_api_version() >= (2 << 16));
    }
}
