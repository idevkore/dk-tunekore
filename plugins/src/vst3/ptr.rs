//! Owned pointers to plugin objects, and every method we call on them, through the shim.
//!
//! Plugin objects are COM objects: a pointer to a struct whose first field points at a table of
//! functions. [`PlugPtr`] owns one reference (released on drop) and each method here looks its
//! function up in that table and calls it through `shim`, so that a C++ exception thrown by the
//! plugin comes back as an `Err` instead of aborting the process. Even `addRef`, `release` and
//! `queryInterface` go through it.
//!
//! The methods are `unsafe`: they take the raw pointers and structs the plugin writes into, which
//! the caller must keep valid for the call, as for any COM call.

use super::shim::{self, Guarded};
use std::ffi::c_void;
use std::ptr::NonNull;
use vst3::Interface;
use vst3::Steinberg::{self as sb, Vst as sv};

/// One counted reference to a plugin's COM object of interface `I`.
pub struct PlugPtr<I: Interface> {
    ptr: NonNull<I>,
}

// SAFETY: a plugin object may be called from the thread the VST3 threading model allows for each
// method (main thread, audio thread); which thread calls what is decided by the code that owns
// the `PlugPtr` (see `instance.rs`). The pointer itself is just an address.
unsafe impl<I: Interface> Send for PlugPtr<I> {}
// SAFETY: as above; shared references only read the address.
unsafe impl<I: Interface> Sync for PlugPtr<I> {}

impl<I: Interface> PlugPtr<I> {
    /// Takes over one reference to `p`. `None` if it is null.
    ///
    /// # Safety
    /// `p` must be null or point to a live COM object that implements `I`, with a reference the
    /// caller owns.
    pub unsafe fn from_raw(p: *mut I) -> Option<Self> {
        NonNull::new(p).map(|ptr| PlugPtr { ptr })
    }

    pub fn as_ptr(&self) -> *mut I {
        self.ptr.as_ptr()
    }

    /// Asks the object for another of its interfaces (a counted reference), or `None` if it does
    /// not have it or threw.
    pub fn query<J: Interface>(&self) -> Option<PlugPtr<J>> {
        let this = self.ptr.as_ptr().cast::<sb::FUnknown>();
        let iid = J::IID;
        let mut obj: *mut c_void = std::ptr::null_mut();
        // SAFETY: a live object's first field is its vtable, which starts with FUnknown's
        // functions; `iid` is 16 readable bytes and `obj` is writable.
        let r = unsafe { shim::i_ppp((*(*this).vtbl).queryInterface, this, iid.as_ptr().cast::<sb::TUID>(), &mut obj as *mut *mut c_void) };
        match r {
            // SAFETY: on success `obj` holds a `J` pointer with a reference for us.
            Ok(sb::kResultOk) => unsafe { PlugPtr::from_raw(obj.cast::<J>()) },
            _ => None,
        }
    }

    /// Gives the reference up without releasing it.
    pub fn into_raw(self) -> *mut I {
        let p = self.ptr.as_ptr();
        std::mem::forget(self);
        p
    }
}

impl<I: Interface> Clone for PlugPtr<I> {
    fn clone(&self) -> Self {
        let this = self.ptr.as_ptr().cast::<sb::FUnknown>();
        // SAFETY: as in `query`. If addRef throws, the count may be one short; the clone is
        // still returned, and the object is leaked rather than freed early, because `drop` of
        // an excepting release is ignored too.
        let _ = unsafe { shim::u_p((*(*this).vtbl).addRef, this) };
        PlugPtr { ptr: self.ptr }
    }
}

impl<I: Interface> Drop for PlugPtr<I> {
    fn drop(&mut self) {
        let this = self.ptr.as_ptr().cast::<sb::FUnknown>();
        // SAFETY: as in `query`; we own the reference being released. A plugin that throws from
        // release is left as it is.
        let _ = unsafe { shim::u_p((*(*this).vtbl).release, this) };
    }
}

/// Generates `PlugPtr<$iface>::$name`, which calls vtable entry `$vt` through `shim::$sig`.
macro_rules! method {
    ($iface:ty, $name:ident, $sig:ident, [$($vt:ident).+], ($($arg:ident: $t:ty),*) -> $ret:ty) => {
        impl PlugPtr<$iface> {
            #[inline]
            #[allow(clippy::missing_safety_doc)]
            pub unsafe fn $name(&self $(, $arg: $t)*) -> Guarded<$ret> {
                let p = self.ptr.as_ptr();
                // SAFETY: the vtable of a live object; the caller keeps the arguments valid.
                unsafe { shim::$sig((*(*p).vtbl).$($vt).+, p.cast() $(, $arg)*) }
            }
        }
    };
}

// IPluginFactory, IPluginFactory2, IPluginFactory3
method!(sb::IPluginFactory, get_factory_info, i_pp, [getFactoryInfo], (info: *mut sb::PFactoryInfo) -> i32);
method!(sb::IPluginFactory, count_classes, i_p, [countClasses], () -> i32);
method!(sb::IPluginFactory, get_class_info, i_pip, [getClassInfo], (index: i32, info: *mut sb::PClassInfo) -> i32);
method!(sb::IPluginFactory, create_instance, i_pppp, [createInstance], (cid: sb::FIDString, iid: sb::FIDString, obj: *mut *mut c_void) -> i32);
method!(sb::IPluginFactory2, get_class_info2, i_pip, [getClassInfo2], (index: i32, info: *mut sb::PClassInfo2) -> i32);
method!(sb::IPluginFactory3, set_host_context, i_pp, [setHostContext], (context: *mut sb::FUnknown) -> i32);

