//! A plugin's editor through the CLAP GUI extension: embedded in a native view the host owns, as
//! VST3 editors are, and following the plugin's resize requests.
//!
//! Derived from soundCraft's `clap-host` (`ffi.rs`: `GuiLink::open_embedded`), MIT OR Apache-2.0;
//! see `third_party/soundcraft/`. What changed: the host window is not made here (the caller
//! passes the view to attach to, as for VST3); every call goes through the shim; the plugin's
//! `request_resize` reaches the caller's callback from `poll`, on the main thread; an editor that
//! can't be closed properly is left alone (and its plugin leaked) rather than torn down.

use super::instance::{Instance, Objects};
use super::shim;
use crate::plugin::{EditorSize, NativeParent, PluginError, ResizeCallback};
use clap_sys::ext::gui::{clap_plugin_gui, clap_window, clap_window_handle};
use std::ffi::CStr;

/// The window API the editor is asked to use.
fn api() -> Option<&'static CStr> {
    if cfg!(target_os = "macos") {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_COCOA)
    } else if cfg!(windows) {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_WIN32)
    } else {
        None
    }
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

/// Clamps a size to a sane window size.
fn sane(w: u32, h: u32) -> EditorSize {
    EditorSize { width: w.clamp(1, 16_384), height: h.clamp(1, 16_384) }
}

/// The editor's size as the plugin reports it, if it does.
fn size_of_editor(inst: &Instance, o: &Objects, g: &clap_plugin_gui) -> Option<EditorSize> {
    let (mut w, mut h) = (0u32, 0u32);
    // SAFETY: a live plugin with a created editor; main thread; both out-pointers are locals.
    let got = inst.soft("clap_plugin_gui.get_size", unsafe { shim::b_ppp(g.get_size, o.plugin, &mut w as *mut u32, &mut h as *mut u32) }).ok().flatten();
    (got == Some(true) && w > 0 && h > 0).then(|| sane(w, h))
}

/// An open editor.
pub(crate) struct Editor {
    on_resize: ResizeCallback,
}

impl Editor {
    /// Whether the plugin has an editor this platform can embed.
    pub fn supported(inst: &Instance) -> bool {
        let (Some(api), Some(o)) = (api(), inst.live()) else { return false };
        let Some(g) = o.gui() else { return false };
        // SAFETY: a live plugin and its own extension; `api` is a static NUL-terminated string.
        inst.soft("clap_plugin_gui.is_api_supported", unsafe { shim::b_ppz(g.is_api_supported, o.plugin, api.as_ptr(), false) }) == Ok(Some(true))
    }

    /// Creates the editor and puts it in `parent`. Main thread only.
    pub fn open(inst: &Instance, parent: NativeParent, on_resize: ResizeCallback) -> Result<(Editor, EditorSize), PluginError> {
        if !on_main_thread() {
            return Err(PluginError::Failed("plugin editors must be opened on the main thread".into()));
        }
        let api = api().ok_or_else(|| PluginError::Unsupported("plugin editors are not hosted on this platform yet".into()))?;
        if parent.0.is_null() {
            return Err(PluginError::Failed("no window to put the editor in".into()));
        }
        let o = inst.live().ok_or_else(|| PluginError::Failed("the plugin is gone".into()))?;
        let g = o.gui().ok_or_else(|| PluginError::Unsupported("this plugin has no editor".into()))?;
        if !Editor::supported(inst) {
            return Err(PluginError::Unsupported("the editor does not support this platform's windows".into()));
        }
        // SAFETY: a live plugin without an editor; main thread; `api` is static. Embedded, not
        // floating: tuneKore owns the window.
        match inst.guard("clap_plugin_gui.create", unsafe { shim::b_ppz(g.create, o.plugin, api.as_ptr(), false) })? {
            Some(true) => {}
            _ => return Err(PluginError::Failed("the plugin couldn't make its editor".into())),
        }
        let destroy = || {
            // SAFETY: a live plugin with a created editor; main thread; destroyed once.
            let _ = inst.guard("clap_plugin_gui.destroy", unsafe { shim::v_p(g.destroy, o.plugin) });
        };
        let first = size_of_editor(inst, &o, &g).unwrap_or(EditorSize { width: 400, height: 300 });
        let window = clap_window { api: api.as_ptr(), specific: clap_window_handle { ptr: parent.0 } };
        // SAFETY: a live plugin with a created editor; `window` names the caller's live view and
        // outlives the call.
        match inst.guard("clap_plugin_gui.set_parent", unsafe { shim::b_pp(g.set_parent, o.plugin, &window as *const clap_window) })? {
            Some(true) => {}
            _ => {
                destroy();
                return Err(PluginError::Failed("the editor could not attach to its window".into()));
            }
        }
        // SAFETY: a live plugin with a created, parented editor. Some editors say no to `show` when
        // embedded and show anyway: not an error.
        if inst.guard("clap_plugin_gui.show", unsafe { shim::b_p(g.show, o.plugin) }).is_err() {
            return Err(PluginError::Failed("the editor failed while showing".into()));
        }
        // Many editors only know their size once they have a window.
        let size = size_of_editor(inst, &o, &g).unwrap_or(first);
        Ok((Editor { on_resize }, size))
    }

