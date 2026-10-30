//! A plugin's editor (`IPlugView`): creating it, attaching it to a window the host owns, and
//! following its resize requests.
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. What changed: the host window is not created here. The caller
//! passes the native view to attach to ([`NativeParent`]) and a callback for resizes, so the
//! engine can put the editor in a window of its own (see [`crate::editor_window`] for one).

use super::instance::Instance;
use super::ptr::PlugPtr;
use super::shim::{self, Exception};
use crate::plugin::{EditorSize, NativeParent, PluginError, ResizeCallback};
use std::sync::Mutex;
use vst3::Steinberg::{self as sb, IPlugFrameTrait, Vst as sv};
use vst3::{Class, ComWrapper};

/// The platform window type the view is asked to support.
#[cfg(target_os = "macos")]
fn platform_type() -> Option<sb::FIDString> {
    Some(sb::kPlatformTypeNSView)
}
#[cfg(windows)]
fn platform_type() -> Option<sb::FIDString> {
    Some(sb::kPlatformTypeHWND)
}
#[cfg(not(any(target_os = "macos", windows)))]
fn platform_type() -> Option<sb::FIDString> {
    None
}

/// Editors may only be opened on the main thread (AppKit, Win32).
fn on_main_thread() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2::MainThreadMarker::new().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// The `IPlugFrame` handed to a plugin view: `resizeView` asks the host to resize its window.
struct PlugFrame {
    on_resize: Mutex<ResizeCallback>,
}

impl Class for PlugFrame {
    type Interfaces = (sb::IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut sb::IPlugView, new_size: *mut sb::ViewRect) -> sb::tresult {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(r) = (unsafe { new_size.as_mut() }) else { return sb::kInvalidArgument };
        let size = rect_size(r);
        // A callback that resizes the window can make the plugin ask again; that nested ask is
        // ignored, the outer one is already being served.
        if let Ok(mut cb) = self.on_resize.try_lock() {
            cb(size);
        }
        if !view.is_null() {
            // SAFETY: the plugin passes its own live view; `r` is the plugin's own writable rect.
            // The shim keeps an exception from the view out of the plugin's own resize call.
            let _ = unsafe { shim::i_pp((*(*view).vtbl).onSize, view, r as *mut sb::ViewRect) };
        }
        sb::kResultOk
    }
}

/// Width and height of a view rect, clamped to a sane window size.
fn rect_size(r: &sb::ViewRect) -> EditorSize {
    EditorSize { width: r.right.saturating_sub(r.left).clamp(1, 16_384) as u32, height: r.bottom.saturating_sub(r.top).clamp(1, 16_384) as u32 }
}

fn rect_of(s: EditorSize) -> sb::ViewRect {
    sb::ViewRect { left: 0, top: 0, right: s.width.min(i32::MAX as u32) as i32, bottom: s.height.min(i32::MAX as u32) as i32 }
}

/// An open editor: the view and the frame it calls back into.
pub(crate) struct Editor {
    view: PlugPtr<sb::IPlugView>,
    _frame: ComWrapper<PlugFrame>,
}

fn create_view(c: &PlugPtr<sv::IEditController>) -> Result<Option<PlugPtr<sb::IPlugView>>, Exception> {
    // SAFETY: valid controller; the name is a static NUL-terminated string.
    let raw = unsafe { c.create_view(c"editor".as_ptr()) }?;
    // SAFETY: a non-null view comes with one reference owned by the caller.
    Ok(unsafe { PlugPtr::from_raw(raw) })
}

impl Editor {
    /// Whether the controller has an editor this platform can show.
    pub fn supported(c: &PlugPtr<sv::IEditController>) -> bool {
        let Some(platform) = platform_type() else { return false };
        let Ok(Some(view)) = create_view(c) else { return false };
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        matches!(unsafe { view.is_platform_type_supported(platform) }, Ok(sb::kResultTrue))
    }

