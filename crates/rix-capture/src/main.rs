use anyhow::{Context, Result};
use clap::Parser;
use image::{Rgba, RgbaImage};
use rix_capture::{
    editor::{self, Edit, Editor, Point, Rect},
    encoding::encode_png,
    handoff, CaptureWindow,
};
use rix_ipc::{IpcClient, IpcEvent, IpcRequest, IpcResponse};
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

#[derive(Parser)]
#[command(about = "Rix Capture — screen selection, PNG clipboard and annotation")]
struct Args {

    #[arg(long, conflicts_with = "screen")]
    area: bool,

    #[arg(long)]
    screen: bool,

    #[arg(long)]
    output: Option<String>,

    #[arg(long)]
    preview: bool,

    #[arg(long, conflicts_with_all = ["area", "screen", "input_fd", "editor"])]
    edit: Option<PathBuf>,

    #[arg(long, conflicts_with_all = ["area", "screen", "input_fd"])]
    editor: bool,
    #[arg(long, hide = true, conflicts_with_all = ["area", "screen", "return_fd"])]
    input_fd: Option<i32>,
    #[arg(long, hide = true)]
    return_fd: Option<i32>,
    #[arg(long, hide = true, default_value = "")]
    monitor_label: String,
    #[arg(long, hide = true, default_value = "")]
    initial_status: String,
}

fn request(runtime: &tokio::runtime::Runtime, request: IpcRequest) -> Result<IpcResponse> {
    runtime
        .block_on(async { IpcClient::connect().await?.send(request).await })
        .map_err(Into::into)
}