    /// The plugin asked for a new size (`request_resize`): tells the host's window. Main thread.
    pub fn requested(&mut self, size: EditorSize) {
        (self.on_resize)(sane(size.width, size.height));
    }

    /// Hides and destroys the editor. Returns `true` if it had to be left instead: off the main
    /// thread, or when the plugin had failed (it is not called again). Its owner must then not
    /// destroy the plugin either, since the editor may still draw into the window.
    pub fn close(self, inst: &Instance) -> bool {
        // Whatever happens next, the host's window is about to go: a late request goes nowhere.
        let Editor { on_resize } = self;
        drop(on_resize);
        let o = match inst.live() {
            Some(o) if on_main_thread() => o,
            _ => {
                log::warn!("clap {}: leaving an editor that cannot be closed here", inst.info.name);
                return true;
            }
        };
        let Some(g) = o.gui() else { return false };
        // SAFETY: a live plugin with an open editor; main thread. Hidden, then destroyed, before
        // the window goes; one that throws is not called again.
        if inst.guard("clap_plugin_gui.hide", unsafe { shim::b_p(g.hide, o.plugin) }).is_err() {
            return true;
        }
        // SAFETY: as above; destroyed once.
        inst.guard("clap_plugin_gui.destroy", unsafe { shim::v_p(g.destroy, o.plugin) }).is_err()
    }

    pub fn resizable(&self, inst: &Instance) -> bool {
        let Some(o) = inst.live() else { return false };
        let Some(g) = o.gui() else { return false };
        // SAFETY: a live plugin with an open editor; main thread.
        inst.soft("clap_plugin_gui.can_resize", unsafe { shim::b_p(g.can_resize, o.plugin) }) == Ok(Some(true))
    }

    /// Offers the editor a new size (it may adjust it) and returns the size it took. An editor
    /// that cannot be resized keeps its size.
    pub fn resize(&self, inst: &Instance, size: EditorSize) -> EditorSize {
        let Some(o) = inst.live() else { return size };
        let Some(g) = o.gui() else { return size };
        if !self.resizable(inst) {
            return size_of_editor(inst, &o, &g).unwrap_or(size);
        }
        let (mut w, mut h) = (size.width, size.height);
        // SAFETY: a live plugin with an open, resizable editor; main thread; locals for the size.
        let _ = inst.soft("clap_plugin_gui.adjust_size", unsafe { shim::b_ppp(g.adjust_size, o.plugin, &mut w as *mut u32, &mut h as *mut u32) });
        let took = sane(w, h);
        // SAFETY: as above.
        match inst.soft("clap_plugin_gui.set_size", unsafe { shim::b_puu(g.set_size, o.plugin, took.width, took.height) }) {
            Ok(Some(true)) => took,
            _ => size_of_editor(inst, &o, &g).unwrap_or(took),
        }
    }
}
