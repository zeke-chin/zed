//! The `EditPredictionDelegate` that shows Windsurf's next-edit predictions.

use std::{ops::Range, sync::Arc, time::Duration};

use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, PredictedCursorPosition,
};
use gpui::{App, AppContext as _, Context, Entity, SharedString, Task, WeakEntity};
use language::{Anchor, Buffer, BufferSnapshot, EditPreview, ToOffset as _};
use project::Project;
use ui::IconName;

use crate::{
    Preparation, PreparedRequest, RequestOutcome, Windsurf, WindsurfSettings,
    diff::{PredictionError, prediction_edits},
    native::get_tab::TabStream,
    tab_jump::{self, JumpOrigin},
};
use settings::Settings as _;

/// Typing in quick succession only sends a request once it pauses this long.
pub const DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct CurrentPrediction {
    pub id: SharedString,
    pub buffer: Entity<Buffer>,
    pub snapshot: BufferSnapshot,
    pub edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    pub edit_preview: EditPreview,
    /// Set when the edits are in another file than the one they were
    /// predicted for (Tab Jump).
    pub jump: Option<JumpOrigin>,
}

impl CurrentPrediction {
    fn interpolate(&self, current: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        edit_prediction_types::interpolate_edits(&self.snapshot, current, &self.edits)
            .filter(|edits| !edits.is_empty())
    }
}

pub struct WindsurfDelegate {
    windsurf: Entity<Windsurf>,
    pending_request: Option<(u64, Task<()>)>,
    current: Option<CurrentPrediction>,
    project: Option<WeakEntity<Project>>,
}

