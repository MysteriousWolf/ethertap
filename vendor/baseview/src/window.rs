use std::marker::PhantomData;

use raw_window_handle::{
    HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle, RawWindowHandle,
};
// rwh04 alias: allows iced_baseview (which pins raw-window-handle 0.4) to
// use this crate through the workspace [patch] override while nih-plug's
// standalone wrapper continues to use the 0.5 API.
extern crate rwh04;

use crate::event::{Event, EventStatus};
use crate::window_open_options::WindowOpenOptions;
use crate::{MouseCursor, Size};

#[cfg(target_os = "macos")]
use crate::macos as platform;
#[cfg(target_os = "windows")]
use crate::win as platform;
#[cfg(target_os = "linux")]
use crate::x11 as platform;

pub struct WindowHandle {
    window_handle: platform::WindowHandle,
    // so that WindowHandle is !Send on all platforms
    phantom: PhantomData<*mut ()>,
}

impl WindowHandle {
    fn new(window_handle: platform::WindowHandle) -> Self {
        Self { window_handle, phantom: PhantomData }
    }

    /// Close the window
    pub fn close(&mut self) {
        self.window_handle.close();
    }

    /// Returns `true` if the window is still open, and returns `false`
    /// if the window was closed/dropped.
    pub fn is_open(&self) -> bool {
        self.window_handle.is_open()
    }
}

unsafe impl HasRawWindowHandle for WindowHandle {
    fn raw_window_handle(&self) -> RawWindowHandle {
        self.window_handle.raw_window_handle()
    }
}

// raw-window-handle 0.4 compatibility — needed by iced_baseview.
unsafe impl rwh04::HasRawWindowHandle for WindowHandle {
    fn raw_window_handle(&self) -> rwh04::RawWindowHandle {
        rwh05_to_rwh04(<Self as HasRawWindowHandle>::raw_window_handle(self))
    }
}

pub trait WindowHandler {
    fn on_frame(&mut self, window: &mut Window);
    fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus;
}

pub struct Window<'a> {
    window: platform::Window<'a>,

    // so that Window is !Send on all platforms
    phantom: PhantomData<*mut ()>,
}

impl<'a> Window<'a> {
    #[cfg(target_os = "windows")]
    pub(crate) fn new(window: platform::Window<'a>) -> Window<'a> {
        Window { window, phantom: PhantomData }
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn new(window: platform::Window) -> Window {
        Window { window, phantom: PhantomData }
    }

    pub fn open_parented<P, H, B>(parent: &P, options: WindowOpenOptions, build: B) -> WindowHandle
    where
        P: rwh04::HasRawWindowHandle,
        H: WindowHandler + 'static,
        B: FnOnce(&mut Window) -> H,
        B: Send + 'static,
    {
        let window_handle = platform::Window::open_parented::<P, H, B>(parent, options, build);
        WindowHandle::new(window_handle)
    }

    pub fn open_blocking<H, B>(options: WindowOpenOptions, build: B)
    where
        H: WindowHandler + 'static,
        B: FnOnce(&mut Window) -> H,
        B: Send + 'static,
    {
        platform::Window::open_blocking::<H, B>(options, build)
    }

    /// Create an embedded view without attaching it to a real parent window.
    /// Used by audio-plugin hosts (and nih-plug-iced) that manage the parent
    /// window connection themselves.
    pub fn open_as_if_parented<H, B>(options: WindowOpenOptions, build: B) -> WindowHandle
    where
        H: WindowHandler + 'static,
        B: FnOnce(&mut Window) -> H,
        B: Send + 'static,
    {
        let window_handle =
            platform::Window::open_as_if_parented::<H, B>(options, build);
        WindowHandle::new(window_handle)
    }

    /// Close the window
    pub fn close(&mut self) {
        self.window.close();
    }

    /// Resize the window to the given size. The size is always in logical pixels. DPI scaling will
    /// automatically be accounted for.
    pub fn resize(&mut self, size: Size) {
        self.window.resize(size);
    }

    pub fn set_mouse_cursor(&mut self, cursor: MouseCursor) {
        self.window.set_mouse_cursor(cursor);
    }

    pub fn has_focus(&mut self) -> bool {
        self.window.has_focus()
    }

    pub fn focus(&mut self) {
        self.window.focus()
    }

    /// If provided, then an OpenGL context will be created for this window. You'll be able to
    /// access this context through [crate::Window::gl_context].
    #[cfg(feature = "opengl")]
    pub fn gl_context(&self) -> Option<&crate::gl::GlContext> {
        self.window.gl_context()
    }
}

unsafe impl<'a> HasRawWindowHandle for Window<'a> {
    fn raw_window_handle(&self) -> RawWindowHandle {
        self.window.raw_window_handle()
    }
}

unsafe impl<'a> HasRawDisplayHandle for Window<'a> {
    fn raw_display_handle(&self) -> RawDisplayHandle {
        self.window.raw_display_handle()
    }
}

// raw-window-handle 0.4 compatibility — needed by iced_baseview.
unsafe impl<'a> rwh04::HasRawWindowHandle for Window<'a> {
    fn raw_window_handle(&self) -> rwh04::RawWindowHandle {
        rwh05_to_rwh04(<Self as HasRawWindowHandle>::raw_window_handle(self))
    }
}

/// Convert a raw-window-handle 0.5 handle to its 0.4 equivalent.
///
/// Both versions carry the same underlying pointer fields for every platform
/// baseview supports; the rename is purely a crate-version-boundary artefact.
#[allow(unused_variables)]
fn rwh05_to_rwh04(h: RawWindowHandle) -> rwh04::RawWindowHandle {
    match h {
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(h) => {
            let mut out = rwh04::AppKitHandle::empty();
            out.ns_view = h.ns_view;
            out.ns_window = h.ns_window;
            rwh04::RawWindowHandle::AppKit(out)
        }
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xcb(h) => {
            let mut out = rwh04::XcbHandle::empty();
            out.window = h.window;
            rwh04::RawWindowHandle::Xcb(out)
        }
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xlib(h) => {
            let mut out = rwh04::XlibHandle::empty();
            out.window = h.window;
            out.display = h.display;
            rwh04::RawWindowHandle::Xlib(out)
        }
        #[cfg(target_os = "windows")]
        RawWindowHandle::Win32(h) => {
            let mut out = rwh04::Win32Handle::empty();
            out.hwnd = h.hwnd;
            out.hinstance = h.hinstance;
            rwh04::RawWindowHandle::Win32(out)
        }
        _ => rwh04::RawWindowHandle::AppKit(rwh04::AppKitHandle::empty()),
    }
}
