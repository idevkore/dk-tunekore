//! A minimal Audio Unit (AUv2, in process) host using the objc2 AudioToolbox bindings.

use crate::audio::{Renderer, MAX_BLOCK};
use crate::transport::{Sequencer, SharedTime, TEMPO};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{sel, ClassType};
use objc2_audio_toolbox::*;
use objc2_core_audio_types::*;
use objc2_foundation::{NSBundle, NSDictionary, NSString, NSURL};
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::Arc;

fn four_cc(s: &str) -> u32 {
    s.bytes().take(4).fold(0u32, |a, b| (a << 8) | b as u32)
}

fn check(what: &str, st: i32) -> Result<(), String> {
    if st == 0 { Ok(()) } else { Err(format!("{what} failed (OSStatus {st})")) }
}

pub struct Unit {
    pub unit: AudioUnit,
    pub name: String,
    time: Arc<SharedTime>,
    sample_rate: f64,
}

// The unit pointer is used on the main thread for setup and the editor, and on
// the audio thread for rendering, as AU hosts do.
unsafe impl Send for Unit {}

pub struct LoadReport {
    pub description: String,
    pub has_cocoa_ui: bool,
    pub latency_s: f64,
}

/// Reads the first AudioComponents entry of a .component bundle's Info.plist.
fn description_of(bundle: &Path) -> Result<(AudioComponentDescription, String), String> {
    let plist = bundle.join("Contents/Info.plist");
    let path = NSString::from_str(plist.to_str().ok_or("bad path")?);
    let dict = unsafe { NSDictionary::<AnyObject, AnyObject>::dictionaryWithContentsOfFile(&path) }
        .ok_or("cannot read Info.plist")?;
    let get = |d: &NSDictionary<AnyObject, AnyObject>, k: &str| -> Option<objc2::rc::Retained<AnyObject>> {
        let key = NSString::from_str(k);
        unsafe { d.objectForKey(&*(&*key as *const NSString as *const AnyObject)) }
    };
    let comps = get(&dict, "AudioComponents").ok_or("no AudioComponents")?;
    let first: objc2::rc::Retained<AnyObject> = unsafe { objc2::msg_send![&*comps, firstObject] };
    let first: &NSDictionary<AnyObject, AnyObject> = unsafe { &*(&*first as *const AnyObject as *const NSDictionary<AnyObject, AnyObject>) };
    let s = |k: &str| -> String {
        get(first, k).map(|v| unsafe { &*(&*v as *const AnyObject as *const NSString) }.to_string()).unwrap_or_default()
    };
    let (ty, sub, manu, name) = (s("type"), s("subtype"), s("manufacturer"), s("name"));
    let desc = AudioComponentDescription {
        componentType: four_cc(&ty),
        componentSubType: four_cc(&sub),
        componentManufacturer: four_cc(&manu),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    Ok((desc, format!("{name} ({ty}/{sub}/{manu})")))
}

// Host callbacks: the AU calls these during AudioUnitRender to read tempo and position.
unsafe extern "C-unwind" fn beat_and_tempo(user: *mut c_void, beat: *mut f64, tempo: *mut f64) -> i32 {
    let t = &*(user as *const SharedTime);
    t.queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !beat.is_null() { *beat = t.beats(); }
    if !tempo.is_null() { *tempo = TEMPO; }
    0
}

unsafe extern "C-unwind" fn musical_time(
    user: *mut c_void, delta_to_next_beat: *mut u32, num: *mut f32, den: *mut u32, downbeat: *mut f64,
) -> i32 {
    let t = &*(user as *const SharedTime);
    t.queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let beats = t.beats();
    if !delta_to_next_beat.is_null() {
        *delta_to_next_beat = 0; // approximate; fine for the spike
    }
    if !num.is_null() { *num = 4.0; }
    if !den.is_null() { *den = 4; }
    if !downbeat.is_null() { *downbeat = (beats / 4.0).floor() * 4.0; }
    0
}

unsafe extern "C-unwind" fn transport2(
    user: *mut c_void, playing: *mut u8, recording: *mut u8, changed: *mut u8,
    sample: *mut f64, cycling: *mut u8, cycle_start: *mut f64, cycle_end: *mut f64,
) -> i32 {
    let t = &*(user as *const SharedTime);
    t.queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !playing.is_null() { *playing = 1; }
    if !recording.is_null() { *recording = 0; }
    if !changed.is_null() { *changed = 0; }
    if !sample.is_null() { *sample = t.samples() as f64; }
    if !cycling.is_null() { *cycling = 0; }
    if !cycle_start.is_null() { *cycle_start = 0.0; }
    if !cycle_end.is_null() { *cycle_end = 0.0; }
    0
}

impl Unit {
    pub fn load(bundle: &Path, sample_rate: f64, time: Arc<SharedTime>) -> Result<(Self, LoadReport), String> {
        let (mut desc, label) = description_of(bundle)?;
        unsafe {
            let comp = AudioComponentFindNext(ptr::null_mut(), NonNull::from(&mut desc));
            if comp.is_null() { return Err(format!("AudioComponentFindNext found nothing for {label}")); }
            let mut unit: AudioComponentInstance = ptr::null_mut();
            check("AudioComponentInstanceNew", AudioComponentInstanceNew(comp, NonNull::from(&mut unit)))?;

            let asbd = AudioStreamBasicDescription {
                mSampleRate: sample_rate,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved,
                mBytesPerPacket: 4,
                mFramesPerPacket: 1,
                mBytesPerFrame: 4,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            let set = |id, scope, el, data: *const c_void, size: usize| {
                AudioUnitSetProperty(unit, id, scope, el, data, size as u32)
            };
            check("StreamFormat", set(kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 0,
                &asbd as *const _ as *const c_void, std::mem::size_of_val(&asbd)))?;
            let max = MAX_BLOCK as u32;
            check("MaximumFramesPerSlice", set(kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0,
                &max as *const _ as *const c_void, 4))?;
            let cb = HostCallbackInfo {
                hostUserData: Arc::as_ptr(&time) as *mut c_void,
                beatAndTempoProc: Some(beat_and_tempo),
                musicalTimeLocationProc: Some(musical_time),
                transportStateProc: None,
                transportStateProc2: Some(transport2),
            };
            check("HostCallbacks", set(kAudioUnitProperty_HostCallbacks, kAudioUnitScope_Global, 0,
                &cb as *const _ as *const c_void, std::mem::size_of_val(&cb)))?;
            check("AudioUnitInitialize", AudioUnitInitialize(unit))?;

            let mut size = 0u32;
            let has_ui = AudioUnitGetPropertyInfo(unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0, &mut size, ptr::null_mut()) == 0 && size > 0;
            let mut lat = 0f64;
            let mut lsz = 8u32;
            AudioUnitGetProperty(unit, kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0,
                NonNull::from(&mut lat).cast(), NonNull::from(&mut lsz));

            Ok((
                Unit { unit, name: label.clone(), time, sample_rate },
                LoadReport { description: label, has_cocoa_ui: has_ui, latency_s: lat },
            ))
        }
    }

    pub fn renderer(&self) -> Box<dyn Renderer> {
        Box::new(AuRenderer {
            unit: self.unit,
            seq: Sequencer::new(self.sample_rate),
            time: self.time.clone(),
            sample_time: 0.0,
        })
    }

    /// Saves and restores the unit's state (kAudioUnitProperty_ClassInfo), as a project would.
    pub fn round_trip_state(&self) -> Result<(), String> {
        unsafe {
            let mut plist: *const c_void = ptr::null();
            let mut size = std::mem::size_of::<*const c_void>() as u32;
            check("get ClassInfo", AudioUnitGetProperty(self.unit, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0,
                NonNull::from(&mut plist).cast(), NonNull::from(&mut size)))?;
            let r = AudioUnitSetProperty(self.unit, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0,
                &plist as *const _ as *const c_void, size);
            objc2_core_foundation::CFRetained::<objc2_core_foundation::CFType>::from_raw(NonNull::new(plist as *mut _).unwrap());
            check("set ClassInfo", r)
        }
    }

    /// Builds the unit's Cocoa editor view. Returns an NSView pointer.
    pub fn editor_view(&self) -> Result<*mut AnyObject, String> {
        unsafe {
            let mut info = std::mem::MaybeUninit::<AudioUnitCocoaViewInfo>::uninit();
            let mut size = std::mem::size_of::<AudioUnitCocoaViewInfo>() as u32;
            check("get CocoaUI", AudioUnitGetProperty(self.unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0,
                NonNull::from(&mut info).cast(), NonNull::from(&mut size)))?;
            let info = info.assume_init();
            let url: &NSURL = &*(info.mCocoaAUViewBundleLocation.as_ptr() as *const NSURL);
            let class_name: &NSString = &*(info.mCocoaAUViewClass[0].as_ptr() as *const NSString);
            let bundle = NSBundle::bundleWithURL(url).ok_or("cannot open the view bundle")?;
            let cls: &AnyClass = bundle.classNamed(class_name).ok_or_else(|| format!("no class {class_name}"))?;
            let factory: *mut AnyObject = objc2::msg_send![cls, new];
            // -[AUCocoaUIBase uiViewForAudioUnit:withSize:], called through objc_msgSend
            // directly: objc2 has no binding for it, and AudioUnit has no Encode impl.
            type F = unsafe extern "C" fn(*mut AnyObject, Sel, AudioUnit, objc2_foundation::NSSize) -> *mut AnyObject;
            let f: F = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
            let view = f(factory, sel!(uiViewForAudioUnit:withSize:), self.unit, objc2_foundation::NSSize::new(0.0, 0.0));
            let _ = NSURL::class();
            if view.is_null() { return Err("uiViewForAudioUnit returned nil".into()); }
            // The method returns an autoreleased view; keep it.
            let _: *mut AnyObject = objc2::msg_send![view, retain];
            Ok(view)
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        unsafe {
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

#[repr(C)]
struct StereoBufferList {
    count: u32,
    buffers: [AudioBuffer; 2],
}

struct AuRenderer {
    unit: AudioUnit,
    seq: Sequencer,
    time: Arc<SharedTime>,
    sample_time: f64,
}

unsafe impl Send for AuRenderer {}

impl Renderer for AuRenderer {
    fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        let unit = self.unit;
        let block = self.seq.block(n, |on, pitch, offset| unsafe {
            MusicDeviceMIDIEvent(unit, if on { 0x90 } else { 0x80 }, pitch as u32, if on { 100 } else { 0 }, offset);
        });
        self.time.set(block.beats, block.start_samples);
        let mut abl = StereoBufferList {
            count: 2,
            buffers: [
                AudioBuffer { mNumberChannels: 1, mDataByteSize: (n * 4) as u32, mData: left.as_mut_ptr().cast() },
                AudioBuffer { mNumberChannels: 1, mDataByteSize: (n * 4) as u32, mData: right.as_mut_ptr().cast() },
            ],
        };
        let mut ts: AudioTimeStamp = unsafe { std::mem::zeroed() };
        ts.mSampleTime = self.sample_time;
        ts.mFlags = AudioTimeStampFlags::SampleTimeValid;
        let mut flags = AudioUnitRenderActionFlags(0);
        let st = unsafe {
            AudioUnitRender(unit, &mut flags, NonNull::from(&mut ts), 0, n as u32,
                NonNull::from(&mut abl).cast())
        };
        if st != 0 {
            left.fill(0.0);
            right.fill(0.0);
        } else {
            // A unit may point mData at its own buffers instead of filling ours.
            for (buf, dst) in abl.buffers.iter().zip([left.as_mut_ptr(), right.as_mut_ptr()]) {
                if !buf.mData.is_null() && buf.mData as *mut f32 != dst {
                    unsafe { ptr::copy_nonoverlapping(buf.mData as *const f32, dst, n) };
                }
            }
        }
        self.sample_time += n as f64;
    }
}
