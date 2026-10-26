use super::*;
use native_dnd::{Action, Event};

const INVALID_TARGET: i32 = -2147483647;

impl App {
    fn begin_file_drag(&mut self, index: i32) -> bool {
        if self.operation.is_some() || self.dialog_kind != 0 {
            return false;
        }
        let Ok(index) = usize::try_from(index) else {
            return false;
        };
        let tab = &mut self.tabs[self.active];
        if tab.loading || tab.path == Path::new("trash:") {
            return false;
        }
        let Some(entry) = tab.entries.get(index).cloned() else {
            return false;
        };

        if !tab.selected.contains(&entry.path) {
            tab.select(index, false, false);
        }
        let paths = tab.selected_paths();
        let icon = self.file_icon(&entry);
        let Some(dnd) = self.native_dnd.clone() else {
            return false;
        };
        let result = dnd.borrow_mut().start(&paths, &icon);
        match result {
            Ok(()) => {
                if let Some(window) = self.window.upgrade() {
                    window.set_file_drag_active(true);
                }
                self.render();
                true
            }
            Err(error) => {
                self.notice = Some(error);
                self.render();
                false
            }
        }
    }

    fn drop_destination(&self, target: i32) -> Option<PathBuf> {
        let tab = &self.tabs[self.active];
        if target == INVALID_TARGET
            || self.operation.is_some()
            || self.dialog_kind != 0
            || tab.loading
            || tab.path == Path::new("trash:")
        {
            return None;
        }
        let destination = if target >= 0 {
            let entry = tab.entries.get(target as usize)?;
            if !entry.is_dir {
                return None;
            }
            entry.path.clone()
        } else if target == -1 {
            tab.path.clone()
        } else {
            let index = usize::try_from(-i64::from(target) - 2).ok()?;
            self.places.get(index)?.path.clone()
        };
        destination.is_absolute().then_some(destination)
    }

    fn drag_event(&mut self, event: Event) {
        match event {
            Event::Hover(position) => {
                let Some(window) = self.window.upgrade() else {
                    return;
                };
                let target = position.map_or(INVALID_TARGET, |point| {
                    window.invoke_drop_target(point.x, point.y)
                });
                let valid = self.drop_destination(target).is_some();
                window.set_drop_hover_entry(if valid && target >= 0 { target } else { -1 });
                window.set_drop_hover_place(if valid && target <= -2 {
                    i32::try_from(-i64::from(target) - 2).unwrap_or(-1)
                } else {
                    -1
                });
            }
            Event::SourceStarted(position) => {
                let weak = self.window.clone();

                let _ = weak.upgrade_in_event_loop(move |window| {
                    window.invoke_cancel_drag_gesture();
                    window
                        .window()
                        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position,
                            button: slint::platform::PointerEventButton::Left,
                        });
                });
            }
            Event::SourceEnded => {
                let weak = self.window.clone();
                let _ = weak.upgrade_in_event_loop(move |window| {
                    window.set_file_drag_active(false);
                    window.set_drop_hover_entry(-1);
                    window.set_drop_hover_place(-1);
                    window.invoke_cancel_drag_gesture();
                });

                self.dirty.store(true, Ordering::Relaxed);
            }
            Event::Offered { id, position, .. } => {
                let destination = self.window.upgrade().and_then(|window| {
                    self.drop_destination(window.invoke_drop_target(position.x, position.y))
                });
                if let Some(dnd) = &self.native_dnd {
                    dnd.borrow_mut().accept(id, destination);
                }
            }
            Event::Dropped {
                id,
                paths,
                destination,
                action,
            } => {
                if self.operation.is_some() || self.dialog_kind != 0 || paths.is_empty() {
                    if let Some(dnd) = &self.native_dnd {
                        dnd.borrow_mut().complete(id, false);
                    }
                    return;
                }

                self.drop_operation = Some(id);
                self.start_operation(
                    match action {
                        Action::Copy => Kind::Copy,
                        Action::Move => Kind::Move,
                    },
                    paths,
                    Some(destination),
                );
            }
            Event::Error(error) => self.error(error),
        }
    }
}

pub(super) fn install(window: &FilesWindow, app: &Rc<RefCell<App>>) {
    let weak = Rc::downgrade(app);
    let dnd = native_dnd::install(window, move |event| {
        if let Some(app) = weak.upgrade() {
            app.borrow_mut().drag_event(event);
        }
    });
    app.borrow_mut().native_dnd = Some(dnd);
    let app = app.clone();
    window.on_begin_file_drag(move |index| app.borrow_mut().begin_file_drag(index));
}
