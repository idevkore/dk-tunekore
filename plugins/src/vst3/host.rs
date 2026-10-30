//! The host side of the VST3 conversation: the objects a plugin is handed or asks for.
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: [`HostContext`] now forwards the plugin editor's
//! edits to the processor through a lock-free queue as well as to the UI, and reports gestures
//! and restart requests as [`ControllerEvent`]s.

use super::util::{tuid_bytes, write_c16};
use crate::plugin::{ControllerEvent, ParamChange};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use vst3::Steinberg::Vst::{self as sv, IComponentHandlerTrait, IHostApplicationTrait};
use vst3::Steinberg::{self as sb, FUnknown, IBStreamTrait};
use vst3::{Class, ComWrapper, Interface};

/// Largest plugin state we buffer (component or controller state).
pub(crate) const MAX_STATE_BYTES: usize = 64 << 20;
/// Most events buffered between two `poll`s.
const EVENT_CAP: usize = 4096;

/// The host context handed to `initialize` (`IHostApplication`) and to the controller
/// (`IComponentHandler`). Plugins call it from whatever thread they like, so it only uses short
/// locks; nothing here runs on the audio thread.
pub(crate) struct HostContext {
    /// Edits made in the plugin's editor, on their way to the processor. Replaced at every
    /// activation (a fresh queue for each audio-thread half).
    pub to_processor: Mutex<Option<rtrb::Producer<ParamChange>>>,
    /// What the controller reports from `poll`.
    events: Mutex<Vec<ControllerEvent>>,
    /// Set when the plugin asked for a reload, so a stale processor can notice.
    pub reload_requested: AtomicBool,
}

impl HostContext {
    pub fn new() -> HostContext {
        HostContext { to_processor: Mutex::new(None), events: Mutex::new(Vec::with_capacity(256)), reload_requested: AtomicBool::new(false) }
    }

    /// Sends a value to the processor as a change at the start of its next block. Dropped if
    /// there is no processor or its queue is full.
    pub fn send_to_processor(&self, id: u32, value: f64) {
        let mut g = self.to_processor.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = g.as_mut() {
            let _ = p.push(ParamChange { id, offset: 0, value });
        }
    }

    fn report(&self, e: ControllerEvent) {
        let mut v = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        // A stream of edits to one control collapses into its latest value.
        if let (ControllerEvent::ParamEdit { id, value }, Some(ControllerEvent::ParamEdit { id: last_id, value: last })) = (e, v.last_mut())
            && id == *last_id
        {
            *last = value;
            return;
        }
        if v.len() < EVENT_CAP {
            v.push(e);
        }
    }

