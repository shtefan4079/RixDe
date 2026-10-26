use super::*;

impl App {
    pub(super) fn history_operation(&mut self, redo: bool) {
        if self.operation.is_some() || self.picker.is_some() || self.dialog_kind != 0 {
            return;
        }
        if !(if redo {
            self.history.can_redo()
        } else {
            self.history.can_undo()
        }) {
            self.notice = Some(
                i18n::tr(if redo {
                    "Nothing to redo"
                } else {
                    "Nothing to undo"
                })
                .to_string(),
            );
            self.render();
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.operation = Some(cancel.clone());
        let mut history = std::mem::take(&mut self.history);
        if let Some(window) = self.window.upgrade() {
            window.set_operation_progress(0.);
            window.set_operation_message(i18n::tr(if redo {
                "Redoing operation…"
            } else {
                "Undoing operation…"
            }));
        }
        self.render();
        let progress = Arc::new(std::sync::Mutex::new(None::<operations::Progress>));
        let worker_progress = progress.clone();
        let weak = self.window.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(progress) = progress.lock().unwrap().take() {
                    if let Some(window) = weak.upgrade() {
                        window.set_operation_progress(if progress.total == 0 {
                            0.
                        } else {
                            progress.completed as f32 / progress.total as f32
                        });
                        window.set_operation_message(progress.message.into());
                    }
                }
            },
        );
        OPERATION_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            let progress = move |value| {
                *worker_progress.lock().unwrap() = Some(value);
            };
            let report = if redo {
                history.redo(&cancel, progress)
            } else {
                history.undo(&cancel, progress)
            };
            let _ = sender.send(Message::History {
                history,
                report,
                redo,
            });
        });
    }
}