impl WindsurfDelegate {
    pub fn new(windsurf: Entity<Windsurf>, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| this.cancel_pending_request(cx))
            .detach();
        Self {
            windsurf,
            pending_request: None,
            current: None,
            project: None,
        }
    }

    /// Lets Tab Jump open the files it may jump to.
    pub fn set_project(&mut self, project: Option<WeakEntity<Project>>) {
        self.project = project;
    }

    /// Takes over edits a Tab Jump from another editor left for `buffer`.
    fn adopt_landing(&mut self, buffer: &Entity<Buffer>, cx: &mut App) {
        if let Some(landing) = tab_jump::take_landing(buffer, cx) {
            self.current = Some(landing);
        }
    }

    fn cancel_pending_request(&mut self, cx: &mut App) {
        if let Some((request_id, _task)) = self.pending_request.take() {
            self.windsurf
                .update(cx, |windsurf, cx| windsurf.cancel_request(request_id, cx));
        }
    }

    async fn run_request(
        this: gpui::WeakEntity<Self>,
        windsurf: Entity<Windsurf>,
        buffer: Entity<Buffer>,
        cursor: Anchor,
        request_id: u64,
        debounce: Duration,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<()> {
        if !debounce.is_zero() {
            cx.background_executor().timer(debounce).await;
        }
        loop {
            let preparation = windsurf.update(cx, |windsurf, cx| {
                windsurf.prepare_request(request_id, &buffer, cursor, cx)
            });
            let prepared = match preparation {
                Preparation::Send(prepared) => prepared,
                Preparation::WaitUntil(retry_at) => {
                    wait_until(retry_at, cx).await;
                    continue;
                }
                Preparation::Skip => {
                    this.update(cx, |this, _| this.finish_pending(request_id))?;
                    return Ok(());
                }
            };
            let PreparedRequest {
                backend,
                api_key,
                input,
                snapshot,
                window,
                abs_path,
            } = prepared;
            let project = this.read_with(cx, |this, _| this.project.clone())?;
            let tab_jump = tab_jump::start(
                &windsurf,
                project,
                &buffer,
                cursor,
                request_id,
                backend.clone(),
                api_key.clone(),
                cx,
            );
            let exchange = cx.background_spawn(backend.get_tab(api_key, input)).await;

            let retry_at = match &exchange.result {
                Ok(stream) => {
                    let conversion = cx
                        .background_spawn({
                            let snapshot = snapshot.clone();
                            let stream = stream.clone();
                            let window = window.byte_range.clone();
                            async move { convert(&snapshot, window, &stream, &abs_path) }
                        })
                        .await;
                    let prediction = match conversion {
                        Ok(edits) if !edits.is_empty() => {
                            let still_valid = buffer.read_with(cx, |buffer, _| {
                                window_unchanged(&snapshot, &buffer.snapshot(), &window)
                            });
                            if still_valid {
                                let edits: Arc<[(Range<Anchor>, Arc<str>)]> = edits
                                    .into_iter()
                                    .map(|(range, text)| {
                                        (
                                            snapshot.anchor_after(range.start)
                                                ..snapshot.anchor_before(range.end),
                                            Arc::from(text),
                                        )
                                    })
                                    .collect();
                                let edit_preview = buffer
                                    .read_with(cx, |buffer, cx| {
                                        buffer.preview_edits(edits.clone(), cx)
                                    })
                                    .await;
                                Ok(Some((edits, edit_preview)))
                            } else {
                                Err("the buffer changed while the request was in flight")
                            }
                        }
                        Ok(_) => Ok(None),
                        Err(reason) => Err(reason),
                    };
                    let outcome = match &prediction {
                        Ok(Some((edits, _))) => RequestOutcome::Predicted(edits.len()),
                        Ok(None) => RequestOutcome::NoPrediction,
                        Err(reason) => RequestOutcome::Discarded(reason),
                    };
                    windsurf.update(cx, |windsurf, cx| {
                        windsurf.finish_request(request_id, &exchange, outcome, cx)
                    });
                    let jump = match (&prediction, tab_jump) {
                        (Ok(None), Some(tab_jump)) => tab_jump.await,
                        (_, Some(tab_jump)) => {
                            tab_jump.detach();
                            None
                        }
                        (_, None) => None,
                    };
                    this.update(cx, |this, cx| {
                        this.finish_pending(request_id);
                        if let Ok(Some((edits, edit_preview))) = prediction {
                            this.current = Some(CurrentPrediction {
                                id: SharedString::from(format!("windsurf-{request_id}")),
                                buffer: buffer.clone(),
                                snapshot,
                                edits,
                                edit_preview,
                                jump: None,
                            });
                        } else if let Some(jump) = jump {
                            this.current = Some(jump);
                        }
                        cx.notify();
                    })?;
                    return Ok(());
                }
                Err(_) => windsurf.update(cx, |windsurf, cx| {
                    windsurf.finish_request(request_id, &exchange, RequestOutcome::Failed, cx)
                }),
            };

            let Some(retry_at) = retry_at else {
                this.update(cx, |this, cx| {
                    this.finish_pending(request_id);
                    cx.notify();
                })?;
                return Ok(());
            };
            wait_until(retry_at, cx).await;
        }
    }

    fn finish_pending(&mut self, request_id: u64) {
        if self
            .pending_request
            .as_ref()
            .is_some_and(|(pending_id, _)| *pending_id == request_id)
        {
            self.pending_request = None;
        }
    }
}

async fn wait_until(instant: std::time::Instant, cx: &mut gpui::AsyncApp) {
    let now = cx.background_executor().now();
    cx.background_executor()
        .timer(instant.saturating_duration_since(now))
        .await;
}

/// Checks that the window the prediction rewrites still has the text it had
/// when the request was sent.
fn window_unchanged(
    request_snapshot: &BufferSnapshot,
    current: &BufferSnapshot,
    window: &crate::context::SelectionWindow,
) -> bool {
    if current.version() == request_snapshot.version() {
        return true;
    }
    let start = request_snapshot
        .anchor_before(window.byte_range.start)
        .to_offset(current);
    let end = request_snapshot
        .anchor_after(window.byte_range.end)
        .to_offset(current);
    current.text_for_range(start..end).collect::<String>() == window.text
}

