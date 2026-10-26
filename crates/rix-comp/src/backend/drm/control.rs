use super::*;
use crate::output_control::{self, Trial};
use rix_ipc::{DesktopAction, IpcRequest, IpcResponse};

pub(super) struct OutputControl {
    trial: Option<Trial>,
    required_presentation: u64,
    next_id: u64,
    retry_at: Instant,
}
impl Default for OutputControl {
    fn default() -> Self {
        Self {
            trial: None,
            required_presentation: 0,
            next_id: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            retry_at: Instant::now(),
        }
    }
}
impl OutputControl {
    pub fn pending(&self) -> bool {
        self.trial.is_some()
    }

    pub fn tick(
        &mut self,
        state: &mut RixState,
        outputs: &mut [ConnectedOutput],
        manager: &DrmOutputManager<
            GbmAllocator<DrmDeviceFd>,
            GbmFramebufferExporter<DrmDeviceFd>,
            (),
            DrmDeviceFd,
        >,
        active: bool,
    ) {
        let now = Instant::now();
        if let Some(trial) = &mut self.trial {

            if let Some(output) = outputs.iter_mut().find(|o| o.output == trial.output) {
                if state.lock.active {
                    trial.revert(now);
                }
                if active
                    && trial.expired(now)
                    && !output.frame_queued
                    && output.pending_submission.is_none()
                    && now >= self.retry_at
                {
                    match outputs::apply_profile(manager, output, trial.previous) {
                        Ok(()) => {
                            info!(output = %output.output.name(), "Unconfirmed display mode reverted");
                            self.trial = None;
                            outputs::arrange_outputs(state, outputs);
                        }
                        Err(error) => {
                            warn!(%error, "Could not restore display mode; retrying");
                            self.retry_at = now + Duration::from_secs(1);
                        }
                    }
                }
            } else {
                self.trial = None;
            }
        }
        for _ in 0..rix_ipc::COMMANDS_PER_TICK {
            let Some(command) = state.output_commands.pop_front() else {
                break;
            };
            if command.responder.is_closed() {
                continue;
            }
            if state.lock.active {
                let _ = command.responder.send(Err("Session is locked".into()));
                continue;
            }
            let DesktopAction::Output(request) = &command.action else {
                unreachable!()
            };
            let result = match request {
                IpcRequest::GetOutputs => Ok(self.snapshot(outputs)),
                _ if !active => Err("Session is inactive".into()),
                IpcRequest::TestOutput {
                    output: name,
                    profile,
                } => {
                    if self.pending() {
                        Err("Confirm or revert the current display trial first".into())
                    } else if let Some(output) =
                        outputs.iter_mut().find(|o| o.output.name() == *name)
                    {
                        if output.frame_queued || output.pending_submission.is_some() {
                            state.output_commands.push_front(command);
                            break;
                        }
                        let Some(previous) = output_control::profile(&output.output) else {
                            let _ = command
                                .responder
                                .send(Err("Output has no active mode".into()));
                            continue;
                        };
                        match outputs::apply_profile(manager, output, *profile) {
                            Ok(()) => {
                                self.required_presentation = output.presentations.wrapping_add(1);
                                self.next_id = self.next_id.wrapping_add(1);
                                self.trial = Some(Trial::new(
                                    self.next_id,
                                    output.output.clone(),
                                    previous,
                                    *profile,
                                    now,
                                ));
                                self.retry_at = now;
                                outputs::arrange_outputs(state, outputs);
                                Ok(self.snapshot(outputs))
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    } else {
                        Err("Output disconnected".into())
                    }
                }
                IpcRequest::ConfirmOutput(id) => {
                    if self
                        .trial
                        .as_ref()
                        .is_some_and(|trial| trial.can_confirm(*id, now))
                        && self.presented(outputs)
                    {
                        let trial = self.trial.take().unwrap();
                        Ok(IpcResponse::OutputConfirmed {
                            output: output_control::identity(&trial.output),
                            profile: trial.tested,
                        })
                    } else {
                        Err("Display trial expired, was replaced, or has not been presented".into())
                    }
                }
                IpcRequest::RevertOutput(id) => {
                    if let Some(trial) = self.trial.as_mut().filter(|trial| trial.id == *id) {
                        trial.revert(now);
                        Ok(IpcResponse::Ok)
                    } else {
                        Err("Display trial expired, was replaced, or has not been presented".into())
                    }
                }
                _ => Err("Invalid output request".into()),
            };
            let _ = command.responder.send(result);
        }
    }
    fn presented(&self, outputs: &[ConnectedOutput]) -> bool {
        self.trial.as_ref().is_some_and(|trial| {
            outputs.iter().any(|output| {
                output.output == trial.output && output.presentations >= self.required_presentation
            })
        })
    }
    fn snapshot(&self, outputs: &[ConnectedOutput]) -> IpcResponse {
        IpcResponse::Outputs(output_control::snapshot(
            outputs.iter().map(|o| &o.output),
            true,
            self.trial.as_ref().map(|t| {
                let mut preview = t.preview(Instant::now());
                preview.ready = self.presented(outputs);
                preview
            }),
        ))
    }
}
