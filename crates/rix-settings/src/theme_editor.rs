use crate::SettingsWindow;
use rix_config::ThemeSettings;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct State(Arc<Mutex<ThemeSettings>>);
impl State {
    pub fn new(theme: ThemeSettings) -> Self {
        Self(Arc::new(Mutex::new(theme)))
    }
    pub fn baseline(&self) -> ThemeSettings {
        self.0.lock().unwrap().clone()
    }
    pub fn load(&self, window: &SettingsWindow, theme: &ThemeSettings) {
        *self.0.lock().unwrap() = theme.clone();
        crate::sync_theme_to_ui(window, theme);
        crate::style_editor::refresh(window, theme);
    }
}
