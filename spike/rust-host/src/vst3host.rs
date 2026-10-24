//! A minimal VST3 host written against the raw `vst3` bindings.
//!
//! Everything the Steinberg SDK's C++ hosting helpers would give us is written
//! here by hand: module loading, the host context, messages and attribute
//! lists, a memory stream, an event list, empty parameter changes, the
//! component handler and the plug frame.

use crate::audio::{Renderer, MAX_BLOCK};
use crate::transport::{Sequencer, SharedTime, TEMPO};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::BusDirections_::{kInput, kOutput};
use vst3::Steinberg::Vst::MediaTypes_::{kAudio, kEvent};
use vst3::Steinberg::Vst::ProcessModes_::kRealtime;
use vst3::Steinberg::Vst::SpeakerArr::kStereo;
use vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32;
use vst3::Steinberg::Vst::ViewType::kEditor;
use vst3::Steinberg::IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet};
use vst3::{Class, ComPtr, ComWrapper, Interface};

// ---------- small helpers ----------

fn ok(r: tresult) -> bool {
    r == kResultOk
}

fn check(what: &str, r: tresult) -> Result<(), String> {
    if ok(r) { Ok(()) } else { Err(format!("{what} failed ({r})")) }
}

fn guid_eq(tuid: *const TUID, guid: &[u8; 16]) -> bool {
    if tuid.is_null() {
        return false;
    }
    let t = unsafe { &*tuid };
    t.iter().zip(guid).all(|(a, b)| *a as u8 == *b)
}

fn cstr_field(f: &[c_char]) -> String {
    let bytes: Vec<u8> = f.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn write_string128(s: &str, dst: *mut String128) {
    let dst = unsafe { &mut *dst };
    let mut n = 0;
    for (i, u) in s.encode_utf16().take(127).enumerate() {
        dst[i] = u as TChar;
        n = i + 1;
    }
    dst[n] = 0;
}

// ---------- attribute list and message (needed for component <-> controller talk) ----------

enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<TChar>),
    Bin(Vec<u8>),
}

#[derive(Default)]
struct AttributeList {
    map: RefCell<HashMap<String, Attr>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

fn attr_key(id: AttrID) -> Option<String> {
    if id.is_null() { None } else { Some(unsafe { CStr::from_ptr(id) }.to_string_lossy().into_owned()) }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        let Some(k) = attr_key(id) else { return kInvalidArgument };
        self.map.borrow_mut().insert(k, Attr::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match attr_key(id).and_then(|k| match self.map.borrow().get(&k) {
            Some(Attr::Int(v)) => Some(*v),
            _ => None,
        }) {
            Some(v) => { *value = v; kResultOk }
            None => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(k) = attr_key(id) else { return kInvalidArgument };
        self.map.borrow_mut().insert(k, Attr::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match attr_key(id).and_then(|k| match self.map.borrow().get(&k) {
            Some(Attr::Float(v)) => Some(*v),
            _ => None,
        }) {
            Some(v) => { *value = v; kResultOk }
            None => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let Some(k) = attr_key(id) else { return kInvalidArgument };
        let mut v = Vec::new();
        let mut p = string;
        while !p.is_null() && *p != 0 {
            v.push(*p);
            p = p.add(1);
        }
        v.push(0);
        self.map.borrow_mut().insert(k, Attr::Str(v));
        kResultOk
    }
    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let map = self.map.borrow();
        let Some(Attr::Str(v)) = attr_key(id).and_then(|k| map.get(&k)) else { return kResultFalse };
        let cap = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if cap == 0 { return kResultFalse; }
        let n = v.len().min(cap);
        ptr::copy_nonoverlapping(v.as_ptr(), string, n);
        *string.add(n - 1) = 0;
        kResultOk
    }
    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let Some(k) = attr_key(id) else { return kInvalidArgument };
        let bytes = if data.is_null() { Vec::new() } else {
            std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize).to_vec()
        };
        self.map.borrow_mut().insert(k, Attr::Bin(bytes));
        kResultOk
    }
    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        let map = self.map.borrow();
        let Some(Attr::Bin(v)) = attr_key(id).and_then(|k| map.get(&k)) else { return kResultFalse };
        // The pointer stays valid while the entry lives, as in the SDK's HostAttributeList.
        *data = v.as_ptr() as *const c_void;
        *size_in_bytes = v.len() as uint32;
        kResultOk
    }
}

