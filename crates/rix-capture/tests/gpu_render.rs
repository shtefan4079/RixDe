use i_slint_renderer_femtovg::{
    opengl::OpenGLInterface, FemtoVGOpenGLRenderer, FemtoVGOpenGLRendererExt, FemtoVGRendererExt,
};
use rix_capture::{apply_theme, CaptureWindow, Theme};
use slint::{
    platform::{Platform, PlatformError, WindowAdapter, WindowEvent},
    ComponentHandle, LogicalSize, PhysicalSize, Window,
};
use std::{
    cell::Cell,
    ffi::{c_char, c_void, CStr},
    num::NonZeroU32,
    ptr,
    rc::Rc,
};

type Handle = *mut c_void;
type GlResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
const NONE: i32 = 0x3038;

#[link(name = "EGL")]
unsafe extern "C" {
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
    fn eglCreatePbufferSurface(display: Handle, config: Handle, attributes: *const i32) -> Handle;
    fn eglMakeCurrent(display: Handle, draw: Handle, read: Handle, context: Handle) -> u32;
    fn eglDestroySurface(display: Handle, surface: Handle) -> u32;
    fn eglDestroyContext(display: Handle, context: Handle) -> u32;
    fn eglTerminate(display: Handle) -> u32;
    fn eglGetError() -> u32;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
}

fn check(success: bool, operation: &str) -> GlResult {
    if success {
        Ok(())
    } else {

        Err(format!("{operation}: EGL error {:#x}", unsafe { eglGetError() }).into())
    }
}

struct Egl {
    display: Handle,
    config: Handle,
    context: Handle,
    surface: Cell<Handle>,
    size: Cell<PhysicalSize>,
}
impl Egl {
    fn new() -> Result<Rc<Self>, Box<dyn std::error::Error + Send + Sync>> {

        let display = unsafe {
            let address = eglGetProcAddress(c"eglGetPlatformDisplayEXT".as_ptr());
            check(!address.is_null(), "load surfaceless EGL")?;
            let get_display: unsafe extern "C" fn(u32, Handle, *const i32) -> Handle =
                std::mem::transmute(address);
            get_display(0x31dd, ptr::null_mut(), ptr::null())
        };
        check(!display.is_null(), "surfaceless display")?;

        unsafe {
            check(
                eglInitialize(display, ptr::null_mut(), ptr::null_mut()) != 0,
                "initialize EGL",
            )?;
            check(eglBindAPI(0x30a0) != 0, "bind GLES")?;
            let attributes = [
                0x3033, 1, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3026, 8, NONE,
            ];
            let mut config = ptr::null_mut();
            let mut count = 0;
            check(
                eglChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) != 0
                    && count == 1,
                "RGBA pbuffer config",
            )?;
            let context =
                eglCreateContext(display, config, ptr::null_mut(), [0x3098, 2, NONE].as_ptr());
            check(!context.is_null(), "create GLES context")?;
            let surface =
                eglCreatePbufferSurface(display, config, [0x3057, 1, 0x3056, 1, NONE].as_ptr());
            check(!surface.is_null(), "create initial 1x1 pbuffer")?;
            check(
                eglMakeCurrent(display, surface, surface, context) != 0,
                "make current",
            )?;
            Ok(Rc::new(Self {
                display,
                config,
                context,
                surface: Cell::new(surface),
                size: Cell::new(PhysicalSize::new(1, 1)),
            }))
        }
    }

    fn pixels(&self) -> image::RgbaImage {
        self.current().unwrap();
        let size = self.size.get();
        let mut pixels = vec![0u8; size.width as usize * size.height as usize * 4];

        unsafe {
            let address = eglGetProcAddress(c"glReadPixels".as_ptr());
            assert!(!address.is_null());
            let read: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void) =
                std::mem::transmute(address);
            read(
                0,
                0,
                size.width as i32,
                size.height as i32,
                0x1908,
                0x1401,
                pixels.as_mut_ptr().cast(),
            );
        }
        let image = image::RgbaImage::from_raw(size.width, size.height, pixels).unwrap();
        image::imageops::flip_vertical(&image)
    }
    fn renderer_name(&self) -> String {
        self.current().unwrap();

        unsafe {
            let address = eglGetProcAddress(c"glGetString".as_ptr());
            assert!(!address.is_null());
            let get: unsafe extern "C" fn(u32) -> *const c_char = std::mem::transmute(address);
            let name = get(0x1f01);
            assert!(!name.is_null());
            CStr::from_ptr(name).to_string_lossy().into_owned()
        }
    }
    fn current(&self) -> GlResult {

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
}
struct GlContext(Rc<Egl>);
impl std::ops::Deref for GlContext {
    type Target = Egl;
    fn deref(&self) -> &Egl {
        &self.0
    }
}