    /// Moves the reported events into `out`.
    pub fn take_events(&self, out: &mut Vec<ControllerEvent>) {
        out.append(&mut self.events.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

impl Class for HostContext {
    type Interfaces = (sv::IHostApplication, sv::IComponentHandler);
}

impl IHostApplicationTrait for HostContext {
    unsafe fn getName(&self, name: *mut sv::String128) -> sb::tresult {
        // SAFETY: the plugin passes a pointer to a writable String128 (or null, handled).
        match unsafe { name.as_mut() } {
            Some(n) => {
                write_c16("tuneKore", n);
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }

    /// Creates the objects plugins may ask the host for: `IMessage` (what a controller and its
    /// processor send each other through `IConnectionPoint::notify`) and `IAttributeList`.
    /// Plugins that talk between their halves this way fail without them.
    unsafe fn createInstance(&self, cid: *mut sb::TUID, iid: *mut sb::TUID, obj: *mut *mut c_void) -> sb::tresult {
        // SAFETY: `obj` is the plugin's out-pointer (or null, handled).
        let Some(out) = (unsafe { obj.as_mut() }) else { return sb::kInvalidArgument };
        *out = std::ptr::null_mut();
        // SAFETY: `cid` and `iid` point to 16-byte TUIDs (or are null, handled).
        let (Some(cid), Some(iid)) = (unsafe { cid.as_ref() }, unsafe { iid.as_ref() }) else { return sb::kInvalidArgument };
        let (cid, iid) = (tuid_bytes(cid), tuid_bytes(iid));
        let wants = |class: [u8; 16]| cid == class && (iid == class || iid == FUnknown::IID);
        let made = if wants(sv::IMessage::IID) {
            ComWrapper::new(HostMessage::default()).to_com_ptr::<sv::IMessage>().map(|p| p.into_raw().cast::<c_void>())
        } else if wants(sv::IAttributeList::IID) {
            ComWrapper::new(HostAttributes::default()).to_com_ptr::<sv::IAttributeList>().map(|p| p.into_raw().cast::<c_void>())
        } else {
            None
        };
        match made {
            Some(p) => {
                *out = p;
                sb::kResultOk
            }
            None => sb::kResultFalse,
        }
    }
}

impl IComponentHandlerTrait for HostContext {
    unsafe fn beginEdit(&self, id: sv::ParamID) -> sb::tresult {
        self.report(ControllerEvent::EditBegin { id });
        sb::kResultOk
    }
    unsafe fn performEdit(&self, id: sv::ParamID, value: sv::ParamValue) -> sb::tresult {
        if value.is_finite() {
            let v = value.clamp(0.0, 1.0);
            self.send_to_processor(id, v);
            self.report(ControllerEvent::ParamEdit { id, value: v });
        }
        sb::kResultOk
    }
    unsafe fn endEdit(&self, id: sv::ParamID) -> sb::tresult {
        self.report(ControllerEvent::EditEnd { id });
        sb::kResultOk
    }
    unsafe fn restartComponent(&self, flags: sb::int32) -> sb::tresult {
        use sv::RestartFlags_ as r;
        let flags = flags as u32;
        let has = |f: u32| flags & f != 0;
        if has(r::kLatencyChanged as u32) {
            self.report(ControllerEvent::LatencyChanged);
        }
        if has(r::kParamValuesChanged as u32) || has(r::kParamTitlesChanged as u32) {
            self.report(ControllerEvent::ParamsChanged);
        }
        if has(r::kReloadComponent as u32) || has(r::kIoChanged as u32) {
            self.reload_requested.store(true, Ordering::Release);
            self.report(ControllerEvent::ReloadRequested);
        }
        sb::kResultOk
    }
}

// ---- messages and attribute lists ----------------------------------------------------------


/// Longest string attribute kept, in UTF-16 units (longer ones are cut).
const ATTR_STRING_MAX: usize = 1 << 20;
/// Largest binary attribute kept.
const ATTR_BINARY_MAX: usize = 256 << 20;

/// One `IAttributeList` value. Strings and blobs live in their own allocation, so a pointer
/// handed out by `getBinary` stays valid while the list holds the value.
enum Attr {
    Int(i64),
    Float(f64),
    Str(Box<[u16]>),
    Bin(Box<[u8]>),
}

/// The host's `IAttributeList`: what a plugin puts in an `IMessage`.
#[derive(Default)]
pub(crate) struct HostAttributes {
    values: Mutex<std::collections::HashMap<Vec<u8>, Attr>>,
}

impl Class for HostAttributes {
    type Interfaces = (sv::IAttributeList,);
}

/// An attribute id's bytes (`None` for a null id).
fn attr_key(id: sv::IAttributeList_::AttrID) -> Option<Vec<u8>> {
    // SAFETY: a non-null AttrID is a NUL-terminated C string owned by the caller.
    (!id.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(id) }.to_bytes().to_vec())
}

impl HostAttributes {
    fn values(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<Vec<u8>, Attr>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, id: sv::IAttributeList_::AttrID, v: Attr) -> sb::tresult {
        let Some(k) = attr_key(id) else { return sb::kInvalidArgument };
        self.values().insert(k, v);
        sb::kResultOk
    }
}

impl sv::IAttributeListTrait for HostAttributes {
    unsafe fn setInt(&self, id: sv::IAttributeList_::AttrID, value: sb::int64) -> sb::tresult {
        self.set(id, Attr::Int(value))
    }

    unsafe fn getInt(&self, id: sv::IAttributeList_::AttrID, value: *mut sb::int64) -> sb::tresult {
        // SAFETY: `value` is the caller's out-pointer (or null, handled).
        match (
            attr_key(id).and_then(|k| match self.values().get(&k) {
                Some(Attr::Int(v)) => Some(*v),
                _ => None,
            }),
            unsafe { value.as_mut() },
        ) {
            (Some(v), Some(out)) => {
                *out = v;
                sb::kResultOk
            }
            _ => sb::kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: sv::IAttributeList_::AttrID, value: f64) -> sb::tresult {
        self.set(id, Attr::Float(value))
    }

    unsafe fn getFloat(&self, id: sv::IAttributeList_::AttrID, value: *mut f64) -> sb::tresult {
        // SAFETY: `value` is the caller's out-pointer (or null, handled).
        match (
            attr_key(id).and_then(|k| match self.values().get(&k) {
                Some(Attr::Float(v)) => Some(*v),
                _ => None,
            }),
            unsafe { value.as_mut() },
        ) {
            (Some(v), Some(out)) => {
                *out = v;
                sb::kResultOk
            }
            _ => sb::kResultFalse,
        }
    }

    unsafe fn setString(&self, id: sv::IAttributeList_::AttrID, string: *const sv::TChar) -> sb::tresult {
        if string.is_null() {
            return sb::kInvalidArgument;
        }
        let mut s = Vec::new();
        // SAFETY: a non-null string is NUL-terminated UTF-16 owned by the caller; reading stops at
        // the NUL or at ATTR_STRING_MAX units.
        while s.len() < ATTR_STRING_MAX {
            let c = unsafe { *string.add(s.len()) };
            if c == 0 {
                break;
            }
            s.push(c);
        }
        self.set(id, Attr::Str(s.into_boxed_slice()))
    }

    unsafe fn getString(&self, id: sv::IAttributeList_::AttrID, string: *mut sv::TChar, size_in_bytes: sb::uint32) -> sb::tresult {
        let units = size_in_bytes as usize / 2;
        if string.is_null() || units == 0 {
            return sb::kInvalidArgument;
        }
        let Some(k) = attr_key(id) else { return sb::kInvalidArgument };
        let values = self.values();
        let Some(Attr::Str(s)) = values.get(&k) else { return sb::kResultFalse };
        let n = s.len().min(units - 1);
        // SAFETY: the caller's buffer holds `units` UTF-16 units; `n + 1 <= units` are written.
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), string, n);
            *string.add(n) = 0;
        }
        sb::kResultOk
    }

    unsafe fn setBinary(&self, id: sv::IAttributeList_::AttrID, data: *const c_void, size_in_bytes: sb::uint32) -> sb::tresult {
        let n = size_in_bytes as usize;
        if n > ATTR_BINARY_MAX {
            return sb::kOutOfMemory;
        }
        let bytes: Box<[u8]> = if n == 0 || data.is_null() {
            Box::new([])
        } else {
            // SAFETY: the caller's `data` holds `n` readable bytes.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), n) }.into()
        };
        self.set(id, Attr::Bin(bytes))
    }

    unsafe fn getBinary(&self, id: sv::IAttributeList_::AttrID, data: *mut *const c_void, size_in_bytes: *mut sb::uint32) -> sb::tresult {
        let Some(k) = attr_key(id) else { return sb::kInvalidArgument };
        let values = self.values();
        let Some(Attr::Bin(b)) = values.get(&k) else { return sb::kResultFalse };
        // SAFETY: both are the caller's out-pointers (or null, handled). The pointer handed out
        // stays valid until this attribute is replaced or the list is released (VST3 contract).
        match (unsafe { data.as_mut() }, unsafe { size_in_bytes.as_mut() }) {
            (Some(d), Some(sz)) => {
                *d = b.as_ptr().cast::<c_void>();
                *sz = u32::try_from(b.len()).unwrap_or(u32::MAX);
                sb::kResultOk
            }
            _ => sb::kInvalidArgument,
        }
    }
}

/// The host's `IMessage`: an id and an attribute list, created for a plugin through
/// `IHostApplication::createInstance`.
pub(crate) struct HostMessage {
    id: Mutex<std::ffi::CString>,
    attributes: ComWrapper<HostAttributes>,
}

impl Default for HostMessage {
    fn default() -> Self {
        HostMessage { id: Mutex::new(std::ffi::CString::default()), attributes: ComWrapper::new(HostAttributes::default()) }
    }
}

impl Class for HostMessage {
    type Interfaces = (sv::IMessage,);
}

impl sv::IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> sb::FIDString {
        // The string's buffer lives until the next `setMessageID`, as the interface expects.
        self.id.lock().unwrap_or_else(PoisonError::into_inner).as_ptr()
    }

    unsafe fn setMessageID(&self, id: sb::FIDString) {
        // SAFETY: a non-null id is a NUL-terminated C string owned by the caller.
        let s = if id.is_null() { std::ffi::CString::default() } else { unsafe { std::ffi::CStr::from_ptr(id) }.to_owned() };
        *self.id.lock().unwrap_or_else(PoisonError::into_inner) = s;
    }

    unsafe fn getAttributes(&self) -> *mut sv::IAttributeList {
        // Not add-ref'ed: the list belongs to the message (VST3 contract).
        self.attributes.as_com_ref::<sv::IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

// ---- state streams -------------------------------------------------------------------------


/// An in-memory `IBStream`, used to hand the component's state to its controller.
pub(crate) struct MemoryStream {
    inner: Mutex<(Vec<u8>, usize)>,
}

impl Class for MemoryStream {
    type Interfaces = (sb::IBStream,);
}

impl MemoryStream {
    pub(crate) fn new(data: Vec<u8>) -> ComWrapper<MemoryStream> {
        ComWrapper::new(MemoryStream { inner: Mutex::new((data, 0)) })
    }

    /// Puts the read position back at the start.
    pub(crate) fn rewind(&self) {
        self.lock().1 = 0;
    }

    pub(crate) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.lock().0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Vec<u8>, usize)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: sb::int32, num_read: *mut sb::int32) -> sb::tresult {
        let mut g = self.lock();
        let (data, pos) = &mut *g;
        let want = usize::try_from(num_bytes).unwrap_or(0);
        let avail = data.len().saturating_sub(*pos);
        let n = want.min(avail);
        if n > 0 && !buffer.is_null() {
            let Some(src) = data.get(*pos..*pos + n) else { return sb::kInternalError };
            // SAFETY: the plugin provides `buffer` with room for `num_bytes` ≥ `n` bytes; `src`
            // is `n` valid bytes; the regions cannot overlap (ours is a private Vec).
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), buffer.cast::<u8>(), n) };
            *pos += n;
        }
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(r) = unsafe { num_read.as_mut() } {
            *r = i32::try_from(n).unwrap_or(0);
        }
        sb::kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: sb::int32, num_written: *mut sb::int32) -> sb::tresult {
        let mut g = self.lock();
        let (data, pos) = &mut *g;
        let n = usize::try_from(num_bytes).unwrap_or(0);
        let end = pos.saturating_add(n);
        if buffer.is_null() || end > MAX_STATE_BYTES {
            return sb::kResultFalse;
        }
        if data.len() < end {
            data.resize(end, 0);
        }
        // SAFETY: the plugin provides `n` readable bytes at `buffer`; the destination range was
        // just made valid.
        let src = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
        if let Some(dst) = data.get_mut(*pos..end) {
            dst.copy_from_slice(src);
        }
        *pos = end;
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(w) = unsafe { num_written.as_mut() } {
            *w = num_bytes;
        }
        sb::kResultOk
    }
    unsafe fn seek(&self, pos: sb::int64, mode: sb::int32, result: *mut sb::int64) -> sb::tresult {
        let mut g = self.lock();
        let len = i64::try_from(g.0.len()).unwrap_or(i64::MAX);
        let cur = i64::try_from(g.1).unwrap_or(0);
        let base = match mode as u32 {
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekSet as u32 => 0,
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekCur as u32 => cur,
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekEnd as u32 => len,
            _ => return sb::kInvalidArgument,
        };
        let new = base.saturating_add(pos).clamp(0, len);
        g.1 = usize::try_from(new).unwrap_or(0);
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(r) = unsafe { result.as_mut() } {
            *r = new;
        }
        sb::kResultOk
    }
    unsafe fn tell(&self, pos: *mut sb::int64) -> sb::tresult {
        let g = self.lock();
        // SAFETY: out-pointer from the plugin (or null, handled).
        match unsafe { pos.as_mut() } {
            Some(p) => {
                *p = i64::try_from(g.1).unwrap_or(0);
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }
}