struct Message {
    id: RefCell<Option<CString>>,
    attrs: ComWrapper<AttributeList>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ref().map_or(ptr::null(), |c| c.as_ptr())
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        *self.id.borrow_mut() = if id.is_null() { None } else { Some(CStr::from_ptr(id).to_owned()) };
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed pointer: the message keeps the list alive (SDK HostMessage does the same).
        self.attrs.as_com_ref::<IAttributeList>().unwrap().as_ptr()
    }
}

// ---------- host application ----------

struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        write_string128("musiKore spike", name);
        kResultOk
    }
    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        *obj = ptr::null_mut();
        if guid_eq(cid, &IMessage::IID) && guid_eq(iid, &IMessage::IID) {
            let m = ComWrapper::new(Message { id: RefCell::new(None), attrs: ComWrapper::new(AttributeList::default()) });
            *obj = m.to_com_ptr::<IMessage>().unwrap().into_raw() as *mut c_void;
            return kResultOk;
        }
        if guid_eq(cid, &IAttributeList::IID) && guid_eq(iid, &IAttributeList::IID) {
            let a = ComWrapper::new(AttributeList::default());
            *obj = a.to_com_ptr::<IAttributeList>().unwrap().into_raw() as *mut c_void;
            return kResultOk;
        }
        kResultFalse
    }
}

// ---------- memory stream (state transfer) ----------

#[derive(Default)]
struct MemoryStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get();
        let n = (num_bytes.max(0) as usize).min(data.len().saturating_sub(pos));
        ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer as *mut u8, n);
        self.pos.set(pos + n);
        if !num_read.is_null() { *num_read = n as int32; }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_written: *mut int32) -> tresult {
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        let n = num_bytes.max(0) as usize;
        if data.len() < pos + n { data.resize(pos + n, 0); }
        ptr::copy_nonoverlapping(buffer as *const u8, data.as_mut_ptr().add(pos), n);
        self.pos.set(pos + n);
        if !num_written.is_null() { *num_written = n as int32; }
        kResultOk
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.data.borrow().len() as i64;
        let base = match mode as u32 {
            m if m == kIBSeekSet as u32 => 0,
            m if m == kIBSeekCur as u32 => self.pos.get() as i64,
            m if m == kIBSeekEnd as u32 => len,
            _ => return kInvalidArgument,
        };
        let p = (base + pos).clamp(0, len);
        self.pos.set(p as usize);
        if !result.is_null() { *result = p; }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() { return kInvalidArgument; }
        *pos = self.pos.get() as int64;
        kResultOk
    }
}

// ---------- event list and parameter changes (audio thread) ----------

const MAX_EVENTS: usize = 256;

struct EventList {
    events: RefCell<Vec<Event>>, // capacity fixed at MAX_EVENTS, never grows
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.events.borrow().len() as int32
    }
    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        match self.events.borrow().get(index as usize) {
            Some(ev) => { *e = *ev; kResultOk }
            None => kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        let mut v = self.events.borrow_mut();
        if v.len() >= MAX_EVENTS { return kResultFalse; }
        v.push(*e);
        kResultOk
    }
}

struct NoParams;

