use i_slint_renderer_femtovg::opengl::OpenGLInterface;
use std::{
    cell::Cell,
    ffi::{c_char, c_void, CStr},
    num::NonZeroU32,
    ptr,
};
use wayland_client::{protocol::wl_surface::WlSurface, Connection, Proxy};
use wayland_egl::WlEglSurface;

type Handle = *mut c_void;
type GlResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
const NONE: i32 = 0x3038;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(native: Handle) -> Handle;
    fn eglInitialize(display: Handle, major: *mut i32, minor: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(
        display: Handle,
        attributes: *const i32,
        config: *mut Handle,
        size: i32,
        count: *mut i32,
    ) -> u32;
    fn eglCreateContext(
        display: Handle,
        config: Handle,
        share: Handle,
        attributes: *const i32,
    ) -> Handle;
    fn eglCreateWindowSurface(
        display: Handle,
        config: Handle,
        window: Handle,
        attributes: *const i32,
    ) -> Handle;
    fn eglMakeCurrent(display: Handle, draw: Handle, read: Handle, context: Handle) -> u32;
    fn eglSwapInterval(display: Handle, interval: i32) -> u32;
    fn eglSwapBuffers(display: Handle, surface: Handle) -> u32;
    fn eglDestroySurface(display: Handle, surface: Handle) -> u32;
    fn eglDestroyContext(display: Handle, context: Handle) -> u32;
    fn eglTerminate(display: Handle) -> u32;
    fn eglGetError() -> u32;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
}

pub struct EglWindow {
    display: Handle,
    context: Handle,
    config: Handle,
    surface: Cell<Handle>,
    size: Cell<(u32, u32)>,
    native: WlEglSurface,

    _surface: WlSurface,
    _connection: Connection,
}
fn check(success: bool, operation: &str) -> GlResult {
    if success {
        Ok(())
    } else {

        Err(format!("{operation}: EGL error {:#x}", unsafe { eglGetError() }).into())
    }
}
impl EglWindow {
    pub fn new(connection: &Connection, surface: &WlSurface) -> Result<Self, slint::PlatformError> {
        Self::create(connection, surface).map_err(|e| slint::PlatformError::Other(e.to_string()))
    }
    fn create(
        connection: &Connection,
        surface: &WlSurface,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let native = WlEglSurface::new(surface.id(), 1, 1)?;

        let display = unsafe { eglGetDisplay(connection.backend().display_ptr().cast()) };
        check(!display.is_null(), "get display")?;

        check(
            unsafe { eglInitialize(display, ptr::null_mut(), ptr::null_mut()) } != 0,
            "initialize",
        )?;
        let mut owned = Self {
            display,
            context: ptr::null_mut(),
            config: ptr::null_mut(),
            surface: Cell::new(ptr::null_mut()),
            size: Cell::new((1, 1)),
            native,
            _surface: surface.clone(),
            _connection: connection.clone(),
        };

        unsafe {
            check(eglBindAPI(0x30a0) != 0, "bind GLES")?;
            let attributes = [
                0x3033, 4, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3026, 8, NONE,
            ];
            let mut config = ptr::null_mut();
            let mut count = 0;
            check(
                eglChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) != 0
                    && count == 1,
                "choose RGBA config",
            )?;
            owned.config = config;
            owned.context =
                eglCreateContext(display, config, ptr::null_mut(), [0x3098, 2, NONE].as_ptr());
            check(!owned.context.is_null(), "create GLES context")?;
            owned.surface.set(eglCreateWindowSurface(
                display,
                config,
                owned.native.ptr().cast_mut().cast(),
                [NONE].as_ptr(),
            ));
            check(!owned.surface.get().is_null(), "create window surface")?;
            owned.ensure_current()?;
            check(eglSwapInterval(display, 0) != 0, "disable blocking swap")?;
            let address = eglGetProcAddress(c"glGetString".as_ptr());
            if !address.is_null() {
                let get_string: unsafe extern "C" fn(u32) -> *const u8 =
                    std::mem::transmute(address);
                let name = get_string(0x1f01);
                if !name.is_null() {
                    tracing::info!(renderer = %CStr::from_ptr(name.cast()).to_string_lossy(), "Layer shell uses EGL/GLES + FemtoVG");
                }
            }
        }
        Ok(owned)
    }
}

unsafe impl OpenGLInterface for EglWindow {
    fn ensure_current(&self) -> GlResult {

        check(
            unsafe {
                eglMakeCurrent(
                    self.display,
                    self.surface.get(),
                    self.surface.get(),
                    self.context,
                )
            } != 0,
            "make current",
        )
    }
    fn swap_buffers(&self) -> GlResult {

        check(
            unsafe { eglSwapBuffers(self.display, self.surface.get()) } != 0,
            "swap buffers",
        )
    }
    fn resize(&self, width: NonZeroU32, height: NonZeroU32) -> GlResult {
        let size = (width.get(), height.get());
        if self.size.get() == size {
            return Ok(());
        }
        self.native
            .resize(width.get() as i32, height.get() as i32, 0, 0);

        check(
            unsafe {
                eglMakeCurrent(
                    self.display,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } != 0,
            "release current drawable for resize",
        )?;
        let previous = self.surface.replace(ptr::null_mut());
        check(
            unsafe { eglDestroySurface(self.display, previous) } != 0,
            "destroy old window surface",
        )?;
        self.surface.set(unsafe {
            eglCreateWindowSurface(
                self.display,
                self.config,
                self.native.ptr().cast_mut().cast(),
                [NONE].as_ptr(),
            )
        });
        check(
            !self.surface.get().is_null(),
            "create resized window surface",
        )?;
        self.ensure_current()?;
        check(
            unsafe { eglSwapInterval(self.display, 0) } != 0,
            "disable blocking swap",
        )?;
        self.size.set(size);
        Ok(())
    }
    fn get_proc_address(&self, name: &CStr) -> *const c_void {

        unsafe { eglGetProcAddress(name.as_ptr()) }
    }
}
impl Drop for EglWindow {
    fn drop(&mut self) {

        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if !self.surface.get().is_null() {
                eglDestroySurface(self.display, self.surface.get());
            }
            if !self.context.is_null() {
                eglDestroyContext(self.display, self.context);
            }
            eglTerminate(self.display);
        }
    }
}