// IComponent (and IPluginBase)
method!(sv::IComponent, initialize, i_pp, [base.initialize], (context: *mut sb::FUnknown) -> i32);
method!(sv::IComponent, terminate, i_p, [base.terminate], () -> i32);
method!(sv::IComponent, get_controller_class_id, i_pp, [getControllerClassId], (id: *mut sb::TUID) -> i32);
method!(sv::IComponent, get_bus_count, i_pii, [getBusCount], (media: i32, dir: i32) -> i32);
method!(sv::IComponent, get_bus_info, i_piiip, [getBusInfo], (media: i32, dir: i32, index: i32, bus: *mut sv::BusInfo) -> i32);
method!(sv::IComponent, activate_bus, i_piiib, [activateBus], (media: i32, dir: i32, index: i32, state: u8) -> i32);
method!(sv::IComponent, set_active, i_pb, [setActive], (state: u8) -> i32);
method!(sv::IComponent, set_state, i_pp, [setState], (stream: *mut sb::IBStream) -> i32);
method!(sv::IComponent, get_state, i_pp, [getState], (stream: *mut sb::IBStream) -> i32);

// IAudioProcessor
method!(sv::IAudioProcessor, set_bus_arrangements, i_ppipi, [setBusArrangements], (ins: *mut sv::SpeakerArrangement, n_ins: i32, outs: *mut sv::SpeakerArrangement, n_outs: i32) -> i32);
method!(sv::IAudioProcessor, get_bus_arrangement, i_piip, [getBusArrangement], (dir: i32, index: i32, arr: *mut sv::SpeakerArrangement) -> i32);
method!(sv::IAudioProcessor, can_process_sample_size, i_pi, [canProcessSampleSize], (size: i32) -> i32);
method!(sv::IAudioProcessor, get_latency_samples, u_p, [getLatencySamples], () -> u32);
method!(sv::IAudioProcessor, setup_processing, i_pp, [setupProcessing], (setup: *mut sv::ProcessSetup) -> i32);
method!(sv::IAudioProcessor, set_processing, i_pb, [setProcessing], (state: u8) -> i32);
method!(sv::IAudioProcessor, process, i_pp, [process], (data: *mut sv::ProcessData) -> i32);
method!(sv::IAudioProcessor, get_tail_samples, u_p, [getTailSamples], () -> u32);

// IEditController (and IPluginBase)
method!(sv::IEditController, initialize, i_pp, [base.initialize], (context: *mut sb::FUnknown) -> i32);
method!(sv::IEditController, terminate, i_p, [base.terminate], () -> i32);
method!(sv::IEditController, set_component_state, i_pp, [setComponentState], (stream: *mut sb::IBStream) -> i32);
method!(sv::IEditController, set_state, i_pp, [setState], (stream: *mut sb::IBStream) -> i32);
method!(sv::IEditController, get_state, i_pp, [getState], (stream: *mut sb::IBStream) -> i32);
method!(sv::IEditController, get_parameter_count, i_p, [getParameterCount], () -> i32);
method!(sv::IEditController, get_parameter_info, i_pip, [getParameterInfo], (index: i32, info: *mut sv::ParameterInfo) -> i32);
method!(sv::IEditController, get_param_string_by_value, i_pudp, [getParamStringByValue], (id: u32, value: f64, out: *mut sv::String128) -> i32);
method!(sv::IEditController, normalized_to_plain, d_pud, [normalizedParamToPlain], (id: u32, value: f64) -> f64);
method!(sv::IEditController, plain_to_normalized, d_pud, [plainParamToNormalized], (id: u32, value: f64) -> f64);
method!(sv::IEditController, get_param_normalized, d_pu, [getParamNormalized], (id: u32) -> f64);
method!(sv::IEditController, set_param_normalized, i_pud, [setParamNormalized], (id: u32, value: f64) -> i32);
method!(sv::IEditController, set_component_handler, i_pp, [setComponentHandler], (handler: *mut sv::IComponentHandler) -> i32);
method!(sv::IEditController, create_view, p_pp, [createView], (name: sb::FIDString) -> *mut sb::IPlugView);

// IConnectionPoint
method!(sv::IConnectionPoint, connect, i_pp, [connect], (other: *mut sv::IConnectionPoint) -> i32);
method!(sv::IConnectionPoint, disconnect, i_pp, [disconnect], (other: *mut sv::IConnectionPoint) -> i32);

// IMidiMapping
method!(sv::IMidiMapping, get_midi_controller_assignment, i_pihhp, [getMidiControllerAssignment], (bus: i32, channel: i16, controller: i16, id: *mut u32) -> i32);

// IPlugView
method!(sb::IPlugView, is_platform_type_supported, i_pp, [isPlatformTypeSupported], (kind: sb::FIDString) -> i32);
method!(sb::IPlugView, attached, i_ppp, [attached], (parent: *mut c_void, kind: sb::FIDString) -> i32);
method!(sb::IPlugView, removed, i_p, [removed], () -> i32);
method!(sb::IPlugView, get_size, i_pp, [getSize], (rect: *mut sb::ViewRect) -> i32);
method!(sb::IPlugView, on_size, i_pp, [onSize], (rect: *mut sb::ViewRect) -> i32);
method!(sb::IPlugView, set_frame, i_pp, [setFrame], (frame: *mut sb::IPlugFrame) -> i32);
method!(sb::IPlugView, can_resize, i_p, [canResize], () -> i32);
method!(sb::IPlugView, check_size_constraint, i_pp, [checkSizeConstraint], (rect: *mut sb::ViewRect) -> i32);