impl Class for NoParams {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for NoParams {
    unsafe fn getParameterCount(&self) -> int32 { 0 }
    unsafe fn getParameterData(&self, _index: int32) -> *mut IParamValueQueue { ptr::null_mut() }
    unsafe fn addParameterData(&self, _id: *const ParamID, _index: *mut int32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

// ---------- component handler and plug frame (UI thread) ----------

#[derive(Default)]
struct Handler {
    edits: Cell<u32>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult { kResultOk }
    unsafe fn performEdit(&self, _id: ParamID, _v: ParamValue) -> tresult {
        self.edits.set(self.edits.get() + 1);
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult { kResultOk }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult { kResultOk }
}

pub struct Frame {
    pub on_resize: RefCell<Option<Box<dyn Fn(i32, i32)>>>,
    pub resizes: Cell<u32>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() { return kInvalidArgument; }
        let r = *new_size;
        self.resizes.set(self.resizes.get() + 1);
        if let Some(f) = self.on_resize.borrow().as_ref() {
            f(r.right - r.left, r.bottom - r.top);
        }
        if let Some(v) = vst3::ComRef::<IPlugView>::from_raw(view) {
            v.onSize(new_size);
        }
        kResultOk
    }
}

// ---------- the module and the plugin ----------

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(a: *const c_void, buf: *const u8, len: isize, dir: u8) -> *const c_void;
    fn CFBundleCreate(a: *const c_void, url: *const c_void) -> *mut c_void;
    fn CFRelease(p: *const c_void);
}

struct Module {
    lib: libloading::Library,
    bundle: *mut c_void,
}

impl Module {
    fn load(bundle_path: &Path) -> Result<Self, String> {
        let macos = bundle_path.join("Contents/MacOS");
        let exe: PathBuf = std::fs::read_dir(&macos)
            .map_err(|e| format!("{}: {e}", macos.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .next()
            .ok_or("no executable in Contents/MacOS")?;
        let lib = unsafe { libloading::Library::new(&exe) }.map_err(|e| e.to_string())?;
        let p = bundle_path.as_os_str().as_encoded_bytes();
        let bundle = unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(ptr::null(), p.as_ptr(), p.len() as isize, 1);
            let b = CFBundleCreate(ptr::null(), url);
            CFRelease(url);
            b
        };
        unsafe {
            let entry: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> bool> =
                lib.get(b"bundleEntry\0").map_err(|e| e.to_string())?;
            if !entry(bundle) {
                return Err("bundleEntry returned false".into());
            }
        }
        Ok(Self { lib, bundle })
    }

    fn factory(&self) -> Result<ComPtr<IPluginFactory>, String> {
        unsafe {
            let get: libloading::Symbol<unsafe extern "system" fn() -> *mut IPluginFactory> =
                self.lib.get(b"GetPluginFactory\0").map_err(|e| e.to_string())?;
            ComPtr::from_raw(get()).ok_or_else(|| "GetPluginFactory returned null".into())
        }
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            if let Ok(exit) = self.lib.get::<unsafe extern "C" fn() -> bool>(b"bundleExit\0") {
                exit();
            }
            if !self.bundle.is_null() { CFRelease(self.bundle); }
        }
    }
}

pub struct Plugin {
    pub name: String,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    pub controller: ComPtr<IEditController>,
    connections: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    separate_controller: bool,
    pub frame: ComWrapper<Frame>,
    handler: ComWrapper<Handler>,
    _host: ComWrapper<HostApp>,
    out_channels: Vec<i32>,
    in_channels: Vec<i32>,
    _factory: ComPtr<IPluginFactory>,
    _module: Module, // dropped last
}

pub struct LoadReport {
    pub classes: Vec<String>,
    pub separate_controller: bool,
    pub audio_in_buses: usize,
    pub audio_out_buses: usize,
    pub event_in_buses: i32,
    pub latency: u32,
    pub state_bytes: usize,
    pub params: i32,
}

impl Plugin {
    pub fn load(bundle: &Path, sample_rate: f64) -> Result<(Self, LoadReport), String> {
        let module = Module::load(bundle)?;
        let factory = module.factory()?;
        let host = ComWrapper::new(HostApp);
        let host_unknown = host.to_com_ptr::<FUnknown>().unwrap();
        unsafe {
            if let Some(f3) = factory.cast::<IPluginFactory3>() {
                f3.setHostContext(host_unknown.as_ptr());
            }
        }

        // Find the instrument class.
        let mut classes = Vec::new();
        let mut chosen: Option<(TUID, String)> = None;
        unsafe {
            for i in 0..factory.countClasses() {
                let mut info: PClassInfo = std::mem::zeroed();
                if !ok(factory.getClassInfo(i, &mut info)) { continue; }
                let cat = cstr_field(&info.category);
                let name = cstr_field(&info.name);
                classes.push(format!("{name} [{cat}]"));
                if cat == "Audio Module Class" && chosen.is_none() {
                    chosen = Some((info.cid, name));
                }
            }
        }
        let (cid, name) = chosen.ok_or("no Audio Module Class in the factory")?;

        unsafe {
            let mut obj = ptr::null_mut();
            check("createInstance(IComponent)", factory.createInstance(cid.as_ptr(), IComponent::IID.as_ptr() as FIDString, &mut obj))?;
            let component = ComPtr::<IComponent>::from_raw(obj as *mut IComponent).ok_or("null component")?;
            check("component.initialize", component.initialize(host_unknown.as_ptr()))?;

            // Controller: the component itself, or a separate class.
            let (controller, separate) = match component.cast::<IEditController>() {
                Some(c) => (c, false),
                None => {
                    let mut ccid: TUID = [0; 16];
                    check("getControllerClassId", component.getControllerClassId(&mut ccid))?;
                    let mut obj = ptr::null_mut();
                    check("createInstance(IEditController)", factory.createInstance(ccid.as_ptr(), IEditController::IID.as_ptr() as FIDString, &mut obj))?;
                    let c = ComPtr::<IEditController>::from_raw(obj as *mut IEditController).ok_or("null controller")?;
                    check("controller.initialize", c.initialize(host_unknown.as_ptr()))?;
                    (c, true)
                }
            };
            let handler = ComWrapper::new(Handler::default());
            controller.setComponentHandler(handler.to_com_ptr::<IComponentHandler>().unwrap().as_ptr());

            let connections = if separate {
                match (component.cast::<IConnectionPoint>(), controller.cast::<IConnectionPoint>()) {
                    (Some(a), Some(b)) => {
                        a.connect(b.as_ptr());
                        b.connect(a.as_ptr());
                        Some((a, b))
                    }
                    _ => None,
                }
            } else {
                None
            };

            // Hand the processor's state to the controller.
            let stream = ComWrapper::new(MemoryStream::default());
            let sp = stream.to_com_ptr::<IBStream>().unwrap();
            let mut state_bytes = 0;
            if ok(component.getState(sp.as_ptr())) {
                state_bytes = stream.data.borrow().len();
                stream.pos.set(0);
                controller.setComponentState(sp.as_ptr());
            }

            let processor = component.cast::<IAudioProcessor>().ok_or("no IAudioProcessor")?;

            // Buses: every audio bus gets a stereo arrangement; activate the main
            // output and the first event input.
            let n_in = component.getBusCount(kAudio as MediaType, kInput as BusDirection);
            let n_out = component.getBusCount(kAudio as MediaType, kOutput as BusDirection);
            let n_ev = component.getBusCount(kEvent as MediaType, kInput as BusDirection);
            let mut ins = vec![kStereo as SpeakerArrangement; n_in as usize];
            let mut outs = vec![kStereo as SpeakerArrangement; n_out as usize];
            processor.setBusArrangements(ins.as_mut_ptr(), n_in, outs.as_mut_ptr(), n_out);
            let chans = |dir: i32, n: i32| -> Vec<i32> {
                (0..n).map(|i| {
                    let mut info: BusInfo = std::mem::zeroed();
                    component.getBusInfo(kAudio as MediaType, dir as BusDirection, i, &mut info);
                    info.channelCount
                }).collect()
            };
            let in_channels = chans(kInput as i32, n_in);
            let out_channels = chans(kOutput as i32, n_out);
            if n_out > 0 { component.activateBus(kAudio as MediaType, kOutput as BusDirection, 0, 1); }
            if n_ev > 0 { component.activateBus(kEvent as MediaType, kInput as BusDirection, 0, 1); }

            let mut setup = ProcessSetup {
                processMode: kRealtime as int32,
                symbolicSampleSize: kSample32 as int32,
                maxSamplesPerBlock: MAX_BLOCK as int32,
                sampleRate: sample_rate,
            };
            check("setupProcessing", processor.setupProcessing(&mut setup))?;
            check("setActive", component.setActive(1))?;
            processor.setProcessing(1);
            let latency = processor.getLatencySamples();
            let params = controller.getParameterCount();

            let report = LoadReport {
                classes,
                separate_controller: separate,
                audio_in_buses: n_in as usize,
                audio_out_buses: n_out as usize,
                event_in_buses: n_ev,
                latency,
                state_bytes,
                params,
            };
            let frame = ComWrapper::new(Frame { on_resize: RefCell::new(None), resizes: Cell::new(0) });
            Ok((
                Plugin {
                    name,
                    component,
                    processor,
                    controller,
                    connections,
                    separate_controller: separate,
                    frame,
                    handler,
                    _host: host,
                    out_channels,
                    in_channels,
                    _factory: factory,
                    _module: module,
                },
                report,
            ))
        }
    }