fn freeze(runtime: &tokio::runtime::Runtime, output: String) -> Result<RgbaImage> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is missing")?;
    let path = PathBuf::from(directory).join(format!(
        "rix-capture-{}-{}.png",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));

    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove = Remove(path.clone());
    let mut attempts = 0;
    let response = loop {
        let response = request(
            runtime,
            IpcRequest::ScreenshotOutput {
                output: output.clone(),
                path: Some(path.to_string_lossy().into_owned()),
            },
        );
        match response {
            Ok(response) => break response,
            Err(error)
                if attempts < 2
                    && matches!(error.downcast_ref::<rix_ipc::ClientError>(), Some(rix_ipc::ClientError::Server(message)) if message == "Display changed during screenshot") =>
            {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(error) => return Err(error),
        }
    };
    anyhow::ensure!(
        matches!(response, IpcResponse::ScreenshotSaved(_)),
        "Unexpected screenshot response"
    );
    let mut reader = image::ImageReader::open(&path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(editor::MAX_PIXELS * 4 + 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.into_rgba8();
    anyhow::ensure!(
        image.width() as u64 * image.height() as u64 <= editor::MAX_PIXELS,
        "Image is too large"
    );
    Ok(image)
}

fn preview_image() -> RgbaImage {
    let mut image = RgbaImage::new(1280, 800);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let inside = (220..1050).contains(&x) && (140..650).contains(&y);
        *pixel = if inside {
            Rgba([38 + (x / 80) as u8, 47 + (y / 40) as u8, 83, 255])
        } else {
            Rgba([24, 34 + (y / 30) as u8, 72 + (x / 20) as u8, 255])
        };
    }
    image
}
fn slint_image(image: &RgbaImage) -> slint::Image {
    slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            image.as_raw(),
            image.width(),
            image.height(),
        ),
    )
}
fn load_theme() -> rix_config::ThemeSettings {

    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    directory
        .and_then(|path| std::fs::read(path.join("rix/theme.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<rix_config::ThemeSettings>(&bytes).ok())
        .filter(|theme| theme.validate().is_ok())
        .unwrap_or_default()
}

fn watch_theme(weak: slint::Weak<CaptureWindow>) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            loop {
                if let Ok(mut subscription) = IpcClient::subscribe().await {

                    if let Ok(mut client) = IpcClient::connect().await {
                        if let Ok(IpcResponse::Theme(theme)) =
                            client.send(IpcRequest::GetTheme).await
                        {
                            if weak
                                .upgrade_in_event_loop(move |ui| {
                                    rix_capture::apply_theme(&ui, &theme)
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    while let Ok(event) = subscription.next_event().await {
                        if let IpcEvent::ThemeChanged(theme) = event {
                            if weak
                                .upgrade_in_event_loop(move |ui| {
                                    rix_capture::apply_theme(&ui, &theme)
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                if weak.upgrade_in_event_loop(|_| {}).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    });
}
enum Command {
    Replace(RgbaImage),
    Edit(Edit),
    Undo,
    Redo,
    Copy,
    Save(PathBuf),
    Open(PathBuf),
    Text {
        point: Point,
        text: String,
        size: f32,
        color: Rgba<u8>,
        family: String,
    },
}

fn clipboard(runtime: &tokio::runtime::Runtime, png: Vec<u8>, preview: bool) -> Result<String> {
    if preview {
        return Ok("Preview · clipboard unchanged".into());
    }
    anyhow::ensure!(
        png.len() <= rix_ipc::CLIPBOARD_PNG_MAX_BYTES,
        "PNG exceeds clipboard limit (16 MiB). Save screenshot to file"
    );
    let response = request(runtime, IpcRequest::ClipboardSetPng { data: png })?;
    anyhow::ensure!(
        response == IpcResponse::Ok,
        "Clipboard did not acknowledge PNG"
    );
    Ok("PNG copied to clipboard".into())
}

fn start_worker(
    source: Option<RgbaImage>,
    preview: bool,
    weak: slint::Weak<CaptureWindow>,
) -> mpsc::SyncSender<Command> {
    let (sender, receiver) = mpsc::sync_channel(4);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Capture runtime");
        let mut editor = source
            .map(Editor::new)
            .transpose()
            .expect("Validated source");
        for command in receiver {
            let mut saving = None;
            let replacing = matches!(command, Command::Replace(_) | Command::Open(_));
            let opening = matches!(command, Command::Open(_));
            let result = (|| -> Result<()> {
                match command {
                    Command::Replace(image) => editor = Some(Editor::new(image)?),
                    Command::Text {
                        point,
                        text,
                        size,
                        color,
                        family,
                    } => editor
                        .as_mut()
                        .context("Open an image first")?
                        .apply(Edit::Text {
                            origin: point,
                            text,
                            size,
                            color,
                            font: rix_capture::text::font(&family)?,
                        })?,
                    Command::Open(path) => editor = Some(Editor::new(handoff::open_image(&path)?)?),
                    other => {
                        let editor = editor
                            .as_mut()
                            .context("Take a screenshot or open an image first")?;
                        match other {
                            Command::Edit(edit) => editor.apply(edit)?,
                            Command::Undo => editor.undo(),
                            Command::Redo => editor.redo(),
                            Command::Copy => {}
                            Command::Save(path) => saving = Some(path),
                            _ => unreachable!(),
                        }
                    }
                }
                Ok(())
            })();
            let Some(editor) = editor.as_ref() else {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_busy(false);
                    ui.set_status(format!("Error: {}", result.unwrap_err()).into());
                });
                continue;
            };
            let pixels = editor.render();
            let result = result.and_then(|()| {
                if let Some(path) = &saving {
                    anyhow::ensure!(
                        path.extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("png")),
                        "Specify a filename with .png extension"
                    );
                    if let Some(directory) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                        std::fs::create_dir_all(directory)?;
                    }
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let data = encode_png(&pixels)?;
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path)?;
                    file.write_all(&data)?;
                    file.sync_all()?;
                    return Ok(format!("Saved: {}", path.display()));
                }
                if opening {
                    return Ok("Image opened · changes copied automatically".into());
                }
                clipboard(&runtime, encode_png(&pixels)?, preview)
            });
            let can_undo = editor.can_undo();
            let can_redo = editor.can_redo();
            let dimensions = pixels.dimensions();
            let display_pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                pixels.as_raw(),
                dimensions.0,
                dimensions.1,
            );
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_result(slint::Image::from_rgba8(display_pixels));
                ui.set_pixel_width(dimensions.0 as i32);
                ui.set_pixel_height(dimensions.1 as i32);
                ui.set_undo_available(can_undo);
                ui.set_redo_available(can_redo);
                ui.set_busy(false);
                ui.set_has_image(true);
                if replacing {
                    ui.set_native_size(false);
                    ui.set_tool(0);
                    ui.set_zoom_factor(1.);
                }
                ui.set_status(
                    result
                        .unwrap_or_else(|error| format!("Error: {error}"))
                        .into(),
                );
                ui.set_stroke_preview("".into());
                ui.invoke_focus_controls();
            });
        }
    });
    sender
}

