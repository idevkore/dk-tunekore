//! A window to put a plugin's editor in (macOS).
//!
//! Derived from soundCraft's `vst3-host` (`ffi.rs`, module `window`), MIT OR Apache-2.0; see
//! `third_party/soundcraft/`. The engine's own editor windows come with the plugins-in-the-app
//! work; this is the small version that examples and tests use, and a starting point for it.

use crate::plugin::{EditorSize, NativeParent, PluginController, PluginError};

#[cfg(target_os = "macos")]
pub use mac::EditorWindow;

#[cfg(not(target_os = "macos"))]
pub use other::EditorWindow;

/// Opens `controller`'s editor in a new titled window and shows it. Main thread only. Close the
/// editor (`controller.close_editor()`) before the window.
pub fn open_editor_window(controller: &mut dyn PluginController, title: &str) -> Result<EditorWindow, PluginError> {
    let window = EditorWindow::new(title, EditorSize { width: 400, height: 300 })?;
    let handle = window.handle();
    let size = controller.open_editor(window.parent(), Box::new(move |s| handle.resize(s)))?;
    window.resize(size);
    window.raise();
    Ok(window)
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSBackingStoreType, NSFloatingWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    /// An `NSWindow` (titled, closable, floating above the app's windows) whose content view the
    /// plugin's editor attaches to. Holds one retain of the window; main thread only.
    pub struct EditorWindow {
        window: *mut NSWindow,
    }

    // SAFETY: the pointer is only dereferenced on the main thread (every method checks
    // `MainThreadMarker`); the struct merely carries the retain between threads.
    unsafe impl Send for EditorWindow {}

    /// A copy of the window's address for the editor's resize callback.
    #[derive(Clone, Copy)]
    pub struct WindowHandle(*mut NSWindow);

    // SAFETY: as for `EditorWindow`: only used on the main thread.
    unsafe impl Send for WindowHandle {}

    impl WindowHandle {
        pub fn resize(self, size: EditorSize) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: the window stays retained by its `EditorWindow` while the editor is open.
            if let Some(w) = unsafe { self.0.as_ref() } {
                w.setContentSize(NSSize::new(f64::from(size.width), f64::from(size.height)));
            }
        }
    }

    impl EditorWindow {
        fn get(&self) -> Option<&NSWindow> {
            MainThreadMarker::new()?;
            // SAFETY: we hold a retain of this window (from `new`) until `close`.
            unsafe { self.window.as_ref() }
        }

        pub fn new(title: &str, size: EditorSize) -> Result<EditorWindow, PluginError> {
            let mtm = MainThreadMarker::new().ok_or_else(|| PluginError::Failed("windows must be made on the main thread".into()))?;
            let _app = NSApplication::sharedApplication(mtm);
            let rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(f64::from(size.width), f64::from(size.height)));
            // Plugin editors are transient windows. In particular they must not be minimizable:
            // minimized NSWindows become persistent thumbnails in the Dock.
            let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
            // SAFETY: a freshly allocated window on the main thread; valid rect and style.
            let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false) };
            // SAFETY: we keep our own retain and release it in `close`, so AppKit must not release
            // the window when the user closes it.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setLevel(NSFloatingWindowLevel);
            window.setHidesOnDeactivate(true);
            window.setExcludedFromWindowsMenu(true);
            window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
            window.center();
            Ok(EditorWindow { window: Retained::into_raw(window) })
        }

        pub fn handle(&self) -> WindowHandle {
            WindowHandle(self.window)
        }

        /// The content view (`NSView*`) the editor attaches to.
        pub fn parent(&self) -> NativeParent {
            NativeParent(self.get().and_then(|w| w.contentView()).map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v).cast_mut().cast()))
        }

        pub fn resize(&self, size: EditorSize) {
            self.handle().resize(size);
        }

        pub fn raise(&self) {
            if let Some(w) = self.get() {
                w.makeKeyAndOrderFront(None);
            }
        }

        pub fn is_visible(&self) -> bool {
            self.get().is_some_and(|w| w.isVisible())
        }

        pub fn close(self) {
            if let Some(w) = self.get() {
                w.orderOut(None);
                w.close();
            }
            if MainThreadMarker::new().is_some() {
                // SAFETY: balances the retain taken in `new`; nothing uses the pointer after this.
                drop(unsafe { Retained::from_raw(self.window) });
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod other {
    use super::*;

    /// Editor windows are not hosted on this platform yet.
    pub struct EditorWindow;

    #[derive(Clone, Copy)]
    pub struct WindowHandle;

    impl WindowHandle {
        pub fn resize(self, _size: EditorSize) {}
    }

    impl EditorWindow {
        pub fn new(_title: &str, _size: EditorSize) -> Result<EditorWindow, PluginError> {
            Err(PluginError::Unsupported("editor windows are only hosted on macOS so far".into()))
        }
        pub fn handle(&self) -> WindowHandle {
            WindowHandle
        }
        pub fn parent(&self) -> NativeParent {
            NativeParent(std::ptr::null_mut())
        }
        pub fn resize(&self, _size: EditorSize) {}
        pub fn raise(&self) {}
        pub fn is_visible(&self) -> bool {
            false
        }
        pub fn close(self) {}
    }
}