    /// Creates the view and attaches it to `parent`. Main thread only.
    pub fn open(inst: &Instance, c: &PlugPtr<sv::IEditController>, parent: NativeParent, on_resize: ResizeCallback) -> Result<(Editor, EditorSize), PluginError> {
        if !on_main_thread() {
            return Err(PluginError::Failed("plugin editors must be opened on the main thread".into()));
        }
        let platform = platform_type().ok_or_else(|| PluginError::Unsupported("plugin editors are not hosted on this platform yet".into()))?;
        if parent.0.is_null() {
            return Err(PluginError::Failed("no window to put the editor in".into()));
        }
        let view = create_view(c)
            .map_err(|e| {
                inst.note_exception("IEditController::createView", &e.0);
                PluginError::Exception { during: "IEditController::createView", message: e.0 }
            })?
            .ok_or_else(|| PluginError::Unsupported("this plugin has no editor".into()))?;
        let throws = |during: &'static str, e: Exception| {
            inst.note_exception(during, &e.0);
            PluginError::Exception { during, message: e.0 }
        };
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        match unsafe { view.is_platform_type_supported(platform) } {
            Ok(sb::kResultTrue) => {}
            Ok(_) => return Err(PluginError::Unsupported("the editor does not support this platform's windows".into())),
            Err(e) => return Err(throws("IPlugView::isPlatformTypeSupported", e)),
        }
        let mut rect = sb::ViewRect { left: 0, top: 0, right: 400, bottom: 300 };
        // SAFETY: valid view; `rect` is writable. On failure the default size stays.
        let _ = unsafe { view.get_size(&mut rect) };
        let size = rect_size(&rect);
        let frame = ComWrapper::new(PlugFrame { on_resize: Mutex::new(on_resize) });
        let frame_ptr = frame.as_com_ref::<sb::IPlugFrame>().map_or(std::ptr::null_mut(), |f| f.as_ptr());
        // SAFETY: valid view; the frame is kept in the `Editor` and detached before it goes.
        unsafe { view.set_frame(frame_ptr) }.map_err(|e| throws("IPlugView::setFrame", e))?;
        // SAFETY: valid view; the parent is a live native view of the type named, owned by the host.
        match unsafe { view.attached(parent.0, platform) } {
            Ok(sb::kResultOk) => {}
            Ok(r) => {
                // SAFETY: valid view; detach our frame again.
                let _ = unsafe { view.set_frame(std::ptr::null_mut()) };
                return Err(PluginError::Failed(format!("the editor could not attach to its window (result {r})")));
            }
            Err(e) => return Err(throws("IPlugView::attached", e)),
        }
        // Many editors only know their size once attached (Kontakt reports one then).
        let mut after = rect;
        // SAFETY: valid, attached view; `after` is writable.
        let size = match unsafe { view.get_size(&mut after) } {
            Ok(sb::kResultOk) => rect_size(&after),
            _ => size,
        };
        Ok((Editor { view, _frame: frame }, size))
    }

    /// Detaches the view. Main thread only (callers on other threads leak it instead).
    pub fn close(self, inst: &Instance) {
        if !on_main_thread() {
            log::warn!("vst3: editor still open while its plugin is closed off the main thread; leaking it");
            std::mem::forget(self);
            inst.mark_leaked();
            return;
        }
        // SAFETY: valid, attached view; main thread. `removed` before the window goes, and the
        // frame is detached before it is released.
        unsafe {
            let _ = self.view.removed();
            let _ = self.view.set_frame(std::ptr::null_mut());
        }
    }

    pub fn resizable(&self, _inst: &Instance) -> bool {
        // SAFETY: valid view.
        matches!(unsafe { self.view.can_resize() }, Ok(sb::kResultTrue))
    }

    /// Offers the editor a new size (it may adjust it) and returns the size it took. An editor
    /// that cannot be resized keeps its size.
    pub fn resize(&self, inst: &Instance, size: EditorSize) -> EditorSize {
        if !self.resizable(inst) {
            let mut r = rect_of(size);
            // SAFETY: valid view; `r` is writable. On failure the offered size is reported back.
            let _ = unsafe { self.view.get_size(&mut r) };
            return rect_size(&r);
        }
        let mut rect = rect_of(size);
        // SAFETY: valid view; `rect` is writable.
        let _ = unsafe { self.view.check_size_constraint(&mut rect) };
        // SAFETY: as above.
        let _ = unsafe { self.view.on_size(&mut rect) };
        rect_size(&rect)
    }
}