fn enqueue(ui: &CaptureWindow, sender: &mpsc::SyncSender<Command>, command: Command) {
    if ui.get_busy() {
        return;
    }
    match sender.try_send(command) {
        Ok(()) => ui.set_busy(true),
        Err(_) => ui.set_status("Busy. Try again.".into()),
    }
}

fn xdg_pictures_directory() -> Option<PathBuf> {
    let mut child = std::process::Command::new("xdg-user-dir")
        .arg("PICTURES")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    path.is_absolute().then_some(path)
}

fn fallback_pictures_directory() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Pictures")
}

fn screenshot_path(pictures: &std::path::Path) -> PathBuf {
    pictures.join("Screenshots").join(format!(
        "Rix-{}.png",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S_%f")
    ))
}

fn initialise(ui: &CaptureWindow, theme: &rix_config::ThemeSettings) {
    rix_capture::apply_theme(ui, theme);
    watch_theme(ui.as_weak());
    set_screenshot_save_path(ui);
}

fn set_screenshot_save_path(ui: &CaptureWindow) {

    let fallback_directory = fallback_pictures_directory();
    let fallback = screenshot_path(&fallback_directory);
    let fallback_text = fallback.to_string_lossy().into_owned();
    ui.set_save_path(fallback_text.clone().into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let Some(pictures) = xdg_pictures_directory() else {
            return;
        };
        if pictures == fallback_directory {
            return;
        }
        let resolved = screenshot_path(&pictures).to_string_lossy().into_owned();
        let _ = weak.upgrade_in_event_loop(move |ui| {

            if ui.get_save_path().as_str() == fallback_text {
                ui.set_save_path(resolved.into());
            }
        });
    });
}

type Transfer = Arc<Mutex<Option<(std::fs::File, String)>>>;

fn finish_selection(
    ui: &CaptureWindow,
    source: Arc<RgbaImage>,
    rect: Rect,
    preview: bool,
    copy: bool,
    transfer: Transfer,
) {
    if ui.get_busy() {
        return;
    }
    ui.set_busy(true);
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let result = (|| -> Result<_> {
            let pixels =
                image::imageops::crop_imm(&*source, rect.x, rect.y, rect.width, rect.height)
                    .to_image();
            let png = encode_png(&pixels)?;
            let mut file = handoff::create()?;
            handoff::write(&mut file, &png)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;

            let status = if copy {
                clipboard(&runtime, png, preview).unwrap_or_else(|error| {
                    format!("Screenshot ready · failed to copy: {error}")
                })
            } else {
                "Screenshot ready".into()
            };
            Ok((file, status))
        })();
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(result) => {
                *transfer.lock().unwrap() = Some(result);
                let _ = slint::quit_event_loop();
            }
            Err(error) => {
                ui.set_busy(false);
                ui.set_status(format!("Error: {error}").into());
            }
        });
    });
}