unsafe impl OpenGLInterface for GlContext {
    fn ensure_current(&self) -> GlResult {
        self.current()
    }
    fn swap_buffers(&self) -> GlResult {
        Ok(())
    }
    fn resize(&self, width: NonZeroU32, height: NonZeroU32) -> GlResult {
        let size = PhysicalSize::new(width.get(), height.get());
        if size == self.size.get() {
            return Ok(());
        }

        unsafe {
            let surface = eglCreatePbufferSurface(
                self.display,
                self.config,
                [0x3057, size.width as i32, 0x3056, size.height as i32, NONE].as_ptr(),
            );
            check(!surface.is_null(), "resize pbuffer")?;
            let old = self.surface.replace(surface);
            self.current()?;
            eglDestroySurface(self.display, old);
            self.size.set(size);
        }
        Ok(())
    }
    fn get_proc_address(&self, name: &CStr) -> *const c_void {

        unsafe { eglGetProcAddress(name.as_ptr()) }
    }
}
impl Drop for Egl {
    fn drop(&mut self) {

        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            eglDestroySurface(self.display, self.surface.get());
            eglDestroyContext(self.display, self.context);
            eglTerminate(self.display);
        }
    }
}

struct GpuWindow {
    window: Window,
    renderer: FemtoVGOpenGLRenderer,
    size: Cell<PhysicalSize>,
}
impl WindowAdapter for GpuWindow {
    fn window(&self) -> &Window {
        &self.window
    }
    fn renderer(&self) -> &dyn slint::platform::Renderer {
        &self.renderer
    }
    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, _: slint::WindowSize) {}
}
struct GpuPlatform(Rc<GpuWindow>);
impl Platform for GpuPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
#[ignore = "requires surfaceless EGL; renders only synthetic pixels"]
fn capture_gpu_draws_the_source_and_controls_after_output_configure() {
    let context = Egl::new().unwrap();
    eprintln!(
        "Synthetic Capture GPU renderer: {}",
        context.renderer_name()
    );
    let renderer = FemtoVGOpenGLRenderer::new_suspended();
    renderer
        .set_opengl_context(GlContext(context.clone()))
        .unwrap();
    let adapter = Rc::new_cyclic(|weak: &std::rc::Weak<GpuWindow>| GpuWindow {
        window: Window::new(weak.clone()),
        renderer,
        size: Cell::new(PhysicalSize::new(1, 1)),
    });
    slint::platform::set_platform(Box::new(GpuPlatform(adapter.clone()))).unwrap();
    let ui = CaptureWindow::new().unwrap();
    let sample = image::RgbaImage::from_fn(1280, 800, |x, y| {
        image::Rgba([
            25 + (x / 50) as u8,
            40 + (y / 25) as u8,
            95 + (x / 20) as u8,
            255,
        ])
    });
    let pixels =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(sample.as_raw(), 1280, 800);
    ui.set_frozen(slint::Image::from_rgba8(pixels.clone()));
    ui.set_result(slint::Image::from_rgba8(pixels));
    ui.set_pixel_width(1280);
    ui.set_pixel_height(800);
    ui.set_monitors(slint::ModelRc::new(slint::VecModel::from(vec![
        "Synthetic output".into(),
    ])));
    ui.show().unwrap();

    for (case, scale, width, height) in [
        ("initial", 1., 1280., 800.),
        ("fractional", 1.5, 1280., 800.),
        ("fullhd", 1., 1920., 1080.),
    ] {
        let physical = PhysicalSize::new((width * scale) as u32, (height * scale) as u32);
        adapter.size.set(physical);
        ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        ui.window().dispatch_event(WindowEvent::Resized {
            size: LogicalSize::new(width, height),
        });
        for (palette_name, settings) in [
            ("dark", rix_config::ThemeSettings::default()),
            ("light", rix_config::ThemeSettings::crisp_light()),
        ] {
            apply_theme(&ui, &settings);
            let text_color = ui.global::<Theme>().get_text_primary();
            for phase in [0, 1] {
                ui.set_phase(phase);
                adapter.renderer.render().unwrap();
                let actual = context.pixels();
                assert_eq!(actual.dimensions(), (physical.width, physical.height));
                for (fraction_x, fraction_y) in
                    [(0.08, 0.25), (0.08, 0.875), (0.86, 0.25), (0.86, 0.875)]
                {
                    let x = (physical.width as f32 * fraction_x) as u32;
                    let y = (physical.height as f32 * fraction_y) as u32;
                    let source_x = (x as f32 / physical.width as f32 * 1280.) as u32;
                    let source_y = (y as f32 / physical.height as f32 * 800.) as u32;
                    let expected = sample.get_pixel(source_x, source_y).0;
                    for channel in 0..3 {
                        let expected = (u32::from(expected[channel]) * 175 / 255) as u8;
                        assert!(actual.get_pixel(x, y)[channel].abs_diff(expected) <= 2, "GPU frozen image is blank or misplaced at ({x}, {y}) in {case} phase {phase}: {:?}", actual.get_pixel(x, y));
                    }
                }
                assert!(
                    actual
                        .enumerate_pixels()
                        .filter(|(x, y, _)| (*x as f32 - physical.width as f32 / 2.).abs()
                            < 150. * scale
                            && *y as f32 > 25. * scale
                            && (*y as f32) < 60. * scale)
                        .any(|(_, _, pixel)| pixel[0].abs_diff(text_color.red()) < 10
                            && pixel[1].abs_diff(text_color.green()) < 10
                            && pixel[2].abs_diff(text_color.blue()) < 10),
                    "GPU must draw visible toolbar text in {case} {palette_name}"
                );
                if let Some(directory) = std::env::var_os("RIX_CAPTURE_RENDER_DIR") {
                    std::fs::create_dir_all(&directory).unwrap();
                    actual
                        .save(std::path::Path::new(&directory).join(format!(
                            "capture-gpu-{case}-{palette_name}-phase-{phase}.png"
                        )))
                        .unwrap();
                }
            }
            ui.set_phase(2);
            ui.set_has_image(true);
            adapter.renderer.render().unwrap();
            let editor = context.pixels();
            let center = editor.get_pixel(physical.width / 2, physical.height / 2);
            assert!(
                center[0] >= 30 && center[1] >= 48 && center[2] >= 100,
                "GPU editor must display the result in {case}: {center:?}"
            );
            let stripes = image::RgbaImage::from_fn(1280, 800, |x, _| {
                let level = if x % 2 == 0 { 0 } else { 255 };
                image::Rgba([level, level, level, 255])
            });
            ui.set_result(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
                slint::Rgba8Pixel,
            >::clone_from_slice(
                stripes.as_raw(), 1280, 800
            )));
            ui.set_native_size(true);
            adapter.renderer.render().unwrap();
            slint::platform::update_timers_and_animations();
            assert_eq!(
                ui.get_preview_zoom(),
                1.,
                "100% is a native physical pixel at output scale {scale}"
            );
            let native = context.pixels();
            let row: Vec<_> = (physical.width / 2..physical.width / 2 + 12)
                .map(|x| native.get_pixel(x, physical.height / 2)[0])
                .collect();
            assert!(
                row.iter().all(|pixel| *pixel == 0 || *pixel == 255),
                "100% must preserve one-pixel text edges without interpolation in {case}: {row:?}"
            );
            assert!(
                row.windows(2).all(|pair| pair[0] != pair[1]),
                "One source pixel must occupy one display pixel in {case}: {row:?}"
            );
            ui.set_native_size(false);
            ui.set_result(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
                slint::Rgba8Pixel,
            >::clone_from_slice(
                sample.as_raw(), 1280, 800
            )));
        }
    }
}
