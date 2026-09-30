//! Create an input-transparent Wayland subsurface of the GTK top-level's
//! wl_surface. wgpu presents into this subsurface; the GTK webview draws on
//! the parent. Empty input region means clicks pass through to the webview.

use glib::translate::ToGlibPtr;
use gtk::prelude::*;
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use std::{ffi::c_void, ptr::NonNull};
use wayland_backend::sys::client::Backend;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::ObjectId,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_compositor::{self, WlCompositor},
        wl_region::{self, WlRegion},
        wl_registry::{self, WlRegistry},
        wl_subcompositor::{self, WlSubcompositor},
        wl_subsurface::{self, WlSubsurface},
        wl_surface::{self, WlSurface},
    },
};

struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSubcompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSubcompositor,
        _: wl_subcompositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSubsurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSubsurface,
        _: wl_subsurface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct WaylandSubsurface {
    display_ptr: usize,
    surface_ptr: usize,
    _subsurface: WlSubsurface,
    _surface: WlSurface,
    _subcompositor: WlSubcompositor,
    _compositor: WlCompositor,
    _event_queue: EventQueue<State>,
    _connection: Connection,
}

impl WaylandSubsurface {
    pub fn new(gtk_window: &gtk::ApplicationWindow) -> Option<Self> {
        unsafe {
            // ---- 1. wl_display from GDK ----
            let gdk_window = gtk_window.window()?;
            let gdk_display = gdk_window.display();

            let gdk_display_ptr: *mut gdk::ffi::GdkDisplay = gdk_display.to_glib_none().0;
            let wl_display_ptr = gdk_wayland_sys::gdk_wayland_display_get_wl_display(
                gdk_display_ptr as *mut gdk_wayland_sys::GdkWaylandDisplay,
            );
            if wl_display_ptr.is_null() {
                eprintln!("gdk_wayland_display_get_wl_display returned null");
                return None;
            }

            // ---- 2. Wrap GDK's connection ----
            let backend = Backend::from_foreign_display(wl_display_ptr as *mut _);
            let connection = Connection::from_backend(backend);

            // ---- 3. Bind globals ----
            let (globals, event_queue) = registry_queue_init::<State>(&connection).ok()?;
            let qh = event_queue.handle();

            let compositor: WlCompositor = globals.bind(&qh, 1..=6, ()).ok()?;
            let subcompositor: WlSubcompositor = globals.bind(&qh, 1..=1, ()).ok()?;

            // ---- 4. Parent surface from GDK ----
            let gdk_window_ptr: *mut gdk::ffi::GdkWindow = gdk_window.to_glib_none().0;
            let parent_ptr = gdk_wayland_sys::gdk_wayland_window_get_wl_surface(
                gdk_window_ptr as *mut gdk_wayland_sys::GdkWaylandWindow,
            );
            if parent_ptr.is_null() {
                eprintln!("gdk_wayland_window_get_wl_surface returned null");
                return None;
            }

            // ---- 5. Reconstruct parent as a WlSurface ----
            let parent_id =
                ObjectId::from_ptr(<WlSurface as Proxy>::interface(), parent_ptr as *mut _).ok()?;
            let parent_surface = WlSurface::from_id(&connection, parent_id).ok()?;

            // ---- 6. Child surface + subsurface ----
            let child_surface = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&child_surface, &parent_surface, &qh, ());

            // ---- 7. Place BELOW the parent so the webview draws on top ----
            //
            // This is the critical line. Without it the default z-order puts
            // the wgpu content above the webview, hiding the Svelte UI.
            subsurface.place_below(&parent_surface);

            // ---- 8. Empty input region (clicks pass through) ----
            let region = compositor.create_region(&qh, ());
            child_surface.set_input_region(Some(&region));
            region.destroy();

            // ---- 9. Desync ----
            subsurface.set_desync();

            // ---- 10. Position at parent-local (0, 0) ----
            subsurface.set_position(0, 0);

            // ---- 11. Commit + flush ----
            child_surface.commit();
            connection.flush().ok()?;

            let surface_raw = child_surface.id().as_ptr() as usize;

            Some(Self {
                display_ptr: wl_display_ptr as usize,
                surface_ptr: surface_raw,
                _subsurface: subsurface,
                _surface: child_surface,
                _subcompositor: subcompositor,
                _compositor: compositor,
                _event_queue: event_queue,
                _connection: connection,
            })
        }
    }

    pub fn display_ptr(&self) -> usize {
        self.display_ptr
    }

    pub fn surface_ptr(&self) -> usize {
        self.surface_ptr
    }

    #[allow(dead_code)]
    pub fn raw_display_handle(&self) -> RawDisplayHandle {
        RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.display_ptr as *mut c_void)
                .expect("display_ptr is non-null by construction"),
        ))
    }

    #[allow(dead_code)]
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(self.surface_ptr as *mut c_void)
                .expect("surface_ptr is non-null by construction"),
        ))
    }

    #[allow(dead_code)]
    pub fn set_position(&self, x: i32, y: i32) {
        self._subsurface.set_position(x, y);
        self._surface.commit();
        self._connection.flush().ok();
    }
}