fn run_selector(
    args: Args,
    runtime: &tokio::runtime::Runtime,
    theme: &rix_config::ThemeSettings,
) -> Result<()> {
    let return_file = args
        .return_fd
        .map(handoff::inherited)
        .transpose()?
        .map(|file| Rc::new(RefCell::new(file)));
    let outputs = if args.preview {
        vec!["Preview".into()]
    } else {
        match request(runtime, IpcRequest::GetOutputs)? {
            IpcResponse::Outputs(outputs) => outputs
                .outputs
                .into_iter()
                .map(|output| output.name)
                .collect::<Vec<_>>(),
            _ => anyhow::bail!("Could not enumerate outputs"),
        }
    };
    let selected = if let Some(name) = &args.output {
        outputs
            .iter()
            .position(|output| output == name)
            .context("Monitor is unavailable")?
    } else {
        0
    };
    let output = outputs.get(selected).context("No active monitors")?.clone();
    let frozen = if args.preview {
        preview_image()
    } else {
        freeze(runtime, output.clone())?
    };
    let dimensions = frozen.dimensions();
    let controller = if args.preview {
        None
    } else {
        let controller = rix_shell_adapter::install(rix_shell_adapter::LayerSurfaceSpec {
            namespace: "rix-capture".into(),
            output: Some(output.clone()),
            layer: rix_shell_adapter::ShellLayer::Overlay,
            anchors: rix_shell_adapter::ShellAnchors {
                top: true,
                bottom: true,
                left: true,
                right: true,
            },
            keyboard_interactive: true,
            exclusive_zone: -1,
            ..Default::default()
        })?;
        controller.set_output_fullscreen(true);
        Some(controller)
    };
    let ui = CaptureWindow::new()?;
    initialise(&ui, theme);
    ui.set_frozen(slint_image(&frozen));
    ui.set_phase(1);
    ui.set_monitor(output.clone().into());
    ui.set_monitor_index(selected as i32);
    ui.set_monitors(slint::ModelRc::new(slint::VecModel::from(
        outputs
            .iter()
            .map(|s| s.clone().into())
            .collect::<Vec<slint::SharedString>>(),
    )));
    let frozen = Arc::new(frozen);
    let transfer: Transfer = Default::default();
    ui.on_cancel(|| {
        let _ = slint::quit_event_loop();
    });
    let weak = ui.as_weak();
    let source = frozen.clone();
    let completed = transfer.clone();
    let preview = args.preview;
    let copy = return_file.is_none();
    ui.on_selection(move |ax, ay, bx, by, width, height| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(rect) = editor::selection(
            Point { x: ax, y: ay },
            Point { x: bx, y: by },
            (width, height),
            dimensions,
        )
        .filter(|r| r.width >= 2 && r.height >= 2)
        {
            finish_selection(&ui, source.clone(), rect, preview, copy, completed.clone());
        }
    });
    let weak = ui.as_weak();
    let completed = transfer.clone();
    ui.on_screen(move || {
        if let Some(ui) = weak.upgrade() {
            finish_selection(
                &ui,
                frozen.clone(),
                Rect {
                    x: 0,
                    y: 0,
                    width: dimensions.0,
                    height: dimensions.1,
                },
                preview,
                copy,
                completed.clone(),
            );
        }
    });
    let restart = Rc::new(RefCell::new(None::<std::process::Command>));
    let restarting = restart.clone();
    let returned = return_file.clone();
    let screen = args.screen;
    ui.on_select_output(move |index| {
        if index < 0 || index as usize == selected {
            return;
        }
        if let Some(output) = outputs.get(index as usize) {
            let Ok(executable) = std::env::current_exe() else {
                return;
            };
            let mut command = std::process::Command::new(executable);
            command.arg("--output").arg(output);
            if preview {
                command.arg("--preview");
            }
            if screen {
                command.arg("--screen");
            }
            if let Some(file) = &returned {
                let owned = file.borrow().try_clone().expect("Owned capture transfer");
                handoff::pass(&mut command, &owned, "--return-fd");
                RESTART_FILE.with(|keep| *keep.borrow_mut() = Some(owned));
            }
            *restarting.borrow_mut() = Some(command);
            let _ = slint::quit_event_loop();
        }
    });
    ui.show()?;
    ui.invoke_focus_controls();
    if args.screen {
        ui.invoke_screen();
    }
    slint::run_event_loop()?;
    ui.hide()?;
    drop(ui);
    drop(controller);
    if let Some(mut command) = restart.borrow_mut().take() {
        use std::os::unix::process::CommandExt;
        return Err(command.exec()).context("Could not switch capture output");
    }
    if let Some((mut pixels, status)) = transfer.lock().unwrap().take() {
        if let Some(file) = return_file {
            let mut destination = file.borrow_mut();
            use std::io::{Read, Write};
            let mut data = Vec::new();
            pixels.read_to_end(&mut data)?;
            handoff::write(&mut destination, &data)?;
            destination.flush()?;
        } else {
            use std::os::unix::process::CommandExt;
            let mut command = handoff::editor_command(&pixels, args.preview, &output, &status)?;
            return Err(command.exec()).context("Could not open screenshot editor");
        }
    }
    Ok(())
}
thread_local! { static RESTART_FILE: RefCell<Option<std::fs::File>> = const { RefCell::new(None) }; }

