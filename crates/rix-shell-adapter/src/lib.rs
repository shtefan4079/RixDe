use rix_core::Position;

mod egl;
mod keyboard;
mod session_lock;
mod wayland;
pub use session_lock::{install_session_lock, SessionLockController};
pub use wayland::{install, SurfaceController};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub radius: f32,
    pub opacity: f32,
    pub clip: Option<SurfaceClip>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceClip {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellLayer {
    Background,
    Bottom,
    Top,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShellAnchors {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl ShellAnchors {
    pub fn from_position(position: Position) -> Self {
        match position {
            Position::Top => Self {
                top: true,
                left: true,
                right: true,
                bottom: false,
            },
            Position::Bottom => Self {
                bottom: true,
                left: true,
                right: true,
                top: false,
            },
            Position::Left => Self {
                left: true,
                top: true,
                bottom: true,
                right: false,
            },
            Position::Right => Self {
                right: true,
                top: true,
                bottom: true,
                left: false,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShellMargins {
    pub top: i32,
    pub bottom: i32,
    pub left: i32,
    pub right: i32,
}

impl ShellMargins {
    pub fn uniform(margin: i32) -> Self {
        Self {
            top: margin,
            bottom: margin,
            left: margin,
            right: margin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerSurfaceSpec {
    pub namespace: String,

    pub output: Option<String>,
    pub layer: ShellLayer,
    pub anchors: ShellAnchors,
    pub exclusive_zone: i32,
    pub margins: ShellMargins,
    pub keyboard_interactive: bool,
}

impl Default for LayerSurfaceSpec {
    fn default() -> Self {
        Self {
            namespace: "rix-panel".to_string(),
            output: None,
            layer: ShellLayer::Top,
            anchors: ShellAnchors::from_position(Position::Bottom),
            exclusive_zone: 44,
            margins: ShellMargins::default(),
            keyboard_interactive: false,
        }
    }
}
