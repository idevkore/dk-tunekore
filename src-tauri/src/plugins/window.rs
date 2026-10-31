//! Plugin editor windows (macOS): one native window per open editor,
//! floating over tuneKore's main window and hidden while tuneKore isn't the
//! active app, titled after the track and the plugin.
//!
//! The window's delegate reports the user closing or resizing it through a
//! callback. That callback runs inside AppKit, maybe while the plugins module
//! is itself closing the window with its document locked, so the module's
//! callback only queues the work for later (it never takes a lock).
//!
//! Everything here is main thread only; each method checks.

use tunekore_engine::plugins::{EditorSize, NativeParent};

/// What the user did to a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    /// Closed it with its close button.
    Closed,
    /// Resized it by its edge, to this content size.
    Resized(EditorSize),
}

/// Where a window was, so it opens there again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Place {
    pub x: f64,
    pub y: f64,
}

#[cfg(target_os = "macos")]
pub use mac::EditorWindow;

#[cfg(not(target_os = "macos"))]
pub use other::EditorWindow;

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSBackingStoreType, NSFloatingWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask};
    use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
    use std::cell::Cell;
    use std::mem::ManuallyDrop;

    type OnEvent = Box<dyn Fn(WindowEvent)>;

    pub struct DelegateIvars {
        on_event: OnEvent,
        /// Set while the app itself resizes the window (for the plugin), so
        /// that resize isn't taken for the user's.
        ours: Cell<bool>,
        closed: Cell<bool>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "TKPluginEditorWindowDelegate"]
        #[ivars = DelegateIvars]
        pub struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl NSWindowDelegate for Delegate {
            #[unsafe(method(windowWillClose:))]
            fn window_will_close(&self, _note: &NSNotification) {
                if !self.ivars().closed.replace(true) {
                    (self.ivars().on_event)(WindowEvent::Closed);
                }
            }

            #[unsafe(method(windowDidResize:))]
            fn window_did_resize(&self, note: &NSNotification) {
                if self.ivars().ours.get() {
                    return;
                }
                let Some(obj) = note.object() else { return };
                let Ok(window) = obj.downcast::<NSWindow>() else { return };
                let frame = window.contentRectForFrameRect(window.frame());
                let size = EditorSize { width: frame.size.width.round().max(1.0) as u32, height: frame.size.height.round().max(1.0) as u32 };
                (self.ivars().on_event)(WindowEvent::Resized(size));
            }
        }
    );

    impl Delegate {
        fn new(mtm: MainThreadMarker, on_event: OnEvent) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(DelegateIvars { on_event, ours: Cell::new(false), closed: Cell::new(false) });
            // SAFETY: NSObject's designated initializer on a freshly allocated instance.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// An editor's window. Holds the window and its delegate; main thread only.
    pub struct EditorWindow {
        window: ManuallyDrop<Retained<NSWindow>>,
        delegate: ManuallyDrop<Retained<Delegate>>,
    }

    // SAFETY: the window is only touched on the main thread: every method
    // checks `MainThreadMarker`, and the plugins module only keeps it in a
    // document it locks on the main thread. The struct merely sits in that
    // document between calls; dropping it off the main thread leaks it
    // (see `Drop`) rather than release AppKit objects there.
    unsafe impl Send for EditorWindow {}

    /// The window's address, for a plugin's resize request (which comes
    /// from inside plugin code on the main thread, where the module's
    /// document may be locked).
    #[derive(Clone, Copy)]
    pub struct WindowHandle(*const NSWindow, *const Delegate);

    // SAFETY: only dereferenced on the main thread, while the `EditorWindow`
    // that owns both objects is alive (the editor is closed before it goes).
    unsafe impl Send for WindowHandle {}

    impl WindowHandle {
        /// Sets the window's content to the plugin's size.
        pub fn resize(self, size: EditorSize) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: see the `Send` note: both live while the editor is open.
            let (Some(w), Some(d)) = (unsafe { self.0.as_ref() }, unsafe { self.1.as_ref() }) else { return };
            d.ivars().ours.set(true);
            w.setContentSize(NSSize::new(f64::from(size.width), f64::from(size.height)));
            d.ivars().ours.set(false);
        }
    }

    impl EditorWindow {
        /// A hidden window for an editor of `size`, at `place` (or centred).
        /// `on_event` hears the user close or resize it; it runs inside
        /// AppKit and must not block or take a lock.
        pub fn new(title: &str, size: EditorSize, place: Option<Place>, on_event: Box<dyn Fn(WindowEvent)>) -> Result<EditorWindow, String> {
            let mtm = MainThreadMarker::new().ok_or("plugin windows open on the main thread")?;
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(f64::from(size.width), f64::from(size.height)));
            // Not minimizable: a minimized window would sit in the Dock.
            let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
            // SAFETY: a new window on the main thread with a valid rect and style.
            let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false) };
            // SAFETY: this struct keeps its own reference; AppKit must not release the window on close.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setLevel(NSFloatingWindowLevel);
            window.setHidesOnDeactivate(true);
            window.setExcludedFromWindowsMenu(true);
            window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenAuxiliary | NSWindowCollectionBehavior::IgnoresCycle);
            let delegate = Delegate::new(mtm, on_event);
            window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            match place {
                Some(p) => window.setFrameTopLeftPoint(NSPoint::new(p.x, p.y)),
                None => window.center(),
            }
            Ok(EditorWindow { window: ManuallyDrop::new(window), delegate: ManuallyDrop::new(delegate) })
        }

        pub fn handle(&self) -> WindowHandle {
            WindowHandle(Retained::as_ptr(&*self.window), Retained::as_ptr(&*self.delegate))
        }

        /// The view the editor attaches to.
        pub fn parent(&self) -> NativeParent {
            NativeParent(self.window.contentView().map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v).cast_mut().cast()))
        }

        pub fn resize(&self, size: EditorSize) {
            self.handle().resize(size);
        }

        pub fn set_resizable(&self, on: bool) {
            let mut style = self.window.styleMask();
            style.set(NSWindowStyleMask::Resizable, on);
            self.window.setStyleMask(style);
        }

        pub fn set_title(&self, title: &str) {
            if MainThreadMarker::new().is_some() {
                self.window.setTitle(&NSString::from_str(title));
            }
        }

        /// Shows the window in front, keyed.
        pub fn raise(&self) {
            if MainThreadMarker::new().is_some() {
                self.window.makeKeyAndOrderFront(None);
            }
        }

        /// Whether the user closed it (the module may not have heard yet).
        pub fn is_closed(&self) -> bool {
            self.delegate.ivars().closed.get()
        }

        /// Where its top left corner is now.
        pub fn place(&self) -> Place {
            let f = self.window.frame();
            Place { x: f.origin.x, y: f.origin.y + f.size.height }
        }

        /// Closes the window without telling the delegate's callback: the
        /// module is closing it itself. Close the editor first.
        pub fn close(self) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            self.delegate.ivars().closed.set(true);
            self.window.setDelegate(None);
            self.window.orderOut(None);
            self.window.close();
        }
    }

    impl Drop for EditorWindow {
        /// Releases the window and its delegate on the main thread; anywhere
        /// else they are leaked rather than released where AppKit forbids it.
        fn drop(&mut self) {
            if MainThreadMarker::new().is_some() {
                // SAFETY: dropped once, here, and never used after.
                unsafe {
                    ManuallyDrop::drop(&mut self.window);
                    ManuallyDrop::drop(&mut self.delegate);
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod other {
    use super::*;

    /// Editor windows are macOS only so far (Windows and Linux: Phases 9 and 10).
    pub struct EditorWindow;

    #[derive(Clone, Copy)]
    pub struct WindowHandle;

    impl WindowHandle {
        pub fn resize(self, _size: EditorSize) {}
    }

    impl EditorWindow {
        pub fn new(_title: &str, _size: EditorSize, _place: Option<Place>, _on_event: Box<dyn Fn(WindowEvent)>) -> Result<EditorWindow, String> {
            Err("plugin windows open on macOS only so far".into())
        }
        pub fn handle(&self) -> WindowHandle {
            WindowHandle
        }
        pub fn parent(&self) -> NativeParent {
            NativeParent(std::ptr::null_mut())
        }
        pub fn resize(&self, _size: EditorSize) {}
        pub fn set_resizable(&self, _on: bool) {}
        pub fn set_title(&self, _title: &str) {}
        pub fn raise(&self) {}
        pub fn place(&self) -> Place {
            Place { x: 0.0, y: 0.0 }
        }
        pub fn is_closed(&self) -> bool {
            true
        }
        pub fn close(self) {}
    }
}