fn convert(
    snapshot: &BufferSnapshot,
    window: Range<usize>,
    stream: &TabStream,
    abs_path: &str,
) -> Result<Vec<(Range<usize>, String)>, &'static str> {
    if !stream.path.is_empty() && stream.path != abs_path {
        return Err("the prediction is for a different file");
    }
    if stream.noop || stream.tab_jump || stream.new_text.is_empty() {
        return Ok(Vec::new());
    }
    prediction_edits(&snapshot.text(), window, &stream.old_text, &stream.new_text).map_err(
        |error| match error {
            PredictionError::OldTextMismatch => "the returned old text doesn't match the buffer",
            PredictionError::WindowOutOfBounds => "the request window is outside the buffer",
        },
    )
}

impl EditPredictionDelegate for WindsurfDelegate {
    fn name() -> &'static str {
        "windsurf"
    }

    fn display_name() -> &'static str {
        "Windsurf"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn supports_jump_to_edit() -> bool {
        tab_jump::is_enabled()
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::ZedPredict)
    }

    fn is_enabled(&self, buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        let settings = WindsurfSettings::get_global(cx);
        settings.enabled
            && settings.api_key.is_some()
            && crate::context::buffer_location(buffer.read(cx), cx).is_some()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce_duration: Duration,
        trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        if !self.windsurf.read(cx).can_request(cx) {
            return;
        }
        self.adopt_landing(&buffer, cx);
        if let Some(current) = &self.current
            && current.buffer == buffer
            && current.interpolate(&buffer.read(cx).snapshot()).is_some()
        {
            return;
        }
        self.cancel_pending_request(cx);
        self.windsurf
            .update(cx, |windsurf, cx| windsurf.track_buffer(&buffer, cx));

        let debounce = if trigger == EditPredictionRequestTrigger::Explicit {
            debounce_duration
        } else {
            debounce_duration.max(DEBOUNCE)
        };
        let request_id = self
            .windsurf
            .update(cx, |windsurf, _| windsurf.next_request_id());
        let windsurf = self.windsurf.clone();
        let task = cx.spawn(async move |this, cx| {
            if let Err(error) = Self::run_request(
                this,
                windsurf,
                buffer,
                cursor_position,
                request_id,
                debounce,
                cx,
            )
            .await
            {
                log::debug!("windsurf: request {request_id} ended early: {error:#}");
            }
        });
        self.pending_request = Some((request_id, task));
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        self.cancel_pending_request(cx);
        self.current = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, cx: &mut Context<Self>) {
        self.cancel_pending_request(cx);
        if let Some(current) = self.current.take() {
            tab_jump::clear_landing(&current.id, cx);
        }
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        self.adopt_landing(buffer, cx);
        let current = self.current.as_ref()?;
        if &current.buffer != buffer {
            let jump = current.jump.as_ref()?;
            let stale = jump.buffer != *buffer
                || &buffer.read(cx).version() != jump.snapshot.version()
                || !tab_jump::is_landing_pending(&current.id, cx);
            if stale {
                if jump.buffer == *buffer {
                    let id = current.id.clone();
                    tab_jump::clear_landing(&id, cx);
                    self.current = None;
                }
                return None;
            }
            return Some(EditPrediction::Jump {
                id: Some(current.id.clone()),
                snapshot: current.buffer.read(cx).snapshot(),
                target: jump.target,
            });
        }
        let snapshot = buffer.read(cx).snapshot();
        let Some(edits) = current.interpolate(&snapshot) else {
            self.current = None;
            return None;
        };
        let cursor_position = edits
            .last()
            .map(|(range, text)| PredictedCursorPosition::new(range.start, text.len()));
        Some(EditPrediction::Local {
            id: Some(current.id.clone()),
            edits,
            cursor_position,
            edit_preview: Some(current.edit_preview.clone()),
        })
    }
}