fn run_editor(args: Args, theme: &rix_config::ThemeSettings) -> Result<()> {
    let source = if let Some(fd) = args.input_fd {
        Some(handoff::read(&mut handoff::inherited(fd)?)?)
    } else if let Some(path) = &args.edit {
        Some(handoff::open_image(path)?)
    } else {
        None
    };
    let software = std::env::var("RIX_SHELL_RENDERER").as_deref() == Ok("software");
    let mut backend = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(if software { "software" } else { "femtovg" }.into());
    if !software {
        backend = backend.require_opengl_es();
    }
    backend.select()?;
    slint::set_xdg_app_id("rix-capture")?;
    let ui = CaptureWindow::new()?;
    initialise(&ui, theme);
    ui.set_phase(2);
    ui.set_native_size(false);
    ui.set_monitor(args.monitor_label.clone().into());
    ui.set_status(args.initial_status.clone().into());
    if let Some(source) = &source {
        ui.set_result(slint_image(source));
        ui.set_pixel_width(source.width() as i32);
        ui.set_pixel_height(source.height() as i32);
        ui.set_has_image(true);
    }
    let sender = start_worker(source, args.preview, ui.as_weak());
    let alive = Arc::new(AtomicBool::new(true));
    let child_slot = Arc::new(Mutex::new(None::<std::process::Child>));
    let closing = alive.clone();
    ui.window().on_close_requested(move || {
        closing.store(false, Ordering::Release);
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    ui.on_cancel(|| {
        let _ = slint::quit_event_loop();
    });
    for action in 0..3 {
        let weak = ui.as_weak();
        let command = sender.clone();
        let callback = move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_has_image() {
                    enqueue(
                        &ui,
                        &command,
                        match action {
                            0 => Command::Undo,
                            1 => Command::Redo,
                            _ => Command::Copy,
                        },
                    );
                }
            }
        };
        match action {
            0 => ui.on_undo(callback),
            1 => ui.on_redo(callback),
            _ => ui.on_copy(callback),
        }
    }
    let weak = ui.as_weak();
    let command = sender.clone();
    ui.on_save(move |path| {
        if let Some(ui) = weak.upgrade() {
            enqueue(&ui, &command, Command::Save(PathBuf::from(path.as_str())));
        }
    });
    let weak = ui.as_weak();
    let command = sender.clone();
    ui.on_open(move |path| {
        if let Some(ui) = weak.upgrade() {
            enqueue(&ui, &command, Command::Open(PathBuf::from(path.as_str())));
        }
    });
    let weak = ui.as_weak();
    let command = sender.clone();
    ui.on_add_text(move |x, y, text, size| {
        if let Some(ui) = weak.upgrade() {
            let color = ui.get_ink();
            use slint::ComponentHandle;
            enqueue(
                &ui,
                &command,
                Command::Text {
                    point: Point { x, y },
                    text: text.to_string(),
                    size,
                    color: Rgba([color.red(), color.green(), color.blue(), 255]),
                    family: ui
                        .global::<rix_capture::Theme>()
                        .get_font_family()
                        .to_string(),
                },
            );
        }
    });
    let stroke = Rc::new(RefCell::new(Vec::<Point>::new()));
    let weak = ui.as_weak();
    let command = sender.clone();
    ui.on_stroke(move |stage, x, y, width, height| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let size = (ui.get_pixel_width() as u32, ui.get_pixel_height() as u32);
        let point = Point {
            x: (f64::from(x.clamp(0., width)) * f64::from(size.0)
                / f64::from(width.max(f32::EPSILON))) as f32,
            y: (f64::from(y.clamp(0., height)) * f64::from(size.1)
                / f64::from(height.max(f32::EPSILON))) as f32,
        };
        let mut points = stroke.borrow_mut();
        if stage == 0 || stage == 3 {
            points.clear();
        }
        if stage == 3 {
            ui.set_stroke_preview("".into());
            return;
        }
        if points.len() < editor::MAX_POINTS {
            points.push(point);
        }
        if stage != 2 {
            if matches!(ui.get_tool(), 1..=3 | 6) {
                let path = editor::preview_path(&points, ui.get_tool());
                ui.set_stroke_preview(path.into());
            }
            return;
        }
        let Some(first) = points.first().copied() else {
            return;
        };
        let color = ui.get_ink();
        let color = Rgba([color.red(), color.green(), color.blue(), 255]);
        let width = ui.get_ink_width();
        let edit =
            match ui.get_tool() {
                1 => Some(Edit::Stroke {
                    points: points.clone(),
                    color,
                    width,
                }),
                2 => Some(Edit::Highlight {
                    points: points.clone(),
                    color,
                    width: width * 4.,
                }),
                3 => Some(Edit::StyledArrow {
                    a: first,
                    b: point,
                    color,
                    width,
                }),
                4..=7 => editor::selection(first, point, (size.0 as f32, size.1 as f32), size).map(
                    |rect| match ui.get_tool() {
                        4 => Edit::Redact(rect),
                        5 => Edit::Blur(rect),
                        6 => Edit::Rectangle { rect, color, width },
                        _ => Edit::Crop(rect),
                    },
                ),
                _ => None,
            };
        points.clear();
        ui.set_stroke_preview("".into());
        if let Some(edit) = edit {
            enqueue(&ui, &command, Command::Edit(edit));
        }
    });
    let weak = ui.as_weak();
    let command = sender.clone();
    let running = alive.clone();
    let preview = args.preview;
    let selector_child = child_slot.clone();
    ui.on_new_capture(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_busy() {
            return;
        }
        ui.set_busy(true);
        let weak = ui.as_weak();
        let command = command.clone();
        let running = running.clone();
        let child_slot = selector_child.clone();
        std::thread::spawn(move || {

            let prepared = (|| -> Result<Option<u64>> {
                if preview {
                    return Ok(None);
                }
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let windows = mapped_windows(&runtime)?;
                let editors: Vec<_> = windows
                    .into_iter()
                    .filter(|window| window.app_id == "rix-capture")
                    .collect();
                let editor = editors.iter().find(|window| window.is_active).or_else(|| {
                    if editors.len() == 1 {
                        editors.first()
                    } else {
                        None
                    }
                });
                anyhow::ensure!(
                    editor.is_some(),
                    "Activate Rix Capture window before taking a new screenshot"
                );
                Ok(editor.map(|window| window.id))
            })();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let id = match prepared {
                    Ok(id) => id,
                    Err(error) => {
                        ui.set_busy(false);
                        ui.set_status(format!("Error: {error}").into());
                        return;
                    }
                };
                if let Err(error) = ui.hide() {
                    ui.set_busy(false);
                    ui.set_status(format!("Error: {error}").into());
                    return;
                }
                let weak = ui.as_weak();
                std::thread::spawn(move || {
                    let result = (|| -> Result<Option<RgbaImage>> {
                        if let Some(id) = id {
                            let runtime = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()?;
                            let deadline = std::time::Instant::now() + Duration::from_secs(2);
                            loop {
                                if !running.load(Ordering::Acquire) {
                                    return Ok(None);
                                }
                                if !mapped_windows(&runtime)?
                                    .iter()
                                    .any(|window| window.id == id)
                                {
                                    break;
                                }
                                anyhow::ensure!(
                                    std::time::Instant::now() < deadline,
                                    "Editor did not hide in time: retry capture"
                                );
                                std::thread::sleep(Duration::from_millis(25));
                            }
                        }
                        let mut pixels = handoff::create()?;
                        let mut process = std::process::Command::new(std::env::current_exe()?);
                        process.arg("--area");
                        if preview {
                            process.arg("--preview");
                        }
                        handoff::pass(&mut process, &pixels, "--return-fd");
                        process.stdin(std::process::Stdio::null());
                        {
                            let mut slot = child_slot.lock().unwrap();
                            if !running.load(Ordering::Acquire) {
                                return Ok(None);
                            }
                            *slot = Some(process.spawn()?);
                        }
                        loop {
                            if !running.load(Ordering::Acquire) {
                                return Ok(None);
                            }
                            let status = child_slot
                                .lock()
                                .unwrap()
                                .as_mut()
                                .context("Selection closed")?
                                .try_wait()?;
                            if let Some(status) = status {
                                child_slot.lock().unwrap().take();
                                anyhow::ensure!(
                                    status.success(),
                                    "Failed to capture new screenshot"
                                );
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(40));
                        }
                        if pixels.metadata()?.len() == 0 {
                            return Ok(None);
                        }
                        Ok(Some(handoff::read(&mut pixels)?))
                    })();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.set_busy(false);
                        match result {
                            Ok(Some(pixels)) => {
                                set_screenshot_save_path(&ui);
                                enqueue(&ui, &command, Command::Replace(pixels));
                            }
                            Ok(None) => {}
                            Err(error) => ui.set_status(format!("Error: {error}").into()),
                        }
                        let _ = ui.show();
                        ui.invoke_focus_controls();
                    });
                });
            });
        });
    });
    let _control = rix_capture::control::install(ui.as_weak())
        .map_err(|error| {
            tracing::warn!(%error, "Capture shortcut forwarding unavailable");
            error
        })
        .ok();
    ui.show()?;
    ui.invoke_focus_controls();
    slint::run_event_loop_until_quit()?;
    alive.store(false, Ordering::Release);
    if let Some(mut child) = child_slot.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    ui.hide()?;
    Ok(())
}

fn mapped_windows(runtime: &tokio::runtime::Runtime) -> Result<Vec<rix_ipc::WindowInfo>> {
    let response = runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(400), async {
            IpcClient::connect()
                .await?
                .send(IpcRequest::ListWindows)
                .await
        })
        .await
    })??;
    match response {
        IpcResponse::Windows(windows) => Ok(windows),
        _ => anyhow::bail!("Could not inspect capture window state"),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let theme = load_theme();
    if args.editor || args.input_fd.is_some() || args.edit.is_some() {
        run_editor(args, &theme)
    } else {
        run_selector(args, &runtime, &theme)
    }
}