    pub fn param_edits(&self) -> u32 {
        self.handler.edits.get()
    }

    /// Saves and restores the processor state, as a project save and reopen would.
    pub fn round_trip_state(&self) -> Result<usize, String> {
        unsafe {
            let s = ComWrapper::new(MemoryStream::default());
            let p = s.to_com_ptr::<IBStream>().unwrap();
            check("getState", self.component.getState(p.as_ptr()))?;
            let n = s.data.borrow().len();
            s.pos.set(0);
            check("setState", self.component.setState(p.as_ptr()))?;
            s.pos.set(0);
            self.controller.setComponentState(p.as_ptr());
            Ok(n)
        }
    }

    pub fn renderer(&self, sample_rate: f64, time: Arc<SharedTime>) -> Box<dyn Renderer> {
        let alloc = |chs: &[i32]| -> Vec<Vec<Vec<f32>>> {
            chs.iter().map(|&c| (0..c.max(0)).map(|_| vec![0.0f32; MAX_BLOCK]).collect()).collect()
        };
        let mut r = Vst3Renderer {
            processor: self.processor.clone(),
            events: ComWrapper::new(EventList { events: RefCell::new(Vec::with_capacity(MAX_EVENTS)) }),
            params: ComWrapper::new(NoParams),
            seq: Sequencer::new(sample_rate),
            sample_rate,
            out_bufs: alloc(&self.out_channels),
            in_bufs: alloc(&self.in_channels),
            out_ptrs: Vec::new(),
            in_ptrs: Vec::new(),
            out_buses: Vec::new(),
            in_buses: Vec::new(),
            time,
        };
        r.out_ptrs = r.out_bufs.iter_mut().map(|b| b.iter_mut().map(|c| c.as_mut_ptr()).collect()).collect();
        r.in_ptrs = r.in_bufs.iter_mut().map(|b| b.iter_mut().map(|c| c.as_mut_ptr()).collect()).collect();
        r.out_buses = r.out_ptrs.iter_mut().map(|p| bus(p)).collect();
        r.in_buses = r.in_ptrs.iter_mut().map(|p| bus(p)).collect();
        Box::new(r)
    }
}

fn bus(ptrs: &mut Vec<*mut f32>) -> AudioBusBuffers {
    AudioBusBuffers {
        numChannels: ptrs.len() as int32,
        silenceFlags: 0,
        __field0: AudioBusBuffers__type0 { channelBuffers32: ptrs.as_mut_ptr() },
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = &self.connections {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if self.separate_controller {
                self.controller.terminate();
            }
            self.component.terminate();
        }
    }
}

struct Vst3Renderer {
    processor: ComPtr<IAudioProcessor>,
    events: ComWrapper<EventList>,
    params: ComWrapper<NoParams>,
    seq: Sequencer,
    sample_rate: f64,
    out_bufs: Vec<Vec<Vec<f32>>>,
    in_bufs: Vec<Vec<Vec<f32>>>,
    out_ptrs: Vec<Vec<*mut f32>>,
    in_ptrs: Vec<Vec<*mut f32>>,
    out_buses: Vec<AudioBusBuffers>,
    in_buses: Vec<AudioBusBuffers>,
    time: Arc<SharedTime>,
}

// The renderer moves to the audio thread once and is only used there.
unsafe impl Send for Vst3Renderer {}

impl Renderer for Vst3Renderer {
    fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        {
            let mut evs = self.events.events.borrow_mut();
            evs.clear();
            let block = self.seq.block(n, |on, pitch, offset| {
                if evs.len() >= MAX_EVENTS { return; }
                let mut e: Event = unsafe { std::mem::zeroed() };
                e.sampleOffset = offset as int32;
                if on {
                    e.r#type = Event_::EventTypes_::kNoteOnEvent as uint16;
                    e.__field0.noteOn = NoteOnEvent { channel: 0, pitch: pitch as int16, tuning: 0.0, velocity: 0.8, length: 0, noteId: -1 };
                } else {
                    e.r#type = Event_::EventTypes_::kNoteOffEvent as uint16;
                    e.__field0.noteOff = NoteOffEvent { channel: 0, pitch: pitch as int16, velocity: 0.0, noteId: -1, tuning: 0.0 };
                }
                evs.push(e);
            });
            self.time.set(block.beats, block.start_samples);
            drop(evs);

            let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
            use ProcessContext_::StatesAndFlags_::*;
            ctx.state = (kPlaying | kTempoValid | kProjectTimeMusicValid | kBarPositionValid | kTimeSigValid) as uint32;
            ctx.sampleRate = self.sample_rate;
            ctx.projectTimeSamples = block.start_samples;
            ctx.projectTimeMusic = block.beats;
            ctx.barPositionMusic = block.bar_start_beats;
            ctx.tempo = TEMPO;
            ctx.timeSigNumerator = 4;
            ctx.timeSigDenominator = 4;

            for b in &mut self.in_bufs { for c in b { c[..n].fill(0.0); } }
            let mut data = ProcessData {
                processMode: kRealtime as int32,
                symbolicSampleSize: kSample32 as int32,
                numSamples: n as int32,
                numInputs: self.in_buses.len() as int32,
                numOutputs: self.out_buses.len() as int32,
                inputs: if self.in_buses.is_empty() { ptr::null_mut() } else { self.in_buses.as_mut_ptr() },
                outputs: self.out_buses.as_mut_ptr(),
                inputParameterChanges: self.params.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
                outputParameterChanges: ptr::null_mut(),
                inputEvents: self.events.as_com_ref::<IEventList>().unwrap().as_ptr(),
                outputEvents: ptr::null_mut(),
                processContext: &mut ctx,
            };
            unsafe { self.processor.process(&mut data) };
        }
        if let Some(main) = self.out_bufs.first() {
            left.copy_from_slice(&main[0][..n]);
            right.copy_from_slice(&main[main.len().min(2) - 1][..n]);
        } else {
            left.fill(0.0);
            right.fill(0.0);
        }
        let _ = &self.out_ptrs;
        let _ = &self.in_ptrs;
    }
}

// ---------- editor ----------

pub struct Editor {
    pub view: ComPtr<IPlugView>,
    pub width: i32,
    pub height: i32,
}

impl Plugin {
    /// Creates the editor view and attaches it to `parent` (an NSView).
    pub fn open_editor(&self, attach: impl FnOnce(i32, i32) -> *mut c_void) -> Result<Editor, String> {
        unsafe {
            let raw = self.controller.createView(kEditor);
            let view = ComPtr::<IPlugView>::from_raw(raw).ok_or("createView returned null")?;
            check("isPlatformTypeSupported(NSView)", view.isPlatformTypeSupported(kPlatformTypeNSView))?;
            let mut r: ViewRect = std::mem::zeroed();
            check("getSize", view.getSize(&mut r))?;
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            view.setFrame(self.frame.to_com_ptr::<IPlugFrame>().unwrap().as_ptr());
            let parent = attach(w, h);
            check("attached", view.attached(parent, kPlatformTypeNSView))?;
            Ok(Editor { view, width: w, height: h })
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            self.view.setFrame(ptr::null_mut());
            self.view.removed();
        }
    }
}
